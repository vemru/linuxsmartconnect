//! Exactly one presenter owns an incoming call's desktop UI.
//!
//! UIs register on the long-lived control-socket connection that carries their status stream;
//! losing that connection unregisters them. The daemon alone picks the presenter — UIs never infer
//! it from what they can see on screen, so two windows can never both claim the same call.
//!
//! Priority: `extension` > `gtk` > `notification`. An explicit GTK dismissal forces the
//! notification for that call key until the next call arrives; a connected extension always wins.

use std::collections::HashSet;

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CallPresenter {
    Extension,
    Gtk,
    #[default]
    Notification,
}

impl CallPresenter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Gtk => "gtk",
            Self::Notification => "notification",
        }
    }
}

/// Registration state. Set per role are kept because a UI may reconnect while the old socket is
/// still winding down; the published `presenter_id` pins the winner to one specific connection.
#[derive(Clone, Debug, Default)]
pub struct UiRegistry {
    next_id: u64,
    extension: HashSet<u64>,
    gtk: HashSet<u64>,
    /// Connection id currently allowed to render, if the presenter is a registered UI.
    winner: Option<u64>,
    dismissed_key: Option<String>,
    call_key: Option<String>,
}

impl UiRegistry {
    /// Registers one connection for `role`; returns its id. Re-registering a role supersedes its
    /// previous connection so a crashed/restarted UI cannot leave a stale owner behind.
    pub fn register(&mut self, role: &str) -> Option<u64> {
        let set = match role {
            "extension" => &mut self.extension,
            "gtk" => &mut self.gtk,
            _ => return None,
        };
        set.clear(); // a reconnect supersedes the role's previous connection
        self.next_id += 1;
        set.insert(self.next_id);
        // Only the role that actually wins may own `winner`; presenter_id() validates the role.
        if role == "extension" || self.extension.is_empty() {
            self.winner = Some(self.next_id);
        }
        Some(self.next_id)
    }

    pub fn unregister(&mut self, id: u64) -> bool {
        let removed = self.extension.remove(&id) | self.gtk.remove(&id);
        if self.winner == Some(id) {
            self.winner = None;
        }
        removed
    }

    /// GTK's X button: hand this call to the notification. A new call key clears the dismissal.
    pub fn dismiss(&mut self, key: &str) {
        if self.call_key.as_deref() == Some(key) {
            self.dismissed_key = Some(key.to_owned());
            if self.presenter() != CallPresenter::Extension {
                self.winner = None;
            }
        }
    }

    /// Returns true when the active call key changed.
    pub fn set_call_key(&mut self, key: Option<String>) -> bool {
        if self.call_key == key {
            return false;
        }
        self.call_key = key;
        self.dismissed_key = None;
        true
    }

    pub fn presenter(&self) -> CallPresenter {
        if !self.extension.is_empty() {
            return CallPresenter::Extension;
        }
        let dismissed = self
            .dismissed_key
            .as_deref()
            .is_some_and(|k| Some(k) == self.call_key.as_deref());
        if !dismissed && !self.gtk.is_empty() {
            return CallPresenter::Gtk;
        }
        CallPresenter::Notification
    }

    /// The only connection allowed to render the call UI. `0` means the notification presenter.
    /// A stale `winner` from another role must never be published as the current presenter's id.
    pub fn presenter_id(&self) -> u64 {
        match self.presenter() {
            CallPresenter::Notification => 0,
            CallPresenter::Extension => self
                .winner
                .filter(|id| self.extension.contains(id))
                .or_else(|| self.extension.iter().next().copied())
                .unwrap_or(0),
            CallPresenter::Gtk => self
                .winner
                .filter(|id| self.gtk.contains(id))
                .or_else(|| self.gtk.iter().next().copied())
                .unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_extension_over_gtk_over_notification() {
        let mut r = UiRegistry::default();
        r.set_call_key(Some("k".into()));
        assert_eq!(r.presenter(), CallPresenter::Notification);
        let gtk = r.register("gtk").unwrap();
        assert_eq!(r.presenter(), CallPresenter::Gtk);
        assert_eq!(r.presenter_id(), gtk);
        let ext = r.register("extension").unwrap();
        assert_eq!(r.presenter(), CallPresenter::Extension);
        assert_eq!(r.presenter_id(), ext);
        r.unregister(ext);
        assert_eq!(r.presenter(), CallPresenter::Gtk);
        assert_eq!(r.presenter_id(), gtk);
        r.unregister(gtk);
        assert_eq!(r.presenter(), CallPresenter::Notification);
        assert_eq!(r.presenter_id(), 0);
    }

    #[test]
    fn dismiss_falls_back_for_that_key_only() {
        let mut r = UiRegistry::default();
        r.set_call_key(Some("a".into()));
        let gtk = r.register("gtk").unwrap();
        r.dismiss("a");
        assert_eq!(r.presenter(), CallPresenter::Notification);
        assert_eq!(r.presenter_id(), 0);
        // A connected extension outranks a GTK dismissal.
        r.register("extension").unwrap();
        assert_eq!(r.presenter(), CallPresenter::Extension);
        r.unregister(r.extension.iter().next().copied().unwrap());
        assert_eq!(r.presenter(), CallPresenter::Notification);
        // The next call resets the dismissal.
        r.set_call_key(Some("b".into()));
        assert_eq!(r.presenter(), CallPresenter::Gtk);
        assert_eq!(r.presenter_id(), gtk);
    }

    #[test]
    fn no_second_presenter_ever_active() {
        let mut r = UiRegistry::default();
        r.set_call_key(Some("k".into()));
        let first = r.register("gtk").unwrap();
        let second = r.register("gtk").unwrap();
        assert_eq!(r.presenter(), CallPresenter::Gtk);
        assert_eq!(r.presenter_id(), second);
        r.unregister(first);
        assert_eq!(r.presenter_id(), second); // stale connect cannot steal the call
    }

    #[test]
    fn unknown_role_is_rejected() {
        let mut r = UiRegistry::default();
        assert_eq!(r.register("notification"), None);
        assert_eq!(r.presenter(), CallPresenter::Notification);
    }
}
