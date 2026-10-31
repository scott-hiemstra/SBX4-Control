//! The Room EQ section of the Effects page: room size, predicted resonances,
//! the cuts, their graph, and test tones. The Effects page wires it up.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::cairo;

use crate::plot::{self, Biquad, LogAxis};
use crate::sbx::{Room, CUTS};
use crate::tone;

pub struct CutRow {
    pub row: adw::ActionRow,
    pub on: gtk::Switch,
    pub freq: gtk::Scale,
    pub gain: gtk::Scale,
    pub q: gtk::Scale,
    pub play: gtk::ToggleButton,
}

pub struct RoomRows {
    pub expander: adw::ExpanderRow,
    pub graph: gtk::DrawingArea,
    /// What the graph draws: the room settings and whether it's bypassed.
    pub graph_state: Rc<Cell<(Room, bool)>>,
    pub size: [gtk::SpinButton; 3],
    pub unit: gtk::DropDown,
    pub modes_row: adw::ActionRow,
    pub suggest: gtk::Button,
    pub cuts: Vec<CutRow>,
    pub sweep: gtk::ToggleButton,
    pub tone: gtk::ToggleButton,
    pub tone_freq: gtk::Scale,
    pub tone_label: gtk::Label,
}

fn labeled_scale(label: &str, min: f64, max: f64, step: f64, format: fn(f64) -> String) -> (gtk::Box, gtk::Scale) {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_width_request(200);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_format_value_func(move |_, v| format(v));
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.append(&gtk::Label::builder().label(label).width_chars(6).xalign(0.0).css_classes(["caption", "dim-label"]).build());
    b.append(&scale);
    (b, scale)
}

pub fn format_hz(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0} Hz")
    } else {
        format!("{v:.1} Hz")
    }
}

pub fn build() -> (RoomRows, adw::PreferencesGroup) {
    let expander = adw::ExpanderRow::builder()
        .title("Room EQ")
        .subtitle("Cuts the bass notes your room makes boom, predicted from its size and tuned by ear")
        .show_enable_switch(true)
        .build();

    let graph_state = Rc::new(Cell::new((Room::default(), false)));
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
        let (room, bypassed) = state.get();
        draw(cr, f64::from(w), f64::from(h), &room, bypassed);
    });
    let g = graph.clone();
    adw::StyleManager::default().connect_dark_notify(move |_| g.queue_draw());
    expander.add_row(&graph);

    let size = std::array::from_fn(|_| {
        gtk::SpinButton::builder()
            .adjustment(&gtk::Adjustment::new(10.0, 1.0, 200.0, 0.5, 1.0, 0.0))
            .digits(1)
            .width_chars(4)
            .valign(gtk::Align::Center)
            .build()
    });
    let unit = gtk::DropDown::builder()
        .model(&gtk::StringList::new(&["ft", "m"]))
        .valign(gtk::Align::Center)
        .build();
    let size_row = adw::ActionRow::builder().title("Room size").subtitle("L × W × H").build();
    let size_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    for (i, spin) in size.iter().enumerate() {
        if i > 0 {
            size_box.append(&gtk::Label::new(Some("×")));
        }
        size_box.append(spin);
    }
    size_box.append(&unit);
    size_row.add_suffix(&size_box);
    expander.add_row(&size_row);

    let suggest = gtk::Button::builder().label("Suggest Cuts").valign(gtk::Align::Center).build();
    let modes_row = adw::ActionRow::builder().title("Resonances").build();
    modes_row.add_suffix(&suggest);
    expander.add_row(&modes_row);

    let cuts = (0..CUTS)
        .map(|i| {
            let on = gtk::Switch::builder().valign(gtk::Align::Center).build();
            let (freq_box, freq) = labeled_scale("Freq", 25.0, 250.0, 0.5, format_hz);
            let (gain_box, gain) = labeled_scale("Depth", -15.0, 0.0, 0.5, |v| format!("{v:.1} dB"));
            let (q_box, q) = labeled_scale("Width", 1.0, 10.0, 0.5, |v| format!("Q {v:.1}"));
            // Higher Q is narrower; show it that way round.
            q.set_inverted(true);
            let controls = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            controls.append(&freq_box);
            controls.append(&gain_box);
            controls.append(&q_box);
            let play = gtk::ToggleButton::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text("Play a Tone at This Frequency")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let row = adw::ActionRow::builder().title(format!("Cut {}", i + 1)).build();
            row.add_prefix(&on);
            row.add_suffix(&controls);
            row.add_suffix(&play);
            expander.add_row(&row);
            CutRow { row, on, freq, gain, q, play }
        })
        .collect();

    let sweep = gtk::ToggleButton::builder().label("Bass Sweep").valign(gtk::Align::Center).build();
    let tone = gtk::ToggleButton::builder().label("Tone").valign(gtk::Align::Center).build();
    let (tone_box, tone_freq) = labeled_scale("", tone::SWEEP_FROM, tone::SWEEP_TO, 0.5, format_hz);
    tone_freq.set_draw_value(false);
    let tone_label = gtk::Label::builder().width_chars(8).css_classes(["numeric"]).build();
    let tones_row = adw::ActionRow::builder()
        .title("Test tones")
        .subtitle("Start with the volume low. Listen for notes that boom, then cut them.")
        .build();
    let tones_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    tones_box.append(&sweep);
    tones_box.append(&tone);
    tones_row.add_suffix(&tones_box);
    expander.add_row(&tones_row);
    let freq_row = adw::ActionRow::builder().title("Tone frequency").build();
    freq_row.add_suffix(&tone_box);
    freq_row.add_suffix(&tone_label);
    expander.add_row(&freq_row);

    let group = adw::PreferencesGroup::builder()
        .title("Room EQ")
        .description("Bypassed on the headphone output.")
        .build();
    group.add(&expander);

    let rows = RoomRows {
        expander,
        graph,
        graph_state,
        size,
        unit,
        modes_row,
        suggest,
        cuts,
        sweep,
        tone,
        tone_freq,
        tone_label,
    };
    (rows, group)
}

