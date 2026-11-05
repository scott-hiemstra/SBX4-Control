//! sbx4-visualizer: a fullscreen-capable spectrum and rhythm visualizer.
//!
//! It listens to the Sound Blaster X4's "What U Hear" loopback when the X4 is
//! there and free, and to whatever PipeWire is playing otherwise. The views:
//! the control panel's bars, spectrogram and ring, and a rhythm view of
//! detected onsets against the beat.

mod controls;
mod prefs;
mod rhythm;
mod rhythm_view;
mod scenes;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use sbx4_control::analysis::Sources;
use sbx4_control::spectrum::{Analyzer, Style};

use controls::Controls;
use prefs::{Prefs, Theme, View};
use rhythm_view::RhythmView;
use scenes::{Kind, SceneView};

const APP_ID: &str = "io.github.Sbx4Control.Visualizer";
/// Fullscreen controls hide after this long without the mouse moving.
const HIDE_AFTER: Duration = Duration::from_millis(2500);

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        Visualizer::new(app).window.present();
    });
    let program = std::env::args().next().unwrap_or_default();
    app.run_with_args(&[program])
}

struct Visualizer {
    window: adw::ApplicationWindow,
    prefs: Rc<RefCell<Prefs>>,
    stack: gtk::Stack,
    analyzers: Vec<(View, Rc<Analyzer>)>,
    rhythm: Rc<RhythmView>,
    scenes: Vec<(View, Rc<SceneView>)>,
    controls: Rc<Controls>,
    header: adw::HeaderBar,
    title: adw::WindowTitle,
    sidebar: gtk::Revealer,
    sidebar_button: gtk::ToggleButton,
    freeze: gtk::ToggleButton,
    /// Fullscreen controls are hidden for now.
    chrome_hidden: Cell<bool>,
    /// Counts mouse moves in fullscreen; a pending hide only runs if none came after it.
    moves: Cell<u64>,
    /// Counts changes; a pending save only runs if no newer change came after it.
    changes: Rc<Cell<u64>>,
}

