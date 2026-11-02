//! The "Effects" page: SBX-style effects through a PipeWire filter, plus the
//! start-at-login option that keeps them running.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::plot::{self, Biquad, LogAxis};
use crate::room_ui::{self, RoomRows};
use crate::sbx::{self, Engine, Kind, Settings, Sub};
use crate::tone::{self, Generator, Signal};
use crate::{device, tray};

struct EffectRow {
    kind: Kind,
    expander: adw::ExpanderRow,
    level: gtk::Scale,
    usable: Cell<bool>,
}

struct SubRows {
    expander: adw::ExpanderRow,
    crossover: gtk::Scale,
    level: gtk::Scale,
    high_pass: gtk::Switch,
    invert: gtk::Switch,
}

fn scale(min: f64, max: f64, step: f64, unit: &'static str) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_width_request(240);
    scale.set_valign(gtk::Align::Center);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_format_value_func(move |_, v| format!("{v:.0} {unit}"));
    scale
}

fn switch_row(title: &str, subtitle: &str) -> (adw::ActionRow, gtk::Switch) {
    let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).activatable_widget(&switch).build();
    row.add_suffix(&switch);
    (row, switch)
}

pub struct SbxPage {
    pub widget: gtk::Stack,
    toasts: adw::ToastOverlay,
    master_row: adw::ActionRow,
    master: gtk::Switch,
    effects: Vec<EffectRow>,
    sub: SubRows,
    graph: gtk::DrawingArea,
    /// What the graph draws: the sub settings and whether the X4 is on headphones.
    graph_state: Rc<Cell<(Sub, bool)>>,
    crossover_listeners: RefCell<Vec<Box<dyn Fn(Option<f64>)>>>,
    room: RoomRows,
    tone: RefCell<Option<Generator>>,
    settings: RefCell<Settings>,
    engine: RefCell<Engine>,
    available: bool,
    updating: Cell<bool>,
    flush_pending: Cell<bool>,
    starting: Cell<bool>,
    /// Set when starting failed; cleared by toggling SBX or replugging the X4.
    failed: Cell<bool>,
}

