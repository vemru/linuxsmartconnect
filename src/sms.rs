//! SMS/MMS ("Messages") over phone service 204. Spec: `evidence-2026-10-20/sms-spec.md`.
//!
//! Every frame on service 204 carries a phone-side `MessageData`:
//! `{"ver":"5.0","cmd":N,"pId":110,"data":…}`. There is no thread concept on the wire: the phone
//! sends flat `MessageBean`s (last 10 days) and the PC groups them, here by counterpart address.
//! The phone only answers data requests once this PC is "enabled" for Messages (svc 100 `cmd 1018`
//! `enabled:true`); `cmd 18` (PC messages ready) enables it the first time a PC sends it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::server::Daemon;

pub const SERVICE_MESSAGE: i64 = 204;
/// Protocol id the phone checks (`pId != 110` → "not message related cmd").
pub const PID: i64 = 110;
/// Above 3.0 the phone wraps `cmd 11` in a `MessageSyncBean` and sends MMS without media data.
pub const VER: &str = "5.0";

/// Message kind (`type` in requests/beans).
pub const KIND_SMS: i64 = 1;
pub const KIND_MMS: i64 = 2;
/// `MessageBean.messageType`: 1 = received, 2 = sent.
pub const MSG_RECEIVED: i64 = 1;
pub const MSG_SENT: i64 = 2;

pub mod cmd {
    pub const GET_CONTACTS: i64 = 1;
    pub const CONTACTS: i64 = 2;
    pub const CONTACTS_CHANGED: i64 = 3;
    pub const SEND: i64 = 4;
    pub const SEND_ACCEPTED: i64 = 5;
    pub const SEND_RESULT: i64 = 6;
    pub const MESSAGES_CHANGED: i64 = 7;
    pub const SET_STATE: i64 = 8;
    pub const GET_MESSAGES: i64 = 10;
    pub const MESSAGES: i64 = 11;
    pub const GET_PHONE_NUMBER: i64 = 12;
    pub const PHONE_NUMBER: i64 = 13;
    pub const MESSAGES_DELETED: i64 = 14;
    pub const PERMISSION_REQUEST: i64 = 16;
    pub const PERMISSION: i64 = 17;
    pub const PC_READY: i64 = 18;
    pub const QUERY_PERMISSION: i64 = 20;
    pub const PERMISSION_STATE: i64 = 21;
    pub const GET_MMS_PARTS: i64 = 22;
    pub const MMS_PARTS: i64 = 23;
    pub const GET_CONTACTS_BY_ID: i64 = 24;
    pub const CONTACTS_BY_ID: i64 = 25;
    pub const CHECK_SEND: i64 = 26;
    pub const SEND_AVAILABILITY: i64 = 27;
    pub const GET_MESSAGES_PLUS: i64 = 28;
    pub const MESSAGES_PLUS: i64 = 29;
    pub const SETTINGS_REQUEST: i64 = 1016;
    /// Unsolicited status, arrives on svc 100 from srcServiceId 204.
    pub const USABLE_STATUS: i64 = 1018;
}

/// Wraps a payload in the phone's `MessageData` envelope.
pub fn request(cmd: i64, data: Option<Value>) -> Value {
    let mut v = json!({ "ver": VER, "cmd": cmd, "pId": PID });
    if let Some(d) = data {
        v["data"] = d;
    }
    v
}

/// All SMS (or MMS) of the last 10 days, in batches of 50 (`cmd 29`).
pub fn request_messages(kind: i64) -> Value {
    request(cmd::GET_MESSAGES_PLUS, Some(json!({ "type": kind, "count": 0 })))
}

/// Up to `count` newest messages of one kind in a single reply (`cmd 11`).
pub fn request_messages_limited(kind: i64, count: i64) -> Value {
    request(cmd::GET_MESSAGES, Some(json!({ "type": kind, "count": count })))
}

pub fn request_contacts() -> Value {
    request(cmd::GET_CONTACTS, None)
}

/// Tells the phone the PC's Messages UI is up; the first time per PC this enables Messages for it.
pub fn pc_ready() -> Value {
    request(cmd::PC_READY, None)
}

/// `MessageBean` for `cmd 4`. `medias` must be present: the phone iterates it unconditionally.
/// `sub_id` -1 = default SIM. The phone acks with `cmd 5 {requestId}`, then `cmd 6` with the result.
pub fn send_sms(request_id: i64, to: &[String], body: &str, sub_id: i64) -> Value {
    request(
        cmd::SEND,
        Some(json!({
            "requestId": request_id,
            "msgId": request_id,
            "subId": sub_id,
            "messageType": MSG_SENT,
            "toAddress": to,
            "body": body,
            "medias": [],
            "read": 1,
            "date": now_ms(),
        })),
    )
}

