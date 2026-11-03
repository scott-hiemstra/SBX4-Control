//! Live spectrum of what the X4 is playing, captured from its hardware
//! "What U Hear" loopback.
//!
//! Reading the loopback straight from ALSA keeps this out of PipeWire, so GNOME
//! doesn't show it as an app using the microphone. While it runs, the X4's
//! recording source is switched to What U Hear; it's restored on stop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use adw::prelude::*;
use alsa::mixer::{Mixer, SelemChannelId, SelemId};
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use gtk::{cairo, glib};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::plot::{self, LogAxis};
use crate::ui;

const PCM_DEVICE: &str = "hw:CARD=X4,DEV=0";
const MIXER_DEVICE: &str = "hw:X4";
const CHANNEL: SelemChannelId = SelemChannelId::FrontLeft;
const RATE: u32 = 48_000;
const FFT_SIZE: usize = 4096;
/// New spectrum every 512 samples: ~94 per second.
const HOP: usize = 512;
const BANDS: usize = 48;
const F_MIN: f64 = 25.0;
const F_MAX: f64 = 20_000.0;
const DB_FLOOR: f32 = -80.0;
/// Same labels as the EQ sliders.
const LABELS: [(&str, f64); 10] = [
    ("31", 31.25),
    ("62", 62.5),
    ("125", 125.0),
    ("250", 250.0),
    ("500", 500.0),
    ("1k", 1000.0),
    ("2k", 2000.0),
    ("4k", 4000.0),
    ("8k", 8000.0),
    ("16k", 16000.0),
];

/// Seconds of history the spectrogram shows.
const HISTORY_SECONDS: f64 = 2.5;
/// One spectrogram row per FFT frame.
const HISTORY: usize = (HISTORY_SECONDS * RATE as f64 / HOP as f64) as usize;
/// Content height for the spectrogram and ring; the bars use 150.
const TALL: i32 = 240;

/// How the analyzer draws the spectrum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Bars,
    Spectrogram,
    Ring,
}

impl Style {
    pub const ALL: [Style; 3] = [Style::Bars, Style::Spectrogram, Style::Ring];

    pub fn title(self) -> &'static str {
        match self {
            Style::Bars => "Bars",
            Style::Spectrogram => "Spectrogram",
            Style::Ring => "Ring",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Style::Bars => "bars",
            Style::Spectrogram => "spectrogram",
            Style::Ring => "ring",
        }
    }
}

struct Shared {
    /// Latest band levels in dBFS, of the two channels mixed.
    bands: Vec<f32>,
    /// The same for each channel, for the ring.
    left: Vec<f32>,
    right: Vec<f32>,
    /// Counts new spectra, so the spectrogram adds each one once.
    seq: u64,
    error: Option<String>,
}

struct Capture {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

pub struct Analyzer {
    pub widget: gtk::DrawingArea,
    shared: Arc<Mutex<Shared>>,
    capture: RefCell<Option<Capture>>,
    view: Rc<RefCell<View>>,
}

impl Analyzer {
    /// Captures only while the widget is on screen.
    pub fn new() -> Rc<Self> {
        let widget = gtk::DrawingArea::builder().content_height(150).hexpand(true).build();
        let shared = Arc::new(Mutex::new(Shared { bands: vec![DB_FLOOR; BANDS], left: vec![DB_FLOOR; BANDS], right: vec![DB_FLOOR; BANDS], seq: 0, error: None }));
        let view = Rc::new(RefCell::new(View::new()));

        let (v, s) = (view.clone(), shared.clone());
        widget.set_draw_func(move |_, cr, w, h| {
            let error = s.lock().unwrap().error.clone();
            let (view, w, h) = (v.borrow(), f64::from(w), f64::from(h));
            match view.style {
                Style::Bars => draw(cr, w, h, &view, error.as_deref()),
                Style::Spectrogram => draw_spectrogram(cr, w, h, &view, error.as_deref()),
                Style::Ring => draw_ring(cr, w, h, &view, error.as_deref()),
            }
        });
        let (v, s) = (view.clone(), shared.clone());
        widget.add_tick_callback(move |area, clock| {
            let (bands, left, right, seq) = {
                let s = s.lock().unwrap();
                (s.bands.clone(), s.left.clone(), s.right.clone(), s.seq)
            };
            v.borrow_mut().advance(&bands, &left, &right, seq, clock.frame_time());
            area.queue_draw();
            glib::ControlFlow::Continue
        });

        let analyzer = Rc::new(Self { widget: widget.clone(), shared, capture: RefCell::new(None), view });
        analyzer.set_style(style());
        let a = analyzer.clone();
        widget.connect_map(move |_| a.start());
        let a = analyzer.clone();
        widget.connect_unmap(move |_| a.stop());
        analyzer
    }

