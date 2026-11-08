//! Photos Browser Page (`smartconnect-gui`).
//! Browses camera roll and screenshots over the FUSE storage mount with async thumbnail
//! caching under `~/.cache/smartconnect-linux/thumbnails/`.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::prefs;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct PhotoItem {
    pub path: PathBuf,
    pub name: String,
    pub mtime: u64,
    pub size: u64,
}

pub struct PhotosPage {
    toasts: adw::ToastOverlay,
    flow_box: gtk::FlowBox,
    status_label: gtk::Label,
    refresh_btn: gtk::Button,
    save_btn: gtk::Button,
    copy_btn: gtk::Button,
    action_bar: gtk::ActionBar,
    empty_banner: adw::StatusPage,
    scrolled: gtk::ScrolledWindow,
    photos: RefCell<Vec<PhotoItem>>,
}

impl PhotosPage {
    pub fn build(toasts: &adw::ToastOverlay) -> (Rc<Self>, gtk::Widget) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(16)
            .margin_end(16)
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();

        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();

        let title = gtk::Label::builder()
            .label("Photos")
            .xalign(0.0)
            .css_classes(["title-2"])
            .build();

        let status_label = gtk::Label::builder()
            .label("Camera roll and screenshots")
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        title_box.append(&title);
        title_box.append(&status_label);
        header.append(&title_box);

        let refresh_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh Photos")
            .build();
        refresh_btn.add_css_class("flat");
        header.append(&refresh_btn);

        root.append(&header);

        // Grid FlowBox
        let flow_box = gtk::FlowBox::builder()
            .valign(gtk::Align::Start)
            .homogeneous(false)
            .column_spacing(12)
            .row_spacing(12)
            .min_children_per_line(2)
            .max_children_per_line(8)
            .selection_mode(gtk::SelectionMode::Multiple)
            .build();

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&flow_box)
            .build();

        // Empty / Not mounted state
        let empty_banner = adw::StatusPage::builder()
            .icon_name("camera-photo-symbolic")
            .title("No Photos Found")
            .description("Mount phone storage in the Files tab to browse camera roll and pictures")
            .vexpand(true)
            .build();

        root.append(&scrolled);
        root.append(&empty_banner);

        // Bottom action bar for multi-select
        let action_bar = gtk::ActionBar::builder().revealed(false).build();

        let save_btn = gtk::Button::builder()
            .label("Save to Downloads")
            .icon_name("folder-download-symbolic")
            .build();

        let copy_btn = gtk::Button::builder()
            .label("Copy")
            .icon_name("edit-copy-symbolic")
            .build();

        action_bar.pack_start(&save_btn);
        action_bar.pack_start(&copy_btn);
        root.append(&action_bar);

        let page = Rc::new(Self {
            toasts: toasts.clone(),
            flow_box,
            status_label,
            refresh_btn,
            save_btn,
            copy_btn,
            action_bar,
            empty_banner,
            scrolled,
            photos: RefCell::new(Vec::new()),
        });

        page.connect_signals();
        page.reload();

        (page, root.upcast())
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.refresh_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.reload();
            }
        });

        let weak = Rc::downgrade(self);
        self.flow_box.connect_selected_children_changed(move |fb| {
            if let Some(p) = weak.upgrade() {
                let selected = fb.selected_children().len();
                p.action_bar.set_revealed(selected > 0);
            }
        });

        // Click / Activate to open in desktop image viewer
        let weak = Rc::downgrade(self);
        self.flow_box.connect_child_activated(move |_, child| {
            if let Some(p) = weak.upgrade() {
                let idx = child.index() as usize;
                p.open_photo(idx);
            }
        });

        let weak = Rc::downgrade(self);
        self.save_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.save_selected();
            }
        });

        let weak = Rc::downgrade(self);
        self.copy_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.copy_selected();
            }
        });
    }

    pub fn reload(self: &Rc<Self>) {
        self.refresh_btn.set_sensitive(false);
        self.status_label.set_label("Scanning phone storage…");

        let mount = prefs::load().files.mount_path();
        let weak = Rc::downgrade(self);

        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || scan_photos(&mount)).await;
            if let Some(p) = weak.upgrade() {
                p.refresh_btn.set_sensitive(true);
                match res {
                    Ok(items) => p.render_photos(items),
                    Err(_) => {
                        p.status_label.set_label("Scan failed");
                    }
                }
            }
        });
    }

    fn render_photos(&self, items: Vec<PhotoItem>) {
        while let Some(child) = self.flow_box.first_child() {
            self.flow_box.remove(&child);
        }

        let count = items.len();
        if count == 0 {
            self.scrolled.set_visible(false);
            self.empty_banner.set_visible(true);
            self.status_label.set_label("0 photos found");
            *self.photos.borrow_mut() = Vec::new();
            return;
        }

        self.empty_banner.set_visible(false);
        self.scrolled.set_visible(true);
        self.status_label.set_label(&format!(
            "{count} photo{}",
            if count == 1 { "" } else { "s" }
        ));

        for (idx, item) in items.iter().enumerate() {
            let container = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .width_request(140)
                .build();

            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .width_request(140)
                .height_request(140)
                .build();
            picture.add_css_class("card");

            let label = gtk::Label::builder()
                .label(&item.name)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(16)
                .css_classes(["caption"])
                .build();

            container.append(&picture);
            container.append(&label);
            self.flow_box.append(&container);

            // Async thumbnail load
            let path = item.path.clone();
            let pic = picture.clone();
            glib::spawn_future_local(async move {
                let texture = gio::spawn_blocking(move || load_thumbnail(&path)).await;
                if let Ok(Some(tex)) = texture {
                    pic.set_paintable(Some(&tex));
                }
            });

            if idx > 300 {
                // Initial batch limit for instant responsiveness
                break;
            }
        }

        *self.photos.borrow_mut() = items;
    }

    fn open_photo(&self, idx: usize) {
        let photos = self.photos.borrow();
        if let Some(item) = photos.get(idx) {
            let file = gio::File::for_path(&item.path);
            let _ =
                gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gio::AppLaunchContext>);
        }
    }

    fn save_selected(&self) {
        let selected_indices: Vec<usize> = self
            .flow_box
            .selected_children()
            .iter()
            .map(|c| c.index() as usize)
            .collect();

        if selected_indices.is_empty() {
            return;
        }

        let downloads = std::env::var_os("XDG_DOWNLOAD_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."));

        let photos = self.photos.borrow();
        let mut saved = 0;
        for idx in selected_indices {
            if let Some(item) = photos.get(idx) {
                let dest = downloads.join(&item.name);
                if fs::copy(&item.path, &dest).is_ok() {
                    saved += 1;
                }
            }
        }
        self.toast(&format!("Saved {saved} photo(s) to Downloads"));
    }

    fn copy_selected(&self) {
        let selected_indices: Vec<usize> = self
            .flow_box
            .selected_children()
            .iter()
            .map(|c| c.index() as usize)
            .collect();

        if let Some(first_idx) = selected_indices.first() {
            let photos = self.photos.borrow();
            if let Some(item) = photos.get(*first_idx) {
                if let Some(display) = gdk::Display::default() {
                    let file = gio::File::for_path(&item.path);
                    display.clipboard().set(&file);
                    self.toast(&format!("Copied {} to clipboard", item.name));
                }
            }
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}

/// Scans DCIM and Pictures directories under mount point.
fn scan_photos(mount_dir: &Path) -> Vec<PhotoItem> {
    let mut items = Vec::new();
    let subdirs = [
        mount_dir.join("DCIM/Camera"),
        mount_dir.join("DCIM"),
        mount_dir.join("Pictures/Screenshots"),
        mount_dir.join("Pictures"),
    ];

    for dir in subdirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_photo(&path) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                items.push(PhotoItem {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path,
                    mtime,
                    size: meta.len(),
                });
            }
        }
    }

    // Sort newest first
    items.sort_by_key(|a| std::cmp::Reverse(a.mtime));
    items.dedup_by(|a, b| a.path == b.path);
    items
}

