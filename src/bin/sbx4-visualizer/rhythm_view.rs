//! The Rhythm view: detected onsets per frequency lane against the beat grid,
//! as a scrolling timeline, a step-sequencer style pulse grid, or a circle
//! that goes round once a bar.

use std::cell::{Cell, RefCell};
use std::f64::consts::{FRAC_PI_2, TAU};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, glib};
use sbx4_control::analysis::{Capture, Lane, Source, Sources, LANES};
use sbx4_control::plot::{self, Rgb};

use crate::prefs::{Mode, Prefs};
use crate::rhythm::{Meter, Onset, Rhythm, Tempo, TempoSource};

/// Everything a frame draws, so a frozen picture can be kept as is.
#[derive(Clone)]
struct Scene {
    time: f64,
    onsets: Vec<Onset>,
    tempo: Option<Tempo>,
    meter: Option<Meter>,
    poly: [Option<(u32, u32)>; LANES],
    source: Option<Source>,
    error: Option<String>,
}

pub struct RhythmView {
    pub widget: gtk::DrawingArea,
    prefs: Rc<RefCell<Prefs>>,
    capture: RefCell<Option<Capture>>,
    rhythm: RefCell<Rhythm>,
    /// Stream time minus frame-clock time, smoothed: turns the display clock
    /// into stream time so scrolling is smooth between audio frames.
    offset: Cell<Option<f64>>,
    clock_now: Cell<f64>,
    frozen: RefCell<Option<Scene>>,
    last_draw: Cell<i64>,
}

impl RhythmView {
    pub fn new(prefs: Rc<RefCell<Prefs>>) -> Rc<Self> {
        let widget = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
        let view = Rc::new(Self {
            widget: widget.clone(),
            prefs,
            capture: RefCell::new(None),
            rhythm: RefCell::new(Rhythm::new()),
            offset: Cell::new(None),
            clock_now: Cell::new(0.0),
            frozen: RefCell::new(None),
            last_draw: Cell::new(0),
        });

        let v = Rc::downgrade(&view);
        widget.set_draw_func(move |_, cr, w, h| {
            let Some(v) = v.upgrade() else { return };
            let scene = v.frozen.borrow().clone().unwrap_or_else(|| v.scene());
            let prefs = v.prefs.borrow();
            let (w, h) = (f64::from(w), f64::from(h));
            match prefs.mode {
                Mode::Timeline => draw_timeline(cr, w, h, &scene, &prefs),
                Mode::PulseGrid => draw_pulse_grid(cr, w, h, &scene, &prefs),
                Mode::Circular => draw_circular(cr, w, h, &scene, &prefs),
            }
        });

        let v = Rc::downgrade(&view);
        widget.add_tick_callback(move |area, clock| {
            let Some(v) = v.upgrade() else { return glib::ControlFlow::Break };
            v.tick(area, clock.frame_time());
            glib::ControlFlow::Continue
        });

        let v = view.clone();
        widget.connect_map(move |_| v.start());
        let v = view.clone();
        widget.connect_unmap(move |_| v.stop());
        view
    }

    fn tick(&self, area: &gtk::DrawingArea, frame_time_us: i64) {
        let clock = frame_time_us as f64 / 1e6;
        self.clock_now.set(clock);
        let frames = self.capture.borrow().as_ref().map(Capture::drain).unwrap_or_default();
        {
            let prefs = self.prefs.borrow();
            let mut rhythm = self.rhythm.borrow_mut();
            rhythm.set_sensitivity(prefs.sensitivity);
            rhythm.set_manual_bpm(prefs.manual_bpm());
            rhythm.set_meter(prefs.meter_choice());
            for frame in &frames {
                rhythm.push(frame);
            }
        }
        if let Some(last) = frames.last() {
            // Audio arrives in bursts; ease the offset rather than jump.
            let target = last.time - clock;
            self.offset.set(Some(match self.offset.get() {
                Some(o) if (o - target).abs() < 0.25 => o + (target - o) * 0.1,
                _ => target,
            }));
        }
        // 30 FPS draws every other display frame.
        let min_gap = if self.prefs.borrow().fps == 30 { 30_000 } else { 0 };
        if frame_time_us - self.last_draw.get() >= min_gap {
            self.last_draw.set(frame_time_us);
            area.queue_draw();
        }
    }

