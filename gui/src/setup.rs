//! First run from the AppImage, and the firewall check shown while pairing.
//!
//! **Setup.** An AppImage's files only exist while it runs, but the daemon has to keep running in
//! the background. So when the app is started from an AppImage that is not installed (or is a
//! different build than the installed one), it opens a welcome window instead of the main one:
//! *Set up* runs the AppImage's own `--install --enable` (unpacks to `~/.local/opt/smartconnect-linux`,
//! adds the launcher and the systemd user service, no password) and opens the installed app.
//! *Try without installing* goes on from the AppImage as it is.
//!
//! **Firewall.** A firewall that drops the phone's connection is the usual reason a scanned QR
//! never pairs. Read-only checks only: `systemctl is-active` and firewalld's default-zone
//! `--query-port` need no authentication. Opening the port asks for a password, so it happens only
//! when the user presses the button, and the button is only offered on GNOME and KDE, which always
//! run a polkit agent to ask. Elsewhere the page shows the commands to copy (a polkit request with no
//! agent to answer it counts as a failed login and can lock the account's sudo for a while).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use adw::prelude::*;
use gtk::{gio, glib};

/// The port the phone connects to (`smartconnectd`'s default `--port`).
const PORT: u16 = 52020;

fn opt_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/opt/smartconnect-linux")
}

