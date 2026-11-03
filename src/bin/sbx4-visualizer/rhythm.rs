//! Rhythm analysis: onsets in each frequency lane, tempo and beat phase, a
//! meter estimate, and cautious pattern flags.
//!
//! It runs on [`Frame`]s in stream time (seconds since capture started), so
//! it's independent of the display. What it reports leans conservative:
//! onsets are measured, tempo and meter are estimates with a confidence, and
//! pattern flags only appear when the evidence is clear.

use std::collections::VecDeque;

use sbx4_control::analysis::{Frame, Lane, FRAME_SECONDS, LANES};

/// The longest history the display can show, in seconds.
pub const MAX_HISTORY: f64 = 32.0;
/// Onsets older than this are dropped.
const KEEP: f64 = MAX_HISTORY + 8.0;
/// Seconds of onset envelope the tempo is estimated from.
const TEMPO_WINDOW: f64 = 10.0;
/// One lane can't have onsets closer together than this.
const MIN_INTERVAL: f64 = 0.07;
const MIN_BPM: f64 = 50.0;
const MAX_BPM: f64 = 200.0;
/// A detected tempo needs this confidence to be shown at all…
const TEMPO_START: f32 = 0.25;
/// …and is dropped when unconfirmed estimates fade it below this.
const TEMPO_DROP: f32 = 0.1;
/// No meter is estimated on a tempo less certain than this.
const METER_MIN_TEMPO: f32 = 0.3;
/// Grid steps per beat used for pattern analysis (sixteenths in 4/4).
const STEPS: f64 = 4.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Flags {
    /// Part of a rhythm that repeated from the previous bar.
    pub repeating: bool,
    /// Accent off the beat, with the next beat unaccented.
    pub syncopated: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Onset {
    pub time: f64,
    pub lane: Lane,
    /// 0–1, how far above the detection threshold it rose.
    pub strength: f32,
    pub flags: Flags,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TempoSource {
    Detected,
    Manual,
}

/// Beat `n` falls at `anchor + n * period`.
#[derive(Clone, Copy, Debug)]
pub struct Tempo {
    pub bpm: f64,
    pub anchor: f64,
    /// 0–1. For a manual tempo, how well the beat phase fits.
    pub confidence: f32,
    pub source: TempoSource,
}

impl Tempo {
    pub fn period(&self) -> f64 {
        60.0 / self.bpm
    }

    /// Beats since the anchor, fractional.
    pub fn beat_at(&self, time: f64) -> f64 {
        (time - self.anchor) / self.period()
    }

    pub fn time_of(&self, beat: f64) -> f64 {
        self.anchor + beat * self.period()
    }

    /// Beat numbers and times in `from..=to`.
    pub fn beats(&self, from: f64, to: f64) -> impl Iterator<Item = (i64, f64)> + '_ {
        let first = self.beat_at(from).ceil() as i64;
        let last = self.beat_at(to).floor() as i64;
        (first..=last).map(move |n| (n, self.time_of(n as f64)))
    }
}

/// How the bar length is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeterChoice {
    Auto,
    /// `beats` detected beats per bar; `unit` is only for the label.
    Fixed { beats: u32, unit: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Meter {
    pub beats: u32,
    pub unit: u32,
    /// The number of a beat that starts a bar.
    pub downbeat: i64,
    /// Detected rather than chosen.
    pub estimated: bool,
    /// 0–1; only meaningful when estimated.
    pub confidence: f32,
}

impl Meter {
    pub fn is_downbeat(&self, beat: i64) -> bool {
        (beat - self.downbeat).rem_euclid(i64::from(self.beats)) == 0
    }

    /// Which bar beat `beat` is in, counting from the downbeat.
    pub fn bar_of(&self, beat: i64) -> i64 {
        (beat - self.downbeat).div_euclid(i64::from(self.beats))
    }

    /// The first beat of bar `bar`.
    pub fn bar_start(&self, bar: i64) -> i64 {
        self.downbeat + bar * i64::from(self.beats)
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.beats, self.unit)
    }
}

/// Finds onsets in one lane's flux: local peaks above an adaptive threshold
/// (the last second's mean plus some standard deviations, with a floor).
struct LaneDetector {
    window: VecDeque<f32>,
    prev: f32,
    prev2: f32,
    prev_time: f64,
    last_onset: f64,
}

impl LaneDetector {
    fn new() -> Self {
        Self { window: VecDeque::new(), prev: 0.0, prev2: 0.0, prev_time: 0.0, last_onset: f64::NEG_INFINITY }
    }

    /// Returns an onset at the previous frame if it was a peak; one frame of delay.
    fn push(&mut self, time: f64, flux: f32, sensitivity: f32) -> Option<(f64, f32)> {
        let n = self.window.len().max(1) as f32;
        let mean = self.window.iter().sum::<f32>() / n;
        let sd = (self.window.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
        // More sensitive: fewer deviations above the mean, and a lower floor.
        let k = 3.0 - 2.4 * sensitivity;
        let floor = 1.0 + 4.0 * (1.0 - sensitivity);
        let threshold = (mean + k * sd).max(floor);

        let peak = self.prev > self.prev2 && self.prev >= flux && self.prev > threshold;
        let onset = (peak && self.prev_time - self.last_onset >= MIN_INTERVAL).then(|| {
            self.last_onset = self.prev_time;
            let over = (self.prev - threshold) / threshold.max(1.0);
            (self.prev_time, (1.0 - (-1.5 * over).exp()).clamp(0.05, 1.0))
        });

        self.window.push_back(self.prev);
        if self.window.len() > (1.0 / FRAME_SECONDS) as usize {
            self.window.pop_front();
        }
        self.prev2 = self.prev;
        self.prev = flux;
        self.prev_time = time;
        onset
    }
}

pub struct Rhythm {
    sensitivity: f32,
    manual_bpm: Option<f64>,
    meter_choice: MeterChoice,
    detectors: [LaneDetector; LANES],
    pub onsets: VecDeque<Onset>,
    /// Onset strength over time: weighted flux of all lanes, minus its running mean.
    envelope: VecDeque<f32>,
    envelope_mean: f32,
    now: f64,
    tempo: Option<Tempo>,
    /// When the current beat grid started; the meter waits for it to settle.
    tempo_since: f64,
    /// A different detected tempo, and how many estimates in a row agreed on it.
    candidate: Option<(f64, u32)>,
    /// Where a tap placed a beat, to start the phase from.
    tap_anchor: Option<f64>,
    taps: Vec<f64>,
    meter: Option<Meter>,
    poly: [Option<(u32, u32)>; LANES],
    next_tempo: f64,
    next_meter: f64,
    next_poly: f64,
    /// Bars up to this one have been checked for repeats.
    bars_checked: Option<i64>,
    /// Onsets before this time have been checked for syncopation.
    sync_checked: f64,
}

impl Rhythm {
    pub fn new() -> Self {
        Self {
            sensitivity: 0.5,
            manual_bpm: None,
            meter_choice: MeterChoice::Auto,
            detectors: [(); LANES].map(|_| LaneDetector::new()),
            onsets: VecDeque::new(),
            envelope: VecDeque::new(),
            envelope_mean: 0.0,
            now: 0.0,
            tempo: None,
            tempo_since: 0.0,
            candidate: None,
            tap_anchor: None,
            taps: Vec::new(),
            meter: None,
            poly: [None; LANES],
            next_tempo: 0.0,
            next_meter: 0.0,
            next_poly: 0.0,
            bars_checked: None,
            sync_checked: 0.0,
        }
    }

    /// Forget everything heard so far, for a new capture.
    pub fn reset(&mut self) {
        let (sensitivity, manual, meter) = (self.sensitivity, self.manual_bpm, self.meter_choice);
        *self = Self::new();
        self.sensitivity = sensitivity;
        self.manual_bpm = manual;
        self.meter_choice = meter;
    }

    /// 0 (only strong transients) to 1 (weak ones too).
    pub fn set_sensitivity(&mut self, sensitivity: f32) {
        self.sensitivity = sensitivity.clamp(0.0, 1.0);
    }

    /// Use this tempo instead of the detected one; the phase is still tracked.
    pub fn set_manual_bpm(&mut self, bpm: Option<f64>) {
        if self.manual_bpm != bpm {
            self.manual_bpm = bpm.map(|b| b.clamp(20.0, 400.0));
            self.retime();
        }
    }

    pub fn set_meter(&mut self, choice: MeterChoice) {
        if self.meter_choice != choice {
            self.meter_choice = choice;
            self.meter = None;
            self.next_meter = 0.0;
            self.bars_checked = None;
        }
    }

    /// A tap at stream time `time`. Returns the tapped tempo once there are
    /// two taps less than two seconds apart; the caller sets it as the manual tempo.
    pub fn tap(&mut self, time: f64) -> Option<f64> {
        if self.taps.last().is_some_and(|&t| time - t > 2.0 || time <= t) {
            self.taps.clear();
        }
        self.taps.push(time);
        if self.taps.len() > 8 {
            self.taps.remove(0);
        }
        if self.taps.len() < 2 {
            return None;
        }
        let span = self.taps.last()? - self.taps.first()?;
        let bpm = (60.0 * (self.taps.len() - 1) as f64 / span).clamp(30.0, 300.0);
        self.tap_anchor = Some(time);
        Some(bpm)
    }

    pub fn now(&self) -> f64 {
        self.now
    }

    pub fn tempo(&self) -> Option<Tempo> {
        self.tempo
    }

    pub fn meter(&self) -> Option<Meter> {
        self.meter
    }

    /// A suspected polyrhythm in `lane` against the beat, as (hits, beats).
    pub fn polyrhythm(&self, lane: Lane) -> Option<(u32, u32)> {
        self.poly[lane.index()]
    }

    pub fn push(&mut self, frame: &Frame) {
        self.now = frame.time;
        for lane in Lane::ALL {
            let i = lane.index();
            if let Some((time, strength)) = self.detectors[i].push(frame.time, frame.flux[i], self.sensitivity) {
                self.onsets.push_back(Onset { time, lane, strength, flags: Flags::default() });
            }
        }
        while self.onsets.front().is_some_and(|o| o.time < self.now - KEEP) {
            self.onsets.pop_front();
        }

        // Lower lanes carry the beat more often than cymbals do.
        let weights = [1.0, 0.8, 0.6, 0.4];
        let e: f32 = frame.flux.iter().zip(weights).map(|(f, w)| f * w).sum();
        self.envelope_mean += (e - self.envelope_mean) * 0.04;
        self.envelope.push_back((e - self.envelope_mean).max(0.0));
        if self.envelope.len() > (TEMPO_WINDOW / FRAME_SECONDS) as usize {
            self.envelope.pop_front();
        }

        if self.now >= self.next_tempo {
            self.next_tempo = self.now + 0.5;
            self.update_tempo();
        }
        if self.now >= self.next_meter {
            self.next_meter = self.now + 2.0;
            self.update_meter();
        }
        if self.now >= self.next_poly {
            self.next_poly = self.now + 1.0;
            self.update_polyrhythms();
        }
        self.mark_repeats();
        self.mark_syncopation();
    }

    /// The manual tempo changed: restart the grid from it (or from detection).
    fn retime(&mut self) {
        self.tempo = None;
        self.candidate = None;
        self.meter = None;
        self.bars_checked = None;
        self.next_tempo = 0.0;
        self.next_meter = 0.0;
    }

    fn update_tempo(&mut self) {
        let enough = self.envelope.len() as f64 * FRAME_SECONDS >= 4.0;
        let bpm = match self.manual_bpm {
            Some(bpm) => bpm,
            None => {
                if !enough {
                    return;
                }
                let Some((bpm, confidence)) = estimate_bpm(&self.envelope) else {
                    self.fade_tempo();
                    return;
                };
                match (&mut self.tempo, self.candidate) {
                    // Too weak to trust: what we have fades, or nothing starts.
                    (Some(_), _) if confidence < TEMPO_DROP => {
                        self.fade_tempo();
                        return;
                    }
                    (None, _) if confidence < TEMPO_START => return,
                    // Half or double the tempo is the same pulse counted differently:
                    // keep counting it the way we were rather than start over.
                    (Some(tempo), _) if [0.5, 2.0].iter().any(|r| (bpm / tempo.bpm / r - 1.0).abs() < 0.04) => return,
                    (Some(tempo), _) if (bpm / tempo.bpm - 1.0).abs() < 0.04 => {
                        tempo.bpm += (bpm - tempo.bpm) * 0.2;
                        tempo.confidence = confidence;
                        tempo.bpm
                    }
                    // A new tempo has to show up twice in a row before it replaces the old one.
                    (Some(_), Some((c, n))) if (bpm / c - 1.0).abs() < 0.04 && n >= 1 && confidence >= TEMPO_START => {
                        self.candidate = None;
                        self.tempo = None;
                        self.meter = None;
                        self.bars_checked = None;
                        bpm
                    }
                    (Some(_), _) => {
                        self.fade_tempo();
                        let n = self.candidate.filter(|&(c, _)| (bpm / c - 1.0).abs() < 0.04).map_or(0, |(_, n)| n);
                        self.candidate = Some((bpm, n + 1));
                        return;
                    }
                    (None, _) => bpm,
                }
            }
        };
        if !enough && self.tap_anchor.is_none() {
            // Manual tempo before there's audio to find the phase in.
            if self.tempo.is_none() {
                self.tempo_since = self.now;
                self.tempo = Some(Tempo { bpm, anchor: self.now, confidence: 0.0, source: TempoSource::Manual });
            }
            return;
        }
        let period = 60.0 / bpm;
        let (anchor, fit) = match self.tap_anchor.take() {
            Some(t) => (t, 1.0),
            None if enough => estimate_phase(&self.envelope, self.now, period),
            None => (self.now, 0.0),
        };
        let source = if self.manual_bpm.is_some() { TempoSource::Manual } else { TempoSource::Detected };
        let confidence = match source {
            TempoSource::Manual => fit,
            TempoSource::Detected => self.tempo.map_or(fit, |t| t.confidence),
        };
        self.tempo = Some(match self.tempo {
            // Ease toward the new phase, keeping beat numbers where they were.
            Some(old) => {
                let n = old.beat_at(anchor).round();
                let predicted = old.time_of(n);
                let mut err = anchor - predicted;
                err -= period * (err / period).round();
                let beat_time = predicted + 0.35 * err;
                Tempo { bpm, anchor: beat_time - n * period, confidence, source }
            }
            None => {
                self.tempo_since = self.now;
                Tempo { bpm, anchor, confidence, source }
            }
        });
    }

    /// A detected tempo that estimates no longer confirm loses confidence,
    /// and goes once it's below [`TEMPO_DROP`].
    fn fade_tempo(&mut self) {
        let Some(tempo) = self.tempo.as_mut().filter(|t| t.source == TempoSource::Detected) else { return };
        tempo.confidence *= 0.85;
        if tempo.confidence < TEMPO_DROP {
            self.tempo = None;
            self.meter = None;
            self.bars_checked = None;
        }
    }

    /// How strongly beat `beat` is accented by the bass and low mids.
    fn accent(&self, tempo: &Tempo, beat: i64) -> f32 {
        let t = tempo.time_of(beat as f64);
        let tolerance = (tempo.period() * 0.15).min(0.07);
        self.onsets
            .iter()
            .filter(|o| (o.time - t).abs() <= tolerance)
            .map(|o| match o.lane {
                Lane::Bass => o.strength,
                Lane::LowMid => o.strength * 0.6,
                _ => 0.0,
            })
            .fold(0.0, f32::max)
    }

    fn update_meter(&mut self) {
        let Some(tempo) = self.tempo else { return };
        if tempo.source == TempoSource::Detected && tempo.confidence < METER_MIN_TEMPO && self.meter_choice == MeterChoice::Auto {
            return;
        }
        let to = tempo.beat_at(self.now - 0.1).floor() as i64;
        let from = tempo.beat_at(self.now - 16.0).ceil() as i64;
        let beats: Vec<(i64, f32)> = (from..=to).map(|b| (b, self.accent(&tempo, b))).collect();

        // Contrast between the best-accented position in a cycle of `n` beats and the rest.
        let score = |n: u32| -> Option<(f32, i64)> {
            let n64 = i64::from(n);
            if beats.len() < 3 * n as usize {
                return None;
            }
            (0..n64)
                .map(|p| {
                    let (on, off): (Vec<_>, Vec<_>) = beats.iter().partition(|(b, _)| b.rem_euclid(n64) == p);
                    let mean = |v: &[&(i64, f32)]| v.iter().map(|(_, a)| a).sum::<f32>() / v.len().max(1) as f32;
                    (mean(&on) - mean(&off), p)
                })
                .max_by(|a, b| a.0.total_cmp(&b.0))
        };

        match self.meter_choice {
            MeterChoice::Fixed { beats: n, unit } => {
                let downbeat = score(n).filter(|&(s, _)| s > 0.05).map_or(0, |(_, p)| p);
                // Keep a found downbeat rather than jumping back to 0 when the evidence fades.
                let downbeat = match self.meter {
                    Some(m) if m.beats == n && score(n).map_or(true, |(s, _)| s <= 0.05) => m.downbeat,
                    _ => downbeat,
                };
                self.meter = Some(Meter { beats: n, unit, downbeat, estimated: false, confidence: 1.0 });
            }
            MeterChoice::Auto => {
                // Wait for the beat grid to settle before reading accents off it.
                if self.now - self.tempo_since < 4.0 {
                    return;
                }
                // An estimate that isn't confirmed fades out rather than sticking.
                let unconfirmed = |meter: &mut Option<Meter>| {
                    if let Some(m) = meter.as_mut().filter(|m| m.estimated) {
                        m.confidence *= 0.6;
                        if m.confidence < 0.15 {
                            *meter = None;
                        }
                    }
                };
                let mut scored: Vec<(u32, f32, i64)> =
                    [3, 4, 5, 7].into_iter().filter_map(|n| score(n).map(|(s, p)| (n, s, p))).collect();
                scored.sort_by(|a, b| b.1.total_cmp(&a.1));
                let Some(&(n, best, p)) = scored.first() else { return };
                if best < 0.08 {
                    unconfirmed(&mut self.meter);
                    return;
                }
                let second = scored.get(1).map_or(0.0, |s| s.1.max(0.0));
                // Keep the current meter unless another is clearly better.
                if let Some(m) = self.meter.filter(|m| m.estimated && m.beats != n) {
                    if scored.iter().find(|s| s.0 == m.beats).is_some_and(|s| best < s.1 * 1.15) {
                        return;
                    }
                }
                let fullness = (beats.len() as f32 / (6 * n) as f32).min(1.0);
                let confidence = ((best - second) / best).clamp(0.0, 1.0) * fullness;
                // Every bar length fits about as well: there's no meter to report.
                if confidence < 0.2 {
                    unconfirmed(&mut self.meter);
                    return;
                }
                let unit = if n == 7 { 8 } else { 4 };
                if self.meter.is_some_and(|m| m.beats != n) {
                    self.bars_checked = None;
                }
                self.meter = Some(Meter { beats: n, unit, downbeat: p, estimated: true, confidence });
            }
        }
    }

    /// Onset's position in grid steps from beat 0, if it's close to a step.
    fn step_of(tempo: &Tempo, time: f64) -> Option<i64> {
        let pos = tempo.beat_at(time) * STEPS;
        let step = pos.round();
        ((pos - step).abs() <= 0.3).then_some(step as i64)
    }

    /// Mark onsets in a lane whose bar repeats the one before (or two before).
    fn mark_repeats(&mut self) {
        let (Some(tempo), Some(meter)) = (self.tempo, self.meter) else { return };
        let latest_done = meter.bar_of(tempo.beat_at(self.now - 0.15).floor() as i64) - 1;
        let first = match self.bars_checked {
            Some(b) => b + 1,
            None => latest_done,
        };
        let steps_per_bar = i64::from(meter.beats) * STEPS as i64;
        for bar in first..=latest_done {
            for lane in Lane::ALL {
                let mask = |bar: i64| -> u64 {
                    let start = meter.bar_start(bar) * STEPS as i64;
                    let (from, to) = (tempo.time_of(meter.bar_start(bar) as f64), tempo.time_of(meter.bar_start(bar + 1) as f64));
                    self.onsets
                        .iter()
                        .filter(|o| o.lane == lane && o.time >= from - 0.05 && o.time < to - 0.05)
                        .filter_map(|o| Self::step_of(&tempo, o.time))
                        .map(|s| s - start)
                        .filter(|s| (0..steps_per_bar.min(64)).contains(s))
                        .fold(0u64, |m, s| m | 1 << s)
                };
                let this = mask(bar);
                if this.count_ones() < 2 {
                    continue;
                }
                let matched = [bar - 1, bar - 2].into_iter().find(|&b| {
                    let other = mask(b);
                    (this & other).count_ones() as f32 / (this | other).count_ones().max(1) as f32 >= 0.75
                });
                if let Some(other) = matched {
                    for b in [bar, other] {
                        let (from, to) = (tempo.time_of(meter.bar_start(b) as f64), tempo.time_of(meter.bar_start(b + 1) as f64));
                        for o in self.onsets.iter_mut().filter(|o| o.lane == lane && o.time >= from - 0.05 && o.time < to - 0.05) {
                            o.flags.repeating = true;
                        }
                    }
                }
            }
        }
        self.bars_checked = Some(latest_done);
    }

    /// Mark accented onsets off the beat whose next beat has no matching onset.
    fn mark_syncopation(&mut self) {
        let Some(tempo) = self.tempo else { return };
        if tempo.confidence < 0.15 && tempo.source == TempoSource::Detected {
            return;
        }
        // Wait until the following beat has passed.
        let until = self.now - tempo.period() - 0.1;
        if until <= self.sync_checked {
            return;
        }
        let candidates: Vec<usize> = self
            .onsets
            .iter()
            .enumerate()
            .filter(|(_, o)| o.time >= self.sync_checked && o.time < until && o.lane != Lane::Treble && o.strength >= 0.45)
            .map(|(i, _)| i)
            .collect();
        for i in candidates {
            let o = self.onsets[i];
            let Some(step) = Self::step_of(&tempo, o.time) else { continue };
            if step.rem_euclid(STEPS as i64) == 0 {
                continue;
            }
            let next_beat = tempo.time_of((step.div_euclid(STEPS as i64) + 1) as f64);
            let tolerance = tempo.period() / STEPS;
            let answered = self.onsets.iter().any(|n| {
                n.lane == o.lane && (n.time - next_beat).abs() <= tolerance && n.strength >= 0.7 * o.strength
            });
            if !answered {
                self.onsets[i].flags.syncopated = true;
            }
        }
        self.sync_checked = until;
    }

    /// A lane whose hits keep a steady interval in a 3:2, 2:3, 4:3 or 3:4
    /// ratio to the beat.
    fn update_polyrhythms(&mut self) {
        let Some(tempo) = self.tempo else {
            self.poly = [None; LANES];
            return;
        };
        for lane in Lane::ALL {
            let times: Vec<f64> =
                self.onsets.iter().filter(|o| o.lane == lane && o.time >= self.now - 8.0).map(|o| o.time).collect();
            let intervals: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
            self.poly[lane.index()] = (|| {
                if intervals.len() < 6 {
                    return None;
                }
                // The interval most others agree with, within 5%.
                let near = |d: f64| intervals.iter().filter(|&&x| (x / d - 1.0).abs() < 0.05).count();
                let (d, count) = intervals.iter().map(|&d| (d, near(d))).max_by_key(|&(_, c)| c)?;
                if (count as f64) < 0.6 * intervals.len() as f64 || count < 6 {
                    return None;
                }
                // Average the agreeing intervals for a steadier ratio.
                let agreeing: Vec<f64> = intervals.iter().copied().filter(|x| (x / d - 1.0).abs() < 0.05).collect();
                let d = agreeing.iter().sum::<f64>() / agreeing.len() as f64;
                let ratio = tempo.period() / d;
                [(3, 2), (2, 3), (4, 3), (3, 4)]
                    .into_iter()
                    .find(|&(hits, beats)| (ratio / (f64::from(hits) / f64::from(beats)) - 1.0).abs() < 0.03)
            })();
        }
    }
}

/// Tempo from the onset envelope.
///
/// Each candidate is scored by laying a grid of beats at that tempo over the
/// envelope, at its best alignment, and taking the average onset energy on the
/// grid lines. A pattern with kicks 1½ beats apart repeats strongly at 1½
/// beats, but a grid at that spacing soon drifts onto weak hi-hats, while the
/// true beat keeps landing on kicks and snares. Between equally good
/// candidates it leans toward 120 BPM.
///
/// The confidence is the autocorrelation at the chosen beat and its
/// multiples: how strongly the audio repeats at that tempo at all, which is
/// near zero for music without a beat. Returns (bpm, confidence 0–1).
fn estimate_bpm(envelope: &VecDeque<f32>) -> Option<(f64, f32)> {
    let env: Vec<f32> = envelope.iter().copied().collect();
    let n = env.len();
    let mean = env.iter().sum::<f32>() / n as f32;
    if mean <= 1e-6 {
        return None;
    }
    let lag_of = |bpm: f64| 60.0 / bpm / FRAME_SECONDS;
    let (min_lag, max_lag) = (lag_of(MAX_BPM), lag_of(MIN_BPM));
    if max_lag * 3.0 >= n as f64 {
        return None;
    }
    // Most music sits between about 80 and 160 BPM.
    let prior = |lag: f64| {
        let bpm = 60.0 / (lag * FRAME_SECONDS);
        (-0.5 * ((bpm / 120.0).log2() / 0.8).powi(2)).exp() as f32
    };
    let (lag, _) = (0..)
        .map(|i| min_lag + i as f64 * 0.25)
        .take_while(|&lag| lag <= max_lag)
        .map(|lag| (lag, comb_salience(&env, lag).0 * prior(lag)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;

    // Lightly smoothed, so onsets a frame early or late still line up.
    let x: Vec<f32> = (0..n)
        .map(|i| 0.5 * env[i] + 0.25 * (env[i.saturating_sub(1)] + env[(i + 1).min(n - 1)]) - mean)
        .collect();
    let ac = |lag: f64| -> f32 {
        let i = lag.round() as usize;
        if i >= n - 1 {
            return 0.0;
        }
        (0..n - i).map(|j| x[j] * x[j + i]).sum::<f32>() / (n - i) as f32
    };
    let zero = ac(0.0);
    if zero <= 1e-6 {
        return None;
    }
    let (sum, total) = [(1.0, 1.0f32), (2.0, 0.7), (4.0, 0.5)]
        .into_iter()
        .filter(|(k, _)| lag * k < (n - 1) as f64)
        .fold((0.0, 0.0), |(s, t), (k, w)| (s + w * ac(lag * k), t + w));
    let confidence = (sum / total / zero).clamp(0.0, 1.0);
    Some(((60.0 / (lag * FRAME_SECONDS)).clamp(MIN_BPM, MAX_BPM), confidence))
}

/// For beats every `lag` frames: the best alignment's average envelope on the
/// grid, and that alignment (frames back from the newest).
fn comb_salience(env: &[f32], lag: f64) -> (f32, usize) {
    let n = env.len();
    let value = |i: usize| env[i] + 0.5 * (env[i.saturating_sub(1)] + env.get(i + 1).copied().unwrap_or(0.0));
    (0..lag.ceil() as usize)
        .map(|offset| {
            let (sum, count) = (0..)
                .map(|k| n as f64 - 1.0 - offset as f64 - k as f64 * lag)
                .take_while(|&p| p >= 0.0)
                .fold((0.0, 0), |(s, c), p| (s + value(p.round() as usize), c + 1));
            (sum / count.max(1) as f32, offset)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .unwrap_or((0.0, 0))
}

/// Where the beats fall for `period`: the offset whose comb of beat times
/// collects the most onset energy. Returns (time of a beat, fit 0–1).
fn estimate_phase(envelope: &VecDeque<f32>, now: f64, period: f64) -> (f64, f32) {
    let n = envelope.len();
    let steps = ((period / FRAME_SECONDS).round() as usize).max(1);
    let value = |back: f64| -> f32 {
        let i = n as f64 - 1.0 - back / FRAME_SECONDS;
        if i < 1.0 {
            return 0.0;
        }
        let i = i.round() as usize;
        envelope[i] + 0.5 * (envelope[i - 1] + envelope.get(i + 1).copied().unwrap_or(0.0))
    };
    let span = n as f64 * FRAME_SECONDS;
    let scores: Vec<f32> = (0..steps)
        .map(|s| {
            let offset = s as f64 * FRAME_SECONDS;
            (0..).map(|k| offset + k as f64 * period).take_while(|&back| back < span).map(value).sum()
        })
        .collect();
    let (best, &top) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
    let mean = scores.iter().sum::<f32>() / scores.len().max(1) as f32;
    let fit = if top > 0.0 { ((top - mean) / top).clamp(0.0, 1.0) } else { 0.0 };
    (now - best as f64 * FRAME_SECONDS, fit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames for `seconds` of audio with flux spikes where `hit` says, over a
    /// low wobbling floor.
    fn feed(rhythm: &mut Rhythm, seconds: f64, hit: impl Fn(f64) -> [f32; LANES]) {
        let frames = (seconds / FRAME_SECONDS) as usize;
        for i in 0..frames {
            let time = i as f64 * FRAME_SECONDS;
            let wobble = 0.6 + 0.3 * ((i as f32) * 0.37).sin();
            let spikes = hit(time);
            let flux = [0, 1, 2, 3].map(|l| wobble + spikes[l]);
            rhythm.push(&Frame { time, bands: vec![], left: vec![], right: vec![], flux });
        }
    }

    /// A spike of `height` in the frame nearest each multiple of `period`, offset by `phase`.
    fn pulse(time: f64, period: f64, phase: f64, height: f32) -> f32 {
        let pos = (time - phase).rem_euclid(period);
        if pos < FRAME_SECONDS { height } else { 0.0 }
    }

    /// `RHYTHM_RAW=file.raw cargo test --release --bin sbx4-visualizer real_audio -- --ignored --nocapture`
    /// on raw 48 kHz 16-bit stereo, e.g. from `pw-record ... -`.
    #[test]
    #[ignore]
    fn real_audio() {
        let bytes = std::fs::read(std::env::var("RHYTHM_RAW").unwrap()).unwrap();
        let samples: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut r = Rhythm::new();
        for (i, frame) in sbx4_control::analysis::analyze(&samples).iter().enumerate() {
            r.push(frame);
            if i % (2.0 / FRAME_SECONDS) as usize == 0 {
                let t = r.tempo();
                eprintln!(
                    "{:5.1}s  bpm {:>6}  conf {:>4}  meter {:?}",
                    frame.time,
                    t.map_or("-".into(), |t| format!("{:.1}", t.bpm)),
                    t.map_or("-".into(), |t| format!("{:.2}", t.confidence)),
                    r.meter().map(|m| (m.label(), (m.confidence * 100.0).round())),
                );
            }
        }
        let secs = r.now();
        // The whole clip's autocorrelation, unweighted: where the real peaks are.
        let frames = sbx4_control::analysis::analyze(&samples);
        let (mut mean, mut env) = (0f32, Vec::new());
        for f in &frames {
            let e: f32 = f.flux.iter().zip([1.0, 0.8, 0.6, 0.4]).map(|(x, w)| x * w).sum();
            mean += (e - mean) * 0.04;
            env.push((e - mean).max(0.0));
        }
        let m = env.iter().sum::<f32>() / env.len() as f32;
        let x: Vec<f32> = env.iter().map(|v| v - m).collect();
        let ac = |l: usize| (0..x.len() - l).map(|i| x[i] * x[i + l]).sum::<f32>() / (x.len() - l) as f32;
        let z = ac(0);
        let mut peaks: Vec<(f64, f32)> = (20..200)
            .filter(|&l| ac(l) > ac(l - 1) && ac(l) >= ac(l + 1))
            .map(|l| (60.0 / (l as f64 * FRAME_SECONDS), ac(l) / z))
            .collect();
        peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
        eprintln!("autocorrelation peaks (bpm, strength): {:?}", peaks.iter().take(8).map(|(b, v)| (b.round(), (v * 100.0).round() / 100.0)).collect::<Vec<_>>());
        for lane in Lane::ALL {
            let n = r.onsets.iter().filter(|o| o.lane == lane).count();
            let rep = r.onsets.iter().filter(|o| o.lane == lane && o.flags.repeating).count();
            let syn = r.onsets.iter().filter(|o| o.lane == lane && o.flags.syncopated).count();
            eprintln!("{:<9} {:5.1}/s  repeating {rep:3}  syncopated {syn:3}  poly {:?}", lane.title(), n as f64 / secs, r.polyrhythm(lane));
        }
    }

    /// A drum loop synthesized at 124 BPM, through the whole audio pipeline:
    /// kick on 1 and the "and" of 2, snare on 2 and 4, hi-hat on the eighths.
    #[test]
    fn drum_loop_end_to_end() {
        let rate = f64::from(sbx4_control::analysis::RATE);
        let beat = 60.0 / 124.0;
        let seconds = 20.0;
        let n = (seconds * rate) as usize;
        let mut mix = vec![0f32; n];
        let mut seed = 7u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let mut add = |at: f64, len: f64, f: &mut dyn FnMut(f64) -> f32| {
            let start = (at * rate) as usize;
            for i in 0..(len * rate) as usize {
                if let Some(s) = mix.get_mut(start + i) {
                    *s += f(i as f64 / rate);
                }
            }
        };
        let bars = (seconds / (4.0 * beat)) as usize;
        for bar in 0..bars {
            let t0 = bar as f64 * 4.0 * beat;
            for kick in [0.0, 1.5] {
                add(t0 + kick * beat, 0.3, &mut |t| (0.9 * (-t * 12.0).exp() * (2.0 * std::f64::consts::PI * (50.0 + 60.0 * (-t * 30.0).exp()) * t).sin()) as f32);
            }
            for snare in [1.0, 3.0] {
                add(t0 + snare * beat, 0.15, &mut |t| 0.35 * (-t * 25.0).exp() as f32 * noise());
            }
            // Hi-hats: noise with its low end taken out, as real cymbals have.
            for hat in 0..8 {
                let mut last = 0.0;
                add(t0 + hat as f64 * beat / 2.0, 0.04, &mut |t| {
                    let n = noise();
                    let high = n - last;
                    last = n;
                    0.12 * (-t * 90.0).exp() as f32 * high
                });
            }
        }
        let samples: Vec<i16> = mix.iter().flat_map(|&s| {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            [v, v]
        }).collect();

        let mut r = Rhythm::new();
        for frame in sbx4_control::analysis::analyze(&samples) {
            r.push(&frame);
        }
        let tempo = r.tempo().expect("a tempo");
        assert!((tempo.bpm - 124.0).abs() < 2.0, "{tempo:?}");
        assert!(tempo.confidence >= TEMPO_START, "{tempo:?}");
        // Beats line up with the snares and kicks: every beat is within 25 ms of one.
        let off = (tempo.anchor).rem_euclid(beat);
        assert!(off.min(beat - off) < 0.025, "phase off by {off}");
        // The bass lane hears the two kicks a bar (and some of the snares'
        // low end); the treble hears the hi-hats.
        let rate_of = |lane| r.onsets.iter().filter(|o| o.lane == lane && o.time > 4.0).count() as f64 / (seconds - 4.0);
        let bar_rate = 1.0 / (4.0 * beat);
        let bass = rate_of(Lane::Bass) / bar_rate;
        assert!((2.0..=4.5).contains(&bass), "bass {bass} a bar");
        assert!(rate_of(Lane::Treble) / bar_rate > 5.0, "treble {}/s", rate_of(Lane::Treble));
        // Kick on 1 and the "and" of 2 accents beat 1 only: a 4-beat bar starting on the kick.
        let meter = r.meter().expect("a meter");
        assert_eq!(meter.beats, 4, "{meter:?}");
        assert!(meter.is_downbeat(tempo.beat_at(16.0 * beat).round() as i64), "{meter:?}");
    }

    /// Steady noise with no beat: nothing should be claimed.
    #[test]
    fn no_tempo_in_noise() {
        let mut seed = 3u32;
        let samples: Vec<i16> = (0..48_000 * 12)
            .flat_map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let v = ((seed >> 16) as i16) / 6;
                [v, v]
            })
            .collect();
        let mut r = Rhythm::new();
        for frame in sbx4_control::analysis::analyze(&samples) {
            r.push(&frame);
        }
        assert!(r.tempo().is_none(), "{:?}", r.tempo());
        assert!(r.meter().is_none());
    }

    #[test]
    fn finds_onsets_only_where_they_are() {
        let mut r = Rhythm::new();
        feed(&mut r, 6.0, |t| [pulse(t, 0.5, 0.25, 20.0), 0.0, 0.0, 0.0]);
        let bass = r.onsets.iter().filter(|o| o.lane == Lane::Bass).count();
        let others = r.onsets.iter().filter(|o| o.lane != Lane::Bass).count();
        assert!((11..=12).contains(&bass), "bass onsets {bass}");
        assert_eq!(others, 0);
        // Each is timed to its spike.
        for o in r.onsets.iter() {
            let off = (o.time - 0.25).rem_euclid(0.5);
            assert!(off < 0.02 || off > 0.48, "onset at {} off by {off}", o.time);
        }
    }

    #[test]
    fn lower_sensitivity_skips_weak_hits() {
        let weak = |t: f64| [pulse(t, 0.5, 0.0, 20.0) + pulse(t, 0.5, 0.25, 3.0), 0.0, 0.0, 0.0];
        let mut strict = Rhythm::new();
        strict.set_sensitivity(0.0);
        feed(&mut strict, 6.0, weak);
        let mut loose = Rhythm::new();
        loose.set_sensitivity(1.0);
        feed(&mut loose, 6.0, weak);
        assert!(loose.onsets.len() > strict.onsets.len() + 6, "{} vs {}", loose.onsets.len(), strict.onsets.len());
    }

    #[test]
    fn detects_tempo_and_beat_phase() {
        for bpm in [90.0, 120.0, 140.0] {
            let period = 60.0 / bpm;
            let mut r = Rhythm::new();
            // Kick on the beat, hi-hat on the eighths.
            feed(&mut r, 12.0, |t| [pulse(t, period, 0.1, 18.0), 0.0, pulse(t, period / 2.0, 0.1, 6.0), 0.0]);
            let tempo = r.tempo().expect("a tempo");
            assert!((tempo.bpm - bpm).abs() < 2.0, "detected {} for {bpm}", tempo.bpm);
            // A beat lands on a kick.
            let off = (tempo.anchor - 0.1).rem_euclid(period);
            let off = off.min(period - off);
            assert!(off < 0.03, "phase off by {off} at {bpm}");
        }
    }

    #[test]
    fn manual_tempo_still_finds_the_phase() {
        let mut r = Rhythm::new();
        r.set_manual_bpm(Some(100.0));
        feed(&mut r, 8.0, |t| [pulse(t, 0.6, 0.2, 18.0), 0.0, 0.0, 0.0]);
        let tempo = r.tempo().unwrap();
        assert_eq!(tempo.source, TempoSource::Manual);
        assert_eq!(tempo.bpm, 100.0);
        let off = (tempo.anchor - 0.2).rem_euclid(0.6);
        assert!(off.min(0.6 - off) < 0.03);
    }

    #[test]
    fn taps_give_a_tempo() {
        let mut r = Rhythm::new();
        assert_eq!(r.tap(10.0), None);
        r.tap(10.5);
        r.tap(11.0);
        let bpm = r.tap(11.5).unwrap();
        assert!((bpm - 120.0).abs() < 0.01);
        // A long pause starts over.
        assert_eq!(r.tap(20.0), None);
    }

    #[test]
    fn estimates_the_meter_from_accents() {
        for (beats, downbeat_at) in [(4u32, 0.3), (3, 0.0)] {
            let period = 0.5;
            let mut r = Rhythm::new();
            let bar = period * f64::from(beats);
            // Strong bass on the downbeat, a lighter low-mid hit on every beat.
            feed(&mut r, 24.0, |t| [pulse(t, bar, downbeat_at, 22.0), pulse(t, period, downbeat_at, 7.0), 0.0, 0.0]);
            let meter = r.meter().expect("a meter");
            assert!(meter.estimated);
            assert_eq!(meter.beats, beats, "{meter:?}");
            let tempo = r.tempo().unwrap();
            let beat = tempo.beat_at(downbeat_at + bar * 20.0).round() as i64;
            assert!(meter.is_downbeat(beat), "{meter:?} {tempo:?}");
        }
    }

    #[test]
    fn no_meter_claimed_without_accents() {
        let mut r = Rhythm::new();
        feed(&mut r, 20.0, |t| [pulse(t, 0.5, 0.0, 15.0), 0.0, 0.0, 0.0]);
        assert!(r.tempo().is_some());
        assert_eq!(r.meter(), None);
    }

    #[test]
    fn suspects_three_against_two() {
        let mut r = Rhythm::new();
        // Kick on the beat at 120 BPM; treble three times per two beats.
        feed(&mut r, 14.0, |t| [pulse(t, 0.5, 0.0, 18.0), 0.0, 0.0, pulse(t, 1.0 / 3.0, 0.05, 12.0)]);
        assert_eq!(r.polyrhythm(Lane::Treble), Some((3, 2)));
        assert_eq!(r.polyrhythm(Lane::Bass), None);
    }

    #[test]
    fn marks_repeating_bars() {
        let mut r = Rhythm::new();
        r.set_meter(MeterChoice::Fixed { beats: 4, unit: 4 });
        // The same bar over and over: bass on 1, the "and" of 2, and 4.
        feed(&mut r, 16.0, |t| {
            let pos = t.rem_euclid(2.0);
            let hit = [0.0, 0.75, 1.5].iter().any(|&p| (pos - p) >= 0.0 && (pos - p) < FRAME_SECONDS);
            [if hit { 20.0 } else { 0.0 }, 0.0, 0.0, 0.0]
        });
        let late: Vec<_> = r.onsets.iter().filter(|o| o.time > 8.0 && o.time < 14.0).collect();
        assert!(!late.is_empty());
        assert!(late.iter().all(|o| o.flags.repeating), "{late:?}");
    }

    #[test]
    fn flags_an_offbeat_accent_before_a_silent_beat() {
        let mut r = Rhythm::new();
        r.set_manual_bpm(Some(120.0));
        // Kick on every beat except where an accent lands just before it.
        feed(&mut r, 12.0, |t| {
            let bar = t.rem_euclid(2.0);
            // The beat at 1.5 is left out.
            let beat = if (1.4..1.6).contains(&bar) { 0.0 } else { pulse(t, 0.5, 0.0, 18.0) };
            let push = if (bar - 1.25).abs() < FRAME_SECONDS / 2.0 { 18.0 } else { 0.0 };
            [beat.max(push), 0.0, 0.0, 0.0]
        });
        let synced: Vec<_> = r.onsets.iter().filter(|o| o.flags.syncopated).collect();
        assert!(!synced.is_empty(), "no syncopation found");
        for o in &synced {
            assert!((o.time.rem_euclid(2.0) - 1.25).abs() < 0.03, "flagged {o:?}");
        }
        // Plain beats aren't flagged.
        assert!(r.onsets.iter().filter(|o| (o.time.rem_euclid(0.5)) < 0.02).all(|o| !o.flags.syncopated));
    }
}