/// `cmd 8`: status 1 = read, 0 = unread. `kind` 1 = SMS, 2 = MMS.
pub fn mark_read(msg_id: i64, kind: i64, read: bool) -> Value {
    request(cmd::SET_STATE, Some(json!({ "msgId": msg_id, "status": i64::from(read), "type": kind })))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// One message as the PC keeps it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Msg {
    pub id: i64,
    pub kind: i64,
    /// 1 received, 2 sent.
    pub message_type: i64,
    pub from: String,
    pub to: Vec<String>,
    pub body: String,
    pub date: i64,
    pub read: bool,
    pub sub_id: i64,
}

impl Msg {
    pub fn parse(kind: i64, b: &Value) -> Option<Self> {
        let int = |k: &str| b.get(k).and_then(Value::as_i64);
        let text = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let to = b
            .get("toAddress")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        Some(Self {
            id: int("msgId")?,
            kind,
            message_type: int("messageType").unwrap_or(MSG_RECEIVED),
            from: text("fromAddress"),
            to,
            body: text("body"),
            date: int("date").unwrap_or_default(),
            read: int("read").unwrap_or(1) != 0,
            sub_id: int("subId").unwrap_or(-1),
        })
    }

    /// The other party: sender of a received message, first recipient of a sent one.
    pub fn counterpart(&self) -> &str {
        if self.message_type == MSG_SENT { self.to.first().map(String::as_str).unwrap_or("") } else { &self.from }
    }

    pub fn thread(&self) -> String {
        thread_key(self.counterpart())
    }
}

