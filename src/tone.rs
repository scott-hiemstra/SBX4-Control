//! Test tones for tuning room EQ by ear: a steady tone or a slow bass sweep,
//! played with `pw-play` into the SBX output so they pass through the
//! crossover and room EQ like music does.

use std::f64::consts::TAU;
use std::io::{self, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

const RATE: f64 = 48_000.0;
/// About −14 dBFS: clearly audible without being a shock at normal volume.
const AMPLITUDE: f64 = 0.2;
const FADE_FRAMES: usize = 2400;
pub const SWEEP_FROM: f64 = 20.0;
pub const SWEEP_TO: f64 = 200.0;
const SWEEP_SECONDS: f64 = 40.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Tone,
    Sweep,
}

pub struct Generator {
    pub signal: Signal,
    child: Child,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    /// Current frequency as f64 bits: set for a tone, reported by a sweep.
    freq: Arc<AtomicU64>,
}

impl Generator {
    /// Start playing into `target` (a sink's node.name), or the default output.
    pub fn start(signal: Signal, freq: f64, target: Option<&str>) -> io::Result<Self> {
        let mut cmd = Command::new("pw-play");
        cmd.args(["--raw", "--rate", "48000", "--channels", "2", "--format", "f32"])
            .args(["-P", "{ media.name = \"Test tone\" node.dont-move = true }"]);
        if let Some(target) = target {
            cmd.args(["--target", target]);
        }
        let mut child = cmd.arg("-").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");

        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let start = if signal == Signal::Sweep { SWEEP_FROM } else { freq };
        let freq = Arc::new(AtomicU64::new(start.to_bits()));
        let (s, d, f) = (stop.clone(), done.clone(), freq.clone());
        thread::spawn(move || {
            let _ = generate(stdin, signal, &s, &f);
            d.store(true, Ordering::Relaxed);
        });
        Ok(Self { signal, child, stop, done, freq })
    }

    pub fn freq(&self) -> f64 {
        f64::from_bits(self.freq.load(Ordering::Relaxed))
    }

    /// Change a steady tone's frequency; glides without clicks.
    pub fn set_freq(&self, freq: f64) {
        if self.signal == Signal::Tone {
            self.freq.store(freq.to_bits(), Ordering::Relaxed);
        }
    }

    /// A sweep has played to the end (or the player went away).
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Give the fade-out a moment to reach the player, then make sure it's gone.
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(300));
            let _ = self.child.kill();
            let _ = self.child.wait();
        });
    }
}

fn generate(mut out: impl Write, signal: Signal, stop: &AtomicBool, freq: &AtomicU64) -> io::Result<()> {
    let sweep_frames = (SWEEP_SECONDS * RATE) as usize;
    let mut phase = 0.0f64;
    let mut frame = 0usize;
    let mut fade_out: Option<usize> = None;
    let mut buf = Vec::with_capacity(1024 * 8);
    loop {
        buf.clear();
        for _ in 0..1024 {
            let f = match signal {
                Signal::Tone => f64::from_bits(freq.load(Ordering::Relaxed)),
                Signal::Sweep => {
                    // Logarithmic, so each octave gets the same time.
                    let t = (frame as f64 / sweep_frames as f64).min(1.0);
                    let f = SWEEP_FROM * (SWEEP_TO / SWEEP_FROM).powf(t);
                    freq.store(f.to_bits(), Ordering::Relaxed);
                    f
                }
            };
            if fade_out.is_none() && (stop.load(Ordering::Relaxed) || (signal == Signal::Sweep && frame >= sweep_frames)) {
                fade_out = Some(FADE_FRAMES);
            }
            let fade_in = (frame as f64 / FADE_FRAMES as f64).min(1.0);
            let gain = match fade_out {
                Some(0) => {
                    out.write_all(&buf)?;
                    return out.flush();
                }
                Some(n) => {
                    fade_out = Some(n - 1);
                    fade_in * n as f64 / FADE_FRAMES as f64
                }
                None => fade_in,
            };
            let v = (AMPLITUDE * gain * phase.sin()) as f32;
            buf.extend_from_slice(&v.to_le_bytes());
            buf.extend_from_slice(&v.to_le_bytes());
            phase = (phase + TAU * f / RATE) % TAU;
            frame += 1;
        }
        out.write_all(&buf)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_ends_on_its_own_and_fades() {
        let (stop, freq) = (AtomicBool::new(false), AtomicU64::new(0));
        let mut out = Vec::new();
        generate(&mut out, Signal::Sweep, &stop, &freq).unwrap();
        let samples: Vec<f32> = out.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let seconds = samples.len() as f64 / 2.0 / RATE;
        assert!((SWEEP_SECONDS..SWEEP_SECONDS + 0.2).contains(&seconds), "{seconds}");
        assert!((f64::from_bits(freq.load(Ordering::Relaxed)) - SWEEP_TO).abs() < 0.01);
        assert!(samples.iter().all(|s| s.abs() <= AMPLITUDE as f32 + 1e-6));
        assert!(samples[0].abs() < 1e-6 && samples.last().unwrap().abs() < 1e-3);
    }
}