    fn start(&self) {
        if self.capture.borrow().is_some() {
            return;
        }
        {
            let mut s = self.shared.lock().unwrap();
            s.error = None;
            s.bands.fill(DB_FLOOR);
            s.left.fill(DB_FLOOR);
            s.right.fill(DB_FLOOR);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_flag, shared) = (stop.clone(), self.shared.clone());
        let thread = thread::spawn(move || {
            if let Err(e) = run(&stop_flag, &shared) {
                let mut s = shared.lock().unwrap();
                s.error = Some(e);
                s.bands.fill(DB_FLOOR);
                s.left.fill(DB_FLOOR);
                s.right.fill(DB_FLOOR);
            }
        });
        self.capture.replace(Some(Capture { stop, thread }));
    }

    /// Shade the range below `crossover` as the subwoofer's; None while the sub is off.
    pub fn set_crossover(&self, crossover: Option<f64>) {
        self.view.borrow_mut().crossover = crossover;
        self.widget.queue_draw();
    }

    pub fn set_style(&self, style: Style) {
        let mut view = self.view.borrow_mut();
        view.style = style;
        view.history.clear();
        self.widget.set_content_height(if style == Style::Bars { 150 } else { TALL });
        self.widget.queue_draw();
    }

    /// Stop capturing and restore the X4's recording source. Blocks briefly.
    pub fn stop(&self) {
        if let Some(capture) = self.capture.take() {
            capture.stop.store(true, Ordering::Relaxed);
            let _ = capture.thread.join();
        }
    }
}

fn run(stop: &AtomicBool, shared: &Mutex<Shared>) -> Result<(), String> {
    // Open the PCM first: if something else is recording from the X4, it's
    // busy and we leave its recording source alone.
    let pcm = open_pcm().map_err(|e| format!("Can't open the X4's capture device: {e}"))?;
    let loopback = Loopback::enable().map_err(|e| format!("Can't switch the X4 to What U Hear: {e}"))?;
    let result = capture(&pcm, stop, shared);
    loopback.restore();
    result
}

fn open_pcm() -> alsa::Result<PCM> {
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

fn capture(pcm: &PCM, stop: &AtomicBool, shared: &Mutex<Shared>) -> Result<(), String> {
    let io = pcm.io_i16().map_err(|e| e.to_string())?;
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos())
        .collect();
    // Scales a full-scale sine to 0 dBFS.
    let norm = 2.0 / window.iter().sum::<f32>();
    let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
    let bins = band_bins();

    let (mut ring_l, mut ring_r) = (vec![0f32; FFT_SIZE], vec![0f32; FFT_SIZE]);
    let mut pos = 0;
    let mut since_fft = 0;
    let mut buf = vec![0i16; 512 * 2];
    let mut spectrum = vec![Complex::default(); FFT_SIZE];

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
        for frame in buf[..frames * 2].chunks_exact(2) {
            ring_l[pos] = f32::from(frame[0]) / 32768.0;
            ring_r[pos] = f32::from(frame[1]) / 32768.0;
            pos = (pos + 1) % FFT_SIZE;
        }
        since_fft += frames;
        if since_fft < HOP {
            continue;
        }
        since_fft %= HOP;

        // Oldest sample first.
        let at = |ring: &[f32], i: usize| ring[(pos + i) % FFT_SIZE];
        let mut levels = |sample: &dyn Fn(usize) -> f32| -> Vec<f32> {
            for (i, (out, w)) in spectrum.iter_mut().zip(&window).enumerate() {
                *out = Complex::new(sample(i) * w, 0.0);
            }
            fft.process(&mut spectrum);
            bins.iter()
                .map(|&(lo, hi)| {
                    let peak = spectrum[lo..=hi].iter().map(|c| c.norm()).fold(0.0, f32::max);
                    (20.0 * (peak * norm).max(1e-9).log10()).max(DB_FLOOR)
                })
                .collect()
        };
        let bands = levels(&|i| (at(&ring_l, i) + at(&ring_r, i)) / 2.0);
        let left = levels(&|i| at(&ring_l, i));
        let right = levels(&|i| at(&ring_r, i));
        let mut s = shared.lock().unwrap();
        s.left = left;
        s.right = right;
        s.bands = bands;
        s.seq += 1;
    }
    Ok(())
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

fn band_edge(i: usize) -> f64 {
    F_MIN * (F_MAX / F_MIN).powf(i as f64 / BANDS as f64)
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

/// Displayed levels: instant rise, smooth fall, and peak markers that hold for a second.
struct Meter {
    level: Vec<f32>,
    peak: Vec<f32>,
    peak_age: Vec<f32>,
}

impl Meter {
    fn new() -> Self {
        Self { level: vec![DB_FLOOR; BANDS], peak: vec![DB_FLOOR; BANDS], peak_age: vec![0.0; BANDS] }
    }

    fn advance(&mut self, bands: &[f32], dt: f32) {
        for (i, &target) in bands.iter().enumerate().take(BANDS) {
            self.level[i] = target.max(self.level[i] - 30.0 * dt);
            if self.level[i] >= self.peak[i] {
                self.peak[i] = self.level[i];
                self.peak_age[i] = 0.0;
            } else {
                self.peak_age[i] += dt;
                if self.peak_age[i] > 1.0 {
                    self.peak[i] = (self.peak[i] - 20.0 * dt).max(self.level[i]);
                }
            }
        }
    }
}

struct View {
    style: Style,
    /// The two channels mixed: the bars and the ring's bass pulse.
    mono: Meter,
    /// Each channel, for the two halves of the ring.
    left: Meter,
    right: Meter,
    last_frame: i64,
    crossover: Option<f64>,
    /// Spectrogram rows as pixels, newest first.
    history: VecDeque<Vec<u8>>,
    last_seq: u64,
    /// Smoothed bass level for the ring's pulse, 0–1.
    bass: f32,
}

impl View {
    fn new() -> Self {
        Self {
            style: Style::Bars,
            mono: Meter::new(),
            left: Meter::new(),
            right: Meter::new(),
            last_frame: 0,
            crossover: None,
            history: VecDeque::with_capacity(HISTORY),
            last_seq: 0,
            bass: 0.0,
        }
    }

    fn advance(&mut self, bands: &[f32], left: &[f32], right: &[f32], seq: u64, frame_time_us: i64) {
        let dt = if self.last_frame == 0 { 0.0 } else { ((frame_time_us - self.last_frame) as f32 / 1e6).min(0.1) };
        self.last_frame = frame_time_us;
        if self.style == Style::Spectrogram && seq != self.last_seq {
            if self.history.len() == HISTORY {
                self.history.pop_back();
            }
            self.history.push_front(spectrogram_row(bands, &plot::Palette::current()));
        }
        self.last_seq = seq;
        self.mono.advance(bands, dt);
        if self.style == Style::Ring {
            self.left.advance(left, dt);
            self.right.advance(right, dt);
        }
        // The ring pulses with the subwoofer's range, or the lowest bass with the sub off.
        let cutoff = self.crossover.unwrap_or(100.0);
        let bass_db = (0..BANDS)
            .filter(|&i| (band_edge(i) * band_edge(i + 1)).sqrt() < cutoff)
            .map(|i| self.mono.level[i])
            .fold(DB_FLOOR, f32::max);
        let target = ((bass_db + 50.0) / 40.0).clamp(0.0, 1.0);
        // Jumps up with a kick, eases back down.
        self.bass = if target > self.bass { target } else { (self.bass - 2.5 * dt).max(target) };
    }
}

/// One spectrogram row as Cairo ARGB32 pixels. Quiet is clear, so it sits on
/// the card in light and dark; loud runs accent → sub color.
fn spectrogram_row(bands: &[f32], colors: &plot::Palette) -> Vec<u8> {
    let (a, b) = (colors.accent, colors.sub);
    bands
        .iter()
        .take(BANDS)
        .flat_map(|&db| {
            let t = loudness(db);
            let mix = ((t - 0.45) / 0.55).clamp(0.0, 1.0);
            let rgb = (a.0 + (b.0 - a.0) * mix, a.1 + (b.1 - a.1) * mix, a.2 + (b.2 - a.2) * mix);
            let alpha = t.powf(1.4);
            // Premultiplied, one native-endian 0xAARRGGBB word.
            let [r, g, b, a] = [rgb.0 * alpha, rgb.1 * alpha, rgb.2 * alpha, alpha].map(|c| (c * 255.0).round() as u32);
            (a << 24 | r << 16 | g << 8 | b).to_ne_bytes()
        })
        .collect()
}

/// 0 at the bottom of the range the new styles show, 1 at the top.
fn loudness(db: f32) -> f64 {
    f64::from(((db + 70.0) / 60.0).clamp(0.0, 1.0))
}

fn draw_error(cr: &cairo::Context, w: f64, h: f64, fg: plot::Rgb, error: &str) {
    cr.set_font_size(12.0);
    plot::set(cr, fg, 0.55);
    if let Ok(ext) = cr.text_extents(error) {
        cr.move_to(((w - ext.width()) / 2.0).max(4.0), h / 2.0);
        let _ = cr.show_text(error);
    }
}

/// Frequency across, time running down from the newest spectrum at the top.
fn draw_spectrogram(cr: &cairo::Context, w: f64, h: f64, view: &View, error: Option<&str>) {
    let colors = plot::Palette::current();
    let plot_h = h - 16.0;
    let axis = LogAxis { f_min: F_MIN, f_max: F_MAX, width: w };
    plot::freq_labels(cr, axis, h, colors.fg, &LABELS);
    if let Some(error) = error {
        draw_error(cr, w, plot_h, colors.fg, error);
        return;
    }

    // One pixel per band and spectrum, scaled up to the plot. Rows are
    // colored once, as they arrive (see `spectrogram_row`).
    let stride = cairo::Format::ARgb32.stride_for_width(BANDS as u32).unwrap_or(BANDS as i32 * 4) as usize;
    let mut pixels = vec![0u8; stride * HISTORY];
    for (row, colored) in view.history.iter().enumerate() {
        pixels[row * stride..row * stride + BANDS * 4].copy_from_slice(colored);
    }
    let Ok(image) = cairo::ImageSurface::create_for_data(pixels, cairo::Format::ARgb32, BANDS as i32, HISTORY as i32, stride as i32)
    else {
        return;
    };
    let _ = cr.save();
    cr.scale(w / BANDS as f64, plot_h / HISTORY as f64);
    let _ = cr.set_source_surface(&image, 0.0, 0.0);
    cr.source().set_filter(cairo::Filter::Good);
    let _ = cr.paint();
    let _ = cr.restore();

    // The subwoofer's range: a line at the crossover, labeled.
    if let Some(x) = view.crossover.map(|f| axis.x(f)) {
        plot::set(cr, colors.sub, 0.6);
        cr.set_line_width(1.0);
        cr.move_to(x.round() + 0.5, 0.0);
        cr.line_to(x.round() + 0.5, plot_h);
        let _ = cr.stroke();
        // Beside the line, clear of the bass under it.
        cr.set_font_size(10.0);
        plot::set(cr, colors.sub, 0.8);
        cr.move_to(x + 4.0, 12.0);
        let _ = cr.show_text("SUB");
    }
}

/// The bands as spokes around a circle, the left channel on the left half and
/// the right on the right, lows at the bottom; the circle swells with the bass.
fn draw_ring(cr: &cairo::Context, w: f64, h: f64, view: &View, error: Option<&str>) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let colors = plot::Palette::current();
    if let Some(error) = error {
        draw_error(cr, w, h, colors.fg, error);
        return;
    }
    let (cx, cy) = (w / 2.0, h / 2.0);
    let outer = (w.min(h) / 2.0 - 4.0).max(10.0);
    let bass = f64::from(view.bass);
    let inner = outer * (0.38 + 0.12 * bass);

    // A glow inside the ring that brightens with the bass.
    let glow = cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, inner);
    let (r, g, b) = colors.sub;
    glow.add_color_stop_rgba(0.0, r, g, b, 0.05 + 0.35 * bass);
    glow.add_color_stop_rgba(1.0, r, g, b, 0.0);
    let _ = cr.set_source(&glow);
    cr.arc(cx, cy, inner, 0.0, 2.0 * PI);
    let _ = cr.fill();