impl SbxPage {
    pub fn new(toasts: adw::ToastOverlay) -> Rc<Self> {
        let engine = Engine::new();
        let available = sbx::tools_available();

        let master = gtk::Switch::builder().valign(gtk::Align::Center).build();
        let master_row = adw::ActionRow::builder()
            .title("SBX effects")
            .activatable_widget(&master)
            .build();
        master_row.add_suffix(&master);
        let master_group = adw::PreferencesGroup::builder()
            .description(
                "Approximations of Creative's SBX effects, processed on this computer by PipeWire. \
                 While on, audio plays through the “Sound Blaster X4 (SBX)” output.",
            )
            .build();
        master_group.add(&master_row);

        let effects_group = adw::PreferencesGroup::builder().title("Effects").build();
        let effects: Vec<EffectRow> = sbx::EFFECTS
            .iter()
            .map(|&kind| {
                let usable = kind != Kind::SmartVolume || engine.has_compressor();
                let subtitle = if usable { kind.description() } else { "Needs the lsp-plugins-lv2 package" };
                let expander = adw::ExpanderRow::builder()
                    .title(kind.title())
                    .subtitle(subtitle)
                    .show_enable_switch(true)
                    .sensitive(usable)
                    .build();
                let level = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
                level.set_width_request(240);
                level.set_valign(gtk::Align::Center);
                level.set_draw_value(true);
                level.set_value_pos(gtk::PositionType::Right);
                level.set_digits(0);
                let level_row = adw::ActionRow::builder().title("Level").build();
                level_row.add_suffix(&level);
                expander.add_row(&level_row);
                effects_group.add(&expander);
                EffectRow { kind, expander, level, usable: Cell::new(usable) }
            })
            .collect();

        let (high_pass_row, high_pass) =
            switch_row("Bass only from the subwoofer", "Takes the bass below the crossover out of the speakers");
        let (invert_row, invert) = switch_row("Invert polarity", "Try this if the bass gets thinner with the sub on");
        let sub = SubRows {
            expander: adw::ExpanderRow::builder()
                .title("Subwoofer")
                .subtitle("Sends the bass to the X4's subwoofer output, with or without SBX")
                .show_enable_switch(true)
                .build(),
            crossover: scale(40.0, 200.0, 5.0, "Hz"),
            level: scale(-12.0, 12.0, 1.0, "dB"),
            high_pass,
            invert,
        };
        let graph_state = Rc::new(Cell::new((Sub::default(), false)));
        let graph = gtk::DrawingArea::builder()
            .content_height(140)
            .hexpand(true)
            .margin_top(12)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        let state = graph_state.clone();
        graph.set_draw_func(move |_, cr, w, h| {
            let (sub, headphones) = state.get();
            draw_crossover(cr, f64::from(w), f64::from(h), sub, headphones);
        });
        sub.expander.add_row(&graph);
        let c = graph.clone();
        adw::StyleManager::default().connect_dark_notify(move |_| c.queue_draw());
        for (title, widget) in [("Crossover", &sub.crossover), ("Level", &sub.level)] {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(widget);
            sub.expander.add_row(&row);
        }
        sub.expander.add_row(&high_pass_row);
        sub.expander.add_row(&invert_row);
        let sub_group = adw::PreferencesGroup::builder()
            .title("Subwoofer")
            .description("Needs the X4's 2.1 profile. Bypassed on the headphone output.")
            .build();
        sub_group.add(&sub.expander);
        let (room, room_group) = room_ui::build();

        let autostart = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(tray::autostart_enabled())
            .build();
        let autostart_row = adw::ActionRow::builder()
            .title("Start in the tray at login")
            .subtitle("Keeps the effects and the tray menu running after you log in")
            .activatable_widget(&autostart)
            .build();
        autostart_row.add_suffix(&autostart);
        let startup_group = adw::PreferencesGroup::builder().title("Startup").build();
        startup_group.add(&autostart_row);

        let prefs = adw::PreferencesPage::new();
        prefs.add(&master_group);
        prefs.add(&effects_group);
        prefs.add(&sub_group);
        prefs.add(&room_group);
        prefs.add(&startup_group);

        let missing = adw::StatusPage::builder()
            .icon_name("applications-multimedia-symbolic")
            .title("SBX effects need PipeWire")
            .description("Couldn't find pipewire, pw-cli, pw-dump and wpctl.")
            .build();
        let widget = gtk::Stack::new();
        widget.add_named(&prefs, Some("ready"));
        widget.add_named(&missing, Some("missing"));
        widget.set_visible_child_name(if available { "ready" } else { "missing" });

        let page = Rc::new(Self {
            widget,
            toasts,
            master_row,
            master,
            effects,
            sub,
            graph,
            graph_state,
            crossover_listeners: RefCell::new(Vec::new()),
            room,
            tone: RefCell::new(None),
            settings: RefCell::new(sbx::load()),
            engine: RefCell::new(engine),
            available,
            updating: Cell::new(false),
            flush_pending: Cell::new(false),
            starting: Cell::new(false),
            failed: Cell::new(false),
        });
        page.load_widgets();
        page.connect_signals(&autostart);
        page.set_status(if page.settings.borrow().wants_filter() { "Starting…" } else { "Off" });
        if available {
            let p = page.clone();
            glib::timeout_add_seconds_local(2, move || {
                p.tick();
                glib::ControlFlow::Continue
            });
            page.tick();
        }
        page
    }

    fn load_widgets(&self) {
        let settings = self.settings.borrow().clone();
        self.guarded(|| {
            self.master.set_active(settings.enabled);
            for row in &self.effects {
                let effect = settings.effect(row.kind);
                row.expander.set_enable_expansion(effect.on && row.usable.get());
                row.level.set_value(effect.level * 100.0);
            }
            self.sub.expander.set_enable_expansion(settings.sub.on);
            self.sub.crossover.set_value(settings.sub.crossover);
            self.sub.level.set_value(settings.sub.level);
            self.sub.high_pass.set_active(settings.sub.high_pass);
            self.sub.invert.set_active(settings.sub.invert);
            let room = &settings.room;
            self.room.expander.set_enable_expansion(room.on);
            self.room.unit.set_selected(u32::from(room.metric));
            for (spin, &d) in self.room.size.iter().zip(&room.size) {
                spin.set_value(d);
            }
            self.load_cuts();
        });
        self.update_graph();
    }

