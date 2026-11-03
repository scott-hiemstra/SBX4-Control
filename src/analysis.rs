//! Audio capture and analysis shared by the control panel's analyzer and the
//! visualizer.
//!
//! Audio comes from the X4's hardware "What U Hear" loopback, read straight
//! from ALSA so GNOME doesn't show an app using the microphone; while it runs
//! the X4's recording source is switched to What U Hear and restored on stop.
//! Where the caller allows it, and the X4 isn't there or is busy, it comes from
//! whatever PipeWire is playing instead, through `pw-record`.
//!
//! Every hop (512 samples, ~94 per second) yields a [`Frame`]: 48 log-spaced
//! bands for the two channels mixed and for each channel, and the spectral
//! flux of four frequency lanes for onset detection.

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use alsa::mixer::{Mixer, SelemChannelId, SelemId};
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

const PCM_DEVICE: &str = "hw:CARD=X4,DEV=0";
const MIXER_DEVICE: &str = "hw:X4";
const CHANNEL: SelemChannelId = SelemChannelId::FrontLeft;

pub const RATE: u32 = 48_000;
const FFT_SIZE: usize = 4096;
/// Shorter window for onsets: sharper in time than the spectrum's.
const ONSET_FFT: usize = 2048;
/// New frame every 512 samples: ~94 per second.
pub const HOP: usize = 512;
/// Seconds between frames.
pub const FRAME_SECONDS: f64 = HOP as f64 / RATE as f64;
pub const BANDS: usize = 48;
pub const F_MIN: f64 = 25.0;
pub const F_MAX: f64 = 20_000.0;
pub const DB_FLOOR: f32 = -80.0;
/// Frames kept for a consumer that hasn't drained them: ~2.7 seconds.
const MAX_QUEUED: usize = 256;

/// Lower edge of band `i`; band `i` spans `band_edge(i)..band_edge(i + 1)`.
pub fn band_edge(i: usize) -> f64 {
    F_MIN * (F_MAX / F_MIN).powf(i as f64 / BANDS as f64)
}

/// Geometric center of band `i`.
pub fn band_center(i: usize) -> f64 {
    (band_edge(i) * band_edge(i + 1)).sqrt()
}

/// Frequency ranges that onsets and rhythms are tracked in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    Bass,
    LowMid,
    HighMid,
    Treble,
}

pub const LANES: usize = 4;

impl Lane {
    pub const ALL: [Lane; LANES] = [Lane::Bass, Lane::LowMid, Lane::HighMid, Lane::Treble];

    pub fn title(self) -> &'static str {
        match self {
            Lane::Bass => "Bass",
            Lane::LowMid => "Low Mids",
            Lane::HighMid => "High Mids",
            Lane::Treble => "Treble",
        }
    }

    /// Frequency range in Hz.
    pub fn range(self) -> (f64, f64) {
        match self {
            Lane::Bass => (20.0, 150.0),
            Lane::LowMid => (150.0, 800.0),
            Lane::HighMid => (800.0, 4000.0),
            Lane::Treble => (4000.0, 16000.0),
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// One hop's analysis.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Seconds since capture started, at the center of the onset window: about
    /// when a transient in it was heard.
    pub time: f64,
    /// Band levels in dBFS of the two channels mixed.
    pub bands: Vec<f32>,
    /// The same for each channel.
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Spectral flux per lane: the average rise in dB across the lane's
    /// frequency bins since the previous frame. Transients show as spikes.
    pub flux: [f32; LANES],
}

/// Where audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    X4Loopback,
    PipeWire,
}

impl Source {
    pub fn title(self) -> &'static str {
        match self {
            Source::X4Loopback => "Sound Blaster X4 (What U Hear)",
            Source::PipeWire => "PipeWire (default output)",
        }
    }
}

/// Which sources a capture may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sources {
    X4Only,
    /// The X4 when it's there and free, PipeWire otherwise.
    X4ThenPipeWire,
}

#[derive(Default)]
struct Shared {
    frames: VecDeque<Frame>,
    error: Option<String>,
    source: Option<Source>,
}

/// A running capture. Stops, and restores the X4's recording source, when dropped.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
}

impl Capture {
    pub fn start(sources: Sources) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (stop_flag, s) = (stop.clone(), shared.clone());
        let thread = thread::spawn(move || {
            if let Err(e) = run(sources, &stop_flag, &s) {
                s.lock().unwrap().error = Some(e);
            }
        });
        Self { stop, thread: Some(thread), shared }
    }

    /// Frames analyzed since the last call, oldest first.
    pub fn drain(&self) -> Vec<Frame> {
        self.shared.lock().unwrap().frames.drain(..).collect()
    }

    /// Why capturing stopped, if it did.
    pub fn error(&self) -> Option<String> {
        self.shared.lock().unwrap().error.clone()
    }

    /// The source in use, once one has opened.
    pub fn source(&self) -> Option<Source> {
        self.shared.lock().unwrap().source
    }
}

