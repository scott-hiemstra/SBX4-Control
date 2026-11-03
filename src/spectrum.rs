//! Live spectrum of what's playing, drawn as bars, a spectrogram or a ring.
//! Audio and analysis come from [`crate::analysis`]: in the control panel,
//! the X4's "What U Hear" loopback only.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, glib};

use crate::analysis::{band_center, Capture, Frame, Sources, BANDS, DB_FLOOR, F_MAX, F_MIN, FRAME_SECONDS};
use crate::plot::{self, LogAxis};
use crate::ui;

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
/// One spectrogram row per frame.
const HISTORY: usize = (HISTORY_SECONDS / FRAME_SECONDS) as usize;
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

    pub fn key(self) -> &'static str {
        match self {
            Style::Bars => "bars",
            Style::Spectrogram => "spectrogram",
            Style::Ring => "ring",
        }
    }
}

pub struct Analyzer {
    pub widget: gtk::DrawingArea,
    sources: Sources,
    /// Sized for the control panel's page; otherwise fills its space.
    compact: bool,
    capture: Rc<RefCell<Option<Capture>>>,
    view: Rc<RefCell<View>>,
    frozen: Rc<Cell<bool>>,
}

impl Analyzer {
    /// The control panel's analyzer: the X4 only, in the style chosen there.
    pub fn new() -> Rc<Self> {
        Self::with(Sources::X4Only, true, style())
    }

    /// Captures only while the widget is on screen.
    pub fn with(sources: Sources, compact: bool, style: Style) -> Rc<Self> {
        let widget = gtk::DrawingArea::builder().hexpand(true).build();
        if compact {
            widget.set_content_height(150);
        } else {
            widget.set_vexpand(true);
        }
        let capture: Rc<RefCell<Option<Capture>>> = Rc::default();
        let view = Rc::new(RefCell::new(View::new()));
        let frozen = Rc::new(Cell::new(false));

        let (v, c) = (view.clone(), capture.clone());
        widget.set_draw_func(move |_, cr, w, h| {
            let error = c.borrow().as_ref().and_then(Capture::error);
            let (view, w, h) = (v.borrow(), f64::from(w), f64::from(h));
            match view.style {
                Style::Bars => draw(cr, w, h, &view, error.as_deref()),
                Style::Spectrogram => draw_spectrogram(cr, w, h, &view, error.as_deref()),
                Style::Ring => draw_ring(cr, w, h, &view, error.as_deref()),
            }
        });
        let (v, c, f) = (view.clone(), capture.clone(), frozen.clone());
        widget.add_tick_callback(move |area, clock| {
            let frames = c.borrow().as_ref().map(Capture::drain).unwrap_or_default();
            if !f.get() {
                let mut view = v.borrow_mut();
                for frame in &frames {
                    view.push(frame);
                }
                view.advance(clock.frame_time());
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });

        let analyzer = Rc::new(Self { widget: widget.clone(), sources, compact, capture, view, frozen });
        analyzer.set_style(style);
        let a = analyzer.clone();
        widget.connect_map(move |_| a.start());
        let a = analyzer.clone();
        widget.connect_unmap(move |_| a.stop());
        analyzer
    }

    pub fn start(&self) {
        if self.capture.borrow().is_none() {
            self.view.borrow_mut().reset();
            self.capture.replace(Some(Capture::start(self.sources)));
        }
    }

    /// Stop capturing and restore the X4's recording source. Blocks briefly.
    pub fn stop(&self) {
        drop(self.capture.take());
    }

    /// The audio source in use, once capture has started.
    pub fn source(&self) -> Option<crate::analysis::Source> {
        self.capture.borrow().as_ref().and_then(Capture::source)
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
        if self.compact {
            self.widget.set_content_height(if style == Style::Bars { 150 } else { TALL });
        }
        self.widget.queue_draw();
    }

    /// Hold the picture still; capture carries on.
    pub fn set_frozen(&self, frozen: bool) {
        self.frozen.set(frozen);
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
    /// The newest frame's bands: mixed, left, right.
    latest: [Vec<f32>; 3],
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
            latest: [(); 3].map(|_| vec![DB_FLOOR; BANDS]),
            bass: 0.0,
        }
    }

    /// Back to silence, for a new capture.
    fn reset(&mut self) {
        self.history.clear();
        for bands in &mut self.latest {
            bands.fill(DB_FLOOR);
        }
    }

    /// Take a new frame. Every one becomes a spectrogram row, so it scrolls in
    /// real time whatever the display's frame rate.
    fn push(&mut self, frame: &Frame) {
        if self.style == Style::Spectrogram {
            if self.history.len() == HISTORY {
                self.history.pop_back();
            }
            self.history.push_front(spectrogram_row(&frame.bands, &plot::Palette::current()));
        }
        self.latest = [frame.bands.clone(), frame.left.clone(), frame.right.clone()];
    }

    /// Move the meters toward the newest frame, for a display frame at `frame_time_us`.
    fn advance(&mut self, frame_time_us: i64) {
        let dt = if self.last_frame == 0 { 0.0 } else { ((frame_time_us - self.last_frame) as f32 / 1e6).min(0.1) };
        self.last_frame = frame_time_us;
        let [bands, left, right] = &self.latest;
        self.mono.advance(bands, dt);
        if self.style == Style::Ring {
            self.left.advance(left, dt);
            self.right.advance(right, dt);
        }
        // The ring pulses with the subwoofer's range, or the lowest bass with the sub off.
        let cutoff = self.crossover.unwrap_or(100.0);
        let bass_db = (0..BANDS)
            .filter(|&i| band_center(i) < cutoff)
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
    let is_sub = |i: usize| view.crossover.is_some_and(|f| band_center(i) < f);
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