    fn connect_signals(self: &Rc<Self>, autostart: &gtk::Switch) {
        let p = self.clone();
        self.master.connect_active_notify(move |sw| {
            if !p.updating.get() {
                p.set_enabled(sw.is_active());
            }
        });

        for row in &self.effects {
            let (p, kind) = (self.clone(), row.kind);
            row.expander.connect_enable_expansion_notify(move |expander| {
                if !p.updating.get() {
                    p.settings.borrow_mut().effect_mut(kind).on = expander.enables_expansion();
                    p.schedule_flush();
                }
            });
            let (p, kind) = (self.clone(), row.kind);
            row.level.connect_value_changed(move |scale| {
                if !p.updating.get() {
                    p.settings.borrow_mut().effect_mut(kind).level = scale.value() / 100.0;
                    p.schedule_flush();
                }
            });
        }

        let p = self.clone();
        self.sub.expander.connect_enable_expansion_notify(move |expander| {
            if !p.updating.get() {
                p.settings.borrow_mut().sub.on = expander.enables_expansion();
                p.update_graph();
                p.save();
                p.failed.set(false);
                p.apply();
            }
        });
        let p = self.clone();
        self.sub.crossover.connect_value_changed(move |scale| {
            if !p.updating.get() {
                p.settings.borrow_mut().sub.crossover = scale.value();
                p.schedule_flush();
                p.update_graph();
            }
        });
        let p = self.clone();
        self.sub.level.connect_value_changed(move |scale| {
            if !p.updating.get() {
                p.settings.borrow_mut().sub.level = scale.value();
                p.schedule_flush();
                p.update_graph();
            }
        });
        let p = self.clone();
        self.sub.high_pass.connect_active_notify(move |sw| {
            if !p.updating.get() {
                p.settings.borrow_mut().sub.high_pass = sw.is_active();
                p.schedule_flush();
                p.update_graph();
            }
        });
        let p = self.clone();
        self.sub.invert.connect_active_notify(move |sw| {
            if !p.updating.get() {
                p.settings.borrow_mut().sub.invert = sw.is_active();
                p.schedule_flush();
                p.update_graph();
            }
        });

        self.connect_room_signals();

        let p = self.clone();
        autostart.connect_active_notify(move |sw| {
            if let Err(e) = tray::set_autostart(sw.is_active()) {
                p.toasts.add_toast(adw::Toast::new(&format!("Couldn't change the login item: {e}")));
            }
        });
    }

    fn connect_room_signals(self: &Rc<Self>) {
        let p = self.clone();
        self.room.expander.connect_enable_expansion_notify(move |expander| {
            if !p.updating.get() {
                p.settings.borrow_mut().room.on = expander.enables_expansion();
                p.update_graph();
                p.save();
                p.failed.set(false);
                p.apply();
            }
        });
        for (i, spin) in self.room.size.iter().enumerate() {
            let p = self.clone();
            spin.connect_value_changed(move |spin| {
                if !p.updating.get() {
                    p.settings.borrow_mut().room.size[i] = spin.value();
                    p.save();
                    p.update_graph();
                }
            });
        }
        let p = self.clone();
        self.room.unit.connect_selected_notify(move |unit| {
            if p.updating.get() {
                return;
            }
            // Convert what's there, so switching units keeps the same room.
            let metric = unit.selected() == 1;
            let factor = if metric { 0.3048 } else { 1.0 / 0.3048 };
            let size = {
                let mut settings = p.settings.borrow_mut();
                settings.room.metric = metric;
                settings.room.size = settings.room.size.map(|d| (d * factor * 10.0).round() / 10.0);
                settings.room.size
            };
            p.guarded(|| {
                for (spin, d) in p.room.size.iter().zip(size) {
                    spin.set_value(d);
                }
            });
            p.save();
            p.update_graph();
        });
        let p = self.clone();
        self.room.suggest.connect_clicked(move |_| {
            {
                let mut settings = p.settings.borrow_mut();
                settings.room.cuts = settings.room.suggest_cuts();
            }
            p.guarded(|| p.load_cuts());
            p.schedule_flush();
            p.update_graph();
        });

        for (i, cut) in self.room.cuts.iter().enumerate() {
            let p = self.clone();
            cut.on.connect_active_notify(move |sw| {
                if !p.updating.get() {
                    p.settings.borrow_mut().room.cuts[i].on = sw.is_active();
                    p.cut_changed(i);
                }
            });
            for (scale, field) in [(&cut.freq, 0), (&cut.gain, 1), (&cut.q, 2)] {
                let p = self.clone();
                scale.connect_value_changed(move |scale| {
                    if p.updating.get() {
                        return;
                    }
                    {
                        let mut settings = p.settings.borrow_mut();
                        let c = &mut settings.room.cuts[i];
                        *[&mut c.freq, &mut c.gain, &mut c.q][field] = scale.value();
                    }
                    p.cut_changed(i);
                });
            }
            let p = self.clone();
            cut.play.connect_toggled(move |button| {
                if p.updating.get() {
                    return;
                }
                if button.is_active() {
                    let freq = p.settings.borrow().room.cuts[i].freq;
                    p.play(Signal::Tone, freq, button);
                } else {
                    p.stop_tone();
                }
            });
        }

        let p = self.clone();
        self.room.sweep.connect_toggled(move |button| {
            if !p.updating.get() {
                if button.is_active() {
                    p.play(Signal::Sweep, tone::SWEEP_FROM, button);
                } else {
                    p.stop_tone();
                }
            }
        });
        let p = self.clone();
        self.room.tone.connect_toggled(move |button| {
            if !p.updating.get() {
                if button.is_active() {
                    p.play(Signal::Tone, p.room.tone_freq.value(), button);
                } else {
                    p.stop_tone();
                }
            }
        });
        let p = self.clone();
        self.room.tone_freq.connect_value_changed(move |scale| {
            p.room.tone_label.set_label(&room_ui::format_hz(scale.value()));
            if p.room.tone.is_active() {
                if let Some(generator) = p.tone.borrow().as_ref() {
                    generator.set_freq(scale.value());
                }
            }
        });
        self.room.tone_freq.set_value(60.0);
    }

