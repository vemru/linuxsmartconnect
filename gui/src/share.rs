//! "Share with the phone" section of the Files page: lets the phone browse a PC folder, read-only,
//! through `smartconnect-files serve` (File Station, the PC→phone direction).
//!
//! `serve` runs in the foreground until stopped, so the section runs it as a tracked child, like
//! the mount above it: output goes to `$XDG_RUNTIME_DIR/smartconnect-files-share.log`, and a 1 s
//! timer polls the child and reads the log's latest milestone for the status line:
//!   - "waiting for the phone's File Station handshake" → waiting for the phone;
//!   - "replied type:1" → the phone was told where to connect; if it hasn't connected within
//!     [`CONNECT_HINT_AFTER`], the status points at the firewall (TCP 2049 in the Wi-Fi's zone);
//!   - "phone connected for file sharing" → the phone has the folder.
//!
//! The choices are remembered in `gui.json` (`files.share`, `files.shareDir`, `files.shareHidden`,
//! `files.shareReadOnly`). The default folder is `~/SmartConnect`; hidden (dot) entries stay
//! invisible to the phone unless *Include hidden files* is on (`--include-hidden`), so `~/.ssh` and
//! app data aren't exposed. Deleting from the phone moves files to the Trash unless *Let the phone
//! delete files* is off (`--read-only`). While
//! the switch is on, the share starts when the phone connects and is restarted (after
//! [`RESTART_DELAY`]) if it ends while the phone is still connected, e.g. after a daemon restart.
//! Quit stops it ([`ShareSection::shutdown`], called from the Files page's own shutdown).

use std::cell::RefCell;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::mirror::binary;
use crate::prefs;

const BIN: &str = "smartconnect-files";

/// After the phone was sent our address, how long to wait for its connection before suggesting the
/// firewall.
const CONNECT_HINT_AFTER: Duration = Duration::from_secs(15);

/// How long to wait before restarting a share that ended while the phone is connected.
const RESTART_DELAY: Duration = Duration::from_secs(5);

fn log_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("smartconnect-files-share.log")
}

/// What the share's log says it has reached so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Milestone {
    Starting,
    Waiting,
    Offered,
    PhoneConnected,
}

/// The latest milestone in the share log (later lines win).
fn milestone(log: &str) -> Milestone {
    let mut m = Milestone::Starting;
    for line in log.lines() {
        if line.contains("phone connected for file sharing") {
            m = Milestone::PhoneConnected;
        } else if line.contains("replied type:1") {
            // A fresh offer after a connection means the phone is reconnecting: keep "connected".
            if m != Milestone::PhoneConnected {
                m = Milestone::Offered;
            }
        } else if line.contains("waiting for the phone's File Station handshake") {
            m = Milestone::Waiting;
        }
    }
    m
}

/// The last non-empty log line without its timestamp/level prefix, for error messages.
fn last_log_line(log: &str) -> Option<String> {
    let line = log.lines().map(str::trim).rev().find(|l| !l.is_empty())?;
    // `2026-01-01T00:00:00Z  INFO target: message` → `message`
    let msg = line.split_once(": ").map_or(line, |(_, m)| m);
    Some(msg.to_owned())
}

fn spawn_serve(dir: &std::path::Path, hidden: bool, read_only: bool) -> Result<Child, String> {
    let bin = binary(BIN);
    let log = File::create(log_path()).map_err(|e| format!("could not open the share log: {e}"))?;
    let log2 = log
        .try_clone()
        .map_err(|e| format!("could not open the share log: {e}"))?;
    let mut cmd = Command::new(&bin);
    cmd.arg("serve").arg("--sharedir").arg(dir);
    if hidden {
        cmd.arg("--include-hidden");
    }
    if read_only {
        cmd.arg("--read-only");
    }
    cmd.env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}", bin.display()))
}

struct State {
    connected: bool,
    child: Option<Child>,
    /// When the log first showed `Offered` in this run (drives the firewall hint).
    offered_at: Option<Instant>,
    /// Why the last run ended, shown until the next start.
    error: Option<String>,
    /// A restart is scheduled.
    restart_pending: bool,
}

