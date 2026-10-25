//! Files page: the phone's storage as a read-only FUSE mount, through `smartconnect-files` (svc
//! File Station).
//!
//! `smartconnect-files mount --mountpoint DIR` runs in the foreground and blocks until the mount is
//! torn down, so the page runs it as a tracked child: its stdout/stderr go to
//! `$XDG_RUNTIME_DIR/smartconnect-files.log`, and a timer polls the child and the mount point.
//!   - the child exits quickly (phone not connected, handshake failed) → read the last log line and
//!     show it in the error banner, back to the unmounted state;
//!   - the child stays alive and `DIR` becomes a mount point → the mounted state (Open in Files /
//!     Unmount, plus the quick-folder shortcuts).
//!
//! Unmount is `fusermount3 -u DIR`, then the child is reaped (SIGTERM if it lingers). Closing the
//! window to the tray keeps the mount; only Quit tears it down (see [`FilesPage::shutdown`], called
//! from `window::Ui::shutdown`).
//!
//! The mount folder and the "mount automatically when connected" choice are remembered in
//! `gui.json` (see `prefs.rs`); the default folder is `~/Phone`.
//!
//! Below the mount settings, [`ShareSection`] (`share.rs`) shares a PC folder with the phone.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::mirror::binary;
use crate::prefs;
use crate::share::ShareSection;

const BIN: &str = "smartconnect-files";

/// How long to wait before treating a still-running mount child as mounted even if the mount point
/// hasn't been detected yet (the FUSE mount is up by the time the handshake and session loop start).
const MOUNT_GRACE: Duration = Duration::from_millis(2500);

/// Quick-folder shortcuts: label and the relative subpaths to try under the mount, in order. The
/// first one that exists is opened; a shortcut with no existing folder is hidden.
const QUICK: [(&str, &[&str]); 4] = [
    ("Camera", &["DCIM/Camera", "DCIM"]),
    ("Screenshots", &["Pictures/Screenshots", "DCIM/Screenshots", "Screenshots"]),
    ("Downloads", &["Download", "Downloads"]),
    ("Documents", &["Documents"]),
];

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

fn log_path() -> PathBuf {
    runtime_dir().join("smartconnect-files.log")
}

/// Whether `dir` is a mount point: its device id differs from its parent's (the classic test, which
/// is how a FUSE mount shows up). A stale mount fails the `stat` and counts as not mounted here; see
/// [`is_stale`] / [`probe`].
fn is_mountpoint(dir: &Path) -> bool {
    match (std::fs::metadata(dir), dir.parent().map(std::fs::metadata)) {
        (Ok(here), Some(Ok(parent))) => here.dev() != parent.dev(),
        _ => false,
    }
}

/// "Transport endpoint is not connected": a FUSE mount whose helper process is gone.
fn is_stale_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTCONN) | Some(libc::ECONNABORTED))
}

/// `statfs(dir)`. Unlike `stat` — which the kernel keeps answering from cached attributes for up to
/// the attribute TTL after the helper died — `statfs` always reaches the FUSE server, so it fails with
/// `ENOTCONN` at once on a stale mount. Our helper answers it locally (no round-trip to the phone).
fn statfs(dir: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid NUL-terminated path and a zeroed, writable statfs buffer.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut buf) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// `dir` is a stale FUSE mount (its helper is gone).
fn is_stale(dir: &Path) -> bool {
    statfs(dir).is_err_and(|e| is_stale_error(&e))
}

/// What a probe of the mount folder found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    /// Mounted and listing works.
    Live,
    /// Mounted and our helper answers, but with `EIO`: the phone is away and the helper reconnects
    /// on its own (see `files/src/fuse.rs`).
    Offline,
    /// A FUSE mount whose helper died ("Transport endpoint is not connected").
    Stale,
    /// Not a mount point.
    NotMounted,
    /// The probe did not finish within [`PROBE_TIMEOUT`].
    Unresponsive,
}

/// Classifies the mount folder from a `statfs` of it, a `stat` of it (its device id), its parent's
/// device id, and a directory listing (only attempted on a mount point). Pure, for tests.
fn classify(fs: io::Result<()>, here: io::Result<u64>, parent: Option<u64>, list: impl FnOnce() -> io::Result<()>) -> Health {
    if fs.is_err_and(|e| is_stale_error(&e)) {
        return Health::Stale;
    }
    match here {
        Err(e) if is_stale_error(&e) => Health::Stale,
        Err(e) if e.raw_os_error() == Some(libc::EIO) => Health::Offline,
        Err(_) => Health::NotMounted,
        Ok(dev) if Some(dev) == parent => Health::NotMounted,
        Ok(_) => match list() {
            Ok(()) => Health::Live,
            Err(e) if is_stale_error(&e) => Health::Stale,
            Err(_) => Health::Offline,
        },
    }
}