    /// Put the saved cuts into their rows.
    fn load_cuts(&self) {
        let cuts = self.settings.borrow().room.cuts;
        for (row, cut) in self.room.cuts.iter().zip(cuts) {
            row.on.set_active(cut.on);
            row.freq.set_value(cut.freq);
            row.gain.set_value(cut.gain);
            row.q.set_value(cut.q);
        }
        for i in 0..cuts.len() {
            self.update_cut_title(i);
        }
    }

    fn update_cut_title(&self, i: usize) {
        let cut = self.settings.borrow().room.cuts[i];
        let subtitle = if cut.on { format!("{}, {:.1} dB", room_ui::format_hz(cut.freq), cut.gain) } else { "Off".into() };
        self.room.cuts[i].row.set_subtitle(&subtitle);
    }

    fn cut_changed(self: &Rc<Self>, i: usize) {
        self.update_cut_title(i);
        self.schedule_flush();
        self.update_graph();
        if self.room.cuts[i].play.is_active() {
            if let Some(generator) = self.tone.borrow().as_ref() {
                generator.set_freq(self.settings.borrow().room.cuts[i].freq);
            }
        }
    }

    /// Play a test tone; `button` stays pressed while it plays, the others are released.
    fn play(self: &Rc<Self>, signal: Signal, freq: f64, button: &gtk::ToggleButton) {
        self.stop_tone();
        self.guarded(|| {
            for other in self.tone_buttons() {
                other.set_active(other == *button);
            }
        });
        let target = self.engine.borrow_mut().is_running().then_some(sbx::SINK_NAME);
        match Generator::start(signal, freq, target) {
            Ok(generator) => {
                self.tone.replace(Some(generator));
                self.watch_tone();
            }
            Err(e) => {
                self.guarded(|| button.set_active(false));
                self.toasts.add_toast(adw::Toast::new(&format!("Couldn't play a test tone: {e}")));
            }
        }
    }

    fn stop_tone(&self) {
        if let Some(generator) = self.tone.take() {
            generator.stop();
        }
        self.guarded(|| {
            for button in self.tone_buttons() {
                button.set_active(false);
            }
        });
        self.room.tone_label.set_label(&room_ui::format_hz(self.room.tone_freq.value()));
    }

    fn tone_buttons(&self) -> Vec<gtk::ToggleButton> {
        let mut buttons = vec![self.room.sweep.clone(), self.room.tone.clone()];
        buttons.extend(self.room.cuts.iter().map(|c| c.play.clone()));
        buttons
    }

