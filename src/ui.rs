//! Small UI preferences (analyzer shown, theme) in ~/.config/sbx4-control/ui.json.

use std::fs;
use std::path::PathBuf;

use gtk::glib;
use serde_json::{Map, Value};

fn path() -> PathBuf {
    glib::user_config_dir().join("sbx4-control").join("ui.json")
}

fn load() -> Map<String, Value> {
    fs::read(path()).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
}

pub fn get(key: &str) -> Option<Value> {
    load().remove(key)
}

pub fn set(key: &str, value: Value) {
    let mut prefs = load();
    prefs.insert(key.to_owned(), value);
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, Value::Object(prefs).to_string());
}