/// Probes `dir` (blocking). `deep` also lists it, which goes to the phone and so tells a live mount
/// from one whose phone is away; a shallow probe is just `stat`s, which still catch a stale mount
/// (the kernel answers those without any server). Run off the main thread.
fn probe(dir: &Path, deep: bool) -> Health {
    classify(
        statfs(dir),
        std::fs::metadata(dir).map(|m| m.dev()),
        dir.parent().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.dev()),
        || if deep { std::fs::read_dir(dir).map(|mut d| drop(d.next())) } else { Ok(()) },
    )
}

/// How long the off-thread probe may take before the mount counts as unresponsive.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a mounted page re-checks the mount (every N 500 ms watch ticks, shallow).
const PROBE_EVERY_TICKS: u32 = 10;

/// [`probe`] on a worker thread with [`PROBE_TIMEOUT`]. A deep probe of a live mount also returns
/// which candidate of each quick folder exists (index into its candidate list).
async fn probe_async(dir: PathBuf, deep: bool) -> (Health, Option<[Option<usize>; 4]>) {
    let work = gio::spawn_blocking(move || {
        let health = probe(&dir, deep);
        let quick = (deep && health == Health::Live)
            .then(|| QUICK.map(|(_, candidates)| candidates.iter().position(|c| dir.join(c).is_dir())));
        (health, quick)
    });
    match glib::future_with_timeout(PROBE_TIMEOUT, work).await {
        Ok(Ok(result)) => result,
        _ => (Health::Unresponsive, None),
    }
}

/// The last non-empty line of the mount log, shown when the child exits with an error.
fn last_log_line() -> Option<String> {
    let text = std::fs::read_to_string(log_path()).ok()?;
    text.lines().map(str::trim).rev().find(|l| !l.is_empty()).map(str::to_owned)
}

/// `fusermount3 -u DIR`, falling back to a lazy `-uz` (busy or stale mount). Returns the first
/// error only if the lazy unmount failed too. Blocking: usually instant, but a busy mount can make
/// it wait, so callers run it off the main thread (Quit and stale clean-up aside).
fn fusermount_unmount(dir: &Path) -> Result<(), String> {
    let run = |flag: &str| match Command::new("fusermount3").arg(flag).arg(dir).output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr);
            let msg = msg.trim();
            Err(if msg.is_empty() { format!("fusermount3 {flag} exited with {}", out.status) } else { msg.to_owned() })
        }
        Err(e) => Err(format!("could not run fusermount3: {e}")),
    };
    run("-u").or_else(|first| run("-uz").map_err(|_| first))
}

/// Clears whatever mount is left at `dir` after our helper ended: a stale or live FUSE mount is
/// unmounted (lazily if needed). A no-op on a plain folder.
fn cleanup_mount(dir: &Path) {
    if is_stale(dir) || is_mountpoint(dir) {
        let _ = fusermount_unmount(dir);
    }
}

/// Spawns `smartconnect-files mount --mountpoint DIR`, output to the log file.
fn spawn_mount(dir: &Path) -> Result<Child, String> {
    let bin = binary(BIN);
    let log = File::create(log_path()).map_err(|e| format!("could not open the mount log: {e}"))?;
    let log2 = log.try_clone().map_err(|e| format!("could not open the mount log: {e}"))?;
    Command::new(&bin)
        .arg("mount")
        .arg("--mountpoint")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}", bin.display()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Unmounted,
    Mounting,
    Mounted,
}

struct State {
    connected: bool,
    phase: Phase,
    /// The running `smartconnect-files mount` child, in Mounting and Mounted.
    child: Option<Child>,
    /// The folder the current (or last) mount uses.
    dir: PathBuf,
    /// When the current mount child was spawned (drives the grace period).
    started: Option<Instant>,
    /// An unmount is in progress: keep the buttons disabled until it finishes.
    busy: bool,
    /// Set after a manual unmount so a still-connected phone is not auto-remounted at once.
    auto_suppressed: bool,
    /// The last probe of a mounted folder (drives the detail line).
    health: Health,
    /// Per quick folder, which candidate subfolder exists (from the last live probe).
    quick: [Option<usize>; 4],
    /// A probe is in flight.
    probing: bool,
    /// Open this once the remount that Open/a quick folder triggered is up: `None` = the root,
    /// `Some(i)` = quick folder `i`.
    open_after: Option<Option<usize>>,
}