fn is_photo(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "heic" | "bmp"
    )
}

fn thumbnail_cache_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("smartconnect-linux/thumbnails");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn load_thumbnail(path: &Path) -> Option<gdk::Texture> {
    // Check thumbnail cache
    let cache_dir = thumbnail_cache_dir();
    let hash = format!("{:x}", md5_hash(path.to_string_lossy().as_bytes()));
    let cache_file = cache_dir.join(format!("{hash}.jpg"));

    if cache_file.exists() {
        if let Ok(texture) = gdk::Texture::from_filename(&cache_file) {
            return Some(texture);
        }
    }

    // Load original image texture
    if let Ok(texture) = gdk::Texture::from_filename(path) {
        // Cache texture if supported
        let _ = texture.save_to_png(&cache_file);
        return Some(texture);
    }

    None
}

fn md5_hash(data: &[u8]) -> u128 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut hasher = DefaultHasher::new();
    hasher.write(data);
    hasher.finish() as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_photo() {
        assert!(is_photo(Path::new("IMG_0001.JPG")));
        assert!(is_photo(Path::new("screenshot.png")));
        assert!(is_photo(Path::new("photo.webp")));
        assert!(is_photo(Path::new("camera.heic")));
        assert!(!is_photo(Path::new("document.pdf")));
        assert!(!is_photo(Path::new("notes.txt")));
    }

    #[test]
    fn test_scan_photos_empty_or_nonexistent() {
        let tmp = std::env::temp_dir().join("sc-photos-nonexistent");
        assert!(scan_photos(&tmp).is_empty());
    }
}