impl Visualizer {
    fn new(app: &adw::Application) -> Rc<Self> {
        let prefs = Rc::new(RefCell::new(Prefs::load()));
        apply_theme(prefs.borrow().theme);

        let stack = gtk::Stack::builder().hexpand(true).vexpand(true).transition_type(gtk::StackTransitionType::Crossfade).build();
        let analyzers: Vec<(View, Rc<Analyzer>)> = [(View::Bars, Style::Bars), (View::Spectrogram, Style::Spectrogram), (View::Ring, Style::Ring)]
            .into_iter()
            .map(|(view, style)| {
                let analyzer = Analyzer::with(Sources::X4ThenPipeWire, false, style);
                analyzer.widget.set_margin_start(16);
                analyzer.widget.set_margin_end(16);
                analyzer.widget.set_margin_top(16);
                analyzer.widget.set_margin_bottom(16);
                stack.add_named(&analyzer.widget, Some(view.title()));
                (view, analyzer)
            })
            .collect();
        let rhythm = RhythmView::new(prefs.clone());
        stack.add_named(&rhythm.widget, Some(View::Rhythm.title()));
        let scenes: Vec<(View, Rc<SceneView>)> = [(View::Tunnel, Kind::Tunnel), (View::Landscape, Kind::Landscape), (View::Plasma, Kind::Plasma)]
            .into_iter()
            .map(|(view, kind)| {
                let scene = SceneView::new(kind, prefs.clone());
                stack.add_named(&scene.widget, Some(view.title()));
                (view, scene)
            })
            .collect();

        // Header: the view picker, freeze, fullscreen, settings and the menu.
        let views: Vec<&str> = View::ALL.iter().map(|v| v.title()).collect();
        let picker = gtk::DropDown::from_strings(&views);
        picker.set_tooltip_text(Some("Visualization"));
        let freeze = gtk::ToggleButton::builder().icon_name("media-playback-pause-symbolic").tooltip_text("Freeze (Space)").build();
        let fullscreen = gtk::Button::builder().icon_name("view-fullscreen-symbolic").tooltip_text("Fullscreen (F11)").build();
        let sidebar_button = gtk::ToggleButton::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Settings")
            .active(prefs.borrow().controls_shown)
            .build();
        let menu = gio::Menu::new();
        let appearance = gio::Menu::new();
        for (label, theme) in [("Follow System", "system"), ("Light", "light"), ("Dark", "dark")] {
            appearance.append(Some(label), Some(&format!("win.theme::{theme}")));
        }
        menu.append_section(Some("Appearance"), &appearance);
        let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
        let title = adw::WindowTitle::new("Visualizer", "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_start(&picker);
        header.pack_start(&freeze);
        header.pack_end(&menu_button);
        header.pack_end(&sidebar_button);
        header.pack_end(&fullscreen);

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Visualizer")
            .default_width(1100)
            .default_height(700)
            .build();

        let sidebar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideLeft)
            .reveal_child(prefs.borrow().controls_shown)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&stack);
        body.append(&sidebar);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&body);
        window.set_content(Some(&content));

        let vis: Rc<RefCell<Option<Rc<Visualizer>>>> = Rc::default();
        let on_change: Rc<dyn Fn()> = {
            let vis = vis.clone();
            Rc::new(move || {
                if let Some(v) = vis.borrow().as_ref() {
                    v.changed();
                }
            })
        };
        let controls = Controls::new(prefs.clone(), rhythm.clone(), on_change);
        sidebar.set_child(Some(&controls.widget));

        let v = Rc::new(Self {
            window,
            prefs,
            stack,
            analyzers,
            rhythm,
            scenes,
            controls,
            header,
            title,
            sidebar,
            sidebar_button,
            freeze,
            chrome_hidden: Cell::new(false),
            moves: Cell::new(0),
            changes: Rc::default(),
        });
        vis.replace(Some(v.clone()));
        // Break the cycle when the window goes.
        let vis_ = vis.clone();
        v.window.connect_destroy(move |_| drop(vis_.take()));

        let current = v.prefs.borrow().view;
        picker.set_selected(View::ALL.iter().position(|&x| x == current).unwrap_or(3) as u32);
        v.show_view(current);
        let vv = Rc::downgrade(&v);
        picker.connect_selected_notify(move |d| {
            let (Some(v), Some(&view)) = (vv.upgrade(), View::ALL.get(d.selected() as usize)) else { return };
            v.prefs.borrow_mut().view = view;
            v.show_view(view);
            v.changed();
        });

        let vv = Rc::downgrade(&v);
        v.freeze.connect_toggled(move |b| {
            if let Some(v) = vv.upgrade() {
                v.set_frozen(b.is_active());
            }
        });
        let vv = Rc::downgrade(&v);
        fullscreen.connect_clicked(move |_| {
            if let Some(v) = vv.upgrade() {
                v.toggle_fullscreen();
            }
        });
        let vv = Rc::downgrade(&v);
        v.sidebar_button.connect_toggled(move |b| {
            let Some(v) = vv.upgrade() else { return };
            v.prefs.borrow_mut().controls_shown = b.is_active();
            v.sidebar.set_reveal_child(b.is_active() && !v.chrome_hidden.get());
            v.changed();
        });
        v.setup_theme_action();
        v.setup_input();
        v.setup_fullscreen();
        v.setup_visibility();

        // The source in the title, kept current.
        let vv = Rc::downgrade(&v);
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(v) = vv.upgrade() else { return glib::ControlFlow::Break };
            v.update_title();
            glib::ControlFlow::Continue
        });
        v
    }

    fn analyzer(&self, view: View) -> Option<&Rc<Analyzer>> {
        self.analyzers.iter().find(|(v, _)| *v == view).map(|(_, a)| a)
    }

    fn scene(&self, view: View) -> Option<&Rc<SceneView>> {
        self.scenes.iter().find(|(v, _)| *v == view).map(|(_, s)| s)
    }

    fn show_view(&self, view: View) {
        self.stack.set_visible_child_name(view.title());
        self.controls.show_for(view);
        self.update_title();
    }

    fn update_title(&self) {
        let view = self.prefs.borrow().view;
        let source = match (self.analyzer(view), self.scene(view)) {
            (Some(a), _) => a.source(),
            (_, Some(s)) => s.source(),
            _ => self.rhythm.source(),
        };
        self.title.set_title(view.title());
        self.title.set_subtitle(source.map_or("", |s| s.title()));
    }

    /// Preferences changed: redraw now, save shortly (sliders change a lot).
    /// Each change schedules a save; only the latest one writes. (Cancelling the
    /// previous timer instead would fail once it had already run.)
    fn changed(&self) {
        self.rhythm.widget.queue_draw();
        let change = self.changes.get() + 1;
        self.changes.set(change);
        let (prefs, changes) = (self.prefs.clone(), self.changes.clone());
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if changes.get() == change {
                prefs.borrow().save();
            }
        });
    }

    fn set_frozen(&self, frozen: bool) {
        self.rhythm.set_frozen(frozen);
        for (_, a) in &self.analyzers {
            a.set_frozen(frozen);
        }
        for (_, s) in &self.scenes {
            s.set_frozen(frozen);
        }
    }

    fn toggle_fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        } else {
            self.window.fullscreen();
        }
    }

    fn setup_theme_action(self: &Rc<Self>) {
        let key = |t: Theme| match t {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        };
        let theme = self.prefs.borrow().theme;
        let action = gio::SimpleAction::new_stateful("theme", Some(glib::VariantTy::STRING), &key(theme).to_variant());
        let vv = Rc::downgrade(self);
        action.connect_activate(move |action, value| {
            let (Some(v), Some(name)) = (vv.upgrade(), value.and_then(|v| v.str())) else { return };
            let theme = match name {
                "light" => Theme::Light,
                "dark" => Theme::Dark,
                _ => Theme::System,
            };
            action.set_state(&name.to_variant());
            apply_theme(theme);
            v.prefs.borrow_mut().theme = theme;
            v.changed();
        });
        self.window.add_action(&action);
    }

    /// Double-click for fullscreen; keys for fullscreen, freeze and tap tempo.
    fn setup_input(self: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        let vv = Rc::downgrade(self);
        click.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                if let Some(v) = vv.upgrade() {
                    v.toggle_fullscreen();
                }
            }
        });
        self.stack.add_controller(click);

        let keys = gtk::EventControllerKey::new();
        let vv = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(v) = vv.upgrade() else { return glib::Propagation::Proceed };
            if state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
                return glib::Propagation::Proceed;
            }
            // Leave typing in the BPM field alone.
            let typing = GtkWindowExt::focus(&v.window).is_some_and(|w| w.is::<gtk::Text>());
            match key {
                gdk::Key::Escape if v.window.is_fullscreen() => v.window.unfullscreen(),
                gdk::Key::F11 => v.toggle_fullscreen(),
                gdk::Key::space if !typing => v.freeze.set_active(!v.freeze.is_active()),
                gdk::Key::t | gdk::Key::T if !typing && v.prefs.borrow().view == View::Rhythm => v.controls.tap(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    /// In fullscreen, hide the header and settings after a while without the
    /// mouse moving, if the setting says so; moving the mouse brings them back.
    fn setup_fullscreen(self: &Rc<Self>) {
        let vv = Rc::downgrade(self);
        self.window.connect_fullscreened_notify(move |w| {
            let Some(v) = vv.upgrade() else { return };
            v.show_chrome();
            if w.is_fullscreen() {
                v.schedule_hide();
            }
        });
        let motion = gtk::EventControllerMotion::new();
        let vv = Rc::downgrade(self);
        let last = Cell::new((f64::NAN, f64::NAN));
        motion.connect_motion(move |_, x, y| {
            let Some(v) = vv.upgrade() else { return };
            // Ignore the synthetic events GTK sends when widgets move under a still pointer.
            if last.replace((x, y)) == (x, y) {
                return;
            }
            if v.window.is_fullscreen() {
                v.show_chrome();
                v.schedule_hide();
            }
        });
        self.window.add_controller(motion);
    }

    fn schedule_hide(self: &Rc<Self>) {
        let moves = self.moves.get() + 1;
        self.moves.set(moves);
        if !self.prefs.borrow().auto_hide {
            return;
        }
        let vv = Rc::downgrade(self);
        glib::timeout_add_local_once(HIDE_AFTER, move || {
            let Some(v) = vv.upgrade() else { return };
            if v.moves.get() != moves {
                return;
            }
            // Not while the pointer is over the settings: someone's using them.
            let over_settings = v.sidebar.is_child_revealed() && v.sidebar.contains_pointer();
            if v.window.is_fullscreen() && v.prefs.borrow().auto_hide && !over_settings {
                v.hide_chrome();
            }
        });
    }

    fn hide_chrome(&self) {
        self.chrome_hidden.set(true);
        self.header.set_visible(false);
        self.sidebar.set_reveal_child(false);
        self.stack.set_cursor_from_name(Some("none"));
    }

    fn show_chrome(&self) {
        if !self.chrome_hidden.replace(false) {
            return;
        }
        self.header.set_visible(true);
        self.sidebar.set_reveal_child(self.prefs.borrow().controls_shown);
        self.stack.set_cursor(None);
    }

    /// Stop listening while the window is minimized; views already stop when
    /// they're not the one showing.
    fn setup_visibility(self: &Rc<Self>) {
        let vv = Rc::downgrade(self);
        self.window.connect_realize(move |w| {
            let Some(toplevel) = w.surface().and_downcast::<gdk::Toplevel>() else { return };
            let vv = vv.clone();
            toplevel.connect_state_notify(move |t| {
                let Some(v) = vv.upgrade() else { return };
                let view = v.prefs.borrow().view;
                let minimized = t.state().contains(gdk::ToplevelState::MINIMIZED);
                if let Some(a) = v.analyzer(view) {
                    if minimized {
                        a.stop();
                    } else if a.widget.is_mapped() {
                        a.start();
                    }
                } else if let Some(s) = v.scene(view) {
                    if minimized {
                        s.stop();
                    } else if s.widget.is_mapped() {
                        s.start();
                    }
                } else if minimized {
                    v.rhythm.stop();
                } else if v.rhythm.widget.is_mapped() {
                    v.rhythm.start();
                }
            });
        });
    }
}

trait PointerInside {
    fn contains_pointer(&self) -> bool;
}

impl PointerInside for gtk::Revealer {
    /// Whether the mouse is over this widget.
    fn contains_pointer(&self) -> bool {
        let Some(root) = self.root() else { return false };
        let Some(device) = self.display().default_seat().and_then(|s| s.pointer()) else { return false };
        let Some(surface) = root.surface() else { return false };
        let Some((x, y, _)) = surface.device_position(&device) else { return false };
        let Some(point) = root.compute_point(self, &gtk::graphene::Point::new(x as f32, y as f32)) else { return false };
        self.contains(f64::from(point.x()), f64::from(point.y()))
    }
}

fn apply_theme(theme: Theme) {
    adw::StyleManager::default().set_color_scheme(match theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    });
}