pub struct ShareSection {
    toasts: adw::ToastOverlay,
    switch: adw::SwitchRow,
    folder: adw::ActionRow,
    hidden: adw::SwitchRow,
    delete: adw::SwitchRow,
    status: adw::ActionRow,
    st: RefCell<State>,
    /// The switch is being set from the preferences: ignore its change signal.
    syncing: std::cell::Cell<bool>,
    watching: std::cell::Cell<bool>,
}

impl ShareSection {
    pub fn build(toasts: &adw::ToastOverlay) -> (Rc<Self>, gtk::Widget) {
        let switch = adw::SwitchRow::builder()
            .title("Share a folder with the phone")
            .subtitle("The phone can browse it in its Files app while it's connected")
            .build();
        let folder = adw::ActionRow::builder()
            .title("Shared folder")
            .use_markup(false)
            .build();
        let open = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Open the shared folder")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let change = gtk::Button::builder()
            .label("Change…")
            .valign(gtk::Align::Center)
            .build();
        folder.add_suffix(&open);
        folder.add_suffix(&change);
        let hidden = adw::SwitchRow::builder()
            .title("Include hidden files")
            .subtitle("Files and folders whose names start with a dot, such as ~/.ssh")
            .build();
        let delete = adw::SwitchRow::builder()
            .title("Let the phone delete files")
            .subtitle("Deleted files go to the Trash, so you can restore them")
            .build();
        let status = adw::ActionRow::builder()
            .title("Status")
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title("Share with the phone")
            .description("The phone sees the folder in its Files app and in Smart Connect's Files tab. It can open and copy files, but not change them.")
            .build();
        group.add(&switch);
        group.add(&folder);
        group.add(&hidden);
        group.add(&delete);
        group.add(&status);

        let section = Rc::new(Self {
            toasts: toasts.clone(),
            switch,
            folder,
            hidden,
            delete,
            status,
            st: RefCell::new(State {
                connected: false,
                child: None,
                offered_at: None,
                error: None,
                restart_pending: false,
            }),
            syncing: std::cell::Cell::new(false),
            watching: std::cell::Cell::new(false),
        });
        section.connect_signals(&open, &change);
        section.sync_from_prefs();
        section.refresh();
        (section, group.upcast())
    }

