//! Diagnostics & Connection Doctor page (`smartconnect-gui`).
//! Renders structured health checks, firewall/dependency status, fix hints, and safe 1-click
//! pkexec repairs.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::daemon::{self, CheckStatus, DoctorReport};

pub struct DiagnosticsPage {
    toasts: adw::ToastOverlay,
    group: adw::PreferencesGroup,
    summary_label: gtk::Label,
    refresh_btn: gtk::Button,
    copy_btn: gtk::Button,
    last_report: RefCell<Option<DoctorReport>>,
}

impl DiagnosticsPage {
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
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .hexpand(true)
            .build();

        let title = gtk::Label::builder()
            .label("Connection Doctor")
            .xalign(0.0)
            .css_classes(["title-2"])
            .build();

        let summary_label = gtk::Label::builder()
            .label("Diagnose network, ports, firewall, and dependencies")
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        title_box.append(&title);
        title_box.append(&summary_label);
        header.append(&title_box);

        let copy_btn = gtk::Button::builder()
            .label("Copy Report")
            .icon_name("edit-copy-symbolic")
            .valign(gtk::Align::Center)
            .build();
        header.append(&copy_btn);

        let refresh_btn = gtk::Button::builder()
            .label("Run Checks")
            .icon_name("view-refresh-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        header.append(&refresh_btn);

        root.append(&header);

        let scrolled = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let group = adw::PreferencesGroup::builder()
            .title("System & Network Checks")
            .build();

        scrolled.set_child(Some(&group));
        root.append(&scrolled);

        let page = Rc::new(Self {
            toasts: toasts.clone(),
            group,
            summary_label,
            refresh_btn,
            copy_btn,
            last_report: RefCell::new(None),
        });

        page.connect_signals();
        page.run_checks();

        (page, root.upcast())
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.refresh_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.run_checks();
            }
        });

        let weak = Rc::downgrade(self);
        self.copy_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.copy_report();
            }
        });
    }

    pub fn run_checks(self: &Rc<Self>) {
        self.refresh_btn.set_sensitive(false);
        self.summary_label.set_label("Running diagnostics…");

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(daemon::doctor).await;
            if let Some(p) = weak.upgrade() {
                p.refresh_btn.set_sensitive(true);
                match res {
                    Ok(Ok(report)) => p.render_report(report),
                    Ok(Err(e)) => {
                        p.summary_label
                            .set_label(&format!("Could not run diagnostics: {e}"));
                        p.toast(&format!("Diagnostics failed: {e}"));
                    }
                    Err(_) => {
                        p.summary_label
                            .set_label("Diagnostics worker thread failed");
                    }
                }
            }
        });
    }

    fn render_report(self: &Rc<Self>, report: DoctorReport) {
        // Clear previous rows
        while let Some(child) = self.group.first_child() {
            // adw::PreferencesGroup wraps children
            self.group.remove(&child);
        }

        let fails = report
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .count();
        let warns = report
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Warn)
            .count();

        if fails == 0 && warns == 0 {
            self.summary_label
                .set_label("All systems operational. Phone connection ready.");
        } else {
            self.summary_label.set_label(&format!(
                "{fails} error{}, {warns} warning{} detected",
                if fails == 1 { "" } else { "s" },
                if warns == 1 { "" } else { "s" }
            ));
        }

        for check in &report.checks {
            let row = adw::ActionRow::builder()
                .title(&check.title)
                .subtitle(&check.detail)
                .build();

            let (icon_name, css) = match check.status {
                CheckStatus::Ok => ("emblem-ok-symbolic", "success"),
                CheckStatus::Warn => ("dialog-warning-symbolic", "warning"),
                CheckStatus::Fail => ("dialog-error-symbolic", "error"),
            };

            let img = gtk::Image::builder()
                .icon_name(icon_name)
                .css_classes([css])
                .pixel_size(24)
                .build();
            row.add_prefix(&img);

            if let Some(fix_cmd) = &check.fix {
                let fix_btn = gtk::Button::builder()
                    .label("Fix")
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action"])
                    .tooltip_text(fix_cmd)
                    .build();

                let weak = Rc::downgrade(self);
                let cmd = fix_cmd.clone();
                fix_btn.connect_clicked(move |_| {
                    if let Some(p) = weak.upgrade() {
                        p.apply_fix(&cmd);
                    }
                });
                row.add_suffix(&fix_btn);
            }

            self.group.add(&row);
        }

        *self.last_report.borrow_mut() = Some(report);
    }

    fn apply_fix(self: &Rc<Self>, fix_cmd: &str) {
        let cmd = fix_cmd.to_string();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || daemon::run_fix_pkexec(&cmd)).await;
            if let Some(p) = weak.upgrade() {
                match res {
                    Ok(Ok(())) => {
                        p.toast("Fix applied successfully");
                        p.run_checks();
                    }
                    Ok(Err(e)) => p.toast(&format!("Could not apply fix: {e}")),
                    Err(_) => p.toast("Worker thread failed"),
                }
            }
        });
    }

    fn copy_report(&self) {
        let Some(ref report) = *self.last_report.borrow() else {
            self.toast("No diagnostic report to copy");
            return;
        };

        let mut text = String::from(
            "Smart Connect Linux Diagnostics Report\n======================================\n",
        );
        for check in &report.checks {
            let tag = match check.status {
                CheckStatus::Ok => "[OK]  ",
                CheckStatus::Warn => "[WARN]",
                CheckStatus::Fail => "[FAIL]",
            };
            text.push_str(&format!("{tag} {}: {}\n", check.title, check.detail));
            if let Some(fix) = &check.fix {
                text.push_str(&format!("       Fix: {fix}\n"));
            }
        }

        let display = gdk::Display::default();
        if let Some(disp) = display {
            disp.clipboard().set_text(&text);
            self.toast("Diagnostic report copied to clipboard");
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}
