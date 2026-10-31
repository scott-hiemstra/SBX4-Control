//! The "Mixer" page: the X4's USB Audio Class volume and routing controls,
//! through ALSA.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use alsa::mixer::{Mixer, Selem, SelemChannelId, SelemId};
use gtk::glib;

const CARD_NAME: &str = "Sound Blaster X4";
const CHANNEL: SelemChannelId = SelemChannelId::FrontLeft;

type Refresher = Box<dyn Fn()>;

#[derive(Clone, Copy)]
enum Dir {
    Playback,
    Capture,
}

impl Dir {
    fn range(self, s: &Selem) -> (i64, i64) {
        match self {
            Dir::Playback => s.get_playback_volume_range(),
            Dir::Capture => s.get_capture_volume_range(),
        }
    }

    fn volume(self, s: &Selem) -> Option<i64> {
        match self {
            Dir::Playback => s.get_playback_volume(CHANNEL),
            Dir::Capture => s.get_capture_volume(CHANNEL),
        }
        .ok()
    }

    fn set_volume(self, s: &Selem, value: i64) {
        let _ = match self {
            Dir::Playback => s.set_playback_volume_all(value),
            Dir::Capture => s.set_capture_volume_all(value),
        };
    }

    fn db(self, s: &Selem) -> Option<f32> {
        match self {
            Dir::Playback => s.get_playback_vol_db(CHANNEL),
            Dir::Capture => s.get_capture_vol_db(CHANNEL),
        }
        .ok()
        .map(|mb| mb.to_db())
    }

    fn has_switch(self, s: &Selem) -> bool {
        match self {
            Dir::Playback => s.has_playback_switch(),
            Dir::Capture => s.has_capture_switch(),
        }
    }

    fn switch(self, s: &Selem) -> Option<bool> {
        if !self.has_switch(s) {
            return None;
        }
        match self {
            Dir::Playback => s.get_playback_switch(CHANNEL),
            Dir::Capture => s.get_capture_switch(CHANNEL),
        }
        .ok()
        .map(|v| v != 0)
    }

    fn set_switch(self, s: &Selem, on: bool) {
        let _ = match self {
            Dir::Playback => s.set_playback_switch_all(i32::from(on)),
            Dir::Capture => s.set_capture_switch_all(i32::from(on)),
        };
    }
}

pub struct MixerPage {
    pub widget: gtk::Stack,
    mixer: RefCell<Option<Rc<Mixer>>>,
    refreshers: RefCell<Vec<Refresher>>,
    updating: Rc<Cell<bool>>,
}

