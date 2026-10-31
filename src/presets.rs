//! EQ presets: a few built-ins plus the user's own, saved as JSON.

use std::fs;
use std::io;
use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub preamp: f32,
    pub bands: [f32; 10],
}

impl Preset {
    pub fn matches(&self, other: &Preset) -> bool {
        (self.preamp - other.preamp).abs() < 0.05
            && self.bands.iter().zip(&other.bands).all(|(a, b)| (a - b).abs() < 0.05)
    }
}

pub fn builtin() -> Vec<Preset> {
    let preset = |name: &str, preamp, bands| Preset { name: name.into(), preamp, bands };
    vec![
        preset("Flat", 0.0, [0.0; 10]),
        preset("Bass boost", -3.0, [6.0, 5.0, 4.0, 2.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0]),
        preset("Treble boost", -3.0, [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.5, 4.0, 5.0, 6.0]),
        preset("V-shape", -3.0, [5.0, 4.0, 2.0, 0.0, -2.0, -2.0, 0.0, 2.0, 4.0, 5.0]),
        preset("Vocal", -2.0, [-2.0, -2.0, -1.0, 1.0, 3.0, 3.5, 3.0, 1.5, 0.0, -1.0]),
        preset("Footsteps", -3.0, [-4.0, -3.0, -2.0, 0.0, 2.0, 3.5, 5.0, 4.0, 2.0, 0.0]),
    ]
}

fn path() -> PathBuf {
    glib::user_config_dir().join("sbx4-control").join("presets.json")
}

pub fn load_user() -> Vec<Preset> {
    fs::read(path())
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn save_user(presets: &[Preset]) -> io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_vec_pretty(presets)?)
}
