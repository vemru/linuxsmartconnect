//! The call window: Answer / Decline while ringing, a local timer and End while active.
//!
//! Driven entirely by the daemon's `call` / `waiting` objects (see `daemon.rs`): the window never
//! guesses a phase, and a button is disabled whenever the daemon says the matching action is not
//! available (`canAnswer` / `canEnd`). Closing the window asks the daemon to hand the call back to
//! its notification presenter, so the X is never a decline.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::glib;

use crate::daemon::{self, Call, CallPhase};

/// Stable, non-localised token used as this window's title. The optional GNOME Shell extension
/// under `extras/gnome-shell-extension/` matches it exactly; never replace it with a human-readable
/// title. Defined once in `call-token.txt` and shared with the extension rule at compile time.
pub const CALL_WINDOW_TITLE: &str = include_str!(
    "../../extras/gnome-shell-extension/window-rules@vamshi.linuxsmartconnect/call-token.txt"
);
// The token file has no trailing newline so the value is exact; the extension trims it as well.

type CommandHandler = Box<dyn Fn(String)>;

pub struct CallWindow {
    window: adw::Window,
    header_title: gtk::Label,
    app_label: gtk::Label,
    name: gtk::Label,
    detail: gtk::Label,
    timer: gtk::Label,
    spinner: adw::Spinner,
    status: gtk::Label,
    error: gtk::Label,
    answer: gtk::Button,
    end: gtk::Button,
    waiting_sep: gtk::Separator,
    waiting_box: gtk::Box,
    waiting_name: gtk::Label,
    waiting_answer: gtk::Button,
    waiting_end: gtk::Button,
    /// The primary / waiting call the buttons target.
    key: RefCell<Option<String>>,
    waiting_key: RefCell<Option<String>>,
    handler: RefCell<Option<CommandHandler>>,
    timer_state: RefCell<Option<TimerState>>,
}

struct TimerState {
    key: String,
    base: u64,
    at: Instant,
}

impl CallWindow {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let header_title = gtk::Label::new(None);
        header_title.add_css_class("title-4");
        let header = adw::HeaderBar::new();
        header.set_decoration_layout(Some(":close"));
        header.set_title_widget(Some(&header_title));

