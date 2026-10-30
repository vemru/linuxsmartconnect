//! Phone app icons: one resolver for the Apps grid, the Recent chips, the running-session rows and
//! the notification rows.
//!
//! Resolution order (first hit wins):
//! 1. `<cache>/<iconName>` — the phone's own icon. `iconName` comes from the svc 17 app list
//!    (`<pkg>_<cmp>.png`, prefixed `<userId>_` for other users); computed when the list lacks it.
//! 2. `<cache>/<pkg>.png` — harvested by `smartconnect-mirror` from svc 15 `cmd 2` / `cmd 13`
//!    (and the name lite builds use).
//! 3. An **exact** desktop icon: the package itself as a themed icon name (`com.discord`), or an
//!    installed application whose name or desktop-file id equals the app's name (case-insensitive).
//!    No partial or package-segment guessing: that is what gave unrelated apps a browser's icon.
//! 4. An `AdwAvatar` with the app name's initials (libadwaita picks its colour from the name).
//!
//! The cache dir, `$XDG_CACHE_HOME/smartconnect-linux/app-icons/` (default `~/.cache/…`), is a
//! contract shared with the mirror crate's harvest and the fex icon download. PNGs are decoded on a
//! worker thread and kept in memory per path + modification time, so a re-downloaded icon reloads.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("smartconnect-linux/app-icons")
}

/// What is known about an app when looking up its icon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub pkg: String,
    pub name: String,
    pub cmp: Option<String>,
    pub user_id: i64,
    /// The phone's `iconName`, when the app list carried it.
    pub icon_name: Option<String>,
}

impl Query {
    pub fn new(pkg: &str, name: &str) -> Self {
        Self {
            pkg: pkg.to_owned(),
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// The phone's file name for this app's icon: `iconName` as sent, else `<pkg>_<cmp>.png` with a
    /// `<userId>_` prefix for another user (needs `cmp`).
    pub fn icon_file(&self) -> Option<String> {
        let sent = self.icon_name.clone().filter(|n| !n.is_empty());
        let computed = || {
            self.cmp
                .as_deref()
                .filter(|c| !c.is_empty() && !self.pkg.is_empty())
                .map(|cmp| {
                    let base = format!("{}_{cmp}.png", self.pkg);
                    if self.user_id != 0 {
                        format!("{}_{base}", self.user_id)
                    } else {
                        base
                    }
                })
        };
        sent.or_else(computed).filter(|n| safe_file_name(n))
    }

    fn label(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.pkg
        } else {
            &self.name
        }
    }
}

/// A single path component (names come from the phone).
fn safe_file_name(n: &str) -> bool {
    !n.is_empty() && !n.starts_with('.') && !n.contains('/') && !n.contains('\0')
}

#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    File(PathBuf),
    Themed(String),
    /// Key into the installed-applications index (lowercase name or desktop id).
    Desktop(String),
    Initials,
}

/// The resolution order, with the environment passed in: `exists` (a file), `themed` (an icon name
/// in the theme), `desktop` (a lowercase key in the installed-applications index).
pub fn choose(
    dir: &Path,
    q: &Query,
    exists: impl Fn(&Path) -> bool,
    themed: impl Fn(&str) -> bool,
    desktop: impl Fn(&str) -> bool,
) -> Choice {
    let pkg_file =
        Some(format!("{}.png", q.pkg)).filter(|n| !q.pkg.is_empty() && safe_file_name(n));
    for file in q.icon_file().into_iter().chain(pkg_file) {
        let path = dir.join(file);
        if exists(&path) {
            return Choice::File(path);
        }
    }
    if !q.pkg.is_empty() && themed(&q.pkg) {
        return Choice::Themed(q.pkg.clone());
    }
    for key in [q.name.trim().to_lowercase(), q.pkg.to_lowercase()] {
        if !key.is_empty() && desktop(&key) {
            return Choice::Desktop(key);
        }
    }
    Choice::Initials
}

// ── Runtime (main thread) ───────────────────────────────────────────────────

#[derive(Default)]
struct Cache {
    textures: HashMap<PathBuf, (Option<SystemTime>, gdk::Texture)>,
    /// Installed applications by lowercase name and lowercase desktop id (without `.desktop`).
    desktop: Option<HashMap<String, gio::Icon>>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

fn desktop_icon(key: &str) -> Option<gio::Icon> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let index = c.desktop.get_or_insert_with(|| {
            let mut map = HashMap::new();
            for info in gio::AppInfo::all() {
                let Some(icon) = info.icon() else { continue };
                map.entry(info.name().trim().to_lowercase())
                    .or_insert_with(|| icon.clone());
                if let Some(id) = info.id() {
                    map.entry(id.trim_end_matches(".desktop").to_lowercase())
                        .or_insert(icon);
                }
            }
            map
        });
        index.get(key).cloned()
    })
}

/// Forget the installed-applications index (re-read on next use), e.g. when the app list refreshes.
pub fn rescan() {
    CACHE.with(|c| c.borrow_mut().desktop = None);
}

/// An icon-sized widget showing one app's icon, or its initials until/unless one is found.
#[derive(Clone)]
pub struct IconSlot {
    stack: gtk::Stack,
    image: gtk::Image,
    avatar: adw::Avatar,
    /// The file this slot is waiting for, so a late load for an older query is ignored.
    want: Rc<RefCell<Option<PathBuf>>>,
}

