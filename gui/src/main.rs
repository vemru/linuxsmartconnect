//! Smart Connect for Linux: GTK4/libadwaita front-end for `smartconnectd`.

mod apps;
mod crosscontrol;
mod daemon;
mod diagnostics;
mod display;
mod files;
mod icons;
mod lastdevice;
mod lifecycle;
mod mirror;
mod palette;
mod prefs;
mod settings;
mod setup;
mod share;
mod timeline;
mod tray;
mod webcam;
mod window;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const APP_ID: &str = "dev.smartconnect.Linux";

// Structural CSS only — spacing and radius, never colours. Colours come from the user's theme via
// libadwaita style classes (.card, .dim-label, .error, …). The one exception is the QR code's white
// backdrop, which is functional: a QR must stay dark-on-light to scan under any theme.
const CSS: &str = "
picture.wallpaper { border-radius: 12px; }
picture.qr { background: white; border-radius: 12px; padding: 12px; }
.sc-card { padding: 16px; }
.sc-mode { padding: 16px 10px; }
.sc-tile { padding: 10px; }
.sc-chip { padding: 6px 10px; }
.sc-dimmed { opacity: 0.55; }
.sc-badge { padding: 1px 8px; border-radius: 9999px; min-height: 0; }
.sc-notif { padding: 12px; }
";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let ui: Rc<RefCell<Option<Rc<window::Ui>>>> = Rc::default();
    // Only the first activation honours `--background`; launching the app again always shows it.
    let first_activation = Rc::new(Cell::new(true));

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    let slot = ui.clone();
    app.connect_command_line(move |app, cmdline| {
        let args: Vec<String> = cmdline
            .arguments()
            .into_iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        let is_palette = args.iter().any(|a| a == "--palette");
        let is_background = args.iter().any(|a| a == "--background");

        let is_first = first_activation.replace(false);
        // Started from the AppImage file: set it up first (or open the installed copy).
        if is_first && slot.borrow().is_none() {
            if setup::installed_same_build() {
                setup::open_installed_and_quit(app);
                return 0.into();
            }
            if let Some((appimage, update)) = setup::pending_install() {
                let (slot, app_c) = (slot.clone(), app.clone());
                setup::welcome(app, appimage, update, move || {
                    let ui = window::Ui::new(&app_c);
                    ui.start();
                    if is_palette {
                        ui.show_palette();
                    } else if !is_background {
                        ui.present();
                    }
                    *slot.borrow_mut() = Some(ui);
                });
                return 0.into();
            }
        }
        let mut slot = slot.borrow_mut();
        let ui = slot.get_or_insert_with(|| {
            let ui = window::Ui::new(app);
            ui.start();
            ui
        });
        if is_palette {
            ui.show_palette();
        } else if !(is_first && is_background) {
            ui.present();
        }
        0.into()
    });

    let slot = ui.clone();
    app.connect_shutdown(move |_| {
        if let Some(ui) = slot.borrow().as_ref() {
            ui.shutdown();
        }
    });

    app.run()
}
