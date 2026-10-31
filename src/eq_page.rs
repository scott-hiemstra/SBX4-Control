//! The "Sound Blaster" page: hardware EQ and output selection, talking to the
//! X4 over its serial control port.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

use crate::device::{self, ConnectError, Connection, Device, Event};
use crate::presets::{self, Preset};
use crate::plot::{self, Biquad, LogAxis};
use crate::{spectrum, tray};

/// After writing a value, ignore the device's echoes of it for this long so a
/// slider being dragged doesn't jump back to a stale value.
const ECHO_GRACE: Duration = Duration::from_millis(400);

const BAND_LABELS: [&str; 10] = ["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];
const BAND_RANGE_DB: f64 = 12.0;
const PREAMP_RANGE_DB: f64 = 6.0;
/// Band centers, and the bandwidth the curve assumes (one octave).
const BAND_FREQS: [f64; 10] = [31.25, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
const BAND_Q: f64 = 1.41;

struct Slider {
    id: u8,
    scale: gtk::Scale,
    value: gtk::Label,
}

pub struct EqPage {
    pub widget: gtk::Stack,
    toasts: adw::ToastOverlay,
    status: adw::StatusPage,
    retry: gtk::Button,
    firmware: adw::ActionRow,
    eq_switch: gtk::Switch,
    speakers: gtk::ToggleButton,
    headphones: gtk::ToggleButton,
    preamp: Slider,
    bands: Vec<Slider>,
    preset_names: gtk::StringList,
    preset_drop: gtk::DropDown,
    delete_preset: gtk::Button,
    analyzer: Rc<spectrum::Analyzer>,
    builtin: Vec<Preset>,
    user: RefCell<Vec<Preset>>,
    device: RefCell<Option<Device>>,
    /// Set while the UI is updated from device state, so handlers don't write it back.
    updating: Cell<bool>,
    last_write: RefCell<HashMap<u8, Instant>>,
    watching: Cell<bool>,
}

impl EqPage {
    pub fn new(toasts: adw::ToastOverlay) -> Rc<Self> {
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .build();
        let connecting = adw::StatusPage::builder().title("Connecting…").child(&spinner).build();
        let retry = gtk::Button::builder()
            .label("Try Again")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let status = adw::StatusPage::builder().icon_name("audio-card-symbolic").child(&retry).build();

        let speakers = gtk::ToggleButton::builder().label("Speakers").build();
        let headphones = gtk::ToggleButton::builder().label("Headphones").build();
        headphones.set_group(Some(&speakers));
        let output_buttons = gtk::Box::builder().css_classes(["linked"]).valign(gtk::Align::Center).build();
        output_buttons.append(&speakers);
        output_buttons.append(&headphones);
        let output_row = adw::ActionRow::builder().title("Output").build();
        output_row.add_suffix(&output_buttons);
        let output_group = adw::PreferencesGroup::new();
        output_group.add(&output_row);

        let eq_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        let eq_row = adw::ActionRow::builder()
            .title("Equalizer")
            .subtitle("Runs on the X4's own DSP")
            .activatable_widget(&eq_switch)
            .build();
        eq_row.add_suffix(&eq_switch);

        let preset_names = gtk::StringList::new(&[]);
        let preset_drop = gtk::DropDown::builder().model(&preset_names).valign(gtk::Align::Center).build();
        let name_entry = gtk::Entry::builder().placeholder_text("Preset name").build();
        let save_button = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
        let save_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        save_box.append(&name_entry);
        save_box.append(&save_button);
        let save_popover = gtk::Popover::builder().child(&save_box).build();
        let save_menu = gtk::MenuButton::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text("Save Curve as Preset")
            .popover(&save_popover)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let delete_preset = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete Preset")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let preset_row = adw::ActionRow::builder().title("Preset").build();
        preset_row.add_suffix(&preset_drop);
        preset_row.add_suffix(&save_menu);
        preset_row.add_suffix(&delete_preset);

        let preamp = slider(device::EQ_PREAMP, PREAMP_RANGE_DB);
        let bands: Vec<Slider> = device::EQ_BANDS.iter().map(|&id| slider(id, BAND_RANGE_DB)).collect();
        let band_box = gtk::Box::builder().homogeneous(true).hexpand(true).spacing(4).build();
        for (band, label) in bands.iter().zip(BAND_LABELS) {
            band_box.append(&column(band, label));
        }
        let columns = gtk::Box::builder()
            .spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        columns.append(&column(&preamp, "Pre"));
        columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        columns.append(&band_box);
        let curve = gtk::DrawingArea::builder()
            .content_height(110)
            .hexpand(true)
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        {
            let (pre, on) = (preamp.scale.clone(), eq_switch.clone());
            let scales: Vec<gtk::Scale> = bands.iter().map(|b| b.scale.clone()).collect();
            curve.set_draw_func(move |_, cr, w, h| {
                let gains: Vec<f64> = scales.iter().map(|s| s.value()).collect();
                draw_curve(cr, f64::from(w), f64::from(h), pre.value(), &gains, on.is_active());
            });
        }
        for scale in std::iter::once(&preamp.scale).chain(bands.iter().map(|b| &b.scale)) {
            let c = curve.clone();
            scale.connect_value_changed(move |_| c.queue_draw());
        }
        let c = curve.clone();
        eq_switch.connect_active_notify(move |_| c.queue_draw());
        let c = curve.clone();
        adw::StyleManager::default().connect_dark_notify(move |_| c.queue_draw());
        let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["card"]).margin_top(12).build();
        card.append(&curve);
        card.append(&columns);

        let analyzer = spectrum::Analyzer::new();
        analyzer.widget.set_margin_top(12);
        analyzer.widget.set_margin_bottom(8);
        analyzer.widget.set_margin_start(12);
        analyzer.widget.set_margin_end(12);
        let analyzer_card = gtk::Box::builder().css_classes(["card"]).margin_top(12).build();
        analyzer_card.append(&analyzer.widget);
        let spectrum_switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(spectrum::enabled())
            .build();
        let spectrum_row = adw::ActionRow::builder()
            .title("Spectrum analyzer")
            .subtitle("Listens through the X4's What U Hear loopback")
            .activatable_widget(&spectrum_switch)
            .build();
        spectrum_row.add_suffix(&spectrum_switch);
        analyzer_card.set_visible(spectrum_switch.is_active());
        let c = analyzer_card.clone();
        spectrum_switch.connect_active_notify(move |sw| {
            c.set_visible(sw.is_active());
            spectrum::set_enabled(sw.is_active());
        });

        let eq_group = adw::PreferencesGroup::builder().title("Equalizer").build();
        eq_group.add(&eq_row);
        eq_group.add(&preset_row);
        eq_group.add(&spectrum_row);
        eq_group.add(&analyzer_card);
        eq_group.add(&card);

        let firmware = adw::ActionRow::builder().title("Firmware").build();
        let info_group = adw::PreferencesGroup::builder().title("Device").build();
        info_group.add(&firmware);

        let prefs = adw::PreferencesPage::new();
        prefs.add(&output_group);
        prefs.add(&eq_group);
        prefs.add(&info_group);

        let widget = gtk::Stack::new();
        widget.add_named(&connecting, Some("connecting"));
        widget.add_named(&status, Some("status"));
        widget.add_named(&prefs, Some("ready"));

        let page = Rc::new(Self {
            widget,
            toasts,
            status,
            retry,
            firmware,
            eq_switch,
            speakers,
            headphones,
            preamp,
            bands,
            preset_names,
            preset_drop,
            delete_preset,
            analyzer,
            builtin: presets::builtin(),
            user: RefCell::new(presets::load_user()),
            device: RefCell::new(None),
            updating: Cell::new(false),
            last_write: RefCell::new(HashMap::new()),
            watching: Cell::new(false),
        });
        page.refresh_preset_list();
        page.connect_signals(&name_entry, &save_button, &save_popover);
        page
    }

    fn connect_signals(self: &Rc<Self>, name_entry: &gtk::Entry, save_button: &gtk::Button, popover: &gtk::Popover) {
        let p = self.clone();
        self.retry.connect_clicked(move |_| p.connect(false));

        let p = self.clone();
        self.eq_switch.connect_active_notify(move |sw| {
            if !p.updating.get() {
                p.write(device::EQ_ENABLE, if sw.is_active() { 1.0 } else { 0.0 });
            }
        });

        for (button, code) in [
            (&self.speakers, device::OUTPUT_SPEAKERS),
            (&self.headphones, device::OUTPUT_HEADPHONES),
        ] {
            let p = self.clone();
            button.connect_toggled(move |b| {
                if b.is_active() && !p.updating.get() {
                    p.set_output(code);
                }
            });
        }

        for slider in std::iter::once(&self.preamp).chain(&self.bands) {
            let p = self.clone();
            let id = slider.id;
            let label = slider.value.clone();
            slider.scale.connect_value_changed(move |scale| {
                label.set_label(&format_db(scale.value()));
                if p.updating.get() {
                    return;
                }
                let snapped = (scale.value() * 2.0).round() / 2.0;
                if (snapped - scale.value()).abs() > 1e-6 {
                    scale.set_value(snapped); // re-enters with the snapped value
                    return;
                }
                p.write(id, snapped as f32);
                p.sync_preset_selection();
            });
        }

        let p = self.clone();
        self.preset_drop.connect_selected_notify(move |drop| {
            p.delete_preset.set_sensitive(p.user_index(drop.selected()).is_some());
            if p.updating.get() {
                return;
            }
            if let Some(preset) = p.preset_at(drop.selected()) {
                p.apply(&preset);
            }
        });

        let save = {
            let p = self.clone();
            let entry = name_entry.clone();
            let popover = popover.clone();
            Rc::new(move || {
                let name = entry.text().trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let preset = p.current_curve(name);
                {
                    let mut user = p.user.borrow_mut();
                    match user.iter_mut().find(|u| u.name == preset.name) {
                        Some(existing) => *existing = preset,
                        None => user.push(preset),
                    }
                }
                p.save_user_presets();
                entry.set_text("");
                popover.popdown();
            })
        };
        let s = save.clone();
        save_button.connect_clicked(move |_| s());
        name_entry.connect_activate(move |_| save());

        let p = self.clone();
        self.delete_preset.connect_clicked(move |_| {
            if let Some(i) = p.user_index(p.preset_drop.selected()) {
                p.user.borrow_mut().remove(i);
                p.save_user_presets();
            }
        });
    }

    /// Connect on a background thread; `settle` waits for udev to finish with a new device.
    pub fn connect(self: &Rc<Self>, settle: bool) {
        self.widget.set_visible_child_name("connecting");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            if settle {
                std::thread::sleep(Duration::from_secs(1));
            }
            let _ = tx.send_blocking(device::connect());
        });
        let p = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(conn)) => p.attach(conn),
                Ok(Err(e)) => p.show_error(&e),
                Err(_) => {}
            }
        });
    }

    fn attach(self: &Rc<Self>, conn: Connection) {
        let Connection { mut device, reader, path, firmware } = conn;
        let (tx, rx) = async_channel::unbounded();
        device.spawn_reader(reader, path, tx);
        self.firmware.set_subtitle(&firmware);
        let init = device.request_state();
        self.device.replace(Some(device));
        if let Err(e) = init {
            self.lost(e);
            return;
        }
        self.widget.set_visible_child_name("ready");

        let p = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                p.handle(event);
            }
        });
    }

    fn handle(self: &Rc<Self>, event: Event) {
        match event {
            Event::Feature { family: device::FAMILY_PLAYBACK, id, value } => {
                if id == device::EQ_ENABLE {
                    self.guarded(|| self.eq_switch.set_active(value >= 0.5));
                } else if let Some(slider) = self.slider(id) {
                    let recent = self.last_write.borrow().get(&id).is_some_and(|t| t.elapsed() < ECHO_GRACE);
                    if !recent {
                        self.guarded(|| slider.scale.set_value(value.into()));
                        self.sync_preset_selection();
                    }
                }
            }
            Event::Feature { .. } => {}
            Event::Output(code) => self.guarded(|| match code {
                device::OUTPUT_SPEAKERS => self.speakers.set_active(true),
                device::OUTPUT_HEADPHONES => self.headphones.set_active(true),
                _ => {}
            }),
            Event::Firmware(version) => self.firmware.set_subtitle(&version),
            Event::Rejected { cmd, status } => {
                let msg = format!("The X4 rejected command {cmd:#04x} (status {status:#04x})");
                self.toasts.add_toast(adw::Toast::new(&msg));
            }
            Event::Disconnected => {
                if self.device.replace(None).is_some() {
                    self.show_error(&ConnectError::Disconnected);
                }
            }
        }
    }

    fn show_error(self: &Rc<Self>, error: &ConnectError) {
        self.status.set_title(&error.title());
        self.status.set_description(Some(&error.description()));
        self.retry.set_visible(!error.retry_automatically());
        self.widget.set_visible_child_name("status");
        if error.retry_automatically() {
            self.watch_for_device();
        }
    }

    fn watch_for_device(self: &Rc<Self>) {
        if self.watching.replace(true) {
            return;
        }
        let p = self.clone();
        glib::timeout_add_seconds_local(2, move || {
            if device::find_port().is_none() {
                return glib::ControlFlow::Continue;
            }
            p.watching.set(false);
            p.connect(true);
            glib::ControlFlow::Break
        });
    }

    fn lost(self: &Rc<Self>, error: std::io::Error) {
        self.device.replace(None);
        if device::find_port().is_none() {
            self.show_error(&ConnectError::Disconnected);
        } else {
            self.show_error(&ConnectError::Io(error));
        }
    }

    fn write(self: &Rc<Self>, id: u8, value: f32) {
        let result = match self.device.borrow_mut().as_mut() {
            Some(d) => d.write_feature(device::FAMILY_PLAYBACK, id, value),
            None => return,
        };
        match result {
            Ok(()) => {
                self.last_write.borrow_mut().insert(id, Instant::now());
            }
            Err(e) => self.lost(e),
        }
    }

    fn set_output(self: &Rc<Self>, code: u8) {
        let result = match self.device.borrow_mut().as_mut() {
            Some(d) => d.set_output(code),
            None => return,
        };
        if let Err(e) = result {
            self.lost(e);
        }
    }

    fn apply(self: &Rc<Self>, preset: &Preset) {
        self.guarded(|| {
            self.preamp.scale.set_value(preset.preamp.into());
            for (slider, &v) in self.bands.iter().zip(&preset.bands) {
                slider.scale.set_value(v.into());
            }
            self.eq_switch.set_active(true);
        });
        self.write(device::EQ_PREAMP, preset.preamp);
        for (slider, &v) in self.bands.iter().zip(&preset.bands) {
            self.write(slider.id, v);
        }
        self.write(device::EQ_ENABLE, 1.0);
    }

    fn guarded(&self, f: impl FnOnce()) {
        let was = self.updating.replace(true);
        f();
        self.updating.set(was);
    }

    fn slider(&self, id: u8) -> Option<&Slider> {
        std::iter::once(&self.preamp).chain(&self.bands).find(|s| s.id == id)
    }

    fn current_curve(&self, name: String) -> Preset {
        Preset {
            name,
            preamp: self.preamp.scale.value() as f32,
            bands: std::array::from_fn(|i| self.bands[i].scale.value() as f32),
        }
    }

    // Drop-down entries are "Custom", then the built-in presets, then the user's.

    fn preset_at(&self, index: u32) -> Option<Preset> {
        let i = (index as usize).checked_sub(1)?;
        self.builtin
            .get(i)
            .cloned()
            .or_else(|| self.user.borrow().get(i - self.builtin.len()).cloned())
    }

    fn user_index(&self, index: u32) -> Option<usize> {
        let i = (index as usize).checked_sub(1 + self.builtin.len())?;
        (i < self.user.borrow().len()).then_some(i)
    }

    fn refresh_preset_list(&self) {
        let mut names = vec!["Custom".to_owned()];
        names.extend(self.builtin.iter().map(|p| p.name.clone()));
        names.extend(self.user.borrow().iter().map(|p| p.name.clone()));
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.guarded(|| self.preset_names.splice(0, self.preset_names.n_items(), &names));
        self.sync_preset_selection();
    }

    /// Select the preset matching the sliders, or "Custom".
    fn sync_preset_selection(&self) {
        let current = self.current_curve(String::new());
        let index = self
            .builtin
            .iter()
            .chain(self.user.borrow().iter())
            .position(|p| p.matches(&current))
            .map_or(0, |i| i as u32 + 1);
        if self.preset_drop.selected() != index {
            self.guarded(|| self.preset_drop.set_selected(index));
        }
        self.delete_preset.set_sensitive(self.user_index(index).is_some());
    }

    /// Shade the subwoofer's range in the analyzer; None while the sub is off.
    pub fn set_crossover(&self, crossover: Option<f64>) {
        self.analyzer.set_crossover(crossover);
    }

    /// Stop the analyzer so the X4's recording source is restored.
    pub fn shutdown(&self) {
        self.analyzer.stop();
    }

    pub fn tray_state(&self, state: &mut tray::State) {
        state.connected = self.device.borrow().is_some();
        state.headphones = self.headphones.is_active();
        state.eq_on = self.eq_switch.is_active();
        state.presets = (0..self.preset_names.n_items())
            .filter_map(|i| self.preset_names.string(i).map(String::from))
            .collect();
        state.preset = self.preset_drop.selected() as usize;
    }

    /// Call `f` with whether the X4 is on its headphone output, now and on every change.
    pub fn connect_headphones(&self, f: impl Fn(bool) + 'static) {
        f(self.headphones.is_active());
        self.headphones.connect_active_notify(move |b| f(b.is_active()));
    }

    // The tray drives the widgets; their handlers talk to the device.

    pub fn set_headphones_from_tray(&self, headphones: bool) {
        if headphones {
            self.headphones.set_active(true);
        } else {
            self.speakers.set_active(true);
        }
    }

    pub fn set_eq_from_tray(&self, on: bool) {
        self.eq_switch.set_active(on);
    }

    pub fn select_preset_from_tray(&self, index: usize) {
        if index > 0 {
            self.preset_drop.set_selected(index as u32);
        }
    }

    fn save_user_presets(&self) {
        if let Err(e) = presets::save_user(&self.user.borrow()) {
            self.toasts.add_toast(adw::Toast::new(&format!("Couldn't save presets: {e}")));
        }
        self.refresh_preset_list();
    }
}