impl MixerPage {
    pub fn new() -> Rc<Self> {
        let missing = adw::StatusPage::builder()
            .icon_name("audio-volume-muted-symbolic")
            .title("No Sound Blaster X4 found")
            .description("The mixer appears when the X4 is connected.")
            .build();
        let widget = gtk::Stack::new();
        widget.add_named(&missing, Some("missing"));

        let page = Rc::new(Self {
            widget,
            mixer: RefCell::new(None),
            refreshers: RefCell::new(Vec::new()),
            updating: Rc::new(Cell::new(false)),
        });
        page.try_open();
        let p = page.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            p.tick();
            glib::ControlFlow::Continue
        });
        page
    }

    /// Pick up changes made elsewhere (PipeWire, alsamixer, the volume knob) and hotplug.
    fn tick(&self) {
        let mixer = self.mixer.borrow().clone();
        match mixer {
            None => self.try_open(),
            Some(mixer) => {
                if mixer.handle_events().is_err() || find_card().is_none() {
                    self.close();
                    return;
                }
                for refresh in self.refreshers.borrow().iter() {
                    refresh();
                }
            }
        }
    }

    fn try_open(&self) {
        let Some(index) = find_card() else { return };
        let Ok(mixer) = Mixer::new(&format!("hw:{index}"), false) else { return };
        let mixer = Rc::new(mixer);
        let content = self.build(&mixer);
        if let Some(old) = self.widget.child_by_name("ready") {
            self.widget.remove(&old);
        }
        self.widget.add_named(&content, Some("ready"));
        self.widget.set_visible_child_name("ready");
        self.mixer.replace(Some(mixer));
    }

    fn close(&self) {
        self.mixer.replace(None);
        self.refreshers.borrow_mut().clear();
        self.widget.set_visible_child_name("missing");
    }

    fn build(&self, mixer: &Rc<Mixer>) -> adw::PreferencesPage {
        let outputs = adw::PreferencesGroup::builder().title("Outputs").build();
        let monitoring = adw::PreferencesGroup::builder()
            .title("Monitoring")
            .description("Inputs played straight through the X4's outputs")
            .build();
        let recording = adw::PreferencesGroup::builder().title("Recording").build();

        // Index 1 controls are the mono "master" duplicates of index 0; skip them.
        let names: Vec<String> = mixer
            .iter()
            .filter_map(Selem::new)
            .filter(|s| s.get_id().get_index() == 0)
            .filter_map(|s| s.get_id().get_name().ok().map(str::to_owned))
            .collect();

        let mut refreshers = Vec::new();
        for name in &names {
            let Some(s) = selem(mixer, name) else { continue };
            let (playback, capture) = (s.has_playback_volume(), s.has_capture_volume());
            if s.is_enumerated() {
                recording.add(&self.enum_row(mixer, name, &mut refreshers));
            } else if playback && capture {
                monitoring.add(&self.volume_row(mixer, name, Dir::Playback, &mut refreshers));
                recording.add(&self.volume_row(mixer, name, Dir::Capture, &mut refreshers));
            } else if playback {
                outputs.add(&self.volume_row(mixer, name, Dir::Playback, &mut refreshers));
            } else if capture {
                recording.add(&self.volume_row(mixer, name, Dir::Capture, &mut refreshers));
            } else if s.has_playback_switch() {
                recording.add(&self.switch_row(mixer, name, &mut refreshers));
            }
        }
        self.refreshers.replace(refreshers);

        let page = adw::PreferencesPage::new();
        page.add(&outputs);
        page.add(&monitoring);
        page.add(&recording);
        page
    }

    fn volume_row(&self, mixer: &Rc<Mixer>, name: &str, dir: Dir, refreshers: &mut Vec<Refresher>) -> adw::ActionRow {
        let s = selem(mixer, name).expect("control listed by the mixer");
        let (min, max) = dir.range(&s);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min as f64, max as f64, 1.0);
        scale.set_draw_value(false);
        scale.set_width_request(220);
        scale.set_valign(gtk::Align::Center);
        let row = adw::ActionRow::builder().title(pretty(name)).build();
        row.add_suffix(&scale);
        let switch = dir.has_switch(&s).then(|| {
            let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
            row.add_suffix(&switch);
            switch
        });

        let refresh = {
            let (mixer, name, updating) = (mixer.clone(), name.to_owned(), self.updating.clone());
            let (scale, switch, row) = (scale.clone(), switch.clone(), row.clone());
            move || {
                let Some(s) = selem(&mixer, &name) else { return };
                let was = updating.replace(true);
                if let Some(v) = dir.volume(&s) {
                    if (scale.value() - v as f64).abs() >= 0.5 {
                        scale.set_value(v as f64);
                    }
                }
                if let (Some(switch), Some(on)) = (&switch, dir.switch(&s)) {
                    if switch.is_active() != on {
                        switch.set_active(on);
                    }
                }
                row.set_subtitle(&level_text(&s, dir, min, max));
                updating.set(was);
            }
        };
        refresh();
        refreshers.push(Box::new(refresh));

        let (m, n, updating) = (mixer.clone(), name.to_owned(), self.updating.clone());
        let r = row.clone();
        scale.connect_value_changed(move |scale| {
            if updating.get() {
                return;
            }
            if let Some(s) = selem(&m, &n) {
                dir.set_volume(&s, scale.value().round() as i64);
                r.set_subtitle(&level_text(&s, dir, min, max));
            }
        });
        if let Some(switch) = switch {
            let (m, n, updating) = (mixer.clone(), name.to_owned(), self.updating.clone());
            switch.connect_active_notify(move |switch| {
                if !updating.get() {
                    if let Some(s) = selem(&m, &n) {
                        dir.set_switch(&s, switch.is_active());
                    }
                }
            });
        }
        row
    }

    fn enum_row(&self, mixer: &Rc<Mixer>, name: &str, refreshers: &mut Vec<Refresher>) -> adw::ComboRow {
        let s = selem(mixer, name).expect("control listed by the mixer");
        let items: Vec<String> = s.iter_enum().map(|it| it.flatten().collect()).unwrap_or_default();
        let items: Vec<&str> = items.iter().map(String::as_str).collect();
        let row = adw::ComboRow::builder()
            .title(pretty(name))
            .model(&gtk::StringList::new(&items))
            .build();

        let refresh = {
            let (mixer, name, updating, row) = (mixer.clone(), name.to_owned(), self.updating.clone(), row.clone());
            move || {
                let Some(item) = selem(&mixer, &name).and_then(|s| s.get_enum_item(CHANNEL).ok()) else {
                    return;
                };
                if row.selected() != item {
                    let was = updating.replace(true);
                    row.set_selected(item);
                    updating.set(was);
                }
            }
        };
        refresh();
        refreshers.push(Box::new(refresh));

        let (m, n, updating) = (mixer.clone(), name.to_owned(), self.updating.clone());
        row.connect_selected_notify(move |row| {
            if !updating.get() {
                if let Some(s) = selem(&m, &n) {
                    let _ = s.set_enum_item(CHANNEL, row.selected());
                }
            }
        });
        row
    }

    fn switch_row(&self, mixer: &Rc<Mixer>, name: &str, refreshers: &mut Vec<Refresher>) -> adw::ActionRow {
        let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        let row = adw::ActionRow::builder().title(pretty(name)).activatable_widget(&switch).build();
        row.add_suffix(&switch);

        let refresh = {
            let (mixer, name, updating, switch) = (mixer.clone(), name.to_owned(), self.updating.clone(), switch.clone());
            move || {
                let Some(on) = selem(&mixer, &name).and_then(|s| Dir::Playback.switch(&s)) else { return };
                if switch.is_active() != on {
                    let was = updating.replace(true);
                    switch.set_active(on);
                    updating.set(was);
                }
            }
        };
        refresh();
        refreshers.push(Box::new(refresh));

        let (m, n, updating) = (mixer.clone(), name.to_owned(), self.updating.clone());
        switch.connect_active_notify(move |switch| {
            if !updating.get() {
                if let Some(s) = selem(&m, &n) {
                    Dir::Playback.set_switch(&s, switch.is_active());
                }
            }
        });
        row
    }
}

fn find_card() -> Option<i32> {
    alsa::card::Iter::new()
        .flatten()
        .find(|card| card.get_name().is_ok_and(|n| n.contains(CARD_NAME)))
        .map(|card| card.get_index())
}

fn selem<'a>(mixer: &'a Mixer, name: &str) -> Option<Selem<'a>> {
    mixer.find_selem(&SelemId::new(name, 0))
}

fn level_text(s: &Selem, dir: Dir, min: i64, max: i64) -> String {
    if dir.switch(s) == Some(false) {
        return "Off".into();
    }
    match dir.db(s) {
        Some(db) => format!("{db:.1} dB"),
        None => {
            let v = dir.volume(s).unwrap_or(min);
            format!("{}%", (v - min) * 100 / (max - min).max(1))
        }
    }
}

fn pretty(name: &str) -> &str {
    match name {
        "PCM Capture Source" => "Recording source",
        "SPDIF-Out" => "S/PDIF out",
        "SPDIF In" => "S/PDIF in",
        "What U Hear" => "What U Hear (loopback)",
        other => other,
    }
}
