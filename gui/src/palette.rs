//! Command Palette (`smartconnect-gui --palette`).
//! Fast keyboard launcher for app actions with fuzzy search, keyboard navigation,
//! and availability filtering based on Phase 1 capability matrix.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

#[allow(dead_code)]
pub struct PaletteAction {
    pub id: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
    pub icon: &'static str,
    pub keywords: &'static [&'static str],
    pub feature_req: Option<&'static str>,
    pub run: fn(&Rc<crate::window::Ui>),
}

pub const ACTIONS: &[PaletteAction] = &[
    PaletteAction {
        id: "connect_pair",
        title: "Connect / Pair Phone",
        subtitle: "Connect to paired phone or show pairing invite",
        icon: "phone-symbolic",
        keywords: &["connect", "pair", "phone", "wifi", "bluetooth"],
        feature_req: None,
        run: |ui| ui.toggle_connection(),
    },
    PaletteAction {
        id: "disconnect",
        title: "Disconnect Phone",
        subtitle: "Close the active phone session",
        icon: "network-offline-symbolic",
        keywords: &["disconnect", "close", "stop"],
        feature_req: None,
        run: |_ui| {
            let _ = std::thread::spawn(|| crate::daemon::command("disconnect"));
        },
    },
    PaletteAction {
        id: "mirror",
        title: "Start / Stop Screen Mirror",
        subtitle: "Mirror phone screen on PC with scrcpy",
        icon: "video-display-symbolic",
        keywords: &["mirror", "screen", "scrcpy", "display"],
        feature_req: Some("mirror"),
        run: |ui| ui.toggle_mirror(),
    },
    PaletteAction {
        id: "webcam",
        title: "Toggle Virtual Webcam",
        subtitle: "Stream phone camera as v4l2loopback webcam",
        icon: "camera-web-symbolic",
        keywords: &["webcam", "camera", "v4l2", "video"],
        feature_req: Some("webcam"),
        run: |ui| ui.toggle_webcam(),
    },
    PaletteAction {
        id: "storage",
        title: "Browse Phone Storage",
        subtitle: "Mount and explore phone files over FUSE",
        icon: "folder-remote-symbolic",
        keywords: &["storage", "files", "sdcard", "fuse", "mount"],
        feature_req: Some("storage_mount"),
        run: |ui| {
            ui.present();
            ui.views().set_visible_child_name("files");
        },
    },
    PaletteAction {
        id: "photos",
        title: "Browse Photos & Camera Roll",
        subtitle: "Browse photos, pictures and screenshots",
        icon: "camera-photo-symbolic",
        keywords: &["photos", "pictures", "screenshots", "gallery", "camera"],
        feature_req: None,
        run: |ui| {
            ui.present();
            ui.views().set_visible_child_name("photos");
        },
    },
    PaletteAction {
        id: "timeline",
        title: "Open Timeline History",
        subtitle: "View transfer, notification, and clipboard history",
        icon: "document-open-recent-symbolic",
        keywords: &[
            "timeline",
            "history",
            "recent",
            "transfers",
            "files",
            "clipboard",
        ],
        feature_req: None,
        run: |ui| {
            ui.present();
            ui.views().set_visible_child_name("timeline");
        },
    },
    PaletteAction {
        id: "diagnostics",
        title: "Open Connection Doctor",
        subtitle: "Diagnose network, ports, firewall, and dependencies",
        icon: "utilities-system-monitor-symbolic",
        keywords: &["doctor", "diagnostics", "health", "fix", "repair", "ports"],
        feature_req: None,
        run: |ui| {
            ui.present();
            ui.views().set_visible_child_name("diagnostics");
        },
    },
    PaletteAction {
        id: "settings",
        title: "Open Settings",
        subtitle: "Configure per-device feature switches",
        icon: "emblem-system-symbolic",
        keywords: &["settings", "preferences", "config", "switches"],
        feature_req: None,
        run: |ui| {
            ui.present();
            ui.views().set_visible_child_name("settings");
        },
    },
];

/// Scores match quality: substring match with bonus, or sequential character match.
pub fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let t = target.to_lowercase();
    if let Some(pos) = t.find(&q) {
        let bonus = if pos == 0 { 100 } else { 50 };
        return Some(bonus - (pos as i32));
    }
    let mut score = 0;
    let mut t_chars = t.char_indices();
    let mut last_idx = 0;
    for qc in q.chars() {
        if let Some((idx, _)) = t_chars.find(|(_, tc)| *tc == qc) {
            if idx == last_idx + 1 {
                score += 10;
            } else {
                score += 2;
            }
            last_idx = idx;
        } else {
            return None;
        }
    }
    Some(score)
}

