//! Call state from Android CallStyle notifications (`type 6`, services 5 and 37).
//!
//! The phone posts one notification per call and keeps its key from ringing to the end, so the
//! tracker can follow a call by key. It folds those frames into the two objects published in
//! `PhoneStatus::call` / `PhoneStatus::waiting` and resolves the action ids that `call_answer` and
//! `call_end` send back.
//!
//! Observed on a device (see `.notes/call-popup.md`):
//! - ringing: `callType 1`, `actions [0 Decline, 1 Answer]`, `priority 2`;
//! - active: `callType 2`, `actions [0 Hang Up, 1 Speaker, 2 Mute]`;
//! - the phone removes and re-posts a ringing notification ~150 ms apart (a re-rank), so a removal
//!   is only final after [`REMOVAL_GRACE`];
//! - the dialer reuses one notification key for every call, so Active → Ringing is a new call;
//! - no periodic updates: the GUI must run its own timer.
//!
//! Actions are resolved by `data.callType` plus the positions in `data.callStyleActions` (INFERRED):
//! ringing lists Decline first and Answer last, an active call lists Hang Up first. `action_name`
//! is used only as the button label. An index that does not name an entry with an `action_id`
//! leaves that button disabled. The unverified paths (decline from the PC, missed calls,
//! WhatsApp/video, call waiting) use the same resolver and never assume a frame exists.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// Android notification `type` for CallStyle calls.
pub const TYPE_CALL: i64 = 6;
/// `data.callType` values seen on the phone. Anything else is inferred from the number of
/// `callStyleActions` entries (INFERRED).
pub const CALL_TYPE_RINGING: i64 = 1;
pub const CALL_TYPE_ACTIVE: i64 = 2;
/// The phone removes and re-posts a ringing notification ~150 ms apart; wait this long before
/// treating a removal as final.
pub const REMOVAL_GRACE: Duration = Duration::from_secs(1);
/// How long a pending Answer/End may stay unconfirmed before it reverts.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(10);
/// Call waiting: never track more than two calls.
pub const MAX_CALLS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CallPhase {
    Ringing,
    Answering,
    Active,
    Ending,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallStatus {
    /// Notification key; also the target for `call_answer` / `call_end`.
    pub key: String,
    pub phase: CallPhase,
    /// Caller name (`person_name`, else `title`).
    pub name: Option<String>,
    /// The notification's free-text line (often the number).
    pub detail: Option<String>,
    /// `apk_name` (e.g. "Phone", "WhatsApp").
    pub app: Option<String>,
    pub video: bool,
    /// Seconds since the call became active; 0 while ringing.
    pub elapsed: u64,
    /// `call_answer` is available now: ringing, an Answer action was resolved, and no action is
    /// pending.
    pub can_answer: bool,
    /// `call_end` is available now: ringing with a Decline action, or active with a Hang Up action,
    /// and no action is pending.
    pub can_end: bool,
    /// The phone's label for the Answer action, if it offered one.
    pub answer_label: Option<String>,
    /// The phone's label for the current End action (`Decline` while ringing, `Hang Up` while
    /// active).
    pub end_label: Option<String>,
    /// Set when a pending action timed out; cleared by the next frame for this call.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallCommand {
    Answer,
    End,
}

/// What the daemon must send to the phone for a command.
#[derive(Clone, Debug, PartialEq)]
pub struct CallAction {
    pub service: i64,
    pub key: String,
    pub action_id: i64,
}

/// One action the phone offered, with the role assigned by its `callStyleActions` position.
#[derive(Clone, Debug, PartialEq)]
struct ResolvedAction {
    id: i64,
    label: String,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    phase: CallPhase,
    command: CallCommand,
    deadline: Instant,
}

#[derive(Debug)]
struct Call {
    key: String,
    service: i64,
    name: Option<String>,
    detail: Option<String>,
    app: Option<String>,
    video: bool,
    /// The last phase the phone reported; never a pending one.
    observed: CallPhase,
    answer: Option<ResolvedAction>,
    decline: Option<ResolvedAction>,
    hang_up: Option<ResolvedAction>,
    active_since: Option<Instant>,
    error: Option<String>,
    pending: Option<Pending>,
    removal_deadline: Option<Instant>,
    /// Insertion order, used to pick the newest call when ranks tie.
    seq: u64,
}

impl Call {
    fn published_phase(&self) -> CallPhase {
        self.pending.map_or(self.observed, |p| p.phase)
    }

    fn status(&self, now: Instant) -> CallStatus {
        let elapsed = match (self.observed, self.active_since) {
            (CallPhase::Active, Some(since)) => now.saturating_duration_since(since).as_secs(),
            _ => 0,
        };
        let pending = self.pending.is_some();
        let (can_answer, answer_label) = match &self.answer {
            Some(action) => (
                self.observed == CallPhase::Ringing && !pending,
                Some(action.label.clone()),
            ),
            None => (false, None),
        };
        let (can_end, end_label) = match self.observed {
            CallPhase::Ringing => match &self.decline {
                Some(action) => (!pending, Some(action.label.clone())),
                None => (false, None),
            },
            CallPhase::Active => match &self.hang_up {
                Some(action) => (!pending, Some(action.label.clone())),
                None => (false, None),
            },
            _ => (false, None),
        };
        CallStatus {
            key: self.key.clone(),
            phase: self.published_phase(),
            name: self.name.clone(),
            detail: self.detail.clone(),
            app: self.app.clone(),
            video: self.video,
            elapsed,
            can_answer,
            can_end,
            answer_label,
            end_label,
            error: self.error.clone(),
        }
    }
}

/// All live calls, keyed by the phone's notification key.
#[derive(Default)]
pub struct CallTracker {
    calls: HashMap<String, Call>,
    next_seq: u64,
}

impl CallTracker {
    /// Folds one notification frame into the tracker. Returns whether the published state changed.
    pub fn apply(&mut self, service: i64, frame: &Value, now: Instant) -> bool {
        let Some(key) = frame
            .get("key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
        else {
            return false;
        };
        let event = frame.get("event").and_then(Value::as_i64).unwrap_or(-1);

        if event == 2 {
            let Some(call) = self.calls.get_mut(key) else {
                return false;
            };
            // The phone sends the removal on services 5 and 37; one grace covers both.
            if call.removal_deadline.is_none() {
                call.removal_deadline = Some(now + REMOVAL_GRACE);
            }
            return false;
        }
        if !matches!(event, 0 | 1) {
            return false;
        }
        let Some(data) = frame.get("data") else {
            return false;
        };

        let text = |k: &str| {
            data.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let name = text("person_name").or_else(|| text("title"));
        let detail = text("content");
        let app = text("apk_name");
        let video = data
            .get("isVideo")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let actions = frame
            .get("actions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let call_type = data.get("callType").and_then(Value::as_i64);
        let (answer, decline, hang_up) = resolve_actions(call_type, &actions, data);
        let reported = match call_type {
            Some(CALL_TYPE_RINGING) => Some(CallPhase::Ringing),
            Some(CALL_TYPE_ACTIVE) => Some(CallPhase::Active),
            _ => None,
        };
        let style_len = data
            .get("callStyleActions")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);

        if let Some(call) = self.calls.get_mut(key) {
            call.service = service;
            call.name = name;
            call.detail = detail;
            call.app = app;
            call.video = video;
            call.answer = answer;
            call.decline = decline;
            call.hang_up = hang_up;
            call.removal_deadline = None;
            call.error = None;
            if let Some(reported) = reported {
                if reported == CallPhase::Active && call.observed != CallPhase::Active {
                    call.active_since = Some(now);
                } else if reported == CallPhase::Ringing && call.observed == CallPhase::Active {
                    // The dialer reuses one notification key for every call (OBSERVED). Active →
                    // Ringing on that key is a new call, not a state change of the old one.
                    call.active_since = None;
                    call.pending = None;
                }
                call.observed = reported;
                if reported == CallPhase::Active
                    && call
                        .pending
                        .is_some_and(|p| p.phase == CallPhase::Answering)
                {
                    call.pending = None;
                }
            }
            return true;
        }

        if self.calls.len() >= MAX_CALLS {
            return false;
        }
        // Unknown `callType` (other apps, future builds): infer the phase from the number of
        // `callStyleActions` entries (INFERRED). No actions are resolved without a known type.
        let observed = reported.unwrap_or({
            if style_len >= 2 {
                CallPhase::Ringing
            } else if style_len == 1 {
                CallPhase::Active
            } else {
                CallPhase::Ringing
            }
        });
        self.next_seq += 1;
        self.calls.insert(
            key.to_owned(),
            Call {
                key: key.to_owned(),
                service,
                name,
                detail,
                app,
                video,
                observed,
                answer,
                decline,
                hang_up,
                active_since: (observed == CallPhase::Active).then_some(now),
                error: None,
                pending: None,
                removal_deadline: None,
                seq: self.next_seq,
            },
        );
        true
    }

    /// Starts a pending Answer/End and returns the action to send. `key` defaults to the published
    /// call. The phase is published immediately by the caller.
    pub fn command(
        &mut self,
        command: CallCommand,
        key: Option<&str>,
        now: Instant,
    ) -> Result<CallAction, String> {
        let key = match key.filter(|k| !k.is_empty()) {
            Some(key) => {
                if !self.calls.contains_key(key) {
                    return Err("unknown call".into());
                }
                key.to_owned()
            }
            None => self.primary_key().ok_or("no call")?,
        };
        let call = self.calls.get_mut(&key).expect("checked above");
        if call.pending.is_some() {
            return Err("the previous action is still in progress".into());
        }
        let (phase, action_id) = match command {
            CallCommand::Answer => {
                if call.observed != CallPhase::Ringing {
                    return Err("the call is not ringing".into());
                }
                let action = call
                    .answer
                    .as_ref()
                    .ok_or("the phone did not offer an Answer action")?;
                (CallPhase::Answering, action.id)
            }
            CallCommand::End => match call.observed {
                CallPhase::Ringing => {
                    let action = call
                        .decline
                        .as_ref()
                        .ok_or("the phone did not offer a Decline action")?;
                    (CallPhase::Ending, action.id)
                }
                CallPhase::Active => {
                    let action = call
                        .hang_up
                        .as_ref()
                        .ok_or("the phone did not offer a Hang Up action")?;
                    (CallPhase::Ending, action.id)
                }
                _ => return Err("the call is not ringing or active".into()),
            },
        };
        call.pending = Some(Pending {
            phase,
            command,
            deadline: now + PENDING_TIMEOUT,
        });
        call.error = None;
        Ok(CallAction {
            service: call.service,
            key: call.key.clone(),
            action_id,
        })
    }

    /// Fires due removal graces and pending timeouts. Returns whether anything changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        let mut remove = Vec::new();
        for (key, call) in &mut self.calls {
            if call.removal_deadline.is_some_and(|d| now >= d) {
                remove.push(key.clone());
                changed = true;
                continue;
            }
            if let Some(pending) = call.pending
                && now >= pending.deadline
            {
                call.pending = None;
                call.error = Some(match pending.command {
                    CallCommand::Answer => {
                        "the answer could not be confirmed (the call may still have connected)"
                            .into()
                    }
                    CallCommand::End => "the end could not be confirmed".into(),
                });
                changed = true;
            }
        }
        for key in remove {
            self.calls.remove(&key);
        }
        changed
    }

    pub fn knows(&self, key: &str) -> bool {
        self.calls.contains_key(key)
    }

    /// Clears all tracked calls, e.g. when the phone disconnects.
    pub fn clear(&mut self) {
        self.calls.clear();
    }

    /// The key of the call published as `call` (the Active one, else the newest Ringing one).
    pub fn primary_key(&self) -> Option<String> {
        self.ranked().first().map(|c| c.key.clone())
    }

    /// The two published objects: `call` first, then `waiting`.
    pub fn snapshot(&self, now: Instant) -> (Option<CallStatus>, Option<CallStatus>) {
        let ranked = self.ranked();
        (
            ranked.first().map(|c| c.status(now)),
            ranked.get(1).map(|c| c.status(now)),
        )
    }

    fn ranked(&self) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self.calls.values().collect();
        calls.sort_by_key(|c| (rank(c), std::cmp::Reverse(c.seq)));
        calls
    }
}

/// Active calls rank first, then a call being answered, then everything else.
fn rank(call: &Call) -> u8 {
    match call.observed {
        CallPhase::Active => 0,
        _ if call
            .pending
            .is_some_and(|p| p.phase == CallPhase::Answering) =>
        {
            1
        }
        _ => 2,
    }
}

/// Resolves the semantic call actions from `data.callStyleActions`, which holds indices into the
/// frame's `actions` array (INFERRED from Android's CallStyle contract):
///
/// - ringing: the first index is Decline, the last is Answer;
/// - active: the first index is Hang Up.
///
/// `action_name` is only the label. An index that is missing, out of range, duplicated or does not
/// point at an entry with an integer `action_id` leaves that action `None`, so the front-end can
/// disable the button.
fn resolve_actions(
    call_type: Option<i64>,
    actions: &[Value],
    data: &Value,
) -> (
    Option<ResolvedAction>,
    Option<ResolvedAction>,
    Option<ResolvedAction>,
) {
    let style: Vec<Option<i64>> = data
        .get("callStyleActions")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::as_i64).collect())
        .unwrap_or_default();
    let at = |index: i64, fallback: &str| -> Option<ResolvedAction> {
        let index = usize::try_from(index).ok()?;
        let action = actions.get(index)?;
        let id = action.get("action_id")?.as_i64()?;
        let label = action
            .get("action_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
            .to_owned();
        Some(ResolvedAction { id, label })
    };
    match call_type {
        Some(CALL_TYPE_RINGING) if style.len() >= 2 => {
            let first = style[0];
            let last = style[style.len() - 1];
            if first == last {
                return (None, None, None);
            }
            (
                last.and_then(|i| at(i, "Answer")),
                first.and_then(|i| at(i, "Decline")),
                None,
            )
        }
        Some(CALL_TYPE_ACTIVE) => (
            None,
            None,
            style.first().and_then(|i| i.and_then(|i| at(i, "Hang Up"))),
        ),
        _ => (None, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(event: i64, call_type: Option<i64>, key: &str, actions: &[(i64, &str)]) -> Value {
        let style: Vec<i64> = match call_type {
            Some(CALL_TYPE_RINGING) => (0..actions.len() as i64).collect(),
            Some(CALL_TYPE_ACTIVE) => vec![0],
            _ => Vec::new(),
        };
        let actions: Vec<Value> = actions
            .iter()
            .map(|(id, name)| json!({ "action_id": id, "action_name": name }))
            .collect();
        let mut data = json!({
            "title": "<Caller>",
            "person_name": "<Caller>",
            "content": "Incoming call",
            "apk_name": "Phone",
            "callStyleActions": style,
        });
        if let Some(call_type) = call_type {
            data["callType"] = json!(call_type);
        }
        json!({ "key": key, "event": event, "type": 6, "actions": actions, "data": data })
    }

    fn with_style(mut frame: Value, style: &[i64]) -> Value {
        frame["data"]["callStyleActions"] = json!(style);
        frame
    }

    fn removal(key: &str) -> Value {
        json!({ "key": key, "event": 2 })
    }

    fn ringing(key: &str) -> Value {
        frame(0, Some(1), key, &[(0, "Decline"), (1, "Answer")])
    }

    fn active(key: &str) -> Value {
        frame(
            1,
            Some(2),
            key,
            &[(0, "Hang Up"), (1, "Speaker"), (2, "Mute")],
        )
    }

    #[test]
    fn ringing_active_then_removal_after_the_grace() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        assert!(tracker.apply(5, &ringing("k"), t0));
        let (call, waiting) = tracker.snapshot(t0);
        assert_eq!(call.unwrap().phase, CallPhase::Ringing);
        assert!(waiting.is_none());

        assert!(tracker.apply(5, &active("k"), t0 + Duration::from_secs(2)));
        let (call, _) = tracker.snapshot(t0 + Duration::from_secs(5));
        let call = call.unwrap();
        assert_eq!(call.phase, CallPhase::Active);
        assert_eq!(call.elapsed, 3);

        assert!(!tracker.apply(5, &removal("k"), t0 + Duration::from_secs(6)));
        assert!(!tracker.tick(t0 + Duration::from_secs(6) + Duration::from_millis(999)));
        assert!(tracker.snapshot(t0 + Duration::from_secs(7)).0.is_some());
        assert!(tracker.tick(t0 + Duration::from_secs(7)));
        assert!(tracker.snapshot(t0 + Duration::from_secs(7)).0.is_none());
    }

    #[test]
    fn a_repost_cancels_the_removal_grace() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.apply(5, &removal("k"), t0 + Duration::from_millis(400));
        tracker.apply(5, &removal("k"), t0 + Duration::from_millis(401));
        tracker.apply(5, &ringing("k"), t0 + Duration::from_millis(550));
        assert!(!tracker.tick(t0 + Duration::from_secs(2)));
        assert!(tracker.snapshot(t0 + Duration::from_secs(2)).0.is_some());
    }

    #[test]
    fn answer_uses_the_answer_id_not_the_first_action() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        let action = tracker.command(CallCommand::Answer, None, t0).unwrap();
        assert_eq!(action.action_id, 1);
        assert_eq!(action.service, 5);
        assert_eq!(tracker.snapshot(t0).0.unwrap().phase, CallPhase::Answering);
        assert!(tracker.command(CallCommand::End, None, t0).is_err());
    }

    #[test]
    fn decline_and_hang_up_use_the_end_action() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        assert_eq!(
            tracker
                .command(CallCommand::End, None, t0)
                .unwrap()
                .action_id,
            0
        );
        assert_eq!(tracker.snapshot(t0).0.unwrap().phase, CallPhase::Ending);

        let mut tracker = CallTracker::default();
        tracker.apply(5, &ringing("k"), t0);
        tracker.apply(5, &active("k"), t0 + Duration::from_secs(2));
        assert_eq!(
            tracker
                .command(CallCommand::End, None, t0 + Duration::from_secs(3))
                .unwrap()
                .action_id,
            0
        );
        assert_eq!(
            tracker
                .snapshot(t0 + Duration::from_secs(3))
                .0
                .unwrap()
                .phase,
            CallPhase::Ending
        );
    }

    #[test]
    fn actions_are_resolved_by_position_not_name() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(
            5,
            &frame(0, Some(1), "k", &[(7, "Decline"), (9, "Answer")]),
            t0,
        );
        let call = tracker.snapshot(t0).0.unwrap();
        assert!(call.can_answer && call.can_end);
        assert_eq!(call.answer_label.as_deref(), Some("Answer"));
        assert_eq!(call.end_label.as_deref(), Some("Decline"));
        assert_eq!(
            tracker
                .command(CallCommand::Answer, None, t0)
                .unwrap()
                .action_id,
            9
        );

        let mut tracker = CallTracker::default();
        tracker.apply(
            5,
            &frame(0, Some(1), "k", &[(7, "Decline"), (9, "Answer")]),
            t0,
        );
        assert_eq!(
            tracker
                .command(CallCommand::End, None, t0)
                .unwrap()
                .action_id,
            7
        );
    }

    #[test]
    fn names_are_labels_only() {
        // The labels are swapped on purpose: the first action is Decline, the last is Answer.
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(
            5,
            &frame(0, Some(1), "k", &[(0, "Answer"), (1, "Decline")]),
            t0,
        );
        let call = tracker.snapshot(t0).0.unwrap();
        assert_eq!(call.answer_label.as_deref(), Some("Decline"));
        assert_eq!(call.end_label.as_deref(), Some("Answer"));
        assert_eq!(
            tracker
                .command(CallCommand::Answer, None, t0)
                .unwrap()
                .action_id,
            1
        );

        let mut tracker = CallTracker::default();
        tracker.apply(
            5,
            &frame(0, Some(1), "k", &[(0, "Answer"), (1, "Decline")]),
            t0,
        );
        assert_eq!(
            tracker
                .command(CallCommand::End, None, t0)
                .unwrap()
                .action_id,
            0
        );
    }

    #[test]
    fn an_out_of_range_style_index_disables_that_button_only() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(
            5,
            &with_style(
                frame(0, Some(1), "k", &[(0, "Decline"), (1, "Answer")]),
                &[0, 5],
            ),
            t0,
        );
        let call = tracker.snapshot(t0).0.unwrap();
        assert!(!call.can_answer);
        assert!(call.can_end);
        assert!(call.answer_label.is_none());
        assert_eq!(call.end_label.as_deref(), Some("Decline"));
        assert!(tracker.command(CallCommand::Answer, None, t0).is_err());
        assert_eq!(
            tracker
                .command(CallCommand::End, None, t0)
                .unwrap()
                .action_id,
            0
        );
    }

    #[test]
    fn out_of_range_style_indices_disable_both_buttons() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(
            5,
            &with_style(
                frame(0, Some(1), "k", &[(0, "Decline"), (1, "Answer")]),
                &[5, 6],
            ),
            t0,
        );
        let call = tracker.snapshot(t0).0.unwrap();
        assert!(!call.can_answer && !call.can_end);
        assert!(call.answer_label.is_none() && call.end_label.is_none());
        assert!(tracker.command(CallCommand::Answer, None, t0).is_err());
        assert!(tracker.command(CallCommand::End, None, t0).is_err());
    }

    #[test]
    fn a_missing_or_single_ringing_style_index_disables_the_buttons() {
        for style in [&[][..], &[1][..], &[0, 0][..]] {
            let mut tracker = CallTracker::default();
            let t0 = Instant::now();
            tracker.apply(
                5,
                &with_style(
                    frame(0, Some(1), "k", &[(0, "Decline"), (1, "Answer")]),
                    style,
                ),
                t0,
            );
            let call = tracker.snapshot(t0).0.unwrap();
            assert!(!call.can_answer && !call.can_end, "style {style:?}");
        }
    }

    #[test]
    fn active_uses_the_first_style_index_for_hang_up() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(
            5,
            &with_style(frame(1, Some(2), "k", &[(5, "Hang Up"), (6, "Mute")]), &[0]),
            t0,
        );
        let call = tracker.snapshot(t0).0.unwrap();
        assert!(call.can_end && !call.can_answer);
        assert_eq!(call.end_label.as_deref(), Some("Hang Up"));
        assert_eq!(
            tracker
                .command(CallCommand::End, None, t0)
                .unwrap()
                .action_id,
            5
        );
    }

    #[test]
    fn can_answer_and_can_end_follow_the_phase() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        let call = tracker.snapshot(t0).0.unwrap();
        assert!(call.can_answer && call.can_end);
        assert_eq!(call.end_label.as_deref(), Some("Decline"));

        tracker.command(CallCommand::Answer, None, t0).unwrap();
        let call = tracker.snapshot(t0).0.unwrap();
        assert!(!call.can_answer && !call.can_end);
        assert_eq!(call.phase, CallPhase::Answering);

        tracker.apply(5, &active("k"), t0 + Duration::from_secs(2));
        let call = tracker.snapshot(t0 + Duration::from_secs(2)).0.unwrap();
        assert!(!call.can_answer && call.can_end);
        assert_eq!(call.end_label.as_deref(), Some("Hang Up"));
    }

    #[test]
    fn an_answer_that_never_lands_reverts_with_an_error() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.command(CallCommand::Answer, None, t0).unwrap();
        assert!(!tracker.tick(t0 + Duration::from_secs(9)));
        assert!(tracker.tick(t0 + Duration::from_secs(10)));
        let call = tracker.snapshot(t0 + Duration::from_secs(10)).0.unwrap();
        assert_eq!(call.phase, CallPhase::Ringing);
        let error = call.error.unwrap();
        assert!(error.contains("could not be confirmed"), "{error}");
        assert!(error.contains("may still have connected"), "{error}");
    }

    #[test]
    fn an_end_that_never_lands_reverts_with_its_own_error() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.command(CallCommand::End, None, t0).unwrap();
        assert!(tracker.tick(t0 + PENDING_TIMEOUT));
        let call = tracker.snapshot(t0 + PENDING_TIMEOUT).0.unwrap();
        assert_eq!(call.phase, CallPhase::Ringing);
        assert_eq!(
            call.error.as_deref(),
            Some("the end could not be confirmed")
        );
    }

    #[test]
    fn active_frame_clears_answering() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.command(CallCommand::Answer, None, t0).unwrap();
        tracker.apply(5, &active("k"), t0 + Duration::from_secs(2));
        let call = tracker.snapshot(t0 + Duration::from_secs(2)).0.unwrap();
        assert_eq!(call.phase, CallPhase::Active);
        assert!(call.error.is_none());
    }

    #[test]
    fn call_waiting_publishes_the_active_call_first() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("a"), t0);
        tracker.apply(5, &active("a"), t0 + Duration::from_secs(1));
        tracker.apply(5, &ringing("b"), t0 + Duration::from_secs(2));
        let (call, waiting) = tracker.snapshot(t0 + Duration::from_secs(3));
        assert_eq!(call.unwrap().key, "a");
        assert_eq!(waiting.unwrap().key, "b");

        let action = tracker
            .command(CallCommand::Answer, Some("b"), t0 + Duration::from_secs(3))
            .unwrap();
        assert_eq!(action.key, "b");
        let (call, waiting) = tracker.snapshot(t0 + Duration::from_secs(3));
        assert_eq!(call.unwrap().key, "a");
        assert_eq!(waiting.unwrap().phase, CallPhase::Answering);
    }

    #[test]
    fn a_third_call_is_ignored() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("a"), t0);
        tracker.apply(5, &ringing("b"), t0);
        assert!(!tracker.apply(5, &ringing("c"), t0));
        let (call, waiting) = tracker.snapshot(t0);
        assert!(call.is_some());
        assert!(waiting.is_some());
        assert!(!tracker.knows("c"));
    }

    #[test]
    fn unknown_call_type_infers_the_phase_but_disables_actions() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(
            5,
            &with_style(
                frame(0, None, "a", &[(0, "Decline"), (1, "Answer")]),
                &[0, 1],
            ),
            t0,
        );
        let call = tracker.snapshot(t0).0.unwrap();
        assert_eq!(call.phase, CallPhase::Ringing);
        assert!(!call.can_answer && !call.can_end);
        tracker.apply(
            5,
            &with_style(frame(1, None, "b", &[(0, "Hang Up")]), &[0]),
            t0,
        );
        let (call, _) = tracker.snapshot(t0);
        let call = call.unwrap();
        assert_eq!(call.key, "b");
        assert_eq!(call.phase, CallPhase::Active);
        assert!(!call.can_end);
    }

    #[test]
    fn a_new_ring_after_active_resets_the_pending_action() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.apply(5, &active("k"), t0 + Duration::from_secs(1));
        tracker
            .command(CallCommand::End, None, t0 + Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            tracker
                .snapshot(t0 + Duration::from_secs(2))
                .0
                .unwrap()
                .phase,
            CallPhase::Ending
        );
        // The dialer reuses the key; a new ring is a new call, not the old one still ending.
        tracker.apply(5, &ringing("k"), t0 + Duration::from_secs(3));
        let call = tracker.snapshot(t0 + Duration::from_secs(3)).0.unwrap();
        assert_eq!(call.phase, CallPhase::Ringing);
        assert!(call.can_answer && call.can_end);
        assert!(call.error.is_none());
    }

    #[test]
    fn a_removal_during_a_pending_answer_still_removes_the_call() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.command(CallCommand::Answer, None, t0).unwrap();
        tracker.apply(5, &removal("k"), t0 + Duration::from_secs(1));
        assert!(tracker.tick(t0 + Duration::from_secs(2)));
        assert!(tracker.snapshot(t0 + Duration::from_secs(2)).0.is_none());
    }

    #[test]
    fn a_rejected_second_command_does_not_cancel_the_pending_timeout() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("k"), t0);
        tracker.command(CallCommand::Answer, None, t0).unwrap();
        assert!(tracker.command(CallCommand::End, None, t0).is_err());
        assert!(tracker.tick(t0 + PENDING_TIMEOUT));
        let call = tracker.snapshot(t0 + PENDING_TIMEOUT).0.unwrap();
        assert_eq!(call.phase, CallPhase::Ringing);
        assert!(call.error.is_some());
    }

    #[test]
    fn two_pending_calls_time_out_independently() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("a"), t0);
        tracker.apply(5, &ringing("b"), t0);
        tracker.command(CallCommand::Answer, Some("a"), t0).unwrap();
        tracker.command(CallCommand::Answer, Some("b"), t0).unwrap();
        assert!(tracker.tick(t0 + PENDING_TIMEOUT));
        let (call, waiting) = tracker.snapshot(t0 + PENDING_TIMEOUT);
        assert!(call.unwrap().error.is_some());
        assert!(waiting.unwrap().error.is_some());
    }

    #[test]
    fn commands_target_the_published_call_by_default() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("a"), t0);
        tracker.apply(5, &ringing("b"), t0);
        let action = tracker.command(CallCommand::Answer, None, t0).unwrap();
        assert_eq!(action.key, "b", "the newest ringing call is published");
    }

    #[test]
    fn clear_empties_all_calls() {
        let mut tracker = CallTracker::default();
        let t0 = Instant::now();
        tracker.apply(5, &ringing("a"), t0);
        assert!(tracker.knows("a"));
        tracker.clear();
        assert!(!tracker.knows("a"));
        let (call, waiting) = tracker.snapshot(t0);
        assert!(call.is_none());
        assert!(waiting.is_none());
    }
}