    /// Show a sweep's frequency as it plays, and release its button when it ends.
    fn watch_tone(self: &Rc<Self>) {
        let p = self.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let (freq, finished, signal) = match p.tone.borrow().as_ref() {
                Some(g) => (g.freq(), g.finished(), g.signal),
                None => return glib::ControlFlow::Break,
            };
            if finished {
                p.stop_tone();
                return glib::ControlFlow::Break;
            }
            if signal == Signal::Sweep {
                p.room.tone_label.set_label(&room_ui::format_hz((freq * 2.0).round() / 2.0));
            }
            glib::ControlFlow::Continue
        });
    }

    fn set_enabled(self: &Rc<Self>, on: bool) {
        self.settings.borrow_mut().enabled = on;
        self.save();
        self.failed.set(false);
        self.apply();
    }

    /// Start, update or stop the filter after SBX or the subwoofer was switched.
    fn apply(self: &Rc<Self>) {
        if !self.settings.borrow().wants_filter() {
            self.stop(true);
            self.set_status("Off");
        } else if self.engine.borrow_mut().is_running() {
            self.engine.borrow().update(&self.settings.borrow());
            self.set_status(&self.running_status());
        } else {
            self.try_start();
        }
    }

    fn update_graph(&self) {
        let settings = self.settings.borrow();
        self.graph_state.set((settings.sub, settings.headphones));
        self.graph.queue_draw();
        self.room.graph_state.set((settings.room, settings.headphones));
        self.room.graph.queue_draw();
        let modes: Vec<String> = settings.room.modes(160.0).iter().map(|m| format!("{:.0}", m.freq)).collect();
        self.room.modes_row.set_subtitle(&if modes.is_empty() {
            "Enter the room's size".to_owned()
        } else {
            format!("Predicted at {} Hz", modes.join(", "))
        });
        let crossover = (settings.sub.on && !settings.headphones).then_some(settings.sub.crossover);
        for listener in self.crossover_listeners.borrow().iter() {
            listener(crossover);
        }
    }

    /// Call `f` with the active crossover frequency (None while the sub is off),
    /// now and on every change.
    pub fn connect_crossover(&self, f: impl Fn(Option<f64>) + 'static) {
        let settings = self.settings.borrow();
        f((settings.sub.on && !settings.headphones).then_some(settings.sub.crossover));
        self.crossover_listeners.borrow_mut().push(Box::new(f));
    }

    fn running_status(&self) -> String {
        if self.settings.borrow().enabled {
            "On: audio plays through “Sound Blaster X4 (SBX)”".into()
        } else {
            "Off (the filter keeps running for the subwoofer and room EQ)".into()
        }
    }

    /// Bypass the crossover while the X4 is on its headphone output.
    pub fn set_headphones(self: &Rc<Self>, on: bool) {
        if self.settings.borrow().headphones != on {
            self.settings.borrow_mut().headphones = on;
            self.schedule_flush();
            self.update_graph();
        }
    }

    /// Start or stop the filter as the X4 comes and goes.
    fn tick(self: &Rc<Self>) {
        if !self.settings.borrow().wants_filter() || self.starting.get() {
            return;
        }
        let present = device::find_port().is_some();
        let running = self.engine.borrow_mut().is_running();
        if !present {
            self.failed.set(false);
            if running {
                self.stop(false);
            }
            self.set_status("Waiting for the Sound Blaster X4…");
        } else if !running && !self.failed.get() {
            self.try_start();
        }
    }

    fn try_start(self: &Rc<Self>) {
        if self.starting.replace(true) {
            return;
        }
        let Some(x4_sink) = sbx::x4_sink() else {
            self.starting.set(false);
            self.set_status("Waiting for the Sound Blaster X4…");
            return;
        };
        self.set_status("Starting…");
        let settings = self.settings.borrow().clone();
        let lfe = x4_sink.lfe;
        let spawned = self.engine.borrow_mut().spawn(&settings, &x4_sink);
        let pid = match spawned.map(|()| self.engine.borrow().pid()) {
            Ok(Some(pid)) => pid,
            Ok(None) => unreachable!("spawn succeeded without a child"),
            Err(e) => {
                self.starting.set(false);
                self.fail(&format!("Couldn't start PipeWire's filter: {e}"));
                return;
            }
        };

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = sbx::wait_for_sink(pid, Duration::from_secs(5));
            if let Ok(id) = result {
                sbx::set_default_sink(id);
            }
            let _ = tx.send_blocking(result);
        });
        let p = self.clone();
        glib::spawn_future_local(async move {
            let result = rx.recv().await.unwrap_or(Err(sbx::StartError::TimedOut));
            p.starting.set(false);
            match result {
                // Switched off while starting.
                Ok(_) if !p.settings.borrow().wants_filter() => p.stop(true),
                Ok(id) => {
                    p.engine.borrow_mut().set_node(id);
                    // Catch up on changes made while starting.
                    p.engine.borrow().update(&p.settings.borrow());
                    p.set_status(&p.running_status());
                    if p.settings.borrow().sub.on && !lfe {
                        p.toasts.add_toast(adw::Toast::new(
                            "The X4 has no subwoofer channel: switch it to the 2.1 profile in Sound settings",
                        ));
                    }
                }
                // lsp-plugins-lv2 is installed but this PipeWire can't load
                // LV2 plugins (Ubuntu 24.04): run everything else without
                // Smart Volume rather than nothing at all.
                Err(e) if e.is_lv2() && p.engine.borrow().has_compressor() => {
                    p.engine.borrow_mut().disable_compressor();
                    p.disable_smart_volume();
                    p.toasts.add_toast(adw::Toast::new(
                        "Smart Volume is off: this PipeWire can't load LV2 plugins",
                    ));
                    p.try_start();
                }
                Err(sbx::StartError::Exited(errors)) => {
                    p.engine.borrow_mut().stop();
                    let reason = errors.lines().next().unwrap_or("no error was logged");
                    p.fail(&format!(
                        "PipeWire's filter quit: {reason} (log: {})",
                        sbx::log_path().display()
                    ));
                }
                Err(sbx::StartError::TimedOut) => {
                    p.engine.borrow_mut().stop();
                    p.fail("PipeWire didn't create the effects output.");
                }
            }
        });
    }

    fn disable_smart_volume(&self) {
        for row in self.effects.iter().filter(|row| row.kind == Kind::SmartVolume) {
            row.usable.set(false);
            self.guarded(|| row.expander.set_enable_expansion(false));
            row.expander.set_sensitive(false);
            row.expander.set_subtitle("This PipeWire build can't load LV2 plugins");
        }
    }

    fn stop(&self, restore_default: bool) {
        self.engine.borrow_mut().stop();
        if restore_default {
            if let Some(sink) = sbx::x4_sink() {
                sbx::set_default_sink(sink.id);
            }
        }
    }

    fn fail(&self, msg: &str) {
        self.failed.set(true);
        self.set_status(msg);
    }

    fn set_status(&self, status: &str) {
        self.master_row.set_subtitle(status);
    }

    /// Apply and save settings shortly after the last change, so dragging a
    /// slider doesn't spawn a pw-cli per pixel.
    fn schedule_flush(self: &Rc<Self>) {
        if self.flush_pending.replace(true) {
            return;
        }
        let p = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(60), move || {
            p.flush_pending.set(false);
            p.engine.borrow().update(&p.settings.borrow());
            p.save();
        });
    }

    fn save(&self) {
        if let Err(e) = sbx::save(&self.settings.borrow()) {
            self.toasts.add_toast(adw::Toast::new(&format!("Couldn't save effect settings: {e}")));
        }
    }

    fn guarded(&self, f: impl FnOnce()) {
        let was = self.updating.replace(true);
        f();
        self.updating.set(was);
    }

    pub fn tray_state(&self, state: &mut tray::State) {
        state.sbx_available = self.available;
        state.sbx_on = self.master.is_active();
        state.effects = self
            .effects
            .iter()
            .map(|row| (row.kind.title().to_owned(), row.expander.enables_expansion(), row.usable.get()))
            .collect();
    }

    pub fn set_enabled_from_tray(&self, on: bool) {
        self.master.set_active(on);
    }

    pub fn set_effect_from_tray(&self, index: usize, on: bool) {
        if let Some(row) = self.effects.get(index).filter(|row| row.usable.get()) {
            row.expander.set_enable_expansion(on);
        }
    }

    /// Stop the filter and hand the default output back to the X4.
    pub fn shutdown(&self) {
        self.stop_tone();
        if self.engine.borrow_mut().is_running() {
            self.stop(true);
        }
    }
}