    /// Stream time now, for drawing.
    fn now(&self) -> f64 {
        let rhythm = self.rhythm.borrow();
        match self.offset.get() {
            // Never ahead of the newest audio by more than a few frames.
            Some(o) => (self.clock_now.get() + o).min(rhythm.now() + 0.05),
            None => rhythm.now(),
        }
    }

    fn scene(&self) -> Scene {
        let time = self.now();
        let rhythm = self.rhythm.borrow();
        let keep = f64::from(self.prefs.borrow().history) + 1.0;
        let capture = self.capture.borrow();
        Scene {
            time,
            onsets: rhythm.onsets.iter().filter(|o| o.time >= time - keep).copied().collect(),
            tempo: rhythm.tempo(),
            meter: rhythm.meter(),
            poly: Lane::ALL.map(|l| rhythm.polyrhythm(l)),
            source: capture.as_ref().and_then(Capture::source),
            error: capture.as_ref().and_then(Capture::error),
        }
    }

    pub fn start(&self) {
        if self.capture.borrow().is_none() {
            self.rhythm.borrow_mut().reset();
            self.offset.set(None);
            self.capture.replace(Some(Capture::start(Sources::X4ThenPipeWire)));
        }
    }

    /// Stop capturing; the X4 gets its recording source back. Blocks briefly.
    pub fn stop(&self) {
        drop(self.capture.take());
    }

    pub fn source(&self) -> Option<Source> {
        self.capture.borrow().as_ref().and_then(Capture::source)
    }

    /// Hold the picture; analysis carries on underneath.
    pub fn set_frozen(&self, frozen: bool) {
        let scene = frozen.then(|| self.scene());
        self.frozen.replace(scene);
        self.widget.queue_draw();
    }

    /// A tap of tap tempo. Returns the tapped BPM once there are enough taps.
    pub fn tap(&self) -> Option<f64> {
        let now = self.now();
        self.rhythm.borrow_mut().tap(now)
    }