    plot::set(cr, colors.fg, 0.12);
    cr.set_line_width(1.0);
    cr.arc(cx, cy, inner, 0.0, 2.0 * PI);
    let _ = cr.stroke();

    // Spokes start a little outside the circle; leave room for the peak dots.
    let start = inner + 3.0;
    let reach = (outer - start - 6.0).max(4.0);
    let step = PI / BANDS as f64;
    let is_sub = |i: usize| view.crossover.is_some_and(|f| (band_edge(i) * band_edge(i + 1)).sqrt() < f);
    // Spokes and peak dashes as thin quads, filled in one pass per color.
    // (Round caps and dot-shaped peaks cost noticeably more to draw.)
    let half_width = (inner * step * 0.3).clamp(0.75, 3.0);
    let quad = |angle: f64, from: f64, to: f64| {
        let (sin, cos) = angle.sin_cos();
        let (nx, ny) = (-sin * half_width, cos * half_width);
        let (x0, y0, x1, y1) = (cx + cos * from, cy + sin * from, cx + cos * to, cy + sin * to);
        cr.move_to(x0 + nx, y0 + ny);
        cr.line_to(x1 + nx, y1 + ny);
        cr.line_to(x1 - nx, y1 - ny);
        cr.line_to(x0 - nx, y0 - ny);
        cr.close_path();
    };
    // Angle 0 points right and π/2 down, so lows start at the bottom;
    // adding to π/2 goes round the left side, subtracting the right.
    let halves = [(&view.left, 1.0), (&view.right, -1.0)];
    // A third cheaper than the default, and still smooth at this size.
    cr.set_antialias(cairo::Antialias::Fast);
    for (color, sub) in [(colors.accent, false), (colors.sub, true)] {
        for (meter, side) in halves {
            for (i, &level) in meter.level.iter().enumerate().filter(|&(i, _)| is_sub(i) == sub) {
                let len = 2.0 + (reach - 2.0) * loudness(level);
                quad(FRAC_PI_2 + side * (i as f64 + 0.5) * step, start, start + len);
            }
        }
        plot::set(cr, color, 0.85);
        let _ = cr.fill();

        for (meter, side) in halves {
            for (i, &peak) in meter.peak.iter().enumerate().filter(|&(i, _)| is_sub(i) == sub) {
                if peak > DB_FLOOR + 0.5 {
                    let rad = start + 3.0 + (reach - 2.0) * loudness(peak);
                    quad(FRAC_PI_2 + side * (i as f64 + 0.5) * step, rad, rad + 2.0);
                }
            }
        }
        plot::set(cr, color, 0.9);
        let _ = cr.fill();
    }