        let app_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        let name = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-1"])
            .build();
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let timer = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["title-1", "numeric"])
            .build();
        let spinner = adw::Spinner::new();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let status_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        status_row.append(&spinner);
        status_row.append(&status);
        let error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["error"])
            .build();

        let answer = gtk::Button::with_label("Answer");
        answer.add_css_class("suggested-action");
        answer.add_css_class("pill");
        let end = gtk::Button::with_label("Decline");
        end.add_css_class("destructive-action");
        end.add_css_class("pill");
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .margin_top(8)
            .build();
        buttons.append(&end);
        buttons.append(&answer);

        let waiting_sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        let waiting_name = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-4"])
            .build();
        let waiting_caption = gtk::Label::builder()
            .label("Call waiting")
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build();
        let waiting_answer = gtk::Button::with_label("Answer");
        waiting_answer.add_css_class("suggested-action");
        let waiting_end = gtk::Button::with_label("Decline");
        waiting_end.add_css_class("destructive-action");
        let waiting_buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();
        waiting_buttons.append(&waiting_end);
        waiting_buttons.append(&waiting_answer);
        let waiting_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        waiting_box.append(&waiting_caption);
        waiting_box.append(&waiting_name);
        waiting_box.append(&waiting_buttons);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        content.append(&app_label);
        content.append(&name);
        content.append(&detail);
        content.append(&timer);
        content.append(&status_row);
        content.append(&error);
        content.append(&buttons);
        content.append(&waiting_sep);
        content.append(&waiting_box);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let window = adw::Window::builder()
            .application(app)
            .title(CALL_WINDOW_TITLE)
            .default_width(380)
            .resizable(false)
            .content(&toolbar)
            .build();
        window.set_hide_on_close(true);

        let me = Rc::new(Self {
            window,
            header_title,
            app_label,
            name,
            detail,
            timer,
            spinner,
            status,
            error,
            answer,
            end,
            waiting_sep,
            waiting_box,
            waiting_name,
            waiting_answer,
            waiting_end,
            key: RefCell::default(),
            waiting_key: RefCell::default(),
            handler: RefCell::default(),
            timer_state: RefCell::default(),
        });

        // The X restores the daemon's notification fallback without declining the call.
        let weak = Rc::downgrade(&me);
        me.window.connect_close_request(move |_| {
            if let Some(cw) = weak.upgrade() {
                if let Some(key) = cw.key.borrow().clone() {
                    cw.send(daemon::call_ui_dismiss_line(&key));
                }
                cw.hide();
            }
            glib::Propagation::Stop
        });

        let weak = Rc::downgrade(&me);
        me.answer.connect_clicked(move |_| {
            if let Some(cw) = weak.upgrade() {
                if let Some(key) = cw.key.borrow().clone() {
                    cw.send(daemon::call_answer_line(&key));
                }
            }
        });
        let weak = Rc::downgrade(&me);
        me.end.connect_clicked(move |_| {
            if let Some(cw) = weak.upgrade() {
                if let Some(key) = cw.key.borrow().clone() {
                    cw.send(daemon::call_end_line(&key));
                }
            }
        });
        let weak = Rc::downgrade(&me);
        me.waiting_answer.connect_clicked(move |_| {
            if let Some(cw) = weak.upgrade() {
                if let Some(key) = cw.waiting_key.borrow().clone() {
                    cw.send(daemon::call_answer_line(&key));
                }
            }
        });
        let weak = Rc::downgrade(&me);
        me.waiting_end.connect_clicked(move |_| {
            if let Some(cw) = weak.upgrade() {
                if let Some(key) = cw.waiting_key.borrow().clone() {
                    cw.send(daemon::call_end_line(&key));
                }
            }
        });

        // The daemon sends no periodic frames, so the elapsed time is local: anchored on the
        // daemon's `elapsed` and corrected only when a status drifts by more than 2 s.
        let weak = Rc::downgrade(&me);
        glib::timeout_add_seconds_local(1, move || {
            let Some(cw) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            cw.tick();
            glib::ControlFlow::Continue
        });

        me.hide();
        me
    }

    /// Routes a control-socket command line to the app (which runs it off the main loop).
    pub fn set_handler(&self, handler: impl Fn(String) + 'static) {
        *self.handler.borrow_mut() = Some(Box::new(handler));
    }

    /// Folds a status snapshot into the window. `call` is the daemon's published call, `waiting`
    /// the second one. Only the registered GTK presenter may render it.
    pub fn sync(&self, owns_call: bool, call: Option<&Call>, waiting: Option<&Call>) {
        if !owns_call {
            self.hide();
            return;
        }
        let Some(call) = call else {
            self.hide();
            return;
        };
        self.render(call, waiting);
        if !self.window.is_visible() {
            self.window.present();
        }
    }

    /// Hides the window and forgets the call, e.g. when the daemon goes away.
    pub fn hide(&self) {
        self.window.set_visible(false);
        self.timer_state.replace(None);
        self.key.replace(None);
        self.waiting_key.replace(None);
    }

    fn send(&self, line: String) {
        if let Some(handler) = self.handler.borrow().as_ref() {
            handler(line);
        }
    }

    fn render(&self, call: &Call, waiting: Option<&Call>) {
        let ringing = call.phase == CallPhase::Ringing;
        let active = call.phase == CallPhase::Active;
        let title = match call.phase {
            CallPhase::Ringing if call.video => "Incoming video call",
            CallPhase::Ringing => "Incoming call",
            CallPhase::Answering => "Answering…",
            CallPhase::Active => "Call in progress",
            CallPhase::Ending => "Ending…",
            CallPhase::Unknown => "Call",
        };
        // Keep the window title as the stable token: the shell extension matches it, and it must
        // not change with the phase or the user's locale. The visible header title is separate.
        self.header_title.set_label(title);

        let name = display(call.name.as_deref(), "Unknown caller");
        self.name.set_label(name);
        self.detail
            .set_visible(!call.detail.as_deref().unwrap_or_default().is_empty());
        self.detail
            .set_label(call.detail.as_deref().unwrap_or_default());
        let app = display(call.app.as_deref(), "Smart Connect");
        self.app_label.set_label(&match call.phase {
            CallPhase::Ringing => format!("{app} · incoming call"),
            CallPhase::Active => format!("{app} · call"),
            _ => app.to_owned(),
        });
        self.key.replace(Some(call.key.clone()));

        // Ringing shows Answer; the End button is present in every phase (Decline while ringing,
        // Hang Up while active) and is disabled when the phone offered no valid action.
        self.answer
            .set_visible(ringing || (call.phase == CallPhase::Unknown && call.can_answer));
        self.answer
            .set_label(call.answer_label.as_deref().unwrap_or("Answer"));
        self.answer.set_sensitive(call.can_answer);
        self.end
            .set_label(call.end_label.as_deref().unwrap_or(if ringing {
                "Decline"
            } else {
                "Hang Up"
            }));
        self.end.set_sensitive(call.can_end);

        let pending = matches!(call.phase, CallPhase::Answering | CallPhase::Ending);
        self.spinner.set_visible(pending);
        self.status.set_visible(pending);
        self.status.set_label(match call.phase {
            CallPhase::Answering => "Answering…",
            CallPhase::Ending => "Ending…",
            _ => "",
        });
        if let Some(error) = call.error.as_deref().filter(|e| !e.is_empty()) {
            self.error.set_label(error);
            self.error.set_visible(true);
        } else {
            self.error.set_visible(false);
        }

        if active {
            self.timer.set_visible(true);
            self.update_timer(call);
        } else {
            self.timer.set_visible(false);
            self.timer_state.replace(None);
        }

        self.render_waiting(waiting);
    }

    fn render_waiting(&self, waiting: Option<&Call>) {
        let Some(waiting) = waiting else {
            self.waiting_sep.set_visible(false);
            self.waiting_box.set_visible(false);
            self.waiting_key.replace(None);
            return;
        };
        self.waiting_sep.set_visible(true);
        self.waiting_box.set_visible(true);
        self.waiting_name
            .set_label(display(waiting.name.as_deref(), "Unknown caller"));
        self.waiting_answer
            .set_label(waiting.answer_label.as_deref().unwrap_or("Answer"));
        self.waiting_answer.set_sensitive(waiting.can_answer);
        self.waiting_end
            .set_label(waiting.end_label.as_deref().unwrap_or("Decline"));
        self.waiting_end.set_sensitive(waiting.can_end);
        self.waiting_key.replace(Some(waiting.key.clone()));
    }

    fn update_timer(&self, call: &Call) {
        let now = Instant::now();
        let mut state = self.timer_state.borrow_mut();
        let reanchor = match state.as_ref() {
            Some(t) if t.key == call.key => {
                let shown = t.base + now.saturating_duration_since(t.at).as_secs();
                call.elapsed.abs_diff(shown) > 2
            }
            _ => true,
        };
        if reanchor {
            *state = Some(TimerState {
                key: call.key.clone(),
                base: call.elapsed,
                at: now,
            });
        }
        let secs = state
            .as_ref()
            .map_or(call.elapsed, |t| t.base + t.at.elapsed().as_secs());
        self.timer.set_label(&format_elapsed(secs));
    }

    fn tick(&self) {
        if let Some(state) = self.timer_state.borrow().as_ref() {
            if self.timer.is_visible() {
                self.timer
                    .set_label(&format_elapsed(state.base + state.at.elapsed().as_secs()));
            }
        }
    }
}

fn display<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    value.filter(|s| !s.is_empty()).unwrap_or(fallback)
}

fn format_elapsed(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::format_elapsed;

    #[test]
    fn elapsed_formats_minutes_and_hours() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(59), "00:59");
        assert_eq!(format_elapsed(60), "01:00");
        assert_eq!(format_elapsed(3599), "59:59");
        assert_eq!(format_elapsed(3600), "1:00:00");
        assert_eq!(format_elapsed(3661), "1:01:01");
    }
}