/// Speakers, sub and their sum around the crossover, as the filter sets them up.
fn draw_crossover(cr: &gtk::cairo::Context, w: f64, h: f64, sub: Sub, headphones: bool) {
    const DB_TOP: f64 = 12.0;
    const DB_BOTTOM: f64 = -30.0;
    let colors = plot::Palette::current();
    let plot_h = h - 16.0;
    let axis = LogAxis { f_min: 20.0, f_max: 500.0, width: w };
    let y_for = |db: f64| plot_h * ((DB_TOP - db) / (DB_TOP - DB_BOTTOM)).clamp(0.0, 1.0);

    plot::grid(cr, w, [6.0, 0.0, -6.0, -12.0, -18.0, -24.0].map(y_for), colors.fg);
    plot::freq_labels(
        cr,
        axis,
        h,
        colors.fg,
        &[("20", 20.0), ("40", 40.0), ("80", 80.0), ("160", 160.0), ("320", 320.0)],
    );

    let active = sub.on && !headphones;
    let (lp, hp) = (Biquad::lowpass(sub.crossover, 0.707), Biquad::highpass(sub.crossover, 0.707));
    let gain = if sub.invert { -1.0 } else { 1.0 } * 10f64.powf(sub.level / 20.0);
    let curve = |x: f64| {
        let f = axis.f(x);
        let speakers = if active && sub.high_pass { hp.response(f) * hp.response(f) } else { 1.0.into() };
        let woofer = if active { lp.response(f) * lp.response(f) * gain } else { 0.0.into() };
        (speakers, woofer)
    };
    let points: Vec<_> = (0..=w as usize / 2)
        .map(|i| {
            let x = i as f64 * 2.0;
            let (speakers, woofer) = curve(x);
            (x, speakers, woofer)
        })
        .collect();

    // The sub: a filled area, so it reads as "this part goes to the sub".
    if active {
        cr.move_to(0.0, plot_h);
        for &(x, _, b) in &points {
            cr.line_to(x, y_for(plot::db(b)));
        }
        cr.line_to(w, plot_h);
        cr.close_path();
        plot::fade(cr, colors.sub, 0.0, plot_h, 0.45, 0.05);
        let _ = cr.fill();
        cr.set_line_width(2.0);
        plot::set(cr, colors.sub, 1.0);
        plot::polyline(cr, points.iter().map(|&(x, _, b)| (x, y_for(plot::db(b)))));
        let _ = cr.stroke();
    }

    cr.set_line_width(2.0);
    plot::set(cr, colors.accent, 1.0);
    plot::polyline(cr, points.iter().map(|&(x, s, _)| (x, y_for(plot::db(s)))));
    let _ = cr.stroke();

    // What you hear: the two added up (in phase, or not, with the polarity switch).
    if active {
        cr.set_line_width(1.5);
        cr.set_dash(&[4.0, 3.0], 0.0);
        plot::set(cr, colors.fg, 0.6);
        plot::polyline(cr, points.iter().map(|&(x, s, b)| (x, y_for(plot::db(s + b)))));
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);

        let x = axis.x(sub.crossover).round() + 0.5;
        cr.set_line_width(1.0);
        plot::set(cr, colors.fg, 0.25);
        cr.move_to(x, 0.0);
        cr.line_to(x, plot_h);
        let _ = cr.stroke();
    }

    // Legend.
    cr.set_font_size(11.0);
    let mut x = 4.0;
    let entries: &[(&str, plot::Rgb, f64)] = if active {
        &[("Speakers", colors.accent, 1.0), ("Sub", colors.sub, 1.0), ("Combined", colors.fg, 0.6)]
    } else {
        &[("Speakers", colors.accent, 1.0)]
    };
    for &(label, color, alpha) in entries {
        plot::set(cr, color, alpha);
        cr.rectangle(x, 6.0, 10.0, 3.0);
        let _ = cr.fill();
        plot::set(cr, colors.fg, 0.7);
        cr.move_to(x + 14.0, 12.0);
        let _ = cr.show_text(label);
        x += 14.0 + cr.text_extents(label).map_or(50.0, |e| e.x_advance()) + 12.0;
    }
    if headphones && sub.on {
        plot::set(cr, colors.fg, 0.55);
        cr.move_to(x, 12.0);
        let _ = cr.show_text("(bypassed on headphones)");
    }
}