/// Identifies one AppImage build (written by `packaging/appimage/build-in-container.sh`).
fn build_id(root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(root.join("share/smartconnect-linux/build-id"))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// The AppImage file this app runs from, when it is not the installed copy or a different build.
pub fn pending_install() -> Option<(PathBuf, bool)> {
    let appimage = PathBuf::from(std::env::var_os("APPIMAGE")?);
    let appdir = PathBuf::from(std::env::var_os("APPDIR")?);
    let installed = build_id(&opt_dir());
    if installed.is_some() && installed == build_id(&appdir) {
        return None; // the same build is installed: nothing to do (the welcome window opens it)
    }
    Some((appimage, installed.is_some()))
}

/// The same build is installed and this was started from the AppImage file again: open that one.
pub fn installed_same_build() -> bool {
    let (Some(_), Some(appdir)) = (std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR")) else {
        return false;
    };
    let installed = build_id(&opt_dir());
    installed.is_some() && installed == build_id(std::path::Path::new(&appdir))
}

/// Starts the installed app once this process (same application id) has gone.
pub fn open_installed_and_quit(app: &adw::Application) {
    let gui = opt_dir().join("bin/smartconnect-gui");
    let _ = Command::new("sh")
        .args(["-c", "sleep 1; exec \"$0\"", &gui.to_string_lossy()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    app.quit();
}

/// The welcome window. `go_on` builds the normal window (for *Try without installing*).
pub fn welcome(
    app: &adw::Application,
    appimage: PathBuf,
    update: bool,
    go_on: impl Fn() + 'static,
) {
    // Nothing is installed yet, so the app icon is only inside the AppImage.
    if let (Some(appdir), Some(display)) =
        (std::env::var_os("APPDIR"), gtk::gdk::Display::default())
    {
        gtk::IconTheme::for_display(&display)
            .add_search_path(PathBuf::from(appdir).join("share/smartconnect-linux"));
    }
    let status = adw::StatusPage::builder()
        .icon_name("dev.smartconnect.Linux")
        .title(if update { "Update Smart Connect" } else { "Welcome to Smart Connect" })
        .description(if update {
            "This file is a different version than the one installed. Update to it? Your pairing and settings are kept."
        } else {
            "Use your Motorola phone with this PC: notifications, clipboard, mirroring, files, \
             and the phone as a second screen.\n\nSetting up installs Smart Connect for your user \
             (no password needed), adds it to your apps and keeps it connected in the background."
        })
        .vexpand(true)
        .build();
    let set_up = gtk::Button::builder()
        .label(if update { "Update" } else { "Set up" })
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .build();
    let try_it = gtk::Button::builder()
        .label(if update {
            "Not now"
        } else {
            "Try without installing"
        })
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .build();
    let spinner = adw::Spinner::builder()
        .visible(false)
        .halign(gtk::Align::Center)
        .build();
    let error = gtk::Label::builder()
        .wrap(true)
        .selectable(true)
        .css_classes(["error"])
        .visible(false)
        .max_width_chars(60)
        .build();
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    for w in [
        set_up.upcast_ref::<gtk::Widget>(),
        spinner.upcast_ref(),
        try_it.upcast_ref(),
        error.upcast_ref(),
    ] {
        buttons.append(w);
    }
    status.set_child(Some(&buttons));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
    toolbar.set_content(Some(&status));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(560)
        .default_height(560)
        .content(&toolbar)
        .build();

    let (app_c, window_c) = (app.clone(), window.clone());
    set_up.connect_clicked(move |button| {
        button.set_sensitive(false);
        spinner.set_visible(true);
        error.set_visible(false);
        let (appimage, app, spinner, error, button) = (
            appimage.clone(),
            app_c.clone(),
            spinner.clone(),
            error.clone(),
            button.clone(),
        );
        glib::spawn_future_local(async move {
            let out = gio::spawn_blocking(move || {
                Command::new(&appimage)
                    .args(["--install", "--enable"])
                    .stdin(Stdio::null())
                    .output()
            })
            .await;
            spinner.set_visible(false);
            match out {
                Ok(Ok(o)) if o.status.success() => open_installed_and_quit(&app),
                other => {
                    let why = match other {
                        Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).trim().to_owned(),
                        Ok(Err(e)) => e.to_string(),
                        Err(_) => "the setup did not finish".to_owned(),
                    };
                    error.set_label(&format!("Setup failed: {why}"));
                    error.set_visible(true);
                    button.set_sensitive(true);
                }
            }
        });
    });
    try_it.connect_clicked(move |_| {
        window_c.close();
        go_on();
    });
    window.present();
}

// ── Firewall ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Firewall {
    /// No firewall service we know of, or the port is open.
    Fine,
    /// firewalld runs and its default zone does not allow the port.
    FirewalldBlocks,
    /// ufw runs; whether it allows the port can only be read as root.
    UfwMaybe,
}

fn active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Read-only: never asks for authentication.
pub fn firewall() -> Firewall {
    if active("firewalld") {
        let open = Command::new("firewall-cmd")
            .arg(format!("--query-port={PORT}/tcp"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        return if open {
            Firewall::Fine
        } else {
            Firewall::FirewalldBlocks
        };
    }
    if active("ufw") {
        return Firewall::UfwMaybe;
    }
    Firewall::Fine
}

fn desktop_has_polkit_agent() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| {
        d.split(':')
            .any(|p| matches!(p.to_ascii_lowercase().as_str(), "gnome" | "kde"))
    })
}

/// A hint row for the pairing dialog; hidden until `check` finds a firewall in the way.
pub struct FirewallHint {
    pub widget: gtk::Box,
    title: gtk::Label,
    body: gtk::Label,
    allow: gtk::Button,
    copy: gtk::Button,
}

impl FirewallHint {
    pub fn build() -> std::rc::Rc<Self> {
        let title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading"])
            .build();
        let body = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .css_classes(["dim-label"])
            .build();
        let allow = gtk::Button::builder()
            .label("Allow")
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        let copy = gtk::Button::builder()
            .label("Copy commands")
            .valign(gtk::Align::Center)
            .build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&body);
        let widget = gtk::Box::builder()
            .spacing(12)
            .css_classes(["card", "sc-card"])
            .visible(false)
            .build();
        widget.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        widget.append(&text);
        widget.append(&allow);
        widget.append(&copy);
        let hint = std::rc::Rc::new(Self {
            widget,
            title,
            body,
            allow,
            copy,
        });
        let weak = std::rc::Rc::downgrade(&hint);
        hint.allow.connect_clicked(move |_| {
            if let Some(h) = weak.upgrade() {
                h.open_firewalld();
            }
        });
        let weak = std::rc::Rc::downgrade(&hint);
        hint.copy.connect_clicked(move |b| {
            if let Some(h) = weak.upgrade() {
                b.clipboard().set_text(&h.commands());
                b.set_label("Copied");
            }
        });
        hint
    }

    fn commands(&self) -> String {
        if self.title.label().contains("ufw") {
            format!("sudo ufw allow {PORT}/tcp && sudo ufw allow 5353/udp")
        } else {
            format!("sudo firewall-cmd --permanent --add-port={PORT}/tcp --add-service=mdns && sudo firewall-cmd --reload")
        }
    }

    /// Checks the firewall (in a thread) and shows the hint when it may block the phone.
    pub fn check(self: &std::rc::Rc<Self>) {
        let weak = std::rc::Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(state) = gio::spawn_blocking(firewall).await else {
                return;
            };
            let Some(h) = weak.upgrade() else { return };
            h.show(state);
        });
    }

    fn show(&self, state: Firewall) {
        self.copy.set_label("Copy commands");
        match state {
            Firewall::Fine => self.widget.set_visible(false),
            Firewall::FirewalldBlocks => {
                // Only the default zone can be read without a password, so this may be a false alarm.
                self.title.set_label("The firewall may block the phone");
                self.body.set_label(&format!(
                    "If the phone can't connect, allow port {PORT}/tcp and mDNS (you'll be asked for your password)."
                ));
                self.allow.set_visible(desktop_has_polkit_agent());
                self.widget.set_visible(true);
            }
            Firewall::UfwMaybe => {
                self.title.set_label("ufw may block the phone");
                self.body.set_label(&format!("If the phone can't connect, allow port {PORT}/tcp and mDNS (5353/udp) in a terminal."));
                self.allow.set_visible(false);
                self.widget.set_visible(true);
            }
        }
    }

    /// firewalld asks for the password itself (polkit), once for both changes.
    fn open_firewalld(self: &std::rc::Rc<Self>) {
        self.allow.set_sensitive(false);
        let weak = std::rc::Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let port = format!("--add-port={PORT}/tcp");
            let ok = gio::spawn_blocking(move || {
                let run = |args: &[&str]| {
                    Command::new("firewall-cmd")
                        .args(args)
                        .stdin(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                };
                run(&["--permanent", &port, "--add-service=mdns"]) && run(&["--reload"])
            })
            .await
            .unwrap_or(false);
            let Some(h) = weak.upgrade() else { return };
            h.allow.set_sensitive(true);
            if ok {
                h.check();
            } else {
                h.body.set_label(
                    "That did not work. Copy the commands and run them in a terminal instead.",
                );
            }
        });
    }
}