impl Drop for Capture {
    /// Blocks briefly while the capture thread finishes.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(sources: Sources, stop: &AtomicBool, shared: &Mutex<Shared>) -> Result<(), String> {
    // Open the PCM first: if something else is recording from the X4, it's
    // busy and we leave its recording source alone.
    match open_x4() {
        Ok(pcm) => {
            let loopback = Loopback::enable().map_err(|e| format!("Can't switch the X4 to What U Hear: {e}"))?;
            shared.lock().unwrap().source = Some(Source::X4Loopback);
            let result = capture_x4(&pcm, stop, shared);
            loopback.restore();
            result
        }
        Err(e) if sources == Sources::X4Only => Err(format!("Can't open the X4's capture device: {e}")),
        Err(_) => {
            shared.lock().unwrap().source = Some(Source::PipeWire);
            capture_pipewire(stop, shared)
        }
    }
}

fn open_x4() -> alsa::Result<PCM> {
    let pcm = PCM::new(PCM_DEVICE, Direction::Capture, false)?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_channels(2)?;
        hwp.set_rate(RATE, ValueOr::Nearest)?;
        hwp.set_period_size_near(512, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(8192)?;
        pcm.hw_params(&hwp)?;
    }
    Ok(pcm)
}

fn capture_x4(pcm: &PCM, stop: &AtomicBool, shared: &Mutex<Shared>) -> Result<(), String> {
    let io = pcm.io_i16().map_err(|e| e.to_string())?;
    let mut dsp = Dsp::new();
    let mut buf = vec![0i16; 512 * 2];
    // Capture otherwise only starts on the first read, and we wait before reading.
    pcm.start().map_err(|e| e.to_string())?;
    while !stop.load(Ordering::Relaxed) {
        // Time out regularly so a stalled device can't block stopping.
        if !pcm.wait(Some(100)).map_err(|e| e.to_string())? {
            continue;
        }
        let frames = match io.readi(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                pcm.try_recover(e, true).map_err(|e| e.to_string())?;
                continue;
            }
        };
        dsp.push(&buf[..frames * 2], shared);
    }
    Ok(())
}