    fn connect_signals(self: &Rc<Self>, open: &gtk::Button, change: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.switch.connect_active_notify(move |row| {
            if let Some(s) = weak.upgrade() {
                s.toggled(row.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        self.hidden.connect_active_notify(move |row| {
            if let Some(s) = weak.upgrade() {
                s.hidden_toggled(row.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        self.delete.connect_active_notify(move |row| {
            if let Some(s) = weak.upgrade() {
                s.delete_toggled(row.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        open.connect_clicked(move |b| {
            if let Some(s) = weak.upgrade() {
                s.open_folder(b);
            }
        });
        let weak = Rc::downgrade(self);
        change.connect_clicked(move |b| {
            if let Some(s) = weak.upgrade() {
                s.choose_folder(b);
            }
        });
    }

    fn sync_from_prefs(&self) {
        let p = prefs::load().files;
        self.syncing.set(true);
        self.switch.set_active(p.share);
        self.hidden.set_active(p.share_hidden);
        self.delete.set_active(!p.share_read_only);
        self.syncing.set(false);
        self.folder.set_subtitle(&folder_text(&p));
    }

    pub fn set_connected(self: &Rc<Self>, connected: bool) {
        let changed =
            std::mem::replace(&mut self.st.borrow_mut().connected, connected) != connected;
        if changed && connected {
            self.start_if_wanted();
        }
        self.refresh();
    }

    fn toggled(self: &Rc<Self>, on: bool) {
        if self.syncing.get() {
            return;
        }
        let mut prefs = prefs::load();
        if prefs.files.share != on {
            prefs.files.share = on;
            if let Err(e) = prefs::save(&prefs) {
                eprintln!("smartconnect-gui: could not save the share setting: {e}");
            }
        }
        if on {
            self.st.borrow_mut().error = None;
            self.start_if_wanted();
        } else {
            self.stop();
        }
        self.refresh();
    }

    /// Persists *Include hidden files* and restarts a running share with it.
    fn hidden_toggled(self: &Rc<Self>, on: bool) {
        if self.syncing.get() {
            return;
        }
        let mut prefs = prefs::load();
        if prefs.files.share_hidden == on {
            return;
        }
        prefs.files.share_hidden = on;
        if let Err(e) = prefs::save(&prefs) {
            eprintln!("smartconnect-gui: could not save the hidden-files setting: {e}");
        }
        self.restart_if_running();
    }

    /// Persists *Let the phone delete files* and restarts a running share with it.
    fn delete_toggled(self: &Rc<Self>, on: bool) {
        if self.syncing.get() {
            return;
        }
        let mut prefs = prefs::load();
        if prefs.files.share_read_only == !on {
            return;
        }
        prefs.files.share_read_only = !on;
        if let Err(e) = prefs::save(&prefs) {
            eprintln!("smartconnect-gui: could not save the delete setting: {e}");
        }
        self.restart_if_running();
    }

    fn restart_if_running(self: &Rc<Self>) {
        if self.st.borrow().child.is_some() {
            self.stop();
            self.start_if_wanted();
        }
    }

    /// Starts `serve` when sharing is on, the phone is connected and nothing runs yet.
    fn start_if_wanted(self: &Rc<Self>) {
        {
            let st = self.st.borrow();
            if !st.connected || st.child.is_some() || !prefs::load().files.share {
                return;
            }
        }
        let p = prefs::load().files;
        let dir = p.share_path();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.st.borrow_mut().error = Some(format!("Could not create {}: {e}", dir.display()));
            self.refresh();
            return;
        }
        match spawn_serve(&dir, p.share_hidden, p.share_read_only) {
            Ok(child) => {
                let mut st = self.st.borrow_mut();
                st.child = Some(child);
                st.offered_at = None;
                st.error = None;
            }
            Err(e) => self.st.borrow_mut().error = Some(e),
        }
        self.refresh();
        self.watch();
    }

    /// Polls the running share: notices it ending, and keeps the status line current.
    fn watch(self: &Rc<Self>) {
        if self.watching.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(s) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let ended = {
                let mut st = s.st.borrow_mut();
                match st.child.as_mut().map(Child::try_wait) {
                    Some(Ok(Some(status))) => {
                        st.child = None;
                        Some(status)
                    }
                    Some(_) => None,
                    None => {
                        s.watching.set(false);
                        return glib::ControlFlow::Break;
                    }
                }
            };
            if let Some(status) = ended {
                s.watching.set(false);
                s.on_exit(status);
                return glib::ControlFlow::Break;
            }
            s.refresh();
            glib::ControlFlow::Continue
        });
    }

    fn on_exit(self: &Rc<Self>, status: std::process::ExitStatus) {
        let log = std::fs::read_to_string(log_path()).unwrap_or_default();
        let reason =
            last_log_line(&log).unwrap_or_else(|| format!("the share helper exited ({status})"));
        let retry = {
            let mut st = self.st.borrow_mut();
            st.error = Some(reason);
            st.offered_at = None;
            let retry = st.connected && prefs::load().files.share && !st.restart_pending;
            st.restart_pending |= retry;
            retry
        };
        self.refresh();
        if retry {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(RESTART_DELAY, move || {
                if let Some(s) = weak.upgrade() {
                    s.st.borrow_mut().restart_pending = false;
                    s.start_if_wanted();
                }
            });
        }
    }

    /// Stops the share (SIGTERM, then reap). Blocking for at most ~1 s.
    fn stop(&self) {
        let child = self.st.borrow_mut().child.take();
        if let Some(mut child) = child {
            if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: SIGTERM to our own, not-yet-reaped child.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            for _ in 0..50 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Quit: stop sharing. Closing the window to the tray keeps it running.
    pub fn shutdown(&self) {
        self.stop();
    }

    fn refresh(&self) {
        let p = prefs::load().files;
        let text = {
            let mut st = self.st.borrow_mut();
            if st.child.is_some() {
                let log = std::fs::read_to_string(log_path()).unwrap_or_default();
                match milestone(&log) {
                    Milestone::PhoneConnected => {
                        "The phone has the folder: Smart Connect's Files tab, or the phone's Files app.".to_owned()
                    }
                    Milestone::Offered => {
                        let since = *st.offered_at.get_or_insert_with(Instant::now);
                        if since.elapsed() >= CONNECT_HINT_AFTER {
                            "The phone hasn't connected yet. Check that TCP port 2049 is open in your firewall \
                             (in the zone of your Wi-Fi connection)."
                                .to_owned()
                        } else {
                            "Waiting for the phone to connect…".to_owned()
                        }
                    }
                    Milestone::Waiting | Milestone::Starting => "Waiting for the phone…".to_owned(),
                }
            } else if let Some(e) = &st.error {
                format!("Stopped: {e}")
            } else if !p.share {
                "Off".to_owned()
            } else if !st.connected {
                "Starts when the phone connects".to_owned()
            } else {
                "Starting…".to_owned()
            }
        };
        self.status.set_subtitle(&text);
    }

    fn open_folder(&self, button: &gtk::Button) {
        let dir = prefs::load().files.share_path();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(&format!("Could not create {}: {e}", dir.display()));
            return;
        }
        let parent = button.root().and_downcast::<gtk::Window>();
        let toasts = self.toasts.clone();
        gtk::FileLauncher::new(Some(&gio::File::for_path(&dir))).launch(
            parent.as_ref(),
            gio::Cancellable::NONE,
            move |res| {
                if let Err(e) = res {
                    toasts.add_toast(adw::Toast::new(&format!("Could not open the folder: {e}")));
                }
            },
        );
    }

    fn choose_folder(self: &Rc<Self>, button: &gtk::Button) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose the folder to share")
            .accept_label("Share folder")
            .build();
        dialog.set_initial_folder(Some(&gio::File::for_path(prefs::load().files.share_path())));
        let parent = button.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.select_folder(parent.as_ref(), gio::Cancellable::NONE, move |res| {
            let Some(s) = weak.upgrade() else { return };
            match res {
                Ok(file) => {
                    if let Some(path) = file.path() {
                        s.save_dir(&path.display().to_string());
                    }
                }
                Err(e) if !e.matches(gtk::DialogError::Dismissed) => {
                    s.toast(&format!("Could not choose a folder: {e}"))
                }
                Err(_) => {}
            }
        });
    }

    /// Persists a new shared folder and restarts a running share on it.
    fn save_dir(self: &Rc<Self>, dir: &str) {
        let mut prefs = prefs::load();
        if prefs.files.share_dir == dir {
            return;
        }
        prefs.files.share_dir = dir.to_owned();
        if let Err(e) = prefs::save(&prefs) {
            eprintln!("smartconnect-gui: could not save the shared folder: {e}");
        }
        self.folder.set_subtitle(&folder_text(&prefs.files));
        self.restart_if_running();
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

fn folder_text(p: &prefs::FilesPrefs) -> String {
    let path = p.share_path();
    if p.share_dir.trim().is_empty() {
        format!("{} (default)", path.display())
    } else {
        path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milestones_follow_the_log() {
        assert_eq!(milestone(""), Milestone::Starting);
        let waiting = "x INFO r: sharing \"/PhoneShare\" to the phone on 192.168.1.20:2049 (fp AB…); waiting for the phone's File Station handshake\n";
        assert_eq!(milestone(waiting), Milestone::Waiting);
        let offered = format!(
            "{waiting}x INFO r: replied type:1 (SSL) — phone should now mount \"/PhoneShare\"\n"
        );
        assert_eq!(milestone(&offered), Milestone::Offered);
        let connected =
            format!("{offered}x INFO s: phone connected for file sharing from 192.168.1.30:1234\n");
        assert_eq!(milestone(&connected), Milestone::PhoneConnected);
        // A later re-offer (phone reconnecting) keeps it shown as connected.
        let again = format!(
            "{connected}x INFO r: replied type:1 (SSL) — phone should now mount \"/PhoneShare\"\n"
        );
        assert_eq!(milestone(&again), Milestone::PhoneConnected);
    }

    #[test]
    fn last_line_drops_the_prefix() {
        let log = "2026-01-01T00:00:00Z  INFO smartconnect_files::server: phone connected\nError: connecting to the daemon at /run/x.sock\n";
        assert_eq!(
            last_log_line(log).as_deref(),
            Some("connecting to the daemon at /run/x.sock")
        );
        assert_eq!(last_log_line("\n\n"), None);
    }
}
