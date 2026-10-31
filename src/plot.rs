//! Shared drawing for the graphs: colors that follow the light/dark style, a
//! log-frequency axis, and biquad responses for drawing filter curves.

use std::f64::consts::PI;

use gtk::cairo;
use rustfft::num_complex::Complex;

pub type Rgb = (f64, f64, f64);

pub struct Palette {
    pub fg: Rgb,
    /// Speakers / the EQ curve.
    pub accent: Rgb,
    /// Subwoofer.
    pub sub: Rgb,
}

impl Palette {
    pub fn current() -> Self {
        if adw::StyleManager::default().is_dark() {
            Self { fg: (1.0, 1.0, 1.0), accent: (0.47, 0.68, 0.93), sub: (1.0, 0.64, 0.34) }
        } else {
            Self { fg: (0.0, 0.0, 0.0), accent: (0.21, 0.52, 0.89), sub: (0.90, 0.38, 0.0) }
        }
    }
}

pub fn set(cr: &cairo::Context, (r, g, b): Rgb, alpha: f64) {
    cr.set_source_rgba(r, g, b, alpha);
}

/// A vertical gradient from `color` at `top` to transparent-ish at `bottom`.
pub fn fade(cr: &cairo::Context, color: Rgb, top: f64, bottom: f64, alpha_top: f64, alpha_bottom: f64) {
    let g = cairo::LinearGradient::new(0.0, top, 0.0, bottom);
    g.add_color_stop_rgba(0.0, color.0, color.1, color.2, alpha_top);
    g.add_color_stop_rgba(1.0, color.0, color.1, color.2, alpha_bottom);
    let _ = cr.set_source(&g);
}

/// Maps frequencies onto `0..width` logarithmically.
#[derive(Clone, Copy)]
pub struct LogAxis {
    pub f_min: f64,
    pub f_max: f64,
    pub width: f64,
}

impl LogAxis {
    pub fn x(&self, f: f64) -> f64 {
        self.width * (f / self.f_min).ln() / (self.f_max / self.f_min).ln()
    }

    pub fn f(&self, x: f64) -> f64 {
        self.f_min * (self.f_max / self.f_min).powf(x / self.width)
    }
}

/// Frequency labels along the bottom edge.
pub fn freq_labels(cr: &cairo::Context, axis: LogAxis, h: f64, fg: Rgb, labels: &[(&str, f64)]) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(10.0);
    set(cr, fg, 0.55);
    for &(label, freq) in labels {
        let x = axis.x(freq);
        if let Ok(ext) = cr.text_extents(label) {
            cr.move_to((x - ext.width() / 2.0).clamp(0.0, axis.width - ext.width()), h - 3.0);
            let _ = cr.show_text(label);
        }
    }
}

/// Starts a new path through `points`.
pub fn polyline(cr: &cairo::Context, points: impl IntoIterator<Item = (f64, f64)>) {
    cr.new_path();
    for (x, y) in points {
        cr.line_to(x, y);
    }
}

/// Faint horizontal grid lines at the given y positions.
pub fn grid(cr: &cairo::Context, w: f64, ys: impl IntoIterator<Item = f64>, fg: Rgb) {
    cr.set_line_width(1.0);
    set(cr, fg, 0.08);
    for y in ys {
        let y = y.round() + 0.5;
        cr.move_to(0.0, y);
        cr.line_to(w, y);
    }
    let _ = cr.stroke();
}

/// Normalized biquad coefficients (RBJ cookbook, as PipeWire's builtin filters).
#[derive(Clone, Copy)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

pub const RATE: f64 = 48_000.0;

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self { b: b.map(|v| v / a[0]), a: [a[1] / a[0], a[2] / a[0]] }
    }

    pub fn lowpass(f: f64, q: f64) -> Self {
        let (w, alpha) = w_alpha(f, q);
        let c = w.cos();
        Self::new([(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0], [1.0 + alpha, -2.0 * c, 1.0 - alpha])
    }

    pub fn highpass(f: f64, q: f64) -> Self {
        let (w, alpha) = w_alpha(f, q);
        let c = w.cos();
        Self::new([(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0], [1.0 + alpha, -2.0 * c, 1.0 - alpha])
    }

    pub fn peaking(f: f64, q: f64, gain_db: f64) -> Self {
        let (w, alpha) = w_alpha(f, q);
        let (c, a) = (w.cos(), 10f64.powf(gain_db / 40.0));
        Self::new(
            [1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a],
        )
    }

    /// Complex response at frequency `f`.
    pub fn response(&self, f: f64) -> Complex<f64> {
        let z1 = Complex::from_polar(1.0, -2.0 * PI * f / RATE);
        let z2 = z1 * z1;
        (self.b[0] + self.b[1] * z1 + self.b[2] * z2) / (1.0 + self.a[0] * z1 + self.a[1] * z2)
    }
}

fn w_alpha(f: f64, q: f64) -> (f64, f64) {
    let w = 2.0 * PI * f / RATE;
    (w, w.sin() / (2.0 * q))
}

pub fn db(c: Complex<f64>) -> f64 {
    20.0 * c.norm().max(1e-6).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linkwitz_riley_halves_sum_flat() {
        let (lp, hp) = (Biquad::lowpass(80.0, 0.707), Biquad::highpass(80.0, 0.707));
        for f in [20.0, 50.0, 80.0, 150.0, 1000.0] {
            let sum = lp.response(f) * lp.response(f) + hp.response(f) * hp.response(f);
            assert!(db(sum).abs() < 0.1, "{f} Hz: {}", db(sum));
        }
        assert!((db(lp.response(80.0) * lp.response(80.0)) + 6.0).abs() < 0.1);
    }

    #[test]
    fn peaking_hits_its_gain_at_center() {
        assert!((db(Biquad::peaking(1000.0, 1.4, 6.0).response(1000.0)) - 6.0).abs() < 0.01);
    }
}
