//! The settings sidebar. Simple controls up top; the rest of the rhythm
//! analysis in a collapsible "Advanced" section. Every change goes into the
//! shared preferences and calls `on_change` (which saves and redraws).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::prefs::{Mode, Prefs, Signature, View};
use crate::rhythm::TempoSource;
use crate::rhythm_view::RhythmView;

pub struct Controls {
    pub widget: gtk::ScrolledWindow,
    rhythm_groups: Vec<adw::PreferencesGroup>,
    tempo_row: adw::ActionRow,
    override_switch: gtk::Switch,
    bpm_spin: gtk::SpinButton,
    rhythm: Rc<RhythmView>,
    prefs: Rc<RefCell<Prefs>>,
    /// Set while the code itself changes a widget, so its handler doesn't echo.
    updating: Rc<Cell<bool>>,
}

type OnChange = Rc<dyn Fn()>;

/// Runs `f` on the preferences, then reports the change, unless the code
/// itself is setting the widget.
fn edit(prefs: &Rc<RefCell<Prefs>>, on_change: &OnChange, updating: &Rc<Cell<bool>>, f: impl FnOnce(&mut Prefs)) {
    if updating.get() {
        return;
    }
    f(&mut prefs.borrow_mut());
    on_change();
}

fn switch_row(
    title: &str,
    subtitle: Option<&str>,
    on: bool,
    prefs: &Rc<RefCell<Prefs>>,
    on_change: &OnChange,
    updating: &Rc<Cell<bool>>,
    set: impl Fn(&mut Prefs, bool) + 'static,
) -> (adw::ActionRow, gtk::Switch) {
    let switch = gtk::Switch::builder().active(on).valign(gtk::Align::Center).build();
    let row = adw::ActionRow::builder().title(title).activatable_widget(&switch).build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    row.add_suffix(&switch);
    let (p, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
    switch.connect_active_notify(move |sw| edit(&p, &c, &u, |prefs| set(prefs, sw.is_active())));
    (row, switch)
}

#[allow(clippy::too_many_arguments)]
fn scale_row(
    title: &str,
    subtitle: Option<&str>,
    (min, max, step): (f64, f64, f64),
    value: f64,
    prefs: &Rc<RefCell<Prefs>>,
    on_change: &OnChange,
    updating: &Rc<Cell<bool>>,
    set: impl Fn(&mut Prefs, f64) + 'static,
) -> adw::ActionRow {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_value(value);
    scale.set_width_request(140);
    scale.set_valign(gtk::Align::Center);
    let row = adw::ActionRow::builder().title(title).build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    row.add_suffix(&scale);
    let (p, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
    scale.connect_value_changed(move |s| edit(&p, &c, &u, |prefs| set(prefs, s.value())));
    row
}

fn combo_row(
    title: &str,
    items: &[&str],
    selected: usize,
    prefs: &Rc<RefCell<Prefs>>,
    on_change: &OnChange,
    updating: &Rc<Cell<bool>>,
    set: impl Fn(&mut Prefs, usize) + 'static,
) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(items)).selected(selected as u32).build();
    let (p, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
    row.connect_selected_notify(move |r| edit(&p, &c, &u, |prefs| set(prefs, r.selected() as usize)));
    row
}

impl Controls {
    pub fn new(prefs: Rc<RefCell<Prefs>>, rhythm: Rc<RhythmView>, on_change: OnChange) -> Rc<Self> {
        let updating = Rc::new(Cell::new(false));
        let p = prefs.borrow().clone();
        let (pr, oc, up) = (&prefs, &on_change, &updating);

        // The simple controls.
        let rhythm_group = adw::PreferencesGroup::builder().title("Rhythm").build();
        let modes: Vec<&str> = Mode::ALL.iter().map(|m| m.title()).collect();
        let mode_index = Mode::ALL.iter().position(|&m| m == p.mode).unwrap_or(0);
        rhythm_group.add(&combo_row("Display", &modes, mode_index, pr, oc, up, |prefs, i| prefs.mode = Mode::ALL[i]));

        let lanes_box = gtk::Box::builder().css_classes(["linked"]).valign(gtk::Align::Center).build();
        for (i, label) in ["Bass", "Low", "High", "Treble"].into_iter().enumerate() {
            let toggle = gtk::ToggleButton::builder().label(label).active(p.lanes[i]).build();
            let (pp, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
            toggle.connect_toggled(move |t| edit(&pp, &c, &u, |prefs| prefs.lanes[i] = t.is_active()));
            lanes_box.append(&toggle);
        }
        let lanes_row = adw::ActionRow::builder()
            .title("Lanes")
            .tooltip_text("Bass to 150 Hz, low mids to 800 Hz, high mids to 4 kHz, treble above")
            .build();
        lanes_row.add_suffix(&lanes_box);
        rhythm_group.add(&lanes_row);

        let histories: Vec<String> = Prefs::HISTORIES.iter().map(|s| format!("{s} seconds")).collect();
        let history_items: Vec<&str> = histories.iter().map(String::as_str).collect();
        let history_index = Prefs::HISTORIES.iter().position(|&h| h == p.history).unwrap_or(2);
        rhythm_group.add(&combo_row("History", &history_items, history_index, pr, oc, up, |prefs, i| {
            prefs.history = Prefs::HISTORIES[i];
        }));
        rhythm_group.add(&scale_row(
            "Sensitivity",
            Some("Higher catches weaker hits"),
            (0.0, 1.0, 0.05),
            f64::from(p.sensitivity),
            pr,
            oc,
            up,
            |prefs, v| prefs.sensitivity = v as f32,
        ));

        // Tempo: what's detected, tap tempo, and a manual override.
        let tap = gtk::Button::builder().label("Tap").valign(gtk::Align::Center).tooltip_text("Tap along to set the tempo (T)").build();
        let tempo_row = adw::ActionRow::builder().title("Tempo").subtitle("Listening…").build();
        tempo_row.add_suffix(&tap);
        rhythm_group.add(&tempo_row);
        let bpm_spin = gtk::SpinButton::with_range(30.0, 300.0, 0.5);
        bpm_spin.set_digits(1);
        bpm_spin.set_value(p.bpm);
        bpm_spin.set_valign(gtk::Align::Center);
        bpm_spin.set_sensitive(p.bpm_override);
        let (override_row, override_switch) =
            switch_row("Set tempo", Some("Use this BPM instead of the detected one"), p.bpm_override, pr, oc, up, |prefs, on| {
                prefs.bpm_override = on;
            });
        override_row.add_suffix(&bpm_spin);
        // Keep the spin button before the switch.
        override_row.remove(&override_switch);
        override_row.add_suffix(&override_switch);
        let spin = bpm_spin.clone();
        override_switch.connect_active_notify(move |sw| spin.set_sensitive(sw.is_active()));
        let (pp, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
        bpm_spin.connect_value_changed(move |s| edit(&pp, &c, &u, |prefs| prefs.bpm = s.value()));
        rhythm_group.add(&override_row);

        // Advanced: grid, time signature, patterns, animation, frame rate.
        let advanced_group = adw::PreferencesGroup::new();
        let advanced = adw::ExpanderRow::builder()
            .title("Advanced")
            .subtitle("Beat grid, time signature, patterns, animation")
            .expanded(p.advanced_open)
            .build();
        let (pp, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
        advanced.connect_expanded_notify(move |e| edit(&pp, &c, &u, |prefs| prefs.advanced_open = e.is_expanded()));
        advanced_group.add(&advanced);

        advanced.add_row(&switch_row("Beat markers", None, p.beat_markers, pr, oc, up, |prefs, on| prefs.beat_markers = on).0);
        let subs: Vec<&str> = Prefs::SUBDIVISIONS.iter().map(|&(_, s)| s).collect();
        let sub_index = Prefs::SUBDIVISIONS.iter().position(|&(s, _)| s == p.subdivision).unwrap_or(1);
        advanced.add_row(&combo_row("Subdivisions", &subs, sub_index, pr, oc, up, |prefs, i| {
            prefs.subdivision = Prefs::SUBDIVISIONS[i].0;
        }));
        advanced.add_row(&switch_row("Measure boundaries", None, p.measures, pr, oc, up, |prefs, on| prefs.measures = on).0);

        let signatures: Vec<&str> = Signature::ALL.iter().map(|s| s.title()).collect();
        let sig_index = Signature::ALL.iter().position(|&s| s == p.signature).unwrap_or(0);
        let signature_row = combo_row("Time signature", &signatures, sig_index, pr, oc, up, |prefs, i| {
            prefs.signature = Signature::ALL[i];
        });
        signature_row.set_subtitle("Auto is an estimate, shown with its confidence");
        advanced.add_row(&signature_row);
        let beats_spin = gtk::SpinButton::with_range(1.0, 16.0, 1.0);
        beats_spin.set_value(f64::from(p.custom_beats));
        let unit_drop = gtk::DropDown::from_strings(&["2", "4", "8", "16"]);
        unit_drop.set_selected([2, 4, 8, 16].iter().position(|&u| u == p.custom_unit).unwrap_or(2) as u32);
        let custom_box = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
        custom_box.append(&beats_spin);
        custom_box.append(&gtk::Label::new(Some("/")));
        custom_box.append(&unit_drop);
        let custom_row = adw::ActionRow::builder()
            .title("Custom")
            .subtitle("Bar length in detected beats")
            .visible(p.signature == Signature::Custom)
            .build();
        custom_row.add_suffix(&custom_box);
        advanced.add_row(&custom_row);
        let cr = custom_row.clone();
        signature_row.connect_selected_notify(move |r| cr.set_visible(Signature::ALL.get(r.selected() as usize) == Some(&Signature::Custom)));
        let (pp, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
        beats_spin.connect_value_changed(move |s| edit(&pp, &c, &u, |prefs| prefs.custom_beats = s.value() as u32));
        let (pp, c, u) = (prefs.clone(), on_change.clone(), updating.clone());
        unit_drop.connect_selected_notify(move |d| edit(&pp, &c, &u, |prefs| prefs.custom_unit = [2, 4, 8, 16][d.selected().min(3) as usize]));

        advanced.add_row(
            &switch_row("Repeating rhythms", Some("Dot under hits that repeat the last bar"), p.repeating, pr, oc, up, |prefs, on| {
                prefs.repeating = on;
            })
            .0,
        );
        advanced.add_row(
            &switch_row("Syncopation", Some("Caret over off-beat accents"), p.syncopation, pr, oc, up, |prefs, on| {
                prefs.syncopation = on;
            })
            .0,
        );
        advanced.add_row(
            &switch_row("Suspected polyrhythms", Some("Labeled per lane; a guess, not a certainty"), p.polyrhythm, pr, oc, up, |prefs, on| {
                prefs.polyrhythm = on;
            })
            .0,
        );
        advanced.add_row(&scale_row("Scroll speed", None, (0.5, 3.0, 0.25), p.scroll_speed, pr, oc, up, |prefs, v| prefs.scroll_speed = v));
        advanced.add_row(&scale_row("Event glow", None, (0.0, 1.0, 0.05), p.glow, pr, oc, up, |prefs, v| prefs.glow = v));
        advanced.add_row(&switch_row("Persistence trails", None, p.trails, pr, oc, up, |prefs, on| prefs.trails = on).0);
        advanced.add_row(&scale_row("Peak decay", Some("How fast the lane lights fade"), (0.0, 1.0, 0.05), p.peak_decay, pr, oc, up, |prefs, v| {
            prefs.peak_decay = v;
        }));
        advanced.add_row(&combo_row("Frame rate", &["30 FPS", "60 FPS"], usize::from(p.fps != 30), pr, oc, up, |prefs, i| {
            prefs.fps = if i == 0 { 30 } else { 60 };
        }));

        // For every view.
        let display_group = adw::PreferencesGroup::builder().title("Fullscreen").build();
        display_group.add(
            &switch_row("Hide controls", Some("In fullscreen, after a few seconds without the mouse moving"), p.auto_hide, pr, oc, up, |prefs, on| {
                prefs.auto_hide = on;
            })
            .0,
        );
        let help = gtk::Label::builder()
            .label("Double-click or F11 for fullscreen, Escape to leave.\nSpace freezes the picture; T taps the tempo.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .margin_top(6)
            .build();
        display_group.add(&help);

        let page = adw::PreferencesPage::new();
        page.add(&rhythm_group);
        page.add(&advanced_group);
        page.add(&display_group);
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(360)
            .child(&page)
            .build();

        let controls = Rc::new(Self {
            widget,
            rhythm_groups: vec![rhythm_group, advanced_group],
            tempo_row,
            override_switch,
            bpm_spin,
            rhythm,
            prefs,
            updating,
        });
        let c = controls.clone();
        tap.connect_clicked(move |_| c.tap());
        let c = Rc::downgrade(&controls);
        glib::timeout_add_local(std::time::Duration::from_millis(400), move || {
            let Some(c) = c.upgrade() else { return glib::ControlFlow::Break };
            c.refresh();
            glib::ControlFlow::Continue
        });
        controls
    }

    /// Rhythm settings only show for the Rhythm view.
    pub fn show_for(&self, view: View) {
        for group in &self.rhythm_groups {
            group.set_visible(view == View::Rhythm);
        }
    }

    /// One tap of tap tempo: once there are enough, the tapped tempo becomes
    /// the manual one.
    pub fn tap(&self) {
        if let Some(bpm) = self.rhythm.tap() {
            let bpm = (bpm * 2.0).round() / 2.0;
            // These go through the widgets' own handlers, which save them.
            self.bpm_spin.set_value(bpm);
            self.override_switch.set_active(true);
        }
    }

    fn refresh(&self) {
        let subtitle = match self.rhythm.tempo() {
            None => "Listening…".to_owned(),
            Some(t) if t.source == TempoSource::Manual => format!("{:.1} BPM, set manually", t.bpm),
            Some(t) => format!("{:.1} BPM detected ({:.0}% sure)", t.bpm, t.confidence * 100.0),
        };
        self.tempo_row.set_subtitle(&subtitle);
        // Keep the switch honest if preferences changed elsewhere.
        let on = self.prefs.borrow().bpm_override;
        if self.override_switch.is_active() != on {
            self.updating.set(true);
            self.override_switch.set_active(on);
            self.updating.set(false);
        }
    }
}
