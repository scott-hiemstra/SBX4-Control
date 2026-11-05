//! The visualizer's preferences, in ~/.config/sbx4-control/visualizer.json.
//! Missing or unknown values fall back to the defaults.

use std::fs;
use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

use crate::rhythm::MeterChoice;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum View {
    Bars,
    Spectrogram,
    Ring,
    Rhythm,
    Tunnel,
    Landscape,
    Plasma,
}

impl View {
    pub const ALL: [View; 7] =
        [View::Bars, View::Spectrogram, View::Ring, View::Rhythm, View::Tunnel, View::Landscape, View::Plasma];

    pub fn title(self) -> &'static str {
        match self {
            View::Bars => "Bars",
            View::Spectrogram => "Spectrogram",
            View::Ring => "Ring",
            View::Rhythm => "Rhythm",
            View::Tunnel => "Frequency Tunnel",
            View::Landscape => "Spectrum Landscape",
            View::Plasma => "Plasma",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Timeline,
    PulseGrid,
    Circular,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Timeline, Mode::PulseGrid, Mode::Circular];

    pub fn title(self) -> &'static str {
        match self {
            Mode::Timeline => "Timeline",
            Mode::PulseGrid => "Pulse Grid",
            Mode::Circular => "Circular Rhythm",
        }
    }
}

/// Time signature setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Signature {
    Auto,
    FourFour,
    ThreeFour,
    FiveFour,
    SevenEight,
    Custom,
}

impl Signature {
    pub const ALL: [Signature; 6] =
        [Signature::Auto, Signature::FourFour, Signature::ThreeFour, Signature::FiveFour, Signature::SevenEight, Signature::Custom];

    pub fn title(self) -> &'static str {
        match self {
            Signature::Auto => "Auto (estimate)",
            Signature::FourFour => "4/4",
            Signature::ThreeFour => "3/4",
            Signature::FiveFour => "5/4",
            Signature::SevenEight => "7/8",
            Signature::Custom => "Custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub view: View,
    pub mode: Mode,
    /// Bass, low mids, high mids, treble.
    pub lanes: [bool; 4],
    /// Seconds of history: 2, 4, 8, 16 or 32.
    pub history: u32,
    /// 0 (only strong transients) to 1 (weak ones too).
    pub sensitivity: f32,
    pub bpm_override: bool,
    pub bpm: f64,
    pub beat_markers: bool,
    /// Grid lines per beat: 1 (¼), 2 (⅛) or 4 (¹⁄₁₆).
    pub subdivision: u32,
    pub measures: bool,
    pub signature: Signature,
    pub custom_beats: u32,
    pub custom_unit: u32,
    pub repeating: bool,
    pub syncopation: bool,
    pub polyrhythm: bool,
    /// 0.5–3: how fast the timeline moves (it then shows less of the history).
    pub scroll_speed: f64,
    /// 0–1.
    pub glow: f64,
    pub trails: bool,
    /// 0 (slow) to 1 (fast).
    pub peak_decay: f64,
    /// 30 or 60.
    pub fps: u32,
    pub auto_hide: bool,
    pub controls_shown: bool,
    pub advanced_open: bool,
    pub theme: Theme,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            view: View::Rhythm,
            mode: Mode::Timeline,
            lanes: [true; 4],
            history: 8,
            sensitivity: 0.5,
            bpm_override: false,
            bpm: 120.0,
            beat_markers: true,
            subdivision: 2,
            measures: true,
            signature: Signature::Auto,
            custom_beats: 6,
            custom_unit: 8,
            repeating: false,
            syncopation: false,
            polyrhythm: false,
            scroll_speed: 1.0,
            glow: 0.6,
            trails: true,
            peak_decay: 0.5,
            fps: 60,
            auto_hide: true,
            controls_shown: true,
            advanced_open: false,
            theme: Theme::Dark,
        }
    }
}

impl Prefs {
    pub const HISTORIES: [u32; 5] = [2, 4, 8, 16, 32];
    pub const SUBDIVISIONS: [(u32, &'static str); 3] = [(1, "¼"), (2, "⅛"), (4, "¹⁄₁₆")];

    pub fn load() -> Self {
        let prefs: Self = fs::read(path()).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
        prefs.sanitized()
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(path, data);
        }
    }

    /// Values from a hand-edited file kept in range.
    fn sanitized(mut self) -> Self {
        if !Self::HISTORIES.contains(&self.history) {
            self.history = 8;
        }
        if !Self::SUBDIVISIONS.iter().any(|&(s, _)| s == self.subdivision) {
            self.subdivision = 2;
        }
        if self.fps != 30 {
            self.fps = 60;
        }
        self.sensitivity = self.sensitivity.clamp(0.0, 1.0);
        self.bpm = self.bpm.clamp(30.0, 300.0);
        self.custom_beats = self.custom_beats.clamp(1, 16);
        if ![2, 4, 8, 16].contains(&self.custom_unit) {
            self.custom_unit = 8;
        }
        self.scroll_speed = self.scroll_speed.clamp(0.5, 3.0);
        self.glow = self.glow.clamp(0.0, 1.0);
        self.peak_decay = self.peak_decay.clamp(0.0, 1.0);
        self
    }

    pub fn meter_choice(&self) -> MeterChoice {
        let fixed = |beats, unit| MeterChoice::Fixed { beats, unit };
        match self.signature {
            Signature::Auto => MeterChoice::Auto,
            Signature::FourFour => fixed(4, 4),
            Signature::ThreeFour => fixed(3, 4),
            Signature::FiveFour => fixed(5, 4),
            Signature::SevenEight => fixed(7, 8),
            Signature::Custom => fixed(self.custom_beats, self.custom_unit),
        }
    }

    pub fn manual_bpm(&self) -> Option<f64> {
        self.bpm_override.then_some(self.bpm)
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("sbx4-control").join("visualizer.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_fills_in_missing_values() {
        let prefs = Prefs { mode: Mode::Circular, lanes: [true, false, true, false], ..Prefs::default() };
        let json = serde_json::to_string(&prefs).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&json).unwrap(), prefs);

        let partial: Prefs = serde_json::from_str(r#"{"mode":"pulse-grid","history":5,"fps":45}"#).unwrap();
        let partial = partial.sanitized();
        assert_eq!(partial.mode, Mode::PulseGrid);
        assert_eq!(partial.history, 8);
        assert_eq!(partial.fps, 60);
        assert_eq!(partial.view, View::Rhythm);
    }
}
