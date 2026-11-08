//! Timeline Page (`smartconnect-gui`).
//! Displays searchable history of transfers, clips, notifications, and sessions.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::daemon::{self, TimelineItem};

pub struct TimelinePage {
    toasts: adw::ToastOverlay,
    search_entry: gtk::SearchEntry,
    kind_dropdown: gtk::DropDown,
    refresh_btn: gtk::Button,
    clear_btn: gtk::Button,
    list_box: gtk::ListBox,
    status_label: gtk::Label,
    current_kind: RefCell<Option<String>>,
    current_search: RefCell<String>,
}

impl TimelinePage {
    pub fn build(toasts: &adw::ToastOverlay) -> (Rc<Self>, gtk::Widget) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(16)
            .margin_bottom(16)
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
            .label("Timeline")
            .xalign(0.0)
            .css_classes(["title-2"])
            .build();

        let status_label = gtk::Label::builder()
            .label("Searchable transfer and event history")
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        title_box.append(&title);
        title_box.append(&status_label);
        header.append(&title_box);

        let clear_btn = gtk::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text("Clear Timeline History")
            .build();
        clear_btn.add_css_class("flat");

        let refresh_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh")
            .build();
        refresh_btn.add_css_class("flat");

        header.append(&clear_btn);
        header.append(&refresh_btn);
        root.append(&header);

        // Filter / Search bar
        let search_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();

        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search files, links, notifications…")
            .hexpand(true)
            .build();

        let kinds = ["All", "Files", "Clipboard", "Notifications", "Sessions"];
        let kind_dropdown = gtk::DropDown::from_strings(&kinds);

        search_box.append(&search_entry);
        search_box.append(&kind_dropdown);
        root.append(&search_box);

        // List box for items in a scrolled window
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let placeholder = gtk::Label::builder()
            .label("No timeline events found")
            .margin_top(32)
            .margin_bottom(32)
            .css_classes(["dim-label"])
            .build();
        list_box.set_placeholder(Some(&placeholder));

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list_box)
            .build();

        root.append(&scrolled);

        let page = Rc::new(Self {
            toasts: toasts.clone(),
            search_entry,
            kind_dropdown,
            refresh_btn,
            clear_btn,
            list_box,
            status_label,
            current_kind: RefCell::new(None),
            current_search: RefCell::new(String::new()),
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
        self.clear_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.clear_history();
            }
        });

        let weak = Rc::downgrade(self);
        self.search_entry.connect_search_changed(move |entry| {
            if let Some(p) = weak.upgrade() {
                *p.current_search.borrow_mut() = entry.text().to_string();
                p.reload();
            }
        });

        let weak = Rc::downgrade(self);
        self.kind_dropdown.connect_selected_notify(move |dd| {
            if let Some(p) = weak.upgrade() {
                let selected = dd.selected();
                let kind_str = match selected {
                    1 => Some("file".to_string()),
                    2 => Some("clipboard".to_string()),
                    3 => Some("notification".to_string()),
                    4 => Some("session".to_string()),
                    _ => None,
                };
                *p.current_kind.borrow_mut() = kind_str;
                p.reload();
            }
        });
    }

    pub fn reload(self: &Rc<Self>) {
        let kind = self.current_kind.borrow().clone();
        let search = self.current_search.borrow().clone();

        self.refresh_btn.set_sensitive(false);
        let weak = Rc::downgrade(self);

        glib::spawn_future_local(async move {
            let res =
                gio::spawn_blocking(move || daemon::fetch_timeline(kind.as_deref(), Some(&search)))
                    .await;

            if let Some(p) = weak.upgrade() {
                p.refresh_btn.set_sensitive(true);
                match res {
                    Ok(Ok(items)) => p.render_items(items),
                    Ok(Err(e)) => {
                        p.status_label
                            .set_label(&format!("Could not load timeline: {e}"));
                    }
                    Err(_) => {
                        p.status_label.set_label("Timeline worker thread failed");
                    }
                }
            }
        });
    }

    fn clear_history(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(daemon::clear_timeline).await;
            if let Some(p) = weak.upgrade() {
                match res {
                    Ok(Ok(())) => {
                        p.toast("Timeline history cleared");
                        p.reload();
                    }
                    Ok(Err(e)) => p.toast(&format!("Failed to clear: {e}")),
                    Err(_) => p.toast("Worker thread error"),
                }
            }
        });
    }

    fn render_items(&self, items: Vec<TimelineItem>) {
        // Clear old rows
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        let count = items.len();
        self.status_label.set_label(&format!(
            "{count} event{}",
            if count == 1 { "" } else { "s" }
        ));

        for item in items {
            let row = adw::ActionRow::builder()
                .title(&item.title)
                .subtitle(&item.detail)
                .build();

            let icon_name = match item.kind.as_str() {
                "file" => {
                    if item.direction == "outgoing" {
                        "document-send-symbolic"
                    } else {
                        "folder-download-symbolic"
                    }
                }
                "clipboard" => "edit-copy-symbolic",
                "notification" => "preferences-system-notifications-symbolic",
                "session" => "phone-symbolic",
                _ => "dialog-information-symbolic",
            };

            let icon = gtk::Image::from_icon_name(icon_name);
            row.add_prefix(&icon);

            let time_str = format_ts(item.ts);
            let time_badge = gtk::Label::builder()
                .label(&time_str)
                .css_classes(["dim-label"])
                .build();
            row.add_suffix(&time_badge);

            if let Some(path_str) = item.path {
                let open_btn = gtk::Button::builder()
                    .icon_name("document-open-symbolic")
                    .valign(gtk::Align::Center)
                    .tooltip_text("Open File / Folder")
                    .build();
                open_btn.add_css_class("flat");

                let p = path_str.clone();
                open_btn.connect_clicked(move |_| {
                    let file = gio::File::for_path(&p);
                    let _ = gio::AppInfo::launch_default_for_uri(
                        &file.uri(),
                        None::<&gio::AppLaunchContext>,
                    );
                });
                row.add_suffix(&open_btn);
            }

            self.list_box.append(&row);
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}

fn format_ts(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = now.saturating_sub(ts);

    if diff < 60 {
        "Just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}
