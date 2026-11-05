//! Animated scenes: a frequency tunnel, a spectrum landscape and a plasma.
//! They share one widget: capture, the beat tracker (for the tunnel), recent
//! spectra (for the landscape) and smoothed bass/mid/treble energy.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, glib};
use sbx4_control::analysis::{band_center, Capture, Lane, Sources, BANDS};
use sbx4_control::plot::{self, Rgb};

use crate::prefs::Prefs;
use crate::rhythm::Rhythm;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tunnel,
    Landscape,
    Plasma,
}

/// Rows of the landscape, newest at the front.
const ROWS: usize = 56;
/// Audio frames per landscape row: 56 rows of 3 frames is about 1.8 seconds.
const FRAMES_PER_ROW: usize = 3;
/// Points along each landscape ridge.
const RIDGE_POINTS: usize = 64;
/// Seconds a tunnel ring takes from the center to the edge.
const RING_FLIGHT: f64 = 2.2;
/// Plasma resolution across; the height follows the window's shape. GTK
/// scales it up to the window on the graphics card.
const PLASMA_W: usize = 192;

/// 0 at the quiet end of what's shown, 1 at the loud end.
fn loudness(db: f32) -> f32 {
    ((db + 70.0) / 60.0).clamp(0.0, 1.0)
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// Deep backgrounds make the scenes glow; light theme gets a near-white.
fn background() -> Rgb {
    if adw::StyleManager::default().is_dark() {
        (0.06, 0.06, 0.07)
    } else {
        (0.97, 0.97, 0.97)
    }
}

struct Ring {
    /// Animation time it was born.
    born: f64,
    /// The spectrum when it was born, 0–1 per band.
    shape: Vec<f32>,
    strength: f64,
    /// 0 bass-heavy (orange) to 1 bright (blue).
    tone: f64,
}

struct State {
    /// Animation clock, seconds; stands still while frozen.
    time: f64,
    last_frame_us: i64,
    /// Newest spectrum, 0–1 per band.
    latest: Vec<f32>,
    /// Smoothed bass, mids and treble, 0–1.
    energy: [f64; 3],
    rows: VecDeque<Vec<f32>>,
    row: Vec<f32>,
    row_frames: usize,
    rings: Vec<Ring>,
    last_beat: Option<i64>,
    /// Newest bass onset already turned into a ring.
    last_onset: f64,
    last_ring: f64,
    phase: [f64; 4],
}

impl State {
    fn new() -> Self {
        Self {
            time: 0.0,
            last_frame_us: 0,
            latest: vec![0.0; BANDS],
            energy: [0.0; 3],
            rows: VecDeque::with_capacity(ROWS),
            row: vec![0.0; BANDS],
            row_frames: 0,
            rings: Vec::new(),
            last_beat: None,
            last_onset: 0.0,
            last_ring: f64::NEG_INFINITY,
            phase: [0.0; 4],
        }
    }

    /// Take the frames since the last display frame, `dt` seconds ago.
    fn advance(&mut self, kind: Kind, frames: &[sbx4_control::analysis::Frame], dt: f64, rhythm: &Rhythm) {
        self.time += dt;
        for frame in frames {
            let levels: Vec<f32> = frame.bands.iter().map(|&db| loudness(db)).collect();
            if kind == Kind::Landscape {
                for (acc, &l) in self.row.iter_mut().zip(&levels) {
                    *acc = acc.max(l);
                }
                self.row_frames += 1;
                if self.row_frames == FRAMES_PER_ROW {
                    if self.rows.len() == ROWS {
                        self.rows.pop_back();
                    }
                    let row = std::mem::replace(&mut self.row, vec![0.0; BANDS]);
                    self.rows.push_front(row);
                    self.row_frames = 0;
                }
            }
            self.latest = levels;
        }

        // Bass, mids and treble: up quickly, down gently.
        let range = |lo: f64, hi: f64, latest: &[f32]| -> f64 {
            let v: Vec<f32> = (0..BANDS).filter(|&i| (lo..hi).contains(&band_center(i))).map(|i| latest[i]).collect();
            let peak = v.iter().copied().fold(0.0, f32::max);
            let mean = v.iter().sum::<f32>() / v.len().max(1) as f32;
            f64::from(0.6 * peak + 0.4 * mean)
        };
        let targets = [range(20.0, 150.0, &self.latest), range(150.0, 4000.0, &self.latest), range(4000.0, 20000.0, &self.latest)];
        for (e, t) in self.energy.iter_mut().zip(targets) {
            *e = if t > *e { *e + (t - *e) * 0.6 } else { (*e - dt * 1.2).max(t) };
        }

        match kind {
            Kind::Tunnel => self.advance_tunnel(rhythm),
            Kind::Plasma => {
                let [bass, mid, treble] = self.energy;
                let speeds = [0.25 + 2.2 * bass, 0.4 + 3.0 * bass, 0.35 + 2.0 * mid, 0.8 + 6.0 * treble];
                for (p, s) in self.phase.iter_mut().zip(speeds) {
                    *p = (*p + dt * s) % (TAU * 1000.0);
                }
            }
            Kind::Landscape => {}
        }
    }

    /// A ring on every beat; without a beat, on each strong bass hit; and a
    /// faint one now and then so the tunnel never stops moving.
    fn advance_tunnel(&mut self, rhythm: &Rhythm) {
        if let Some(tempo) = rhythm.tempo().filter(|t| t.confidence >= 0.25) {
            // Counted in half beats: a strong ring on the beat, a faint one between.
            let half = (tempo.beat_at(rhythm.now()) * 2.0).floor() as i64;
            if self.last_beat.is_some_and(|b| half > b) {
                let strength = if half % 2 == 0 { 0.55 + 0.45 * self.energy[0] } else { 0.3 };
                self.spawn_ring(strength);
            }
            self.last_beat = Some(half);
        } else {
            self.last_beat = None;
            let newest = rhythm.onsets.iter().rev().find(|o| o.lane == Lane::Bass && o.strength > 0.35).map(|o| (o.time, o.strength));
            if let Some((time, strength)) = newest {
                if time > self.last_onset {
                    self.last_onset = time;
                    self.spawn_ring(f64::from(strength));
                }
            }
        }
        if self.time - self.last_ring > 0.9 {
            self.spawn_ring(0.2);
        }
        let now = self.time;
        self.rings.retain(|r| now - r.born < RING_FLIGHT);
    }

    fn spawn_ring(&mut self, strength: f64) {
        let total: f32 = self.latest.iter().sum();
        let high: f32 = self.latest.iter().enumerate().filter(|&(i, _)| band_center(i) > 1000.0).map(|(_, v)| v).sum();
        let tone = if total > 0.0 { f64::from(high / total).clamp(0.0, 1.0) * 1.6 } else { 0.5 };
        self.rings.push(Ring { born: self.time, shape: self.latest.clone(), strength: strength.clamp(0.15, 1.0), tone: tone.min(1.0) });
        self.last_ring = self.time;
    }
}

pub struct SceneView {
    /// What goes in the window: a drawing area, or for the plasma a picture.
    pub widget: gtk::Widget,
    area: Option<gtk::DrawingArea>,
    picture: Option<gtk::Picture>,
    message: Option<gtk::Label>,
    kind: Kind,
    prefs: Rc<RefCell<Prefs>>,
    capture: RefCell<Option<Capture>>,
    rhythm: RefCell<Rhythm>,
    state: RefCell<State>,
    frozen: Cell<bool>,
    last_draw: Cell<i64>,
}

impl SceneView {
    pub fn new(kind: Kind, prefs: Rc<RefCell<Prefs>>) -> Rc<Self> {
        let (widget, area, picture, message) = if kind == Kind::Plasma {
            // GTK scales the small plasma image up on the graphics card;
            // Cairo would do it on the CPU, at several times the cost.
            let picture = gtk::Picture::builder().hexpand(true).vexpand(true).can_shrink(true).keep_aspect_ratio(false).build();
            let message = gtk::Label::builder().css_classes(["title-3"]).visible(false).build();
            let overlay = gtk::Overlay::builder().child(&picture).build();
            overlay.add_overlay(&message);
            (overlay.upcast::<gtk::Widget>(), None, Some(picture), Some(message))
        } else {
            let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
            (area.clone().upcast(), Some(area), None, None)
        };
        let view = Rc::new(Self {
            widget: widget.clone(),
            area: area.clone(),
            picture,
            message,
            kind,
            prefs,
            capture: RefCell::new(None),
            rhythm: RefCell::new(Rhythm::new()),
            state: RefCell::new(State::new()),
            frozen: Cell::new(false),
            last_draw: Cell::new(0),
        });
        if let Some(area) = &area {
            let v = Rc::downgrade(&view);
            area.set_draw_func(move |_, cr, w, h| {
                let Some(v) = v.upgrade() else { return };
                let (w, h) = (f64::from(w), f64::from(h));
                plot::set(cr, background(), 1.0);
                let _ = cr.paint();
                if let Some(error) = v.error() {
                    let colors = plot::Palette::current();
                    cr.set_font_size(14.0);
                    plot::set(cr, colors.fg, 0.6);
                    let width = cr.text_extents(&error).map_or(0.0, |e| e.width());
                    cr.move_to((w - width) / 2.0, h / 2.0);
                    let _ = cr.show_text(&error);
                    return;
                }
                let state = v.state.borrow();
                match v.kind {
                    Kind::Tunnel => draw_tunnel(cr, w, h, &state),
                    _ => draw_landscape(cr, w, h, &state),
                }
            });
        }
        let v = Rc::downgrade(&view);
        widget.add_tick_callback(move |_, clock| {
            let Some(v) = v.upgrade() else { return glib::ControlFlow::Break };
            v.tick(clock.frame_time());
            glib::ControlFlow::Continue
        });
        let v = view.clone();
        widget.connect_map(move |_| v.start());
        let v = view.clone();
        widget.connect_unmap(move |_| v.stop());
        view
    }

    pub fn start(&self) {
        if self.capture.borrow().is_none() {
            self.rhythm.borrow_mut().reset();
            *self.state.borrow_mut() = State::new();
            self.capture.replace(Some(Capture::start(Sources::X4ThenPipeWire)));
        }
    }

    pub fn stop(&self) {
        drop(self.capture.take());
    }

    pub fn source(&self) -> Option<sbx4_control::analysis::Source> {
        self.capture.borrow().as_ref().and_then(Capture::source)
    }

    pub fn set_frozen(&self, frozen: bool) {
        self.frozen.set(frozen);
    }

    fn error(&self) -> Option<String> {
        self.capture.borrow().as_ref().and_then(Capture::error)
    }

    fn tick(&self, frame_us: i64) {
        let frames = self.capture.borrow().as_ref().map(Capture::drain).unwrap_or_default();
        let mut state = self.state.borrow_mut();
        let dt = if state.last_frame_us == 0 { 0.0 } else { ((frame_us - state.last_frame_us) as f64 / 1e6).min(0.1) };
        state.last_frame_us = frame_us;
        if self.kind == Kind::Tunnel {
            let mut rhythm = self.rhythm.borrow_mut();
            for frame in &frames {
                rhythm.push(frame);
            }
        }
        // While frozen, audio is still read (so nothing piles up) but the picture holds.
        if self.frozen.get() {
            return;
        }
        state.advance(self.kind, &frames, dt, &self.rhythm.borrow());

        let min_gap = if self.prefs.borrow().fps == 30 { 30_000 } else { 0 };
        if frame_us - self.last_draw.get() < min_gap {
            return;
        }
        self.last_draw.set(frame_us);
        if let Some(area) = &self.area {
            area.queue_draw();
        }
        if let (Some(picture), Some(message)) = (&self.picture, &self.message) {
            let error = self.error();
            message.set_visible(error.is_some());
            message.set_label(error.as_deref().unwrap_or(""));
            let (w, h) = (f64::from(self.widget.width().max(1)), f64::from(self.widget.height().max(1)));
            let (pixels, pw, ph) = plasma_pixels(&state, w / h);
            let texture = gtk::gdk::MemoryTexture::new(
                pw as i32,
                ph as i32,
                gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
                &glib::Bytes::from_owned(pixels),
                pw * 4,
            );
            picture.set_paintable(Some(&texture));
        }
    }
}

fn draw_tunnel(cr: &cairo::Context, w: f64, h: f64, state: &State) {
    let colors = plot::Palette::current();
    let (cx, cy) = (w / 2.0, h / 2.0);
    let max_r = (w * w + h * h).sqrt() / 2.0 + 20.0;
    let bass = state.energy[0];

    // A glow at the far end that breathes with the bass.
    let glow_r = w.min(h) * (0.08 + 0.14 * bass);
    let glow = cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, glow_r);
    let (r, g, b) = colors.sub;
    glow.add_color_stop_rgba(0.0, r, g, b, 0.2 + 0.55 * bass);
    glow.add_color_stop_rgba(1.0, r, g, b, 0.0);
    let _ = cr.set_source(&glow);
    cr.arc(cx, cy, glow_r, 0.0, TAU);
    let _ = cr.fill();

    // Rings grow exponentially, which reads as moving toward them at a steady speed.
    let r0 = 4.0;
    let rate = (max_r / r0).ln() / RING_FLIGHT;
    let points = 2 * BANDS;
    cr.set_line_join(cairo::LineJoin::Round);
    for ring in &state.rings {
        let age = state.time - ring.born;
        let radius = r0 * (age * rate).exp();
        let near = (radius / max_r).clamp(0.0, 1.0);
        let fade_in = (near / 0.05).min(1.0);
        let fade_out = ((1.0 - near) / 0.35).min(1.0);
        let alpha = ring.strength * fade_in * fade_out;
        if alpha < 0.01 {
            continue;
        }
        let color = mix(colors.sub, colors.accent, ring.tone);
        let spin = ring.born * 0.35 + age * 0.2;
        cr.new_path();
        for j in 0..points {
            // Mirrored, so the bass sits at the bottom on both sides.
            let band = if j < BANDS { j } else { points - 1 - j };
            let bump = 1.0 + 0.35 * f64::from(ring.shape[band]) * near.sqrt();
            let a = std::f64::consts::FRAC_PI_2 + j as f64 / points as f64 * TAU + spin;
            let (x, y) = (cx + a.cos() * radius * bump, cy + a.sin() * radius * bump);
            if j == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        cr.close_path();
        // A soft wide glow under a bright core, both thickening as the ring comes close.
        let width = 1.0 + 9.0 * near * near;
        plot::set(cr, color, alpha * 0.18);
        cr.set_line_width(width * 3.5);
        let _ = cr.stroke_preserve();
        plot::set(cr, color, alpha);
        cr.set_line_width(width);
        let _ = cr.stroke();
    }
}

fn draw_landscape(cr: &cairo::Context, w: f64, h: f64, state: &State) {
    let colors = plot::Palette::current();
    let bg = background();
    let (horizon, front) = (h * 0.3, h * 0.94);
    // Depth 0 at the front row to 1 at the horizon row; things shrink as 1 / (1 + 3·depth).
    let squash = 3.0;
    let far = 1.0 / (1.0 + squash);
    // Ridges are colored in solid steps from bass orange to treble blue: a
    // true gradient on every ridge costs several times as much to draw.
    const STEPS: usize = 8;
    let step_color = |s: usize| mix(colors.sub, colors.accent, (s as f64 / (STEPS - 1) as f64).powf(0.8));
    cr.set_antialias(cairo::Antialias::Fast);
    cr.set_line_join(cairo::LineJoin::Round);

    // Back to front, each ridge hiding the ones behind it.
    for (k, row) in state.rows.iter().enumerate().rev() {
        let depth = k as f64 / (ROWS - 1) as f64;
        let scale = 1.0 / (1.0 + squash * depth);
        let t = (scale - far) / (1.0 - far);
        let base = horizon + (front - horizon) * t;
        let half = w * 0.55 * scale;
        let height = h * 0.38 * scale;
        let point = |p: usize| -> (f64, f64) {
            let pos = p as f64 / (RIDGE_POINTS - 1) as f64;
            let at = pos * (BANDS - 1) as f64;
            let (i, f) = (at.floor() as usize, at.fract());
            let level = f64::from(row[i]) * (1.0 - f) + f64::from(row[(i + 1).min(BANDS - 1)]) * f;
            (w / 2.0 + (pos - 0.5) * 2.0 * half, base - height * level)
        };
        let ridge: Vec<(f64, f64)> = (0..RIDGE_POINTS).map(point).collect();
        cr.new_path();
        cr.move_to(ridge[0].0, base);
        for &(x, y) in &ridge {
            cr.line_to(x, y);
        }
        cr.line_to(ridge[RIDGE_POINTS - 1].0, base);
        cr.close_path();
        plot::set(cr, bg, 1.0);
        let _ = cr.fill();

        // Farther ridges fade into the background; the front ones are brightest.
        let alpha = 0.15 + 0.85 * t;
        cr.set_line_width(0.7 + 1.6 * t);
        let per = (RIDGE_POINTS - 1) as f64 / STEPS as f64;
        for s in 0..STEPS {
            let (from, to) = ((s as f64 * per).floor() as usize, (((s + 1) as f64 * per).ceil() as usize).min(RIDGE_POINTS - 1));
            cr.move_to(ridge[from].0, ridge[from].1);
            for &(x, y) in &ridge[from + 1..=to] {
                cr.line_to(x, y);
            }
            plot::set(cr, step_color(s), alpha);
            let _ = cr.stroke();
        }
    }
}

/// The plasma as B8G8R8A8 pixels for a window of this aspect ratio; returns
/// (pixels, width, height).
fn plasma_pixels(state: &State, aspect: f64) -> (Vec<u8>, usize, usize) {
    let colors = plot::Palette::current();
    let pw = PLASMA_W;
    let ph = ((PLASMA_W as f64 / aspect.max(0.1)).round() as usize).clamp(8, PLASMA_W * 2);
    let [bass, mid, treble] = state.energy;
    let [p0, p1, p2, p3] = state.phase;
    let dark = adw::StyleManager::default().is_dark();

    // A palette that wraps around: deep navy, the accent blue, a near-white
    // crest, the subwoofer orange and back to navy. Sampled once a frame.
    let deep = if dark { (0.02, 0.03, 0.07) } else { (0.85, 0.89, 0.96) };
    let crest = if dark { (0.92, 0.95, 1.0) } else { (1.0, 1.0, 1.0) };
    let stops = [(0.0, deep), (0.3, colors.accent), (0.48, crest), (0.68, colors.sub), (1.0, deep)];
    // Brighter overall when there's more going on.
    let lift = 0.45 + 0.55 * ((bass + mid + treble) / 1.5).min(1.0);
    let lut: Vec<[u8; 4]> = (0..256)
        .map(|i| {
            let x = i as f64 / 255.0;
            let k = stops.iter().rposition(|&(at, _)| at <= x).unwrap_or(0).min(stops.len() - 2);
            let (a, ca) = stops[k];
            let (b, cb) = stops[k + 1];
            let c = mix(ca, cb, ((x - a) / (b - a)).clamp(0.0, 1.0));
            let c = if dark { (c.0 * lift, c.1 * lift, c.2 * lift) } else { mix(deep, c, lift) };
            let [b, g, r] = [c.2, c.1, c.0].map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8);
            [b, g, r, 255]
        })
        .collect();

    // The shorter side spans -1..1, so the waves keep their shape in any window.
    let (su, sv) = if aspect >= 1.0 { (aspect, 1.0) } else { (1.0, 1.0 / aspect) };
    let mut pixels = vec![0u8; pw * ph * 4];
    for y in 0..ph {
        let v = ((y as f64 / ph as f64 * 2.0 - 1.0) * sv) as f32;
        for x in 0..pw {
            let u = ((x as f64 / pw as f64 * 2.0 - 1.0) * su) as f32;
            let (p0, p1, p2, p3) = (p0 as f32, p1 as f32, p2 as f32, p3 as f32);
            // Bass: broad slow waves and a pulse from the middle.
            let broad = (0.3 + bass as f32) * ((u * 1.4 + p0).sin() + (v * 1.2 - p0 * 0.7).sin() + ((u + v) * 0.9 + p0 * 0.5).sin()) / 3.0;
            let radial = bass as f32 * ((u * u + v * v).sqrt() * 3.2 - p1 * 2.0).sin();
            // Mids: medium swirls.
            let swirl = (0.2 + mid as f32) * 0.7 * (u * 4.2 + v * 2.9 + p2).sin() * (v * 3.3 - u * 1.7 - p2 * 0.6).cos();
            // Treble: fine ripples, crossing at an angle and spreading from the middle.
            let ripple = treble as f32 * (0.25 * (u * 11.0 + v * 6.0 + p3).sin() + 0.25 * ((u * u + v * v).sqrt() * 16.0 - p3 * 1.4).sin());
            let value = broad + 0.6 * radial + swirl + ripple;
            // Bands of color that drift slowly through the palette.
            let i = ((value * 0.45 + p0 * 0.02).rem_euclid(1.0) * 255.0) as usize;
            let at = (y * pw + x) * 4;
            pixels[at..at + 4].copy_from_slice(&lut[i.min(255)]);
        }
    }
    (pixels, pw, ph)
}
