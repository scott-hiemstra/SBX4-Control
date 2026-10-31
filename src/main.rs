mod device;
mod eq_page;
mod mixer_page;
mod plot;
mod presets;
mod room_ui;
mod sbx;
mod sbx_page;
mod spectrum;
mod tone;
mod tray;
mod ui;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use ksni::blocking::TrayMethods;

const APP_ID: &str = "io.github.Sbx4Control";

fn main() -> glib::ExitCode {
    // `--background` (used by the login item) starts in the tray without a window.
    let background = std::env::args().any(|a| a == "--background");
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| setup_theme(app));
    let window: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::default();
    let first = Cell::new(true);
    app.connect_activate(move |app| {
        if window.borrow().is_none() {
            window.replace(Some(build_window(app)));
        }
        if !(first.replace(false) && background) {
            if let Some(w) = window.borrow().as_ref() {
                w.present();
            }
        }
    });
    let program = std::env::args().next().unwrap_or_default();
    app.run_with_args(&[program])
}

fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
    let toasts = adw::ToastOverlay::new();
    let stack = adw::ViewStack::new();
    let eq = eq_page::EqPage::new(toasts.clone());
    stack
        .add_titled(&eq.widget, Some("device"), "Sound Blaster")
        .set_icon_name(Some("audio-card-symbolic"));
    let sbx = sbx_page::SbxPage::new(toasts.clone());
    stack
        .add_titled(&sbx.widget, Some("effects"), "Effects")
        .set_icon_name(Some("applications-multimedia-symbolic"));
    let mixer = mixer_page::MixerPage::new();
    stack
        .add_titled(&mixer.widget, Some("mixer"), "Mixer")
        .set_icon_name(Some("audio-volume-high-symbolic"));
    toasts.set_child(Some(&stack));
    toasts.set_vexpand(true);

    let title = adw::ViewSwitcherTitle::builder().stack(&stack).title("Sound Blaster X4").build();
    let menu = gio::Menu::new();
    let appearance = gio::Menu::new();
    for (label, theme) in [("Follow System", "system"), ("Light", "light"), ("Dark", "dark")] {
        appearance.append(Some(label), Some(&format!("app.theme::{theme}")));
    }
    menu.append_section(Some("Appearance"), &appearance);
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_end(&menu_button);
    let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
    title.bind_property("title-visible", &switcher_bar, "reveal").sync_create().build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&toasts);
    content.append(&switcher_bar);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Sound Blaster X4")
        .default_width(760)
        .default_height(720)
        .content(&content)
        .build();
    window.add_controller(wheel_scrolls_pages());
    eq.connect(false);
    let s = sbx.clone();
    eq.connect_headphones(move |on| s.set_headphones(on));
    let e = eq.clone();
    sbx.connect_crossover(move |crossover| e.set_crossover(crossover));

    let (e, s) = (eq.clone(), sbx.clone());
    app.connect_shutdown(move |_| {
        e.shutdown();
        s.shutdown();
    });
    // Quit cleanly on logout or Ctrl+C too, so the X4 gets its default output back.
    for signal in [nix::libc::SIGTERM, nix::libc::SIGINT] {
        let app = app.clone();
        glib::unix_signal_add_local(signal, move || {
            app.quit();
            glib::ControlFlow::Break
        });
    }

    // With a tray icon, closing the window just hides it and the app keeps running.
    let (tx, rx) = async_channel::unbounded();
    match tray::Tray::new(tx).spawn() {
        Ok(handle) => {
            let hold = app.hold();
            window.set_hide_on_close(true);
            run_tray(app, &window, &eq, &sbx, handle, rx, hold);
        }
        Err(e) => eprintln!("sbx4-control: no tray icon ({e}); closing the window quits"),
    }
    window
}

/// The mouse wheel scrolls the page even with the pointer over a slider or
/// number field. GTK would otherwise change the value under the pointer, so
/// scrolling down a page quietly drags whatever it passes over. Values still
/// change by dragging, clicking or the keyboard.
fn wheel_scrolls_pages() -> gtk::EventControllerScroll {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    scroll.connect_scroll(|controller, _, dy| {
        let target = (|| {
            let window = controller.widget()?;
            let (x, y) = controller.current_event()?.position()?;
            let (dx, dy) = window.native()?.surface_transform();
            window.pick(x - dx, y - dy, gtk::PickFlags::DEFAULT)
        })();
        let Some(target) = target else { return glib::Propagation::Proceed };
        let over_value = std::iter::successors(Some(target.clone()), |w| w.parent())
            .take_while(|w| !w.is::<gtk::ScrolledWindow>())
            .any(|w| w.is::<gtk::Range>() || w.is::<gtk::SpinButton>());
        if !over_value {
            return glib::Propagation::Proceed;
        }
        if let Some(page) = target.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>() {
            let adj = page.vadjustment();
            adj.set_value(adj.value() + dy * 50.0);
        }
        glib::Propagation::Stop
    });
    scroll
}

/// The "app.theme" action (system / light / dark), remembered in ui.json.
fn setup_theme(app: &adw::Application) {
    let apply = |theme: &str| {
        adw::StyleManager::default().set_color_scheme(match theme {
            "light" => adw::ColorScheme::ForceLight,
            "dark" => adw::ColorScheme::ForceDark,
            _ => adw::ColorScheme::Default,
        });
    };
    let theme = ui::get("theme").and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "system".into());
    apply(&theme);
    let action = gio::SimpleAction::new_stateful("theme", Some(glib::VariantTy::STRING), &theme.to_variant());
    action.connect_activate(move |action, value| {
        let Some(theme) = value.and_then(|v| v.str()) else { return };
        action.set_state(&theme.to_variant());
        apply(theme);
        ui::set("theme", theme.into());
    });
    app.add_action(&action);
}

fn run_tray(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    eq: &Rc<eq_page::EqPage>,
    sbx: &Rc<sbx_page::SbxPage>,
    handle: ksni::blocking::Handle<tray::Tray>,
    commands: async_channel::Receiver<tray::Command>,
    hold: gio::ApplicationHoldGuard,
) {
    let (app, window, eq_, sbx_) = (app.clone(), window.clone(), eq.clone(), sbx.clone());
    glib::spawn_future_local(async move {
        let _hold = hold;
        while let Ok(command) = commands.recv().await {
            match command {
                tray::Command::Open => window.present(),
                tray::Command::Quit => app.quit(),
                tray::Command::Headphones(on) => eq_.set_headphones_from_tray(on),
                tray::Command::Eq(on) => eq_.set_eq_from_tray(on),
                tray::Command::Preset(i) => eq_.select_preset_from_tray(i),
                tray::Command::Sbx(on) => sbx_.set_enabled_from_tray(on),
                tray::Command::Effect(i, on) => sbx_.set_effect_from_tray(i, on),
            }
        }
    });

    // Mirror the UI into the tray menu whenever something changed.
    let (eq, sbx) = (eq.clone(), sbx.clone());
    let mut last = tray::State::default();
    glib::timeout_add_local(Duration::from_millis(300), move || {
        let mut state = tray::State::default();
        eq.tray_state(&mut state);
        sbx.tray_state(&mut state);
        if state != last {
            last = state.clone();
            handle.update(move |t| t.state = state);
        }
        glib::ControlFlow::Continue
    });
}