pub struct CommandPalette {
    window: gtk::Window,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    visible_actions: RefCell<Vec<&'static PaletteAction>>,
    ui: Rc<crate::window::Ui>,
}

impl CommandPalette {
    pub fn show(ui: &Rc<crate::window::Ui>) {
        let window = gtk::Window::builder()
            .title("Command Palette")
            .modal(true)
            .decorated(false)
            .default_width(520)
            .default_height(340)
            .build();
        window.add_css_class("card");

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Type a command or search…")
            .hexpand(true)
            .build();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .build();

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();

        root.append(&search);
        root.append(&scrolled);
        window.set_child(Some(&root));

        let palette = Rc::new(Self {
            window: window.clone(),
            search: search.clone(),
            list: list.clone(),
            visible_actions: RefCell::new(Vec::new()),
            ui: ui.clone(),
        });

        palette.connect_events();
        palette.filter("");

        window.present();
        search.grab_focus();
    }

    fn connect_events(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.search.connect_search_changed(move |entry| {
            if let Some(p) = weak.upgrade() {
                p.filter(&entry.text());
            }
        });

        // Key controller for navigation and execution
        let key_ctl = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        key_ctl.connect_key_pressed(move |_, key, _, _| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => {
                    p.window.close();
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    p.execute_selected();
                    glib::Propagation::Stop
                }
                gdk::Key::Down => {
                    p.move_selection(1);
                    glib::Propagation::Stop
                }
                gdk::Key::Up => {
                    p.move_selection(-1);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(key_ctl);

        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, row| {
            if let Some(p) = weak.upgrade() {
                let idx = row.index() as usize;
                p.execute_at(idx);
            }
        });
    }

    fn filter(&self, query: &str) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let mut matches: Vec<(i32, &'static PaletteAction)> = Vec::new();
        let caps = self.ui.current_capabilities();

        for action in ACTIONS {
            // Check availability if feature is required
            if let Some(feat) = action.feature_req {
                let available = match feat {
                    "mirror" => caps.mirror.is_available(),
                    "webcam" => caps.webcam.is_available(),
                    "app_streaming" => caps.app_streaming.is_available(),
                    "cross_control" => caps.cross_control.is_available(),
                    "storage_mount" => caps.storage_mount.is_available(),
                    _ => true,
                };
                if !available {
                    continue;
                }
            }

            let mut best_score = None;
            if let Some(s) = fuzzy_score(query, action.title) {
                best_score = Some(s);
            }
            for kw in action.keywords {
                if let Some(s) = fuzzy_score(query, kw) {
                    best_score = Some(best_score.map_or(s, |b| b.max(s)));
                }
            }
            if let Some(score) = best_score {
                matches.push((score, action));
            }
        }

        // Sort descending by score
        matches.sort_by_key(|a| std::cmp::Reverse(a.0));

        let mut visible = Vec::new();
        for (_, action) in matches {
            let row = adw::ActionRow::builder()
                .title(action.title)
                .subtitle(action.subtitle)
                .build();
            let icon = gtk::Image::from_icon_name(action.icon);
            row.add_prefix(&icon);
            self.list.append(&row);
            visible.push(action);
        }

        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }

        *self.visible_actions.borrow_mut() = visible;
    }

    fn move_selection(&self, delta: i32) {
        let count = self.visible_actions.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let current = self.list.selected_row().map(|r| r.index()).unwrap_or(0);
        let next = (current + delta).clamp(0, count - 1);
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
        }
    }

    fn execute_selected(&self) {
        if let Some(row) = self.list.selected_row() {
            self.execute_at(row.index() as usize);
        }
    }

    fn execute_at(&self, idx: usize) {
        let actions = self.visible_actions.borrow();
        if let Some(action) = actions.get(idx) {
            self.window.close();
            (action.run)(&self.ui);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_score_matches() {
        assert!(fuzzy_score("mirr", "Start / Stop Screen Mirror").is_some());
        assert!(fuzzy_score("cam", "Toggle Virtual Webcam").is_some());
        assert!(fuzzy_score("xyz", "Mirror").is_none());
        assert!(fuzzy_score("", "Anything").is_some());
    }
}
