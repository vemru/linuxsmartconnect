//! Per-device Feature Settings page (`smartconnect-gui`).
//!
//! Provides switches for all 9 features:
//! - Sensitive features (`cross_control`, `storage_mount`, `folder_share`) default to OFF.
//! - Other features default to ON.
//!
//! Toggling a switch sends `feature.set` to the daemon and updates local capabilities.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::daemon::{self, DeviceFeatures};

pub struct SettingsPage {
    toasts: adw::ToastOverlay,
    current_device: Rc<RefCell<String>>,
    updating_ui: Rc<RefCell<bool>>,
    mirror_row: adw::SwitchRow,
    webcam_row: adw::SwitchRow,
    app_streaming_row: adw::SwitchRow,
    cross_control_row: adw::SwitchRow,
    phone_display_row: adw::SwitchRow,
    storage_mount_row: adw::SwitchRow,
    folder_share_row: adw::SwitchRow,
    clipboard_row: adw::SwitchRow,
    notifications_row: adw::SwitchRow,
}

impl SettingsPage {
    pub fn build(toasts: &adw::ToastOverlay) -> (Rc<Self>, gtk::Widget) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();

        let title = gtk::Label::builder()
            .label("Feature Settings")
            .xalign(0.0)
            .css_classes(["title-2"])
            .build();

        let subtitle = gtk::Label::builder()
            .label("Enable or disable specific features for this phone. Sensitive features default to OFF.")
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        header.append(&title);
        header.append(&subtitle);
        root.append(&header);

        let group = adw::PreferencesGroup::builder()
            .title("Connectivity & Sync")
            .build();

        let clipboard_row = adw::SwitchRow::builder()
            .title("Clipboard Synchronization")
            .subtitle("Bi-directional clipboard sharing between PC and phone")
            .active(true)
            .build();

        let notifications_row = adw::SwitchRow::builder()
            .title("Phone Notifications")
            .subtitle("Show notifications and quick replies on desktop")
            .active(true)
            .build();

        group.add(&clipboard_row);
        group.add(&notifications_row);
        root.append(&group);

        let group_media = adw::PreferencesGroup::builder()
            .title("Screen & Video")
            .build();

        let mirror_row = adw::SwitchRow::builder()
            .title("Screen Mirroring")
            .subtitle("Stream phone screen into a PC desktop window")
            .active(true)
            .build();

        let webcam_row = adw::SwitchRow::builder()
            .title("Phone Webcam")
            .subtitle("Use phone camera as a Linux webcam via v4l2loopback")
            .active(true)
            .build();

        let app_streaming_row = adw::SwitchRow::builder()
            .title("App Streaming")
            .subtitle("Stream and interact with individual Android apps")
            .active(true)
            .build();

        let phone_display_row = adw::SwitchRow::builder()
            .title("PC Screen on Phone")
            .subtitle("Extend or duplicate PC display onto phone")
            .active(true)
            .build();

        group_media.add(&mirror_row);
        group_media.add(&webcam_row);
        group_media.add(&app_streaming_row);
        group_media.add(&phone_display_row);
        root.append(&group_media);

        let group_sensitive = adw::PreferencesGroup::builder()
            .title("Advanced & File Access (Sensitive)")
            .description("These features allow remote input injection or filesystem exposure.")
            .build();

        let cross_control_row = adw::SwitchRow::builder()
            .title("Cross Control")
            .subtitle("Seamless mouse and keyboard gliding between PC and phone (Default: OFF)")
            .active(false)
            .build();

        let storage_mount_row = adw::SwitchRow::builder()
            .title("Phone Storage Mount")
            .subtitle("Mount phone /sdcard locally via FUSE filesystem (Default: OFF)")
            .active(false)
            .build();

        let folder_share_row = adw::SwitchRow::builder()
            .title("Folder Share")
            .subtitle("Expose local PC folder (~/SmartConnect) to phone (Default: OFF)")
            .active(false)
            .build();

        group_sensitive.add(&cross_control_row);
        group_sensitive.add(&storage_mount_row);
        group_sensitive.add(&folder_share_row);
        root.append(&group_sensitive);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&root)
            .build();

        let page = Rc::new(Self {
            toasts: toasts.clone(),
            current_device: Rc::new(RefCell::new("default".to_string())),
            updating_ui: Rc::new(RefCell::new(false)),
            mirror_row,
            webcam_row,
            app_streaming_row,
            cross_control_row,
            phone_display_row,
            storage_mount_row,
            folder_share_row,
            clipboard_row,
            notifications_row,
        });

        // Wire switches to daemon::set_feature
        page.wire_switch(&page.mirror_row, "mirror");
        page.wire_switch(&page.webcam_row, "webcam");
        page.wire_switch(&page.app_streaming_row, "app_streaming");
        page.wire_switch(&page.cross_control_row, "cross_control");
        page.wire_switch(&page.phone_display_row, "phone_display");
        page.wire_switch(&page.storage_mount_row, "storage_mount");
        page.wire_switch(&page.folder_share_row, "folder_share");
        page.wire_switch(&page.clipboard_row, "clipboard");
        page.wire_switch(&page.notifications_row, "notifications");

        (page, scroller.upcast())
    }

    fn wire_switch(&self, row: &adw::SwitchRow, feature: &'static str) {
        let toasts = self.toasts.clone();
        let current_dev = self.current_device.clone();
        let updating_ui = self.updating_ui.clone();
        row.connect_active_notify(glib::clone!(
            #[strong]
            toasts,
            move |r| {
                if *updating_ui.borrow() {
                    return;
                }
                let active = r.is_active();
                let dev = current_dev.borrow().clone();
                let r_clone = r.clone();
                let toasts = toasts.clone();
                glib::spawn_future_local(async move {
                    let res =
                        gio::spawn_blocking(move || daemon::set_feature(&dev, feature, active))
                            .await;
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            toasts
                                .add_toast(adw::Toast::new(&format!("Error saving feature: {e}")));
                            r_clone.set_active(!active);
                        }
                        Err(_) => {
                            toasts.add_toast(adw::Toast::new("Failed to contact daemon"));
                            r_clone.set_active(!active);
                        }
                    }
                });
            }
        ));
    }

    pub fn set_device(self: &Rc<Self>, device_id: Option<&str>) {
        let dev = device_id.unwrap_or("default").to_string();
        *self.current_device.borrow_mut() = dev;
        self.load_features();
    }

    pub fn load_features(self: &Rc<Self>) {
        let dev = self.current_device.borrow().clone();
        let (tx, rx) = async_channel::bounded(1);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || daemon::fetch_features(&dev)).await;
            if let Ok(Ok(feats)) = res {
                let _ = tx.send(feats).await;
            }
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(feats) = rx.recv().await {
                this.apply_features(&feats);
            }
        });
    }

    pub fn apply_features(&self, feats: &DeviceFeatures) {
        if *self.updating_ui.borrow() {
            return;
        }
        *self.updating_ui.borrow_mut() = true;
        self.mirror_row.set_active(feats.mirror);
        self.webcam_row.set_active(feats.webcam);
        self.app_streaming_row.set_active(feats.app_streaming);
        self.cross_control_row.set_active(feats.cross_control);
        self.phone_display_row.set_active(feats.phone_display);
        self.storage_mount_row.set_active(feats.storage_mount);
        self.folder_share_row.set_active(feats.folder_share);
        self.clipboard_row.set_active(feats.clipboard);
        self.notifications_row.set_active(feats.notifications);
        *self.updating_ui.borrow_mut() = false;
    }
}