impl IconSlot {
    pub fn new(size: i32) -> Self {
        let avatar = adw::Avatar::new(size, None, true);
        let image = gtk::Image::builder().pixel_size(size).build();
        let stack = gtk::Stack::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        stack.add_child(&avatar);
        stack.add_child(&image);
        stack.set_visible_child(&avatar);
        Self {
            stack,
            image,
            avatar,
            want: Rc::default(),
        }
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    fn show_initials(&self) {
        self.stack.set_visible_child(&self.avatar);
    }

    fn show_image(&self) {
        self.stack.set_visible_child(&self.image);
    }

    /// Resolves `q` and shows the result. A file not yet in memory is decoded off the main thread;
    /// meanwhile the slot keeps what it shows (initials for a new slot).
    pub fn set(&self, q: &Query) {
        self.avatar.set_text(Some(q.label()));
        let theme = gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d));
        let choice = choose(
            &cache_dir(),
            q,
            Path::is_file,
            |n| theme.as_ref().is_some_and(|t| t.has_icon(n)),
            |k| desktop_icon(k).is_some(),
        );
        *self.want.borrow_mut() = None;
        match choice {
            Choice::File(path) => self.load(path),
            Choice::Themed(name) => {
                self.image.set_icon_name(Some(&name));
                self.show_image();
            }
            Choice::Desktop(key) => match desktop_icon(&key) {
                Some(icon) => {
                    self.image.set_from_gicon(&icon);
                    self.show_image();
                }
                None => self.show_initials(),
            },
            Choice::Initials => self.show_initials(),
        }
    }

    fn load(&self, path: PathBuf) {
        let stamp = mtime(&path);
        let cached = CACHE.with(|c| {
            c.borrow()
                .textures
                .get(&path)
                .filter(|(m, _)| *m == stamp)
                .map(|(_, t)| t.clone())
        });
        if let Some(texture) = cached {
            self.image.set_paintable(Some(&texture));
            self.show_image();
            return;
        }
        *self.want.borrow_mut() = Some(path.clone());
        let (image, stack, avatar, want) = (
            self.image.downgrade(),
            self.stack.downgrade(),
            self.avatar.downgrade(),
            self.want.clone(),
        );
        glib::spawn_future_local(async move {
            let p = path.clone();
            let texture = gio::spawn_blocking(move || gdk::Texture::from_filename(&p).ok())
                .await
                .ok()
                .flatten();
            if let Some(t) = &texture {
                CACHE.with(|c| {
                    c.borrow_mut()
                        .textures
                        .insert(path.clone(), (stamp, t.clone()))
                });
            }
            if want.borrow().as_ref() != Some(&path) {
                return; // the slot was re-pointed meanwhile
            }
            let (Some(image), Some(stack), Some(avatar)) =
                (image.upgrade(), stack.upgrade(), avatar.upgrade())
            else {
                return;
            };
            match texture {
                Some(t) => {
                    image.set_paintable(Some(&t));
                    stack.set_visible_child(&image);
                }
                None => stack.set_visible_child(&avatar), // unreadable file: initials
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pkg: &str, name: &str, cmp: Option<&str>, user: i64) -> Query {
        Query {
            pkg: pkg.into(),
            name: name.into(),
            cmp: cmp.map(Into::into),
            user_id: user,
            icon_name: None,
        }
    }

    #[test]
    fn icon_file_names() {
        assert_eq!(
            q(
                "com.example.notes",
                "Notes",
                Some("com.example.notes.Main"),
                0
            )
            .icon_file()
            .as_deref(),
            Some("com.example.notes_com.example.notes.Main.png")
        );
        assert_eq!(
            q(
                "com.example.notes",
                "Notes",
                Some("com.example.notes.Main"),
                10
            )
            .icon_file()
            .as_deref(),
            Some("10_com.example.notes_com.example.notes.Main.png")
        );
        assert_eq!(
            q("com.example.notes", "Notes", None, 0).icon_file(),
            None,
            "no cmp, no computed name"
        );
        let sent = Query {
            icon_name: Some("x_y.png".into()),
            ..q("com.example", "E", Some("c"), 0)
        };
        assert_eq!(
            sent.icon_file().as_deref(),
            Some("x_y.png"),
            "the phone's iconName wins"
        );
        let evil = Query {
            icon_name: Some("../../etc/passwd".into()),
            ..q("com.example", "E", None, 0)
        };
        assert_eq!(evil.icon_file(), None);
    }

    #[test]
    fn resolver_order() {
        let dir = Path::new("/icons");
        let app = q(
            "com.example.notes",
            "Notes",
            Some("com.example.notes.Main"),
            0,
        );
        let named = dir.join("com.example.notes_com.example.notes.Main.png");
        let by_pkg = dir.join("com.example.notes.png");
        let none = |_: &str| false;

        // 1. iconName beats everything.
        let all = |p: &Path| p == named || p == by_pkg;
        assert_eq!(
            choose(dir, &app, all, |_| true, |_| true),
            Choice::File(named.clone())
        );
        // 2. <pkg>.png next.
        assert_eq!(
            choose(dir, &app, |p| p == by_pkg, |_| true, |_| true),
            Choice::File(by_pkg)
        );
        // 3a. Exact themed name = the package.
        assert_eq!(
            choose(dir, &app, |_| false, |n| n == "com.example.notes", |_| true),
            Choice::Themed("com.example.notes".into())
        );
        // A themed icon named after a package *segment* is not used.
        assert_eq!(
            choose(
                dir,
                &app,
                |_| false,
                |n| n == "notes" || n == "example",
                none
            ),
            Choice::Initials
        );
        // 3b. An installed app with exactly that name (case-insensitive).
        assert_eq!(
            choose(dir, &app, |_| false, none, |k| k == "notes"),
            Choice::Desktop("notes".into())
        );
        // A partial name match is not used.
        assert_eq!(
            choose(dir, &app, |_| false, none, |k| k == "notes app"),
            Choice::Initials
        );
        // 4. Nothing: initials.
        assert_eq!(choose(dir, &app, |_| false, none, none), Choice::Initials);
    }
}