/// Conversation key: digits (with a leading `+` kept), last 10 digits for numbers so `+91…` and
/// local forms match; alphanumeric senders (e.g. "VM-BANK") are kept lowercased.
pub fn thread_key(addr: &str) -> String {
    let digits: String = addr.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 6 && addr.chars().all(|c| c.is_ascii_digit() || " +-()".contains(c)) {
        digits[digits.len().saturating_sub(10)..].to_owned()
    } else {
        addr.trim().to_lowercase()
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct SmsState {
    /// Latest usable status (`cmd 1018/17/21`): ver, enabled, granted, triggered, unreadCnt…
    pub usable: Option<Value>,
    /// Keyed by (kind, msgId).
    #[serde(skip)]
    pub messages: BTreeMap<(i64, i64), Msg>,
    /// thread key → contact name (from `cmd 2/3/25`).
    pub names: BTreeMap<String, String>,
    pub sims: Option<Value>,
    pub send_availability: Option<Value>,
    /// Recent `cmd 5`/`cmd 6` replies, newest first.
    pub send_results: Vec<Value>,
}

impl SmsState {
    pub fn enabled(&self) -> bool {
        self.usable.as_ref().and_then(|u| u.get("enabled")).and_then(Value::as_bool) == Some(true)
    }

    /// Folds one phone frame in. Returns true if it was a Messages frame.
    pub fn apply(&mut self, service_id: i64, d: &Value) -> bool {
        let c = d.get("cmd").and_then(Value::as_i64);
        if service_id == 100 && c == Some(cmd::USABLE_STATUS) {
            self.usable = Some(d.clone());
            return true;
        }
        if d.get("pId").and_then(Value::as_i64) != Some(PID) && service_id != SERVICE_MESSAGE {
            return false;
        }
        let data = d.get("data").cloned().unwrap_or(Value::Null);
        let kind_of = |v: &Value| v.get("type").and_then(Value::as_i64).unwrap_or(KIND_SMS);
        match c {
            Some(cmd::PERMISSION | cmd::PERMISSION_STATE) => self.usable = Some(data),
            // Older phones send a bare array; ver > 3 wraps it as MessageSyncBean{action:4,…}.
            Some(cmd::MESSAGES) => match &data {
                Value::Array(list) => self.upsert(KIND_SMS, list, true),
                v => self.upsert(kind_of(v), bean_list(v), true),
            },
            Some(cmd::MESSAGES_PLUS) => {
                let first = data.get("currentBatch").and_then(Value::as_i64).unwrap_or(1) <= 1;
                self.upsert(kind_of(&data), bean_list(&data), first);
            }
            Some(cmd::MESSAGES_CHANGED) => self.upsert(kind_of(&data), bean_list(&data), false),
            Some(cmd::MMS_PARTS) => {
                if let Value::Array(list) = &data {
                    self.upsert(KIND_MMS, list, false);
                }
            }
            Some(cmd::MESSAGES_DELETED) => {
                let kind = kind_of(&data);
                for id in data.get("deleteList").and_then(Value::as_array).into_iter().flatten() {
                    let id = id.as_i64().or_else(|| id.as_str().and_then(|s| s.parse().ok()));
                    if let Some(id) = id {
                        self.messages.remove(&(kind, id));
                    }
                }
            }
            Some(cmd::CONTACTS | cmd::CONTACTS_BY_ID) => self.add_contacts(&data),
            Some(cmd::CONTACTS_CHANGED) => self.add_contacts(data.get("contactBeanList").unwrap_or(&Value::Null)),
            Some(cmd::PHONE_NUMBER) => self.sims = Some(data),
            Some(cmd::SEND_AVAILABILITY) => self.send_availability = Some(data),
            Some(cmd::SEND_ACCEPTED | cmd::SEND_RESULT) => {
                self.send_results.insert(0, json!({ "cmd": c, "data": data }));
                self.send_results.truncate(20);
            }
            _ => {}
        }
        true
    }

    fn upsert(&mut self, kind: i64, list: &[Value], replace: bool) {
        if replace {
            self.messages.retain(|(k, _), _| *k != kind);
        }
        for m in list.iter().filter_map(|b| Msg::parse(kind, b)) {
            self.messages.insert((kind, m.id), m);
        }
    }

    fn add_contacts(&mut self, list: &Value) {
        for c in list.as_array().into_iter().flatten() {
            let Some(name) = c.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) else { continue };
            let phones = match c.get("phone") {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                Some(Value::String(s)) => vec![s.as_str()],
                _ => vec![],
            };
            for p in phones {
                self.names.insert(thread_key(p), name.to_owned());
            }
        }
    }

    /// Conversations, newest first.
    pub fn threads(&self) -> Vec<Value> {
        let mut by: BTreeMap<String, Vec<&Msg>> = BTreeMap::new();
        for m in self.messages.values() {
            by.entry(m.thread()).or_default().push(m);
        }
        let mut out: Vec<(i64, Value)> = by
            .into_iter()
            .map(|(key, msgs)| {
                let last = msgs.iter().max_by_key(|m| m.date).expect("non-empty group");
                let unread = msgs.iter().filter(|m| !m.read && m.message_type == MSG_RECEIVED).count();
                let v = json!({
                    "thread": key,
                    "address": last.counterpart(),
                    "name": self.names.get(&key),
                    "count": msgs.len(),
                    "unread": unread,
                    "last_body": last.body,
                    "last_date": last.date,
                });
                (last.date, v)
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out.into_iter().map(|(_, v)| v).collect()
    }

    /// One conversation, oldest first.
    pub fn thread(&self, key: &str) -> Vec<&Msg> {
        let key = thread_key(key);
        let mut v: Vec<&Msg> = self.messages.values().filter(|m| m.thread() == key).collect();
        v.sort_by_key(|m| m.date);
        v
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "usable": self.usable,
            "enabled": self.enabled(),
            "message_count": self.messages.len(),
            "threads": self.threads(),
            "sims": self.sims,
            "send_availability": self.send_availability,
            "send_results": self.send_results,
        })
    }
}

fn bean_list(v: &Value) -> &[Value] {
    v.get("messageBeanList").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// The daemon-wide store (the session loop only broadcasts frames; this keeps the latest data).
pub fn store() -> &'static Mutex<SmsState> {
    static STORE: OnceLock<Mutex<SmsState>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// Set by a sync request while Messages isn't enabled yet: re-request once the phone says enabled.
static PENDING_SYNC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Requests a full SMS sync (and contacts/SIM info), first telling the phone the PC is ready.
pub fn request_sync(daemon: &Daemon) {
    let enabled = store().lock().unwrap().enabled();
    let send = |d: Value| {
        let _ = daemon.to_phone.send((SERVICE_MESSAGE, d));
    };
    if !enabled {
        PENDING_SYNC.store(true, std::sync::atomic::Ordering::SeqCst);
        send(pc_ready());
    }
    send(request(cmd::GET_PHONE_NUMBER, None));
    send(request_contacts());
    send(request_messages(KIND_SMS));
}

/// Keeps `store()` up to date from `daemon.from_phone`. Called once (from the control server).
pub fn spawn_collector(daemon: Arc<Daemon>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let mut rx = daemon.from_phone.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok((svc, data)) => {
                    let became_enabled = {
                        let mut st = store().lock().unwrap();
                        let was = st.enabled();
                        st.apply(svc, &data) && !was && st.enabled()
                    };
                    if became_enabled && PENDING_SYNC.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        request_sync(&daemon);
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!(n, "sms: skipped phone frames"),
                Err(RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_shapes() {
        assert_eq!(request_messages(1), json!({"ver":"5.0","cmd":28,"pId":110,"data":{"type":1,"count":0}}));
        assert_eq!(request_messages_limited(1, 50)["data"], json!({"type":1,"count":50}));
        assert_eq!(pc_ready(), json!({"ver":"5.0","cmd":18,"pId":110}));
        assert_eq!(mark_read(42, 1, true), json!({"ver":"5.0","cmd":8,"pId":110,"data":{"msgId":42,"status":1,"type":1}}));
        let s = send_sms(7, &["+911234567890".into()], "hi", -1);
        assert_eq!(s["cmd"], 4);
        let d = &s["data"];
        assert_eq!(d["requestId"], 7);
        assert_eq!(d["toAddress"], json!(["+911234567890"]));
        assert_eq!(d["body"], "hi");
        assert_eq!(d["medias"], json!([]));
        assert_eq!(d["subId"], -1);
        assert_eq!(d["messageType"], 2);
    }

    fn bean(id: i64, mt: i64, from: &str, to: &str, body: &str, date: i64, read: i64) -> Value {
        json!({"msgId":id,"messageType":mt,"fromAddress":from,"toAddress":[to],"body":body,"date":date,"read":read,"subId":1})
    }

    #[test]
    fn sync_threads_and_updates() {
        let mut st = SmsState::default();
        assert!(st.apply(100, &json!({"cmd":1018,"ver":"5.0","enabled":false,"granted":true,"unreadCnt":1})));
        assert!(!st.enabled());
        let batch1 = json!({"ver":"5.0","cmd":29,"pId":110,"data":{"action":4,"type":1,"totalCount":3,"currentBatch":1,"totalBatches":1,
            "messageBeanList":[
                bean(1,1,"+1 555 555 0123","+910000000000","hello",1000,1),
                bean(2,2,"+910000000000","5555550123","hey back",2000,1),
                bean(3,1,"VM-BANK","+910000000000","otp 1234",3000,0)]}});
        assert!(st.apply(204, &batch1));
        let threads = st.threads();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0]["thread"], "vm-bank");
        assert_eq!(threads[0]["unread"], 1);
        assert_eq!(threads[1]["thread"], "5555550123");
        assert_eq!(threads[1]["count"], 2);
        assert_eq!(threads[1]["last_body"], "hey back");
        let conv = st.thread("+15555550123");
        assert_eq!(conv.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2]);

        // Incremental add (cmd 7 action 1), then delete (cmd 14).
        st.apply(204, &json!({"ver":"5.0","cmd":7,"pId":110,"data":{"action":1,"type":1,"messageBeanList":[bean(4,1,"5555550123","x","new",4000,0)]}}));
        assert_eq!(st.thread("5555550123").len(), 3);
        st.apply(204, &json!({"ver":"5.0","cmd":14,"pId":110,"data":{"type":1,"deleteList":["3"]}}));
        assert_eq!(st.threads().len(), 1);

        // Contacts name the thread; send results are kept.
        st.apply(204, &json!({"ver":"5.0","cmd":2,"pId":110,"data":[{"id":5,"name":"Mum","phone":["+1 555-555-0123"]}]}));
        assert_eq!(st.threads()[0]["name"], "Mum");
        st.apply(204, &json!({"ver":"5.0","cmd":6,"pId":110,"data":{"requestId":7,"msgId":7,"type":1,"status":0,"code":0}}));
        assert_eq!(st.send_results[0]["data"]["requestId"], 7);

        // Usable status flip.
        st.apply(204, &json!({"ver":"5.0","cmd":21,"pId":110,"data":{"granted":true,"enabled":true,"triggered":true}}));
        assert!(st.enabled());
        // Unrelated frames are ignored.
        assert!(!st.apply(25, &json!({"cmd":1,"battery":50})));
    }

    #[test]
    fn legacy_bare_list_and_full_resync_replace() {
        let mut st = SmsState::default();
        st.apply(204, &json!({"cmd":11,"pId":110,"data":[bean(1,1,"123456789","me","a",1,1)]}));
        assert_eq!(st.messages.len(), 1);
        st.apply(204, &json!({"cmd":11,"pId":110,"data":{"action":4,"type":1,"messageBeanList":[bean(9,1,"123456789","me","b",2,1)]}}));
        assert_eq!(st.messages.keys().copied().collect::<Vec<_>>(), vec![(1, 9)]);
    }
}