fn slider(id: u8, range: f64) -> Slider {
    let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, -range, range, 0.5);
    scale.set_inverted(true);
    scale.set_vexpand(true);
    scale.set_height_request(200);
    scale.set_draw_value(false);
    let value = gtk::Label::builder().label(format_db(0.0)).css_classes(["caption", "numeric"]).build();
    Slider { id, scale, value }
}

fn column(slider: &Slider, name: &str) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.append(&slider.value);
    column.append(&slider.scale);
    column.append(&gtk::Label::builder().label(name).css_classes(["caption", "dim-label"]).build());
    column
}

fn format_db(value: f64) -> String {
    let value = if value.abs() < 0.05 { 0.0 } else { value };
    format!("{value:+.1}")
}

/// The EQ's overall response: pre-amp plus one peaking filter per band.
fn draw_curve(cr: &gtk::cairo::Context, w: f64, h: f64, preamp: f64, gains: &[f64], on: bool) {
    const RANGE_DB: f64 = 18.0;
    let colors = plot::Palette::current();
    let axis = LogAxis { f_min: 25.0, f_max: 20_000.0, width: w };
    let y_for = |db: f64| h / 2.0 - (h / 2.0 - 4.0) * (db / RANGE_DB).clamp(-1.0, 1.0);

    plot::grid(cr, w, [-12.0, -6.0, 6.0, 12.0].map(y_for), colors.fg);
    cr.set_line_width(1.0);
    plot::set(cr, colors.fg, 0.2);
    cr.move_to(0.0, y_for(0.0).round() + 0.5);
    cr.line_to(w, y_for(0.0).round() + 0.5);
    let _ = cr.stroke();

    let filters: Vec<Biquad> = BAND_FREQS.iter().zip(gains).map(|(&f, &g)| Biquad::peaking(f, BAND_Q, g)).collect();
    let response = |f: f64| preamp + filters.iter().map(|b| plot::db(b.response(f))).sum::<f64>();
    let points: Vec<(f64, f64)> = (0..=w as usize / 2)
        .map(|i| {
            let x = i as f64 * 2.0;
            (x, y_for(response(axis.f(x))))
        })
        .collect();
    let alpha = if on { 1.0 } else { 0.35 };

    // Fill between the curve and 0 dB, brightest where it's furthest from flat.
    let zero = y_for(0.0);
    plot::polyline(cr, points.iter().copied());
    cr.line_to(w, zero);
    cr.line_to(0.0, zero);
    cr.close_path();
    let g = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
    let c = colors.accent;
    g.add_color_stop_rgba(0.0, c.0, c.1, c.2, 0.45 * alpha);
    g.add_color_stop_rgba(0.5, c.0, c.1, c.2, 0.05 * alpha);
    g.add_color_stop_rgba(1.0, c.0, c.1, c.2, 0.45 * alpha);
    let _ = cr.set_source(&g);
    let _ = cr.fill();

    cr.set_line_width(2.0);
    plot::set(cr, colors.accent, alpha);
    plot::polyline(cr, points.iter().copied());
    let _ = cr.stroke();

    // A handle at each band's center.
    plot::set(cr, colors.accent, alpha);
    for f in BAND_FREQS {
        cr.arc(axis.x(f), y_for(response(f)), 3.5, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    }

    if !on {
        cr.set_font_size(11.0);
        plot::set(cr, colors.fg, 0.55);
        cr.move_to(6.0, 14.0);
        let _ = cr.show_text("Equalizer off");
    }
}
