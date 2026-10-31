//! Tray icon (StatusNotifierItem) and the start-at-login entry.
//!
//! The tray runs on ksni's own thread. Its menu sends [`Command`]s to the GTK
//! main loop, and the main loop pushes [`State`] back when the UI changes.

use std::fs;
use std::io;
use std::path::PathBuf;

use gtk::glib;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub connected: bool,
    pub headphones: bool,
    pub eq_on: bool,
    pub presets: Vec<String>,
    pub preset: usize,
    pub sbx_available: bool,
    pub sbx_on: bool,
    /// (name, on, usable)
    pub effects: Vec<(String, bool, bool)>,
}

#[derive(Clone, Debug)]
pub enum Command {
    Open,
    Quit,
    Headphones(bool),
    Eq(bool),
    Preset(usize),
    Sbx(bool),
    Effect(usize, bool),
}

pub struct Tray {
    pub state: State,
    commands: async_channel::Sender<Command>,
}

impl Tray {
    pub fn new(commands: async_channel::Sender<Command>) -> Self {
        Self { state: State::default(), commands }
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send_blocking(command);
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "sbx4-control".into()
    }

    fn title(&self) -> String {
        "Sound Blaster X4".into()
    }

    fn icon_name(&self) -> String {
        match (self.state.connected, self.state.headphones) {
            (false, _) => "audio-card-symbolic",
            (true, true) => "audio-headphones-symbolic",
            (true, false) => "audio-speakers-symbolic",
        }
        .into()
    }

    fn tool_tip(&self) -> ToolTip {
        let s = &self.state;
        let description = if s.connected {
            let output = if s.headphones { "Headphones" } else { "Speakers" };
            let on_off = |on: bool| if on { "on" } else { "off" };
            format!("{output} · EQ {} · SBX {}", on_off(s.eq_on), on_off(s.sbx_on))
        } else {
            "Not connected".into()
        };
        ToolTip { title: "Sound Blaster X4".into(), description, ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Open);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let mut items: Vec<MenuItem<Self>> = vec![
            RadioGroup {
                selected: usize::from(s.headphones),
                select: Box::new(|t: &mut Self, i| t.send(Command::Headphones(i == 1))),
                options: ["Speakers", "Headphones"]
                    .map(|label| RadioItem { label: label.into(), enabled: s.connected, ..Default::default() })
                    .into(),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Equalizer".into(),
                checked: s.eq_on,
                enabled: s.connected,
                activate: Box::new(|t: &mut Self| t.send(Command::Eq(!t.state.eq_on))),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "EQ Preset".into(),
                enabled: s.connected,
                submenu: vec![RadioGroup {
                    selected: s.preset,
                    select: Box::new(|t: &mut Self, i| t.send(Command::Preset(i))),
                    // Entry 0 is "Custom": shown, but not something to pick.
                    options: s
                        .presets
                        .iter()
                        .enumerate()
                        .map(|(i, name)| RadioItem { label: name.clone(), enabled: i != 0, ..Default::default() })
                        .collect(),
                    ..Default::default()
                }
                .into()],
                ..Default::default()
            }
            .into(),
        ];

        if s.sbx_available {
            items.push(MenuItem::Separator);
            items.push(
                CheckmarkItem {
                    label: "SBX Effects".into(),
                    checked: s.sbx_on,
                    activate: Box::new(|t: &mut Self| t.send(Command::Sbx(!t.state.sbx_on))),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                SubMenu {
                    label: "Effects".into(),
                    submenu: s
                        .effects
                        .iter()
                        .enumerate()
                        .map(|(i, (name, on, usable))| {
                            CheckmarkItem {
                                label: name.clone(),
                                checked: *on,
                                enabled: *usable,
                                activate: Box::new(move |t: &mut Self| {
                                    let on = t.state.effects.get(i).is_some_and(|e| !e.1);
                                    t.send(Command::Effect(i, on));
                                }),
                                ..Default::default()
                            }
                            .into()
                        })
                        .collect(),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Open Sound Blaster X4".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

fn autostart_path() -> PathBuf {
    glib::user_config_dir().join("autostart").join("io.github.Sbx4Control.desktop")
}

pub fn autostart_enabled() -> bool {
    autostart_path().exists()
}

pub fn set_autostart(on: bool) -> io::Result<()> {
    let path = autostart_path();
    if !on {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(
        path,
        "[Desktop Entry]\nType=Application\nName=Sound Blaster X4\n\
         Exec=sbx4-control --background\nIcon=audio-card\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
    )
}