    // Which side is which.
    cr.set_font_size(11.0);
    plot::set(cr, colors.fg, 0.4);
    for (label, x) in [("L", cx - outer - 14.0), ("R", cx + outer + 6.0)] {
        cr.move_to(x, cy + 4.0);
        let _ = cr.show_text(label);
    }
}

fn draw(cr: &cairo::Context, w: f64, h: f64, view: &View, error: Option<&str>) {
    let colors = plot::Palette::current();
    let plot_h = h - 16.0;
    let axis = LogAxis { f_min: F_MIN, f_max: F_MAX, width: w };
    let y_for = |db: f32| plot_h * f64::from((db / DB_FLOOR).clamp(0.0, 1.0));

    // The subwoofer's range, behind everything else.
    let sub_x = view.crossover.map(|f| axis.x(f));
    if let Some(x) = sub_x {
        plot::set(cr, colors.sub, 0.07);
        cr.rectangle(0.0, 0.0, x, plot_h);
        let _ = cr.fill();
        cr.set_font_size(10.0);
        plot::set(cr, colors.sub, 0.8);
        cr.move_to(4.0, 12.0);
        let _ = cr.show_text("SUB");
    }

    plot::grid(cr, w, [-20.0, -40.0, -60.0].map(y_for), colors.fg);
    plot::freq_labels(cr, axis, h, colors.fg, &LABELS);

    if let Some(error) = error {
        cr.set_font_size(12.0);
        plot::set(cr, colors.fg, 0.55);
        if let Ok(ext) = cr.text_extents(error) {
            cr.move_to(((w - ext.width()) / 2.0).max(4.0), plot_h / 2.0);
            let _ = cr.show_text(error);
        }
        return;
    }

    let slot = w / BANDS as f64;
    let gap = (slot * 0.2).max(1.0);
    let is_sub = |i: usize| sub_x.is_some_and(|x| (i as f64 + 0.5) * slot < x);
    for (color, sub) in [(colors.accent, false), (colors.sub, true)] {
        for (i, &level) in view.mono.level.iter().enumerate().filter(|&(i, _)| is_sub(i) == sub) {
            let y = y_for(level);
            cr.rectangle(i as f64 * slot + gap / 2.0, y, slot - gap, plot_h - y);
        }
        // Brighter toward the top of the plot, so loud bands glow.
        plot::fade(cr, color, 0.0, plot_h, 1.0, 0.35);
        let _ = cr.fill();

        for (i, &peak) in view.mono.peak.iter().enumerate().filter(|&(i, _)| is_sub(i) == sub) {
            if peak > DB_FLOOR + 0.5 {
                cr.rectangle(i as f64 * slot + gap / 2.0, y_for(peak) - 1.0, slot - gap, 2.0);
            }
        }
        plot::set(cr, color, 0.9);
        let _ = cr.fill();
    }
}

/// Whether the analyzer is shown (on unless switched off).
pub fn enabled() -> bool {
    ui::get("spectrum").and_then(|v| v.as_bool()).unwrap_or(true)
}

pub fn set_enabled(on: bool) {
    ui::set("spectrum", on.into());
}

/// The chosen style (bars unless changed).
pub fn style() -> Style {
    let key = ui::get("spectrum-style");
    Style::ALL.into_iter().find(|s| key.as_ref().and_then(|v| v.as_str()) == Some(s.key())).unwrap_or(Style::Bars)
}

pub fn set_style(style: Style) {
    ui::set("spectrum-style", style.key().into());
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
}