/// The cuts' combined response, with the predicted resonances marked.
fn draw(cr: &cairo::Context, w: f64, h: f64, room: &Room, bypassed: bool) {
    const DB_TOP: f64 = 3.0;
    const DB_BOTTOM: f64 = -18.0;
    let colors = plot::Palette::current();
    let plot_h = h - 16.0;
    let axis = LogAxis { f_min: 20.0, f_max: 300.0, width: w };
    let y_for = |db: f64| plot_h * ((DB_TOP - db) / (DB_TOP - DB_BOTTOM)).clamp(0.0, 1.0);

    plot::grid(cr, w, [-6.0, -12.0].map(y_for), colors.fg);
    plot::freq_labels(cr, axis, h, colors.fg, &[("20", 20.0), ("40", 40.0), ("80", 80.0), ("160", 160.0)]);

    // Resonances: taller and brighter where dimensions coincide.
    cr.set_font_size(10.0);
    let mut label_end = f64::NEG_INFINITY;
    for mode in room.modes(300.0) {
        let x = axis.x(mode.freq).round() + 0.5;
        let strength = mode.strength.min(3) as f64;
        plot::set(cr, colors.sub, 0.25 + 0.2 * strength);
        cr.set_line_width(1.0 + strength);
        cr.move_to(x, plot_h);
        cr.line_to(x, plot_h * (1.0 - 0.25 * strength));
        let _ = cr.stroke();
        // Label what's in the sub's range, skipping labels that would overlap.
        let label = format!("{:.0}", mode.freq);
        if mode.freq < 200.0 && x + 3.0 > label_end {
            cr.move_to(x + 3.0, plot_h * (1.0 - 0.25 * strength) + 10.0);
            let _ = cr.show_text(&label);
            label_end = x + 3.0 + cr.text_extents(&label).map_or(16.0, |e| e.x_advance()) + 2.0;
        }
    }

    let zero = y_for(0.0).round() + 0.5;
    cr.set_line_width(1.0);
    plot::set(cr, colors.fg, 0.2);
    cr.move_to(0.0, zero);
    cr.line_to(w, zero);
    let _ = cr.stroke();

    let filters: Vec<Biquad> = room.cuts.iter().filter(|c| c.on).map(|c| Biquad::peaking(c.freq, c.q, c.gain)).collect();
    let points: Vec<(f64, f64)> = (0..=w as usize / 2)
        .map(|i| {
            let x = i as f64 * 2.0;
            let db: f64 = filters.iter().map(|b| plot::db(b.response(axis.f(x)))).sum();
            (x, y_for(db))
        })
        .collect();
    let alpha = if room.on && !bypassed { 1.0 } else { 0.35 };
    plot::polyline(cr, points.iter().copied());
    cr.line_to(w, zero);
    cr.line_to(0.0, zero);
    cr.close_path();
    plot::fade(cr, colors.accent, zero, plot_h, 0.1 * alpha, 0.45 * alpha);
    let _ = cr.fill();
    cr.set_line_width(2.0);
    plot::set(cr, colors.accent, alpha);
    plot::polyline(cr, points.iter().copied());
    let _ = cr.stroke();

    cr.set_font_size(11.0);
    plot::set(cr, colors.fg, 0.7);
    cr.move_to(4.0, 12.0);
    let note = if bypassed && room.on { "Room EQ (bypassed on headphones)" } else { "Room EQ" };
    let _ = cr.show_text(note);
    let legend_x = cr.text_extents(note).map_or(70.0, |e| e.x_advance()) + 16.0;
    plot::set(cr, colors.sub, 0.9);
    cr.rectangle(legend_x, 6.0, 3.0, 8.0);
    let _ = cr.fill();
    plot::set(cr, colors.fg, 0.7);
    cr.move_to(legend_x + 7.0, 12.0);
    let _ = cr.show_text("Predicted resonances");
}