    pub fn tempo(&self) -> Option<Tempo> {
        self.rhythm.borrow().tempo()
    }
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// Bass in the subwoofer's orange through to treble in a lighter blue.
fn lane_color(lane: Lane, colors: &plot::Palette) -> Rgb {
    match lane {
        Lane::Bass => colors.sub,
        Lane::LowMid => mix(colors.sub, colors.accent, 0.45),
        Lane::HighMid => colors.accent,
        Lane::Treble => mix(colors.accent, colors.fg, 0.35),
    }
}

/// Enabled lanes, highest frequencies first (top of the screen).
fn lanes(prefs: &Prefs) -> Vec<Lane> {
    Lane::ALL.into_iter().rev().filter(|l| prefs.lanes[l.index()]).collect()
}

/// How lit a lane is: its latest onsets' strength, decaying.
fn activity(scene: &Scene, lane: Lane, prefs: &Prefs) -> f64 {
    let rate = 1.5 + 10.0 * prefs.peak_decay;
    scene
        .onsets
        .iter()
        .rev()
        .filter(|o| o.lane == lane && o.time <= scene.time)
        .take(4)
        .map(|o| f64::from(o.strength) * (-(scene.time - o.time) * rate).exp())
        .fold(0.0, f64::max)
}

/// A bar length to draw with: the meter, or four beats while there's none.
fn bar_meter(scene: &Scene) -> Meter {
    scene.meter.unwrap_or(Meter { beats: 4, unit: 4, downbeat: 0, estimated: true, confidence: 0.0 })
}

/// "124 BPM · 4/4 (estimated, 70% sure)" and the like.
fn status(scene: &Scene) -> String {
    let Some(tempo) = scene.tempo else {
        return match (&scene.error, scene.source) {
            (Some(e), _) => e.clone(),
            (None, None) => "Starting…".into(),
            (None, Some(_)) => "Listening for a beat…".into(),
        };
    };
    let bpm = match tempo.source {
        TempoSource::Manual => format!("{:.0} BPM (manual)", tempo.bpm),
        TempoSource::Detected => format!("{:.0} BPM", tempo.bpm),
    };
    match scene.meter {
        Some(m) if m.estimated => format!("{bpm} · {} (estimated, {:.0}% sure)", m.label(), m.confidence * 100.0),
        Some(m) => format!("{bpm} · {}", m.label()),
        None => bpm,
    }
}

fn text(cr: &cairo::Context, x: f64, y: f64, size: f64, color: Rgb, alpha: f64, s: &str) {
    cr.set_font_size(size);
    plot::set(cr, color, alpha);
    cr.move_to(x, y);
    let _ = cr.show_text(s);
    // Text leaves the current point at its end; start the next shape afresh.
    cr.new_path();
}

fn centered(cr: &cairo::Context, x: f64, y: f64, size: f64, color: Rgb, alpha: f64, s: &str) {
    cr.set_font_size(size);
    let width = cr.text_extents(s).map_or(0.0, |e| e.width());
    text(cr, x - width / 2.0, y, size, color, alpha, s);
}

fn draw_header(cr: &cairo::Context, w: f64, scene: &Scene, colors: &plot::Palette) {
    text(cr, 12.0, 22.0, 15.0, colors.fg, 0.85, &status(scene));
    if let Some(source) = scene.source {
        cr.set_font_size(11.0);
        let s = source.title();
        let width = cr.text_extents(s).map_or(0.0, |e| e.width());
        text(cr, w - width - 12.0, 20.0, 11.0, colors.fg, 0.4, s);
    }
}

/// Glow behind a fresh event: brightest when it lands, gone after 0.3 s.
fn glow_alpha(scene: &Scene, onset: &Onset, prefs: &Prefs) -> f64 {
    let age = scene.time - onset.time;
    if !(0.0..0.3).contains(&age) {
        return 0.0;
    }
    prefs.glow * (1.0 - age / 0.3) * f64::from(onset.strength)
}

/// Small marks for the pattern flags: a dot under repeating hits, a caret over syncopated ones.
fn draw_flags(cr: &cairo::Context, x: f64, top: f64, bottom: f64, onset: &Onset, prefs: &Prefs, fg: Rgb) {
    if prefs.repeating && onset.flags.repeating {
        plot::set(cr, fg, 0.55);
        cr.arc(x, bottom + 4.0, 1.8, 0.0, TAU);
        let _ = cr.fill();
    }
    if prefs.syncopation && onset.flags.syncopated {
        plot::set(cr, fg, 0.8);
        cr.move_to(x - 3.5, top - 2.0);
        cr.line_to(x, top - 6.5);
        cr.line_to(x + 3.5, top - 2.0);
        cr.set_line_width(1.3);
        let _ = cr.stroke();
    }
}

/// Time across, now near the right with the predicted beats ahead of it;
/// one row per lane.
fn draw_timeline(cr: &cairo::Context, w: f64, h: f64, scene: &Scene, prefs: &Prefs) {
    let colors = plot::Palette::current();
    draw_header(cr, w, scene, &colors);
    let lanes = lanes(prefs);
    if lanes.is_empty() {
        centered(cr, w / 2.0, h / 2.0, 14.0, colors.fg, 0.5, "No frequency lanes selected");
        return;
    }
    let (left, top, bottom) = (96.0, 40.0, h - 12.0);
    let now_x = left + (w - 12.0 - left) * 0.85;
    let span = f64::from(prefs.history) / prefs.scroll_speed;
    let px_per_s = (now_x - left) / span;
    let x_of = |t: f64| now_x - (scene.time - t) * px_per_s;
    let ahead = (w - 12.0 - now_x) / px_per_s;
    let lane_h = (bottom - top) / lanes.len() as f64;

    // Lane backgrounds and labels.
    for (row, &lane) in lanes.iter().enumerate() {
        let y = top + row as f64 * lane_h;
        let color = lane_color(lane, &colors);
        plot::set(cr, color, if row % 2 == 0 { 0.05 } else { 0.025 });
        cr.rectangle(left, y, w - 12.0 - left, lane_h);
        let _ = cr.fill();
        text(cr, 12.0, y + lane_h / 2.0 + 4.0, 12.0, color, 0.9, lane.title());
        if prefs.polyrhythm {
            if let Some((hits, beats)) = scene.poly[lane.index()] {
                text(cr, 12.0, y + lane_h / 2.0 + 19.0, 10.0, colors.fg, 0.6, &format!("{hits}:{beats} suspected"));
            }
        }
    }

    // Beat grid, with the beats still to come drawn fainter.
    if let Some(tempo) = scene.tempo {
        let meter = scene.meter;
        let sub = f64::from(prefs.subdivision);
        cr.set_line_width(1.0);
        if sub > 1.0 {
            let first = (tempo.beat_at(scene.time - span) * sub).ceil() as i64;
            let last = (tempo.beat_at(scene.time + ahead) * sub).floor() as i64;
            for s in (first..=last).filter(|s| s.rem_euclid(sub as i64) != 0) {
                let x = x_of(tempo.time_of(s as f64 / sub)).round() + 0.5;
                plot::set(cr, colors.fg, 0.05);
                cr.move_to(x, top);
                cr.line_to(x, bottom);
                let _ = cr.stroke();
            }
        }
        for (n, t) in tempo.beats(scene.time - span, scene.time + ahead) {
            let x = x_of(t).round() + 0.5;
            let future = t > scene.time;
            let bar = prefs.measures && meter.is_some_and(|m| m.is_downbeat(n));
            let alpha = match (bar, prefs.beat_markers) {
                (true, _) => 0.45,
                (false, true) => 0.16,
                (false, false) => continue,
            } * if future { 0.5 } else { 1.0 };
            plot::set(cr, colors.fg, alpha);
            cr.set_line_width(if bar { 1.5 } else { 1.0 });
            cr.move_to(x, top);
            cr.line_to(x, bottom);
            let _ = cr.stroke();
            if bar {
                if let Some(m) = meter {
                    text(cr, x + 3.0, top + 11.0, 10.0, colors.fg, 0.5 * if future { 0.5 } else { 1.0 }, &(m.bar_of(n) + 1).to_string());
                }
            }
        }
    }

    // Onsets: a bar per hit, taller for stronger ones, fading with age when trails are on.
    for o in scene.onsets.iter().filter(|o| o.time <= scene.time) {
        let Some(row) = lanes.iter().position(|&l| l == o.lane) else { continue };
        let x = x_of(o.time);
        if x < left - 4.0 {
            continue;
        }
        let color = lane_color(o.lane, &colors);
        let mid = top + (row as f64 + 0.5) * lane_h;
        let height = lane_h * (0.15 + 0.65 * f64::from(o.strength));
        let age = (scene.time - o.time) / span;
        let alpha = if prefs.trails { 0.25 + 0.7 * (1.0 - age).max(0.0) } else { 0.9 };
        let glow = glow_alpha(scene, o, prefs);
        if glow > 0.01 {
            plot::set(cr, color, glow * 0.45);
            cr.rectangle(x - 6.0, mid - height / 2.0 - 4.0, 12.0, height + 8.0);
            let _ = cr.fill();
        }
        plot::set(cr, color, alpha);
        cr.rectangle(x - 1.5, mid - height / 2.0, 3.0, height);
        let _ = cr.fill();
        draw_flags(cr, x, mid - height / 2.0, mid + height / 2.0, o, prefs, colors.fg);
    }

    // The present: a line, and each lane's level lighting up at it.
    plot::set(cr, colors.fg, 0.35);
    cr.set_line_width(1.0);
    cr.move_to(now_x.round() + 0.5, top);
    cr.line_to(now_x.round() + 0.5, bottom);
    let _ = cr.stroke();
    for (row, &lane) in lanes.iter().enumerate() {
        let level = activity(scene, lane, prefs);
        if level > 0.02 {
            let mid = top + (row as f64 + 0.5) * lane_h;
            plot::set(cr, lane_color(lane, &colors), level * (0.5 + 0.5 * prefs.glow));
            cr.arc(now_x, mid, 3.0 + 7.0 * level, 0.0, TAU);
            let _ = cr.fill();
        }
    }
}

/// One bar as a step grid, like a drum machine: a row per lane, a column per
/// step. Recent bars show through when trails are on.
fn draw_pulse_grid(cr: &cairo::Context, w: f64, h: f64, scene: &Scene, prefs: &Prefs) {
    let colors = plot::Palette::current();
    draw_header(cr, w, scene, &colors);
    let lanes = lanes(prefs);
    let Some(tempo) = scene.tempo else {
        centered(cr, w / 2.0, h / 2.0, 16.0, colors.fg, 0.5, "The pulse grid needs a beat: listening…");
        return;
    };
    if lanes.is_empty() {
        centered(cr, w / 2.0, h / 2.0, 14.0, colors.fg, 0.5, "No frequency lanes selected");
        return;
    }
    let meter = bar_meter(scene);
    let sub = i64::from(prefs.subdivision);
    let beats = i64::from(meter.beats);
    let cols = beats * sub;
    let (left, top, right, bottom) = (96.0, 64.0, w - 16.0, h - 16.0);
    let cell_w = (right - left) / cols as f64;
    let cell_h = ((bottom - top) / lanes.len() as f64).min(cell_w * 1.6);
    let grid_h = cell_h * lanes.len() as f64;
    let top = top + ((bottom - top) - grid_h) / 2.0;

    let now_beat = tempo.beat_at(scene.time);
    let bar = meter.bar_of(now_beat.floor() as i64);
    let bar_start = meter.bar_start(bar);
    let current = ((now_beat - bar_start as f64) * sub as f64).floor() as i64;
    let bar_seconds = tempo.period() * beats as f64;
    let shown_bars = if prefs.trails { ((f64::from(prefs.history) / bar_seconds).ceil() as i64).clamp(1, 8) } else { 1 };

    // Beat numbers over the columns, the current step lit.
    for c in 0..cols {
        let x = left + c as f64 * cell_w;
        if c == current {
            plot::set(cr, colors.fg, 0.08);
            cr.rectangle(x, top - 22.0, cell_w, grid_h + 22.0);
            let _ = cr.fill();
        }
        if c % sub == 0 && prefs.beat_markers {
            centered(cr, x + cell_w / 2.0, top - 8.0, 12.0, colors.fg, if c == 0 { 0.8 } else { 0.5 }, &(c / sub + 1).to_string());
        }
    }

    // Each cell's value: the strongest hit quantized into it, this bar and
    // (fading) the bars before.
    let mut cells = vec![vec![(0.0f64, None::<&Onset>); cols as usize]; lanes.len()];
    for o in scene.onsets.iter().filter(|o| o.time <= scene.time) {
        let Some(row) = lanes.iter().position(|&l| l == o.lane) else { continue };
        let step = (tempo.beat_at(o.time) * sub as f64).round() as i64;
        let bars_ago = bar - meter.bar_of(step.div_euclid(sub));
        if !(0..shown_bars).contains(&bars_ago) {
            continue;
        }
        let col = (step - meter.bar_start(bar - bars_ago) * sub).clamp(0, cols - 1) as usize;
        let value = f64::from(o.strength) * 0.5f64.powi(bars_ago as i32);
        if value > cells[row][col].0 {
            cells[row][col] = (value, (bars_ago == 0).then_some(o));
        }
    }

    for (row, &lane) in lanes.iter().enumerate() {
        let y = top + row as f64 * cell_h;
        let color = lane_color(lane, &colors);
        text(cr, 12.0, y + cell_h / 2.0 + 4.0, 12.0, color, 0.9, lane.title());
        if prefs.polyrhythm {
            if let Some((hits, b)) = scene.poly[lane.index()] {
                text(cr, 12.0, y + cell_h / 2.0 + 19.0, 10.0, colors.fg, 0.6, &format!("{hits}:{b} suspected"));
            }
        }
        for (col, &(value, onset)) in cells[row].iter().enumerate() {
            let x = left + col as f64 * cell_w;
            let (pad_x, pad_y) = ((cell_w * 0.12).min(6.0), (cell_h * 0.12).min(6.0));
            let (cx, cy, cw, ch) = (x + pad_x, y + pad_y, cell_w - 2.0 * pad_x, cell_h - 2.0 * pad_y);
            plot::set(cr, color, 0.07);
            cr.rectangle(cx, cy, cw, ch);
            let _ = cr.fill();
            if value > 0.01 {
                if let Some(o) = onset {
                    let glow = glow_alpha(scene, o, prefs);
                    if glow > 0.01 {
                        plot::set(cr, color, glow * 0.5);
                        cr.rectangle(cx - 4.0, cy - 4.0, cw + 8.0, ch + 8.0);
                        let _ = cr.fill();
                    }
                }
                plot::set(cr, color, 0.15 + 0.8 * value);
                cr.rectangle(cx, cy, cw, ch);
                let _ = cr.fill();
                if let Some(o) = onset {
                    draw_flags(cr, x + cell_w / 2.0, cy + 2.0, cy + ch - 8.0, o, prefs, colors.fg);
                }
            }
        }
    }

    // Beat and bar lines between the cells.
    cr.set_line_width(1.0);
    for c in 0..=cols {
        let x = (left + c as f64 * cell_w).round() + 0.5;
        let alpha = if (c == 0 || c == cols) && prefs.measures {
            0.5
        } else if c % sub == 0 && prefs.beat_markers {
            0.2
        } else {
            0.06
        };
        plot::set(cr, colors.fg, alpha);
        cr.move_to(x, top);
        cr.line_to(x, top + grid_h);
        let _ = cr.stroke();
    }
    if scene.meter.is_none() {
        centered(cr, w / 2.0, h - 10.0, 11.0, colors.fg, 0.45, "Bar length not detected yet: showing 4 beats");
    } else if prefs.measures {
        centered(cr, w / 2.0, h - 10.0, 11.0, colors.fg, 0.45, &format!("Bar {}", bar + 1));
    }
}

/// A circle that goes round once a bar, a ring per lane (bass outside); each
/// hit lands at its place in the bar.
fn draw_circular(cr: &cairo::Context, w: f64, h: f64, scene: &Scene, prefs: &Prefs) {
    let colors = plot::Palette::current();
    draw_header(cr, w, scene, &colors);
    let lanes: Vec<Lane> = lanes(prefs).into_iter().rev().collect();
    if lanes.is_empty() {
        centered(cr, w / 2.0, h / 2.0, 14.0, colors.fg, 0.5, "No frequency lanes selected");
        return;
    }
    let (cx, cy) = (w / 2.0, h / 2.0 + 14.0);
    let outer = ((w.min(h - 28.0)) / 2.0 - 24.0).max(40.0);
    let inner = outer * 0.3;
    let ring_w = (outer - inner) / lanes.len() as f64;
    // Ring for a lane: bass on the outside.
    let ring_r = |row: usize| outer - (row as f64 + 0.5) * ring_w;

    let (tempo, meter) = (scene.tempo, bar_meter(scene));
    // Where in the circle a time falls, 0–1 from the top; and how many turns ago.
    let place = |t: f64| -> (f64, i64) {
        match tempo {
            Some(tempo) => {
                let beat = tempo.beat_at(t);
                let bar = meter.bar_of(beat.floor() as i64);
                ((beat - meter.bar_start(bar) as f64) / f64::from(meter.beats), bar)
            }
            // No beat: one turn every two seconds.
            None => ((t / 2.0).rem_euclid(1.0), (t / 2.0).floor() as i64),
        }
    };
    let angle = |frac: f64| -FRAC_PI_2 + frac * TAU;
    let (now_frac, now_turn) = place(scene.time);

    for (row, &lane) in lanes.iter().enumerate() {
        plot::set(cr, lane_color(lane, &colors), 0.07);
        cr.set_line_width(ring_w * 0.8);
        // A fresh path, or the arc would join on from where the header text ended.
        cr.new_path();
        cr.arc(cx, cy, ring_r(row), 0.0, TAU);
        let _ = cr.stroke();
    }

    // Beat ticks, the downbeat at the top, and subdivisions.
    if tempo.is_some() {
        let beats = f64::from(meter.beats);
        let sub = f64::from(prefs.subdivision);
        cr.set_line_width(1.0);
        for s in 0..(beats * sub) as i64 {
            let on_beat = s % prefs.subdivision as i64 == 0;
            let downbeat = s == 0;
            if !(downbeat && prefs.measures || on_beat && prefs.beat_markers || sub > 1.0 && !on_beat) {
                continue;
            }
            let a = angle(s as f64 / (beats * sub));
            let (from, alpha) = if downbeat && prefs.measures {
                (inner * 0.8, 0.5)
            } else if on_beat {
                (inner, 0.2)
            } else {
                (outer - 6.0, 0.15)
            };
            plot::set(cr, colors.fg, alpha);
            cr.move_to(cx + a.cos() * from, cy + a.sin() * from);
            cr.line_to(cx + a.cos() * (outer + 6.0), cy + a.sin() * (outer + 6.0));
            let _ = cr.stroke();
            if on_beat && prefs.beat_markers {
                let r = outer + 16.0;
                centered(cr, cx + a.cos() * r, cy + a.sin() * r + 4.0, 11.0, colors.fg, 0.5, &(s / sub as i64 + 1).to_string());
            }
        }
    }

    // Hits: this turn bright, earlier turns fading when trails are on.
    let turns = if prefs.trails { 4 } else { 1 };
    for o in scene.onsets.iter().filter(|o| o.time <= scene.time) {
        let Some(row) = lanes.iter().position(|&l| l == o.lane) else { continue };
        let (frac, turn) = place(o.time);
        let ago = now_turn - turn;
        if !(0..turns).contains(&ago) {
            continue;
        }
        let a = angle(frac);
        let r = ring_r(row);
        let (x, y) = (cx + a.cos() * r, cy + a.sin() * r);
        let color = lane_color(o.lane, &colors);
        let size = 2.0 + (ring_w * 0.35) * f64::from(o.strength);
        let glow = glow_alpha(scene, o, prefs);
        if glow > 0.01 {
            plot::set(cr, color, glow * 0.4);
            cr.arc(x, y, size * 2.2, 0.0, TAU);
            let _ = cr.fill();
        }
        plot::set(cr, color, 0.9 * 0.45f64.powi(ago as i32));
        cr.arc(x, y, size, 0.0, TAU);
        let _ = cr.fill();
        if ago == 0 {
            draw_flags(cr, x, y - size, y + size, o, prefs, colors.fg);
        }
    }

    // The hand, with a short fading trail behind it.
    let a = angle(now_frac);
    for i in 0..10 {
        let back = a - i as f64 * 0.035;
        plot::set(cr, colors.fg, 0.45 * (1.0 - i as f64 / 10.0));
        cr.set_line_width(if i == 0 { 2.0 } else { 1.0 });
        cr.move_to(cx + back.cos() * inner, cy + back.sin() * inner);
        cr.line_to(cx + back.cos() * (outer + 4.0), cy + back.sin() * (outer + 4.0));
        let _ = cr.stroke();
    }

    // In the middle: the tempo, and the bar.
    match tempo {
        Some(t) => {
            centered(cr, cx, cy + 2.0, (inner * 0.45).clamp(12.0, 40.0), colors.fg, 0.85, &format!("{:.0}", t.bpm));
            centered(cr, cx, cy + 2.0 + (inner * 0.3).clamp(10.0, 18.0), 11.0, colors.fg, 0.5, "BPM");
        }
        None => centered(cr, cx, cy + 4.0, 12.0, colors.fg, 0.5, "listening…"),
    }
}