pub struct FilesPage {
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    title: gtk::Label,
    detail: gtk::Label,
    readonly: gtk::Label,
    mount: gtk::Button,
    open: gtk::Button,
    unmount: gtk::Button,
    quick_group: adw::PreferencesGroup,
    quick: Vec<gtk::Button>,
    location: adw::ActionRow,
    auto_mount: adw::SwitchRow,
    /// The "Share with the phone" section (PC→phone, `smartconnect-files serve`).
    share: Rc<ShareSection>,
    st: RefCell<State>,
    /// A poll timer is running: don't start a second one.
    watching: Cell<bool>,
    /// Watch ticks since the last health probe.
    ticks: Cell<u32>,
    /// A switch/row is being set from the preferences: ignore its change signal.
    syncing: Cell<bool>,
}

/// A themed "card" status tile is built from a plain box; this gives it inner padding (structural
/// only — the colours come from the `.card` style class, i.e. the user's theme).
fn card_box() -> gtk::Box {
    gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).css_classes(["card", "sc-card"]).build()
}

impl FilesPage {
    pub fn build(toasts: &adw::ToastOverlay) -> (Rc<Self>, gtk::Widget) {
        // ── Storage card (hero) ───────────────────────────────────────────────
        let icon = gtk::Image::builder().icon_name("folder-symbolic").pixel_size(32).build();
        let title = gtk::Label::builder().label("Phone storage").xalign(0.0).css_classes(["title-4"]).wrap(true).build();
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["dim-label"])
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk::Align::Center).build();
        text.append(&title);
        text.append(&detail);
        let readonly = gtk::Label::builder().label("Read-only").css_classes(["caption", "dim-label", "card", "sc-badge"]).valign(gtk::Align::Center).visible(false).build();
        let head = gtk::Box::builder().spacing(12).build();
        head.append(&icon);
        head.append(&text);
        head.append(&readonly);

        let banner = adw::Banner::builder().revealed(false).build();

        let mount = gtk::Button::builder().css_classes(["suggested-action"]).label("Mount").build();
        let unmount = gtk::Button::builder().label("Unmount").visible(false).build();
        let open = gtk::Button::builder().css_classes(["suggested-action"]).label("Open in Files").visible(false).build();
        let actions = gtk::Box::builder().spacing(8).halign(gtk::Align::Start).build();
        actions.append(&mount);
        actions.append(&unmount);
        actions.append(&open);

        let card = card_box();
        card.append(&head);
        card.append(&banner);
        card.append(&actions);

        // ── Quick folders ─────────────────────────────────────────────────────
        let quick_flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(2)
            .max_children_per_line(6)
            .row_spacing(8)
            .column_spacing(8)
            .build();
        let mut quick = Vec::new();
        let page_ref: Rc<RefCell<Option<std::rc::Weak<Self>>>> = Rc::new(RefCell::new(None));
        for (index, (label, candidates)) in QUICK.into_iter().enumerate() {
            let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
            inner.append(&gtk::Label::builder().label(label).css_classes(["heading"]).xalign(0.0).build());
            let name = gtk::Label::builder().label(candidates[0]).css_classes(["caption", "dim-label"]).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
            inner.append(&name);
            let button = gtk::Button::builder().css_classes(["card", "sc-tile"]).hexpand(true).child(&inner).build();
            let slot = page_ref.clone();
            button.connect_clicked(move |b| {
                if let Some(p) = slot.borrow().as_ref().and_then(std::rc::Weak::upgrade) {
                    p.open_checked(Some(index), b);
                }
            });
            quick_flow.append(&button);
            quick.push(button);
        }
        let quick_group = adw::PreferencesGroup::builder().title("Quick folders").visible(false).build();
        quick_group.add(&quick_flow);

        // ── Storage settings ──────────────────────────────────────────────────
        let location = adw::ActionRow::builder().title("Mount location").use_markup(false).build();
        let change = gtk::Button::builder().label("Change…").valign(gtk::Align::Center).build();
        location.add_suffix(&change);
        let auto_mount = adw::SwitchRow::builder().title("Mount automatically when connected").build();
        let group = adw::PreferencesGroup::builder()
            .title("Storage")
            .description("The phone's storage is mounted read-only.")
            .build();
        group.add(&location);
        group.add(&auto_mount);

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&card);
        column.append(&quick_group);
        column.append(&group);
        let (share, share_widget) = ShareSection::build(toasts);
        column.append(&share_widget);

        let clamp = adw::Clamp::builder().maximum_size(1400).tightening_threshold(1400).child(&column).build();
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build();

        let page = Rc::new(Self {
            toasts: toasts.clone(),
            banner,
            title,
            detail,
            readonly,
            mount,
            open,
            unmount,
            quick_group,
            quick,
            location,
            auto_mount,
            share,
            st: RefCell::new(State {
                connected: false,
                phase: Phase::Unmounted,
                child: None,
                dir: PathBuf::new(),
                started: None,
                busy: false,
                auto_suppressed: false,
                health: Health::NotMounted,
                quick: [None; 4],
                probing: false,
                open_after: None,
            }),
            watching: Cell::new(false),
            ticks: Cell::new(0),
            syncing: Cell::new(false),
        });
        *page_ref.borrow_mut() = Some(Rc::downgrade(&page));
        page.connect_signals(&change);
        page.sync_from_prefs();
        page.refresh();
        // A helper that died while the app was closed leaves a stale mount ("Transport endpoint is
        // not connected"): clear it at startup. Off the main thread — a `stat` on somebody else's
        // live mount of that folder would go to the phone.
        let dir = page.mount_dir();
        glib::spawn_future_local(async move {
            let _ = gio::spawn_blocking(move || {
                if is_stale(&dir) {
                    eprintln!("smartconnect-gui: clearing a stale mount at {}", dir.display());
                    let _ = fusermount_unmount(&dir);
                }
            })
            .await;
        });
        (page, scroller.upcast())
    }

    fn connect_signals(self: &Rc<Self>, change: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.mount.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.start_mount();
            }
        });
        let weak = Rc::downgrade(self);
        self.unmount.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.unmount();
            }
        });
        let weak = Rc::downgrade(self);
        self.open.connect_clicked(move |b| {
            if let Some(p) = weak.upgrade() {
                p.open_checked(None, b);
            }
        });
        let weak = Rc::downgrade(self);
        change.connect_clicked(move |b| {
            if let Some(p) = weak.upgrade() {
                p.choose_folder(b);
            }
        });
        let weak = Rc::downgrade(self);
        self.auto_mount.connect_active_notify(move |row| {
            if let Some(p) = weak.upgrade() {
                p.save_auto_mount(row.is_active());
            }
        });
    }

    pub fn set_connected(self: &Rc<Self>, connected: bool) {
        self.share.set_connected(connected);
        let changed = {
            let mut st = self.st.borrow_mut();
            let was = std::mem::replace(&mut st.connected, connected);
            if !connected {
                st.auto_suppressed = false; // a fresh connection may auto-mount again
            }
            was != connected
        };
        if changed {
            self.refresh();
            // Auto-mount on the connect transition, if asked and nothing is mounted yet.
            let want_auto = {
                let st = self.st.borrow();
                connected && st.phase == Phase::Unmounted && !st.busy && !st.auto_suppressed
            };
            if want_auto && prefs::load().files.auto_mount {
                self.start_mount();
            }
        }
    }

    /// Sets the Storage widgets from the preferences.
    fn sync_from_prefs(&self) {
        let p = prefs::load().files;
        self.syncing.set(true);
        self.auto_mount.set_active(p.auto_mount);
        self.syncing.set(false);
        self.location.set_subtitle(&self.location_text(&p));
    }

    fn location_text(&self, p: &prefs::FilesPrefs) -> String {
        let path = p.mount_path();
        if p.mount_dir.trim().is_empty() {
            format!("{} (default)", path.display())
        } else {
            path.display().to_string()
        }
    }

    /// Persists the "mount automatically" switch.
    fn save_auto_mount(&self, on: bool) {
        if self.syncing.get() {
            return;
        }
        let mut prefs = prefs::load();
        if prefs.files.auto_mount == on {
            return;
        }
        prefs.files.auto_mount = on;
        if let Err(e) = prefs::save(&prefs) {
            eprintln!("smartconnect-gui: could not save the auto-mount setting: {e}");
        }
    }

    /// Opens a folder chooser to pick where the phone's storage is mounted.
    fn choose_folder(self: &Rc<Self>, button: &gtk::Button) {
        let dialog = gtk::FileDialog::builder().title("Choose the mount folder").accept_label("Use folder").build();
        dialog.set_initial_folder(Some(&gio::File::for_path(prefs::load().files.mount_path())));
        let parent = button.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.select_folder(parent.as_ref(), gio::Cancellable::NONE, move |res| {
            let Some(p) = weak.upgrade() else { return };
            match res {
                Ok(file) => {
                    if let Some(path) = file.path() {
                        p.save_dir(&path.display().to_string());
                    }
                }
                Err(e) if !e.matches(gtk::DialogError::Dismissed) => p.toast(&format!("Could not choose a folder: {e}")),
                Err(_) => {}
            }
        });
    }

    /// Persists a new mount folder and updates the location row.
    fn save_dir(&self, dir: &str) {
        let mut prefs = prefs::load();
        if prefs.files.mount_dir == dir {
            return;
        }
        prefs.files.mount_dir = dir.to_owned();
        if let Err(e) = prefs::save(&prefs) {
            eprintln!("smartconnect-gui: could not save the mount folder: {e}");
        }
        self.location.set_subtitle(&self.location_text(&prefs.files));
    }

    /// The folder to mount: the configured one expanded (blank → `~/Phone`).
    fn mount_dir(&self) -> PathBuf {
        prefs::load().files.mount_path()
    }

    // ── Mount ─────────────────────────────────────────────────────────────────

    fn start_mount(self: &Rc<Self>) {
        {
            let st = self.st.borrow();
            if st.phase != Phase::Unmounted || st.busy {
                return;
            }
        }
        self.banner.set_revealed(false);
        let dir = self.mount_dir();
        // A helper that died (crash, kill -9, logout) can leave a stale FUSE mount here; `stat`,
        // `mkdir` and the mount itself would fail on it. Clear it lazily (instant, no server involved).
        if is_stale(&dir) {
            let _ = fusermount_unmount(&dir);
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.fail(&format!("Could not create {}: {e}", dir.display()));
            return;
        }
        if is_mountpoint(&dir) {
            self.fail(&format!(
                "{} is already mounted by another program (the Noctalia panel?). Unmount it there, or with fusermount3 -u.",
                dir.display()
            ));
            return;
        }
        let child = match spawn_mount(&dir) {
            Ok(child) => child,
            Err(e) => {
                self.fail(&e);
                return;
            }
        };
        {
            let mut st = self.st.borrow_mut();
            st.phase = Phase::Mounting;
            st.child = Some(child);
            st.dir = dir;
            st.started = Some(Instant::now());
        }
        self.refresh();
        self.watch();
    }

    /// Polls the mount child while it runs: promote Mounting → Mounted once the folder is a mount
    /// point (or the grace period passes), catch an exit (a quick failure during Mounting, or the
    /// helper ending later), and every few seconds check that a mounted folder isn't stale. A
    /// deliberate unmount (`busy`) owns the reaping itself, so the timer just steps aside then.
    fn watch(self: &Rc<Self>) {
        if self.watching.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(p) = weak.upgrade() else { return glib::ControlFlow::Break };
            // An unmount is reaping the child; stop polling and let it finish.
            if p.st.borrow().busy {
                p.watching.set(false);
                return glib::ControlFlow::Break;
            }
            let exited = {
                let mut st = p.st.borrow_mut();
                match st.child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(_))) => {
                        st.child = None;
                        true
                    }
                    // Still running, or a transient wait error: nothing to do here.
                    Some(_) => false,
                    // No child (already handled elsewhere): stop polling.
                    None => {
                        p.watching.set(false);
                        return glib::ControlFlow::Break;
                    }
                }
            };
            if exited {
                p.watching.set(false);
                p.on_child_exit();
                return glib::ControlFlow::Break;
            }
            // Still running: in Mounting, become Mounted once the mount shows up (or grace passes).
            let promote = {
                let st = p.st.borrow();
                st.phase == Phase::Mounting
                    && (is_mountpoint(&st.dir) || st.started.is_some_and(|t| t.elapsed() >= MOUNT_GRACE))
            };
            if promote {
                p.st.borrow_mut().phase = Phase::Mounted;
                p.ticks.set(0);
                p.refresh();
                p.mounted_now();
            } else if p.st.borrow().phase == Phase::Mounted {
                let t = p.ticks.get() + 1;
                p.ticks.set(if t >= PROBE_EVERY_TICKS { 0 } else { t });
                if t >= PROBE_EVERY_TICKS {
                    p.check_health(false, None);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    /// The mount just came up: look at it properly (quick folders), then open whatever Open or a
    /// quick folder asked for while it was (re)mounting.
    fn mounted_now(self: &Rc<Self>) {
        self.check_health(
            true,
            Some(Box::new(|p: &Rc<Self>, health| {
                let Some(target) = p.st.borrow_mut().open_after.take() else { return };
                match health {
                    Health::Live => p.open_target(target, None),
                    _ => p.toast("The phone's storage mounted, but it isn't readable yet. Try Open again in a moment."),
                }
            })),
        );
    }

    /// Probes the mount off the main thread (`deep`: list it and find the quick folders; else a
    /// cheap `stat` that still catches a stale mount), records the result, deals with a lost mount,
    /// then runs `then` with the health.
    fn check_health(self: &Rc<Self>, deep: bool, then: Option<Box<dyn FnOnce(&Rc<Self>, Health)>>) {
        let dir = {
            let mut st = self.st.borrow_mut();
            if st.probing && then.is_none() {
                return; // a periodic probe while another one runs: skip it
            }
            st.probing = true;
            st.dir.clone()
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (health, quick) = probe_async(dir, deep).await;
            let Some(p) = weak.upgrade() else { return };
            p.st.borrow_mut().probing = false;
            p.apply_health(health, quick);
            if let Some(then) = then {
                then(&p, health);
            }
        });
    }

    fn apply_health(self: &Rc<Self>, health: Health, quick: Option<[Option<usize>; 4]>) {
        let lost = {
            let mut st = self.st.borrow_mut();
            if st.phase != Phase::Mounted || st.busy {
                return; // stale result for a mount that changed meanwhile
            }
            st.health = health;
            if let Some(q) = quick {
                st.quick = q;
            }
            matches!(health, Health::Stale | Health::NotMounted)
        };
        if lost {
            let message = if health == Health::Stale {
                "The phone's storage mount stopped working (its helper ended), so it was cleared."
            } else {
                "The phone's storage is no longer mounted."
            };
            self.lost_mount(message);
        } else {
            self.refresh();
        }
    }

    /// Our mount is gone or dead while we thought it was up: stop the helper (it may be wedged),
    /// clear the mount point, show it as not mounted, and remount if that's wanted.
    fn lost_mount(self: &Rc<Self>, message: &str) {
        let dir = self.st.borrow().dir.clone();
        self.reap_child();
        cleanup_mount(&dir);
        self.fail(message);
        self.maybe_auto_remount();
    }

    /// Remounts when "Mount automatically when connected" is on, the phone is connected, and the
    /// user didn't just unmount.
    fn maybe_auto_remount(self: &Rc<Self>) {
        let want = {
            let st = self.st.borrow();
            st.connected && st.phase == Phase::Unmounted && !st.busy && !st.auto_suppressed
        };
        if want && prefs::load().files.auto_mount {
            self.start_mount();
        }
    }

    /// The mount child exited on its own. During Mounting that's a failed mount (show the log line);
    /// after Mounted the helper ended (it gave up after a long outage, or crashed). Either way the
    /// mount point is cleared — stale or not — and the page goes back to Unmounted.
    fn on_child_exit(self: &Rc<Self>) {
        let (was_mounting, dir) = {
            let st = self.st.borrow();
            (st.phase == Phase::Mounting, st.dir.clone())
        };
        // A helper that died without unmounting leaves a (stale) FUSE mount behind; clear it.
        cleanup_mount(&dir);
        let detail = last_log_line();
        let message = match (was_mounting, detail) {
            (true, Some(line)) => format!("Could not mount the phone's storage: {line}"),
            (true, None) => "Could not mount the phone's storage. Is the phone connected?".to_owned(),
            (false, Some(line)) => format!("The phone's storage was unmounted: {line}"),
            (false, None) => "The phone's storage was unmounted.".to_owned(),
        };
        self.fail(&message);
        if !was_mounting {
            self.maybe_auto_remount();
        }
    }

    // ── Unmount ───────────────────────────────────────────────────────────────

    fn unmount(self: &Rc<Self>) {
        let dir = {
            let mut st = self.st.borrow_mut();
            if st.phase != Phase::Mounted || st.busy {
                return;
            }
            st.busy = true;
            st.auto_suppressed = true; // don't auto-remount right after a manual unmount
            st.dir.clone()
        };
        self.refresh();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let d = dir.clone();
            let result = gio::spawn_blocking(move || fusermount_unmount(&d)).await.unwrap_or_else(|_| Err("unmount thread failed".into()));
            let Some(p) = weak.upgrade() else { return };
            p.reap_child();
            {
                let mut st = p.st.borrow_mut();
                st.busy = false;
                st.phase = Phase::Unmounted;
                st.started = None;
            }
            match result {
                Ok(()) => p.toast("Phone storage unmounted"),
                Err(e) => p.toast(&format!("Unmount failed: {e}")),
            }
            p.banner.set_revealed(false);
            p.refresh();
        });
    }

    /// Reaps the mount child: it exits once the mount is gone; SIGTERM then wait if it lingers.
    fn reap_child(&self) {
        let Some(mut child) = self.st.borrow_mut().child.take() else { return };
        for _ in 0..25 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: SIGTERM to our own, not-yet-reaped child.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let _ = child.wait();
    }

    // ── Open ──────────────────────────────────────────────────────────────────

    /// **Open in Files** (`None`) or a quick folder (`Some(i)`): checks the mount first. Live →
    /// open; phone offline / not responding → say so; stale or gone → remount, then open.
    fn open_checked(self: &Rc<Self>, target: Option<usize>, button: &gtk::Button) {
        if self.st.borrow().phase == Phase::Mounting {
            self.st.borrow_mut().open_after = Some(target);
            self.toast("Still mounting; it opens when ready");
            return;
        }
        let parent = button.root().and_downcast::<gtk::Window>().map(|w| w.downgrade());
        self.check_health(
            true,
            Some(Box::new(move |p: &Rc<Self>, health| {
                let parent = parent.and_then(|w| w.upgrade());
                match health {
                    Health::Live => p.open_target(target, parent.as_ref()),
                    Health::Offline => p.toast("The phone is offline right now. Its storage reconnects by itself when the phone is back."),
                    Health::Unresponsive => p.toast("The phone's storage isn't responding. Try again in a moment."),
                    Health::Stale | Health::NotMounted => p.remount_then_open(target),
                }
            })),
        );
    }

    /// The mount was lost (and already cleared by `apply_health`): mount again, open when up.
    fn remount_then_open(self: &Rc<Self>, target: Option<usize>) {
        if !self.st.borrow().connected {
            self.toast("Connect the phone to open its storage");
            return;
        }
        self.st.borrow_mut().auto_suppressed = false;
        self.start_mount(); // may already be under way (auto-mount)
        if self.st.borrow().phase == Phase::Mounting {
            self.st.borrow_mut().open_after = Some(target);
            self.toast("Remounting the phone's storage…");
        }
    }

    /// Opens the mount (`None`) or a quick folder's existing subfolder in the file manager.
    fn open_target(&self, target: Option<usize>, parent: Option<&gtk::Window>) {
        let (dir, sub) = {
            let st = self.st.borrow();
            (st.dir.clone(), target.map(|i| st.quick[i].map(|c| QUICK[i].1[c])))
        };
        let path = match sub {
            None => dir,
            Some(Some(sub)) => dir.join(sub),
            Some(None) => {
                self.toast("That folder is not on the phone");
                return;
            }
        };
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        let weak = self.toasts.downgrade();
        launcher.launch(parent, gio::Cancellable::NONE, move |res| {
            if let Err(e) = res {
                if !e.matches(gtk::DialogError::Dismissed) {
                    if let Some(toasts) = weak.upgrade() {
                        toasts.add_toast(adw::Toast::new(&format!("Could not open the folder: {e}")));
                    }
                }
            }
        });
    }

    // ── Teardown on Quit ────────────────────────────────────────────────────────

    /// Synchronous unmount for app Quit: `fusermount3 -u`, then reap the child, so no stale mount is
    /// left. A no-op when nothing is mounted. Closing the window to the tray never calls this.
    pub fn shutdown(&self) {
        self.share.shutdown();
        let (child, dir) = {
            let mut st = self.st.borrow_mut();
            if st.phase == Phase::Unmounted {
                return;
            }
            st.phase = Phase::Unmounted;
            (st.child.take(), std::mem::take(&mut st.dir))
        };
        let _ = fusermount_unmount(&dir);
        if let Some(mut child) = child {
            for _ in 0..50 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: SIGTERM to our own, not-yet-reaped child.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            let _ = child.wait();
        }
    }

    // ── View ─────────────────────────────────────────────────────────────────

    /// Goes back to the unmounted state and shows `message` in the error banner.
    fn fail(self: &Rc<Self>, message: &str) {
        {
            let mut st = self.st.borrow_mut();
            st.phase = Phase::Unmounted;
            st.busy = false;
            st.started = None;
            st.health = Health::NotMounted;
            st.quick = [None; 4];
            st.open_after = None;
        }
        self.banner.set_title(message);
        self.banner.set_revealed(true);
        self.refresh();
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    fn refresh(&self) {
        let st = self.st.borrow();
        let p = &st.phase;
        let mounted = *p == Phase::Mounted;
        let mounting = *p == Phase::Mounting;

        self.title.set_label(match p {
            Phase::Unmounted => "Phone storage",
            Phase::Mounting => "Mounting…",
            Phase::Mounted => "Phone storage mounted",
        });
        self.detail.set_label(&match p {
            Phase::Unmounted if !st.connected => "Connect your phone to mount its storage.".to_owned(),
            Phase::Unmounted => "Mount the phone's storage to browse its files from this PC.".to_owned(),
            Phase::Mounting => "Handshaking with the phone…".to_owned(),
            Phase::Mounted => match st.health {
                Health::Offline => format!("{} · the phone is offline; it reconnects when the phone is back", st.dir.display()),
                Health::Unresponsive => format!("{} · not responding right now", st.dir.display()),
                _ => st.dir.display().to_string(),
            },
        });
        self.readonly.set_visible(mounted);

        self.mount.set_visible(!mounted);
        self.mount.set_label(if mounting { "Mounting…" } else { "Mount" });
        // Mount only when the phone is connected and nothing is in flight.
        self.mount.set_sensitive(st.connected && *p == Phase::Unmounted && !st.busy);

        self.open.set_visible(mounted);
        self.open.set_sensitive(!st.busy);
        self.unmount.set_visible(mounted);
        self.unmount.set_label(if st.busy { "Unmounting…" } else { "Unmount" });
        self.unmount.set_sensitive(!st.busy);

        // Quick folders: shown only while mounted, each hidden (cell and all) when its target is absent.
        // Which ones exist comes from the last deep probe (no file-system calls on the main thread).
        let mut any_quick = false;
        for (button, found) in self.quick.iter().zip(st.quick) {
            let present = mounted && found.is_some();
            any_quick |= present;
            // Hide the wrapping FlowBoxChild so no empty cell is left behind.
            match button.parent() {
                Some(cell) => cell.set_visible(present),
                None => button.set_visible(present),
            }
        }
        self.quick_group.set_visible(mounted && any_quick);

        // The folder can only be changed while nothing is mounted.
        self.location.set_sensitive(*p == Phase::Unmounted && !st.busy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(code: i32) -> io::Result<u64> {
        Err(io::Error::from_raw_os_error(code))
    }

    #[test]
    fn mount_health() {
        let ok = || Ok(());
        // "Transport endpoint is not connected": the helper is gone.
        assert_eq!(classify(Ok(()), err(libc::ENOTCONN), Some(1), ok), Health::Stale);
        // Our helper answers EIO while the phone is away (it reconnects by itself).
        assert_eq!(classify(Ok(()), err(libc::EIO), Some(1), ok), Health::Offline);
        // A missing folder, or a plain folder on the parent's file system.
        assert_eq!(classify(Ok(()), err(libc::ENOENT), Some(1), ok), Health::NotMounted);
        assert_eq!(classify(Ok(()), Ok(1), Some(1), ok), Health::NotMounted);
        // `stat` still answers from the kernel's attribute cache after the helper died; `statfs` doesn't.
        assert_eq!(classify(err(libc::ENOTCONN).map(drop), Ok(7), Some(1), ok), Health::Stale);
        // A mount point: the listing decides.
        assert_eq!(classify(Ok(()), Ok(7), Some(1), ok), Health::Live);
        assert_eq!(classify(Ok(()), Ok(7), Some(1), || Err(io::Error::from_raw_os_error(libc::EIO))), Health::Offline);
        assert_eq!(classify(Ok(()), Ok(7), Some(1), || Err(io::Error::from_raw_os_error(libc::ENOTCONN))), Health::Stale);
    }

    #[test]
    fn a_plain_folder_is_not_mounted_or_stale() {
        let dir = std::env::temp_dir().join(format!("sc-gui-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_stale(&dir));
        assert!(!is_mountpoint(&dir));
        assert_eq!(probe(&dir, true), Health::NotMounted);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