/// Records the default output's monitor with `pw-record`, as raw 16-bit
/// stereo on its stdout. PipeWire keeps sending (silence included), so the
/// blocking read returns often enough to notice `stop`.
fn capture_pipewire(stop: &AtomicBool, shared: &Mutex<Shared>) -> Result<(), String> {
    let mut cmd = Command::new("pw-record");
    cmd.args(["-P", "stream.capture.sink=true", "-P", "node.name=sbx4-visualizer", "-P", "media.name=Visualizer"])
        .args(["--format", "s16", "--rate", &RATE.to_string(), "--channels", "2", "--latency", "10ms", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Take the recorder down with us if the app dies.
    unsafe {
        cmd.pre_exec(|| nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM).map_err(std::io::Error::from));
    }
    let mut child = cmd.spawn().map_err(|e| format!("Can't record from PipeWire (needs pw-record): {e}"))?;
    let mut out = child.stdout.take().ok_or("pw-record has no output")?;

    let mut dsp = Dsp::new();
    let mut bytes = vec![0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();
    let mut samples: Vec<i16> = Vec::new();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let n = match out.read(&mut bytes) {
            Ok(0) => break Err("PipeWire stopped recording".to_owned()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(e.to_string()),
        };
        // Whole stereo frames only; keep any partial one for the next read.
        pending.extend_from_slice(&bytes[..n]);
        let whole = pending.len() / 4 * 4;
        samples.clear();
        samples.extend(pending[..whole].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        pending.drain(..whole);
        dsp.push(&samples, shared);
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Analyze interleaved 16-bit stereo samples already in memory, as a capture
/// would: for tests and offline checks.
pub fn analyze(interleaved: &[i16]) -> Vec<Frame> {
    let shared = Mutex::new(Shared::default());
    let mut dsp = Dsp::new();
    let mut frames = Vec::new();
    // In chunks, so the queue limit never drops any.
    for chunk in interleaved.chunks(HOP * 2 * 64) {
        dsp.push(chunk, &shared);
        frames.extend(shared.lock().unwrap().frames.drain(..));
    }
    frames
}

/// Turns interleaved stereo samples into [`Frame`]s.
struct Dsp {
    ring_l: Vec<f32>,
    ring_r: Vec<f32>,
    pos: usize,
    since_frame: usize,
    /// Samples (per channel) seen since capture started.
    total: u64,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Scales a full-scale sine to 0 dBFS.
    norm: f32,
    bins: Vec<(usize, usize)>,
    onset_fft: Arc<dyn Fft<f32>>,
    onset_window: Vec<f32>,
    onset_norm: f32,
    /// Bin range of each lane in the onset spectrum.
    lane_bins: [(usize, usize); LANES],
    prev_db: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    onset_spectrum: Vec<Complex<f32>>,
}

fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect()
}

impl Dsp {
    fn new() -> Self {
        let mut planner = FftPlanner::new();
        let window = hann(FFT_SIZE);
        let onset_window = hann(ONSET_FFT);
        let df = f64::from(RATE) / ONSET_FFT as f64;
        let lane_bins = Lane::ALL.map(|lane| {
            let (lo, hi) = lane.range();
            let a = ((lo / df).ceil() as usize).max(1);
            (a, ((hi / df).floor() as usize).max(a))
        });
        Self {
            ring_l: vec![0.0; FFT_SIZE],
            ring_r: vec![0.0; FFT_SIZE],
            pos: 0,
            since_frame: 0,
            total: 0,
            fft: planner.plan_fft_forward(FFT_SIZE),
            norm: 2.0 / window.iter().sum::<f32>(),
            window,
            bins: band_bins(),
            onset_fft: planner.plan_fft_forward(ONSET_FFT),
            onset_norm: 2.0 / onset_window.iter().sum::<f32>(),
            onset_window,
            lane_bins,
            prev_db: vec![DB_FLOOR; ONSET_FFT / 2],
            spectrum: vec![Complex::default(); FFT_SIZE],
            onset_spectrum: vec![Complex::default(); ONSET_FFT],
        }
    }

    fn push(&mut self, interleaved: &[i16], shared: &Mutex<Shared>) {
        for frame in interleaved.chunks_exact(2) {
            self.ring_l[self.pos] = f32::from(frame[0]) / 32768.0;
            self.ring_r[self.pos] = f32::from(frame[1]) / 32768.0;
            self.pos = (self.pos + 1) % FFT_SIZE;
            self.total += 1;
            self.since_frame += 1;
            if self.since_frame == HOP {
                self.since_frame = 0;
                let frame = self.analyze();
                let mut s = shared.lock().unwrap();
                if s.frames.len() == MAX_QUEUED {
                    s.frames.pop_front();
                }
                s.frames.push_back(frame);
            }
        }
    }

    /// Sample `i` of the last `n`, oldest first.
    fn at(&self, ring: &[f32], n: usize, i: usize) -> f32 {
        ring[(self.pos + FFT_SIZE - n + i) % FFT_SIZE]
    }

    fn bands_of(&mut self, channel: impl Fn(&Self, usize) -> f32) -> Vec<f32> {
        let mut spectrum = std::mem::take(&mut self.spectrum);
        for (i, out) in spectrum.iter_mut().enumerate() {
            *out = Complex::new(channel(self, i) * self.window[i], 0.0);
        }
        self.fft.process(&mut spectrum);
        let bands = self
            .bins
            .iter()
            .map(|&(lo, hi)| {
                let peak = spectrum[lo..=hi].iter().map(|c| c.norm()).fold(0.0, f32::max);
                (20.0 * (peak * self.norm).max(1e-9).log10()).max(DB_FLOOR)
            })
            .collect();
        self.spectrum = spectrum;
        bands
    }

    fn analyze(&mut self) -> Frame {
        let n = FFT_SIZE;
        let bands = self.bands_of(|d, i| (d.at(&d.ring_l, n, i) + d.at(&d.ring_r, n, i)) / 2.0);
        let left = self.bands_of(|d, i| d.at(&d.ring_l, n, i));
        let right = self.bands_of(|d, i| d.at(&d.ring_r, n, i));

        // Onsets: how much each lane's bins rose since the last frame.
        let m = ONSET_FFT;
        let mut spectrum = std::mem::take(&mut self.onset_spectrum);
        for (i, out) in spectrum.iter_mut().enumerate() {
            let s = (self.at(&self.ring_l, m, i) + self.at(&self.ring_r, m, i)) / 2.0;
            *out = Complex::new(s * self.onset_window[i], 0.0);
        }
        self.onset_fft.process(&mut spectrum);
        let mut flux = [0f32; LANES];
        for (lane, &(lo, hi)) in self.lane_bins.iter().enumerate() {
            let mut rise = 0.0;
            let hi = hi.min(m / 2 - 1);
            for (bin, prev) in spectrum[lo..=hi].iter().zip(&mut self.prev_db[lo..=hi]) {
                let db = (20.0 * (bin.norm() * self.onset_norm).max(1e-9).log10()).max(DB_FLOOR);
                rise += (db - *prev).max(0.0);
                *prev = db;
            }
            flux[lane] = rise / (hi - lo + 1) as f32;
        }
        self.onset_spectrum = spectrum;

        let time = (self.total as f64 - ONSET_FFT as f64 / 2.0).max(0.0) / f64::from(RATE);
        Frame { time, bands, left, right, flux }
    }
}

/// FFT bin range for each log-spaced band. Low bands narrower than one bin
/// use the bin nearest their center.
fn band_bins() -> Vec<(usize, usize)> {
    let df = f64::from(RATE) / FFT_SIZE as f64;
    (0..BANDS)
        .map(|i| {
            let (lo, hi) = (band_edge(i), band_edge(i + 1));
            let (a, b) = ((lo / df).ceil() as usize, (hi / df).floor() as usize);
            if a <= b {
                (a, b)
            } else {
                let center = ((lo * hi).sqrt() / df).round() as usize;
                (center, center)
            }
        })
        .collect()
}

/// Switches the X4's recording source to What U Hear and remembers what to put back.
struct Loopback {
    mixer: Mixer,
    source: Option<u32>,
    switch: Option<i32>,
}

impl Loopback {
    fn enable() -> alsa::Result<Self> {
        let mixer = Mixer::new(MIXER_DEVICE, false)?;
        let (mut source, mut switch) = (None, None);
        if let Some(s) = mixer.find_selem(&SelemId::new("PCM Capture Source", 0)) {
            let index = s.iter_enum()?.position(|name| name.is_ok_and(|n| n == "What U Hear"));
            if let Some(index) = index {
                source = s.get_enum_item(CHANNEL).ok();
                s.set_enum_item(CHANNEL, index as u32)?;
            }
        }
        if let Some(s) = mixer.find_selem(&SelemId::new("What U Hear", 0)) {
            switch = s.get_capture_switch(CHANNEL).ok();
            s.set_capture_switch_all(1)?;
        }
        Ok(Self { mixer, source, switch })
    }

    fn restore(&self) {
        if let (Some(item), Some(s)) = (self.source, self.mixer.find_selem(&SelemId::new("PCM Capture Source", 0))) {
            let _ = s.set_enum_item(CHANNEL, item);
        }
        if let (Some(on), Some(s)) = (self.switch, self.mixer.find_selem(&SelemId::new("What U Hear", 0))) {
            let _ = s.set_capture_switch_all(on);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_cover_the_spectrum_in_order() {
        let bins = band_bins();
        assert_eq!(bins.len(), BANDS);
        assert!(bins.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(bins.last().unwrap().1 < FFT_SIZE / 2);
    }

    /// A burst of noise after silence shows as flux in every lane, and steady
    /// sound afterwards doesn't.
    #[test]
    fn a_transient_spikes_the_flux() {
        let shared = Mutex::new(Shared::default());
        let mut dsp = Dsp::new();
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 16) as i16) / 4
        };
        let silence = vec![0i16; RATE as usize / 2 * 2];
        dsp.push(&silence, &shared);
        let burst: Vec<i16> = (0..RATE as usize).flat_map(|_| {
            let s = noise();
            [s, s]
        }).collect();
        dsp.push(&burst, &shared);
        let frames: Vec<Frame> = shared.lock().unwrap().frames.drain(..).collect();
        let peak = |lane: usize| frames.iter().map(|f| f.flux[lane]).fold(0.0, f32::max);
        for lane in 0..LANES {
            assert!(peak(lane) > 10.0, "lane {lane} peak {}", peak(lane));
        }
        // The last half second is steady noise.
        let tail = &frames[frames.len() - 40..];
        let steady = tail.iter().map(|f| f.flux.iter().sum::<f32>() / LANES as f32).sum::<f32>() / tail.len() as f32;
        assert!(steady < 6.0, "steady flux {steady}");
        // Times run at one hop per frame, once a full onset window is in.
        let warm = ONSET_FFT / HOP;
        assert!(frames[warm..].windows(2).all(|w| ((w[1].time - w[0].time) - FRAME_SECONDS).abs() < 1e-9));
    }
}
