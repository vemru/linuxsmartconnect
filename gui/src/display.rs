//! Display page: the phone as a screen for this PC, through `smartconnect-display` (svc 202).
//!
//! *Extend* makes the phone a second monitor (a vkms `Virtual-*` output, or `--output NAME`);
//! *Duplicate* (`--mode mirror`) shows this PC's screen on the phone, the opposite direction of the
//! Phone page's Mirror Screen.
//!
//! A session runs detached from the GUI. A small `sh` wrapper writes `pid N` and then the helper's
//! stdout/stderr to `$XDG_RUNTIME_DIR/smartconnect-display.log`; the Noctalia plugin starts it the
//! same way. The page reads the state from:
//! - `/proc`: every running `smartconnect-display`, ours or not, with its arguments, so a session
//!   started elsewhere shows up and *Apply* knows what the running one uses;
//! - the log: progress (`peer connection: Connected`, `stream size → WxH`) and why the session ended
//!   (`stopping: <reason>`, `Error: …`, `svc 202: …`).
//!
//! The choices also go to the shared `display.json` (see `prefs.rs`), which the helper reads for a
//! start from the phone. Flags are still passed, but only those the installed helper lists in
//! `--help`, so an older build keeps working.
//!
//! Stop is SIGINT: the helper tells the phone to stop and switches an extend output off. The output
//! goes to a file, not a pipe: the helper prints on every step, and a write to a pipe whose reader is
//! gone (GUI closed) would panic and abort it before that clean-up.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::Value;

use crate::mirror::binary;
use crate::prefs::{self, DisplayPrefs, Prefs};

const BIN: &str = "smartconnect-display";
/// Session and log poll period while the page is shown or a session is running.
const POLL: Duration = Duration::from_millis(500);
/// Output list refresh period while the page is shown (notices `modprobe vkms` or a dummy plug).
const OUTPUTS_EVERY: Duration = Duration::from_secs(5);
/// How long a session gets to exit after SIGINT.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause between the old session's exit and the new one, so the phone has closed its display view.
const RESTART_GAP: Duration = Duration::from_millis(1500);
/// How long a just-spawned session may take to appear in /proc.
const START_GRACE: Duration = Duration::from_secs(5);
/// Display scale choices in percent; 200 is the helper's default.
const SCALES: [u32; 7] = [100, 125, 150, 175, 200, 250, 300];
/// Resolution choices, in the order of the Resolution row.
const RESOLUTIONS: [&str; 3] = ["native", "phone", "custom"];
/// The wrapper: log file as stdout/stderr, a `pid N` header, then the helper in the same process.
const WRAPPER: &str = r#"exec >"$1" 2>&1; echo "pid $$"; shift; exec "$@""#;
/// Optional flags, passed only when the installed helper lists them in `--help`.
const OSK_FLAG: &str = "--osk-height";
const AUDIO_FLAG: &str = "--audio";
const NO_AUDIO_FLAG: &str = "--no-audio";
const ROTATE_FLAG: &str = "--follow-rotation";
const NO_ROTATE_FLAG: &str = "--no-follow-rotation";
/// A helper that has this flag reads `display.json` for whatever its command line leaves out.
const NO_CONFIG_FLAG: &str = "--no-config";

// ── Session options ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Extend,
    Mirror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Size {
    /// What the phone asks for (no `--size`), usually 1920x1080.
    Phone,
    Native,
    Fixed(u32, u32),
}

/// What a session runs with: built from the page, or read back from a running process.
#[derive(Clone, Debug, PartialEq)]
struct Opts {
    mode: Mode,
    size: Size,
    /// Extend only: `--display-scale` in percent.
    scale: u32,
    /// Extend only: `--output`.
    output: Option<String>,
    /// `--osk-height`. For a running session, `None` means not on its command line (the helper's
    /// default or `display.json` decides), which matches any choice.
    osk: Option<u32>,
    /// Experimental switches, `None` like `osk`.
    audio: Option<bool>,
    rotate: Option<bool>,
    /// Arguments the page never sets (e.g. `--test-pattern`); they make a session differ.
    extra: Vec<String>,
}

impl Opts {
    fn from_prefs(p: &DisplayPrefs) -> Self {
        let mode = if p.mode == "mirror" { Mode::Mirror } else { Mode::Extend };
        let size = match p.resolution() {
            "native" => Size::Native,
            "custom" => Size::Fixed(p.custom_width, p.custom_height),
            _ => Size::Phone,
        };
        Self {
            mode,
            size,
            scale: p.scale,
            output: Some(p.output.clone()).filter(|o| !o.is_empty()),
            osk: Some(p.osk_height),
            audio: Some(p.audio),
            rotate: Some(p.follow_rotation),
            extra: Vec::new(),
        }
        .normalized()
    }

    /// What a bare `smartconnect-display` does without a settings file (`--no-config`, or an older
    /// helper): duplicate at the phone's size. Unknown keyboard/experimental choices match anything.
    fn cli_defaults() -> Self {
        Self { mode: Mode::Mirror, size: Size::Phone, scale: 200, output: None, osk: None, audio: None, rotate: None, extra: Vec::new() }
    }

    /// Reads the arguments of a running `smartconnect-display` on top of `base`: what it uses for
    /// whatever its command line leaves out (`display.json` for a newer helper, else its defaults).
    fn parse(args: &[String], base: Opts) -> Self {
        let mut o = Self { extra: Vec::new(), ..base };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let flag = arg.as_str();
            match flag {
                AUDIO_FLAG => o.audio = Some(true),
                NO_AUDIO_FLAG => o.audio = Some(false),
                ROTATE_FLAG => o.rotate = Some(true),
                NO_ROTATE_FLAG => o.rotate = Some(false),
                // How it was started, not what it shows.
                "--replace" | NO_CONFIG_FLAG => {}
                "--config" => {
                    it.next();
                }
                "--mode" | "--size" | "--display-scale" | "--output" | OSK_FLAG => {
                    let value = it.next().cloned().unwrap_or_default();
                    if !o.set(flag, &value) {
                        o.extra.extend([arg.clone(), value]);
                    }
                }
                _ => o.extra.push(arg.clone()),
            }
        }
        o.normalized()
    }

    /// Applies one `--flag value` pair; false when the value is not understood.
    fn set(&mut self, flag: &str, value: &str) -> bool {
        match (flag, value) {
            ("--mode", "extend") => self.mode = Mode::Extend,
            ("--mode", "mirror") => self.mode = Mode::Mirror,
            ("--size", "native") => self.size = Size::Native,
            ("--size", "requested") => self.size = Size::Phone,
            ("--size", v) => match parse_size(v) {
                Some((w, h)) => self.size = Size::Fixed(w, h),
                None => return false,
            },
            ("--display-scale", v) => match parse_scale(v) {
                Some(s) => self.scale = s,
                None => return false,
            },
            ("--output", "auto") => self.output = None,
            ("--output", v) => self.output = Some(v.to_owned()),
            (OSK_FLAG, v) => match v.trim_end_matches('%').parse() {
                Ok(h) => self.osk = Some(h),
                Err(_) => return false,
            },
            _ => return false,
        }
        true
    }

    /// Scale and output only matter for extend.
    fn normalized(mut self) -> Self {
        if self.mode == Mode::Mirror {
            self.scale = 200;
            self.output = None;
        }
        self
    }

    /// Whether a session running with `self` already does what `want` asks for.
    fn same_as(&self, want: &Self) -> bool {
        fn either<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            a.is_none() || b.is_none() || a == b
        }
        self.mode == want.mode
            && self.size == want.size
            && self.scale == want.scale
            && self.output == want.output
            && self.extra == want.extra
            && either(&self.osk, &want.osk)
            && either(&self.audio, &want.audio)
            && either(&self.rotate, &want.rotate)
    }

    /// Command line for the helper `f` describes: optional flags only when it lists them. A helper
    /// that reads `display.json` gets every choice spelled out, so its command line alone says what
    /// it shows (also to a later run of this app, or to the Noctalia plugin).
    fn args(&self, f: &Flags) -> Vec<String> {
        let explicit = f.has(NO_CONFIG_FLAG);
        let mode = match self.mode {
            Mode::Extend => "extend",
            Mode::Mirror => "mirror",
        };
        let mut a = vec!["--mode".to_owned(), mode.to_owned()];
        match self.size {
            Size::Phone if explicit => a.extend(["--size".to_owned(), "requested".to_owned()]),
            Size::Phone => {}
            Size::Native => a.extend(["--size".to_owned(), "native".to_owned()]),
            Size::Fixed(w, h) => a.extend(["--size".to_owned(), format!("{w}x{h}")]),
        }
        if self.mode == Mode::Extend {
            a.extend(["--display-scale".to_owned(), (f64::from(self.scale) / 100.0).to_string()]);
            match &self.output {
                Some(o) => a.extend(["--output".to_owned(), o.clone()]),
                None if explicit => a.extend(["--output".to_owned(), "auto".to_owned()]),
                None => {}
            }
        }
        if let Some(h) = self.osk.filter(|_| f.has(OSK_FLAG)) {
            a.extend([OSK_FLAG.to_owned(), h.to_string()]);
        }
        for (value, on, off) in [(self.audio, AUDIO_FLAG, NO_AUDIO_FLAG), (self.rotate, ROTATE_FLAG, NO_ROTATE_FLAG)] {
            match value {
                Some(true) if f.has(on) => a.push(on.to_owned()),
                Some(false) if f.has(off) => a.push(off.to_owned()),
                _ => {}
            }
        }
        a.extend(self.extra.iter().cloned());
        a
    }

    /// One line for a session this page did not start, e.g. "Extend, native size, 150 %".
    fn summary(&self) -> String {
        let mut s = match self.mode {
            Mode::Extend => "Extend".to_owned(),
            Mode::Mirror => "Duplicate".to_owned(),
        };
        s += &match self.size {
            Size::Phone => ", phone's default size".to_owned(),
            Size::Native => ", native size".to_owned(),
            Size::Fixed(w, h) => format!(", {w}×{h}"),
        };
        if self.mode == Mode::Extend {
            s += &format!(", {} %", self.scale);
            if let Some(o) = &self.output {
                s += &format!(", on {o}");
            }
        }
        if !self.extra.is_empty() {
            s += &format!(", {}", self.extra.join(" "));
        }
        s
    }
}

fn parse_size(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// "1.5" or "150%" → 150.
fn parse_scale(v: &str) -> Option<u32> {
    let pct = match v.trim().strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().ok()?,
        None => v.trim().parse::<f64>().ok()? * 100.0,
    };
    (pct.is_finite() && pct > 0.0).then(|| pct.round() as u32)
}

// ── Processes ───────────────────────────────────────────────────────────────

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Shared with the Noctalia plugin, which starts sessions the same way.
fn log_path() -> PathBuf {
    runtime_dir().join("smartconnect-display.log")
}

/// The helper binary (`$SMARTCONNECT_DISPLAY_BIN`, next to this program, else `$PATH`; see
/// [`binary`]) and its file name, which is what running sessions are recognised by.
fn program() -> (PathBuf, String) {
    let bin = binary(BIN);
    let name = bin.file_name().and_then(|n| n.to_str()).unwrap_or(BIN).to_owned();
    (bin, name)
}

fn is_executable(p: &Path) -> bool {
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

/// The helper as a file: `bin` itself when it is a path, else its `$PATH` match.
fn locate(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        is_executable(bin).then(|| bin.to_owned())
    } else {
        bin.to_str().and_then(on_path)
    }
}

/// Flags the helper lists in its `--help` (which only prints the usage and exits).
#[derive(Clone, Debug, Default)]
struct Flags {
    /// Helper file and modification time the list was read from.
    from: Option<(PathBuf, SystemTime)>,
    names: HashSet<String>,
}

impl Flags {
    fn has(&self, flag: &str) -> bool {
        self.names.contains(flag)
    }
}

fn helper_stamp() -> Option<(PathBuf, SystemTime)> {
    let file = locate(&program().0)?;
    let mtime = file.metadata().ok()?.modified().ok()?;
    Some((file, mtime))
}

fn read_flags(stamp: (PathBuf, SystemTime)) -> Flags {
    let out = Command::new(&stamp.0).arg("--help").stdin(Stdio::null()).stderr(Stdio::null()).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    Flags { from: Some(stamp), names: flag_names(&text) }
}

/// Every `--word` in a usage text.
fn flag_names(usage: &str) -> HashSet<String> {
    usage
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.starts_with("--") && w.len() > 2)
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
struct Running {
    pid: u32,
    /// Its command line after the program name.
    args: Vec<String>,
}

/// Every running process of the helper `name`, ours or not. Matches the kernel's short command
/// name first (cheap, at most 15 bytes), then argv[0], or argv[1] for a script run by an interpreter.
fn running_sessions(name: &str) -> Vec<Running> {
    let comm: String = name.chars().take(15).collect();
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return found };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if std::fs::read_to_string(entry.path().join("comm")).map_or(true, |c| c.trim_end() != comm) {
            continue;
        }
        // Empty for a zombie, which then counts as gone.
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let args: Vec<String> = raw.split(|b| *b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        let is_us = |a: &String| Path::new(a).file_name().and_then(|f| f.to_str()) == Some(name);
        if let Some(i) = args.iter().take(2).position(is_us) {
            let rest: Vec<String> = args[i + 1..].iter().filter(|a| !a.is_empty()).cloned().collect();
            found.push(Running { pid, args: rest });
        }
    }
    found.sort_by_key(|r| r.pid);
    found
}

/// Starts a session detached from the GUI: its own process group (Ctrl+C in the GUI's terminal does
/// not reach it), output in the log file. After `exec` the child's pid is the helper's.
fn spawn(bin: &Path, args: &[String], log: &Path) -> Result<Child, String> {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", WRAPPER, "sh"])
        .arg(log)
        .arg(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // A GUI started with SIGINT ignored (e.g. as a background job of a script) would pass that on,
    // and a session started like that could not be stopped: give it the default dispositions.
    // SAFETY: only signal(2), which is async-signal-safe, runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("Could not start {}: {e}", bin.display()))
}

/// SIGINT: the helper tells the phone to stop, switches an extend output off and exits.
fn interrupt(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain kill(2) on a pid just read from /proc as a smartconnect-display process.
        unsafe { libc::kill(pid, libc::SIGINT) };
    }
}

// ── Session log ─────────────────────────────────────────────────────────────

/// What the log of the latest session started by a front-end says.
#[derive(Clone, Debug, Default, PartialEq)]
struct LogState {
    /// From the wrapper's `pid N` line; `None` when the file is missing or not ours.
    pid: Option<u32>,
    /// Stream size being sent.
    size: Option<(u32, u32)>,
    /// Output being captured (e.g. `Virtual-1`, `eDP-1`).
    output: Option<String>,
    streaming: bool,
    /// The connection dropped after streaming had started.
    lost: bool,
    /// `stopping: <reason>`.
    stopped: Option<String>,
    error: Option<String>,
    /// A `svc 202:` error: the session cannot recover (no stream).
    fatal: bool,
    /// Last non-empty line, for a session that ended without saying why.
    last: Option<String>,
}

fn parse_log(text: &str) -> LogState {
    let mut lines = text.lines();
    let Some(pid) = lines.next().and_then(|l| l.strip_prefix("pid ")).and_then(|p| p.trim().parse().ok()) else {
        return LogState::default();
    };
    let mut s = LogState { pid: Some(pid), ..LogState::default() };
    // anyhow prints `Error: <msg>`, a blank line, `Caused by:` and the causes, indented.
    let mut cause_next = false;
    for line in lines.map(str::trim_end).filter(|l| !l.trim().is_empty()) {
        if cause_next && line.starts_with(char::is_whitespace) {
            let cause = line.trim();
            let cause = cause.split_once(": ").filter(|(n, _)| n.parse::<u32>().is_ok()).map_or(cause, |(_, c)| c);
            if let Some(e) = s.error.as_mut() {
                *e = format!("{e} ({cause})");
            }
            cause_next = false;
        } else if line != "Caused by:" {
            cause_next = false;
        }
        s.last = Some(line.to_owned());
        if let Some(reason) = line.strip_prefix("stopping: ") {
            s.stopped = Some(reason.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("phone panel ") {
            s.size = rest.rsplit("sending ").next().and_then(parse_size).or(s.size);
        } else if let Some(rest) = line.strip_prefix("stream size → ") {
            s.size = parse_size(rest).or(s.size);
        } else if let Some(rest) = line.strip_prefix("capturing ") {
            s.output = rest.split(" (").next().map(str::to_owned);
        } else if let Some(state) = line.strip_prefix("peer connection: ") {
            match state.trim() {
                "Connected" => {
                    s.streaming = true;
                    s.lost = false;
                    s.error = None; // whatever went wrong before, the stream is up now
                }
                "Failed" | "Disconnected" | "Closed" => {
                    s.lost |= s.streaming;
                    s.streaming = false;
                }
                _ => {}
            }
        } else if let Some(e) = line.strip_prefix("svc 202: ") {
            s.error = Some(e.trim().to_owned());
            s.fatal = true;
        } else if let Some(e) = line.strip_prefix("Error: ") {
            s.error = Some(e.trim().to_owned());
            cause_next = true;
        } else if line.starts_with("phone refused: ") {
            s.error = Some("the phone refused the session".to_owned());
        } else if let Some(rest) = line.strip_prefix("daemon: ") {
            let msg = serde_json::from_str::<Value>(rest).ok().and_then(|v| v.get("message")?.as_str().map(str::to_owned));
            s.error = Some(format!("smartconnectd: {}", msg.as_deref().unwrap_or(rest)));
        } else if line.starts_with("pipeline error from ") {
            let msg = line.split_once("): ").map_or(line, |(_, m)| m);
            s.error = Some(msg.split(" (").next().unwrap_or(msg).trim().to_owned());
        } else if line.starts_with("sh:") && line.contains("exec") {
            s.error = Some(line.to_owned()); // the wrapper could not run the helper
        }
    }
    s
}

/// The log, or its first line plus the last 256 KiB when it is large.
fn read_log(path: &Path) -> Option<String> {
    const TAIL: u64 = 256 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut buf = Vec::new();
    if len <= 2 * TAIL {
        f.read_to_end(&mut buf).ok()?;
    } else {
        let mut head = [0u8; 64];
        let n = f.read(&mut head).ok()?;
        buf.extend_from_slice(head[..n].split(|b| *b == b'\n').next().unwrap_or_default());
        buf.push(b'\n');
        f.seek(SeekFrom::Start(len - TAIL)).ok()?;
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).ok()?;
        let start = tail.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
        buf.extend_from_slice(&tail[start..]);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

type Stamp = (u64, SystemTime);

struct Snapshot {
    sessions: Vec<Running>,
    /// Set when the log changed since the previous stamp.
    log: Option<(Option<Stamp>, LogState)>,
}

fn snapshot(name: &str, log: &Path, stamp: Option<Stamp>) -> Snapshot {
    let sessions = running_sessions(name);
    let now = std::fs::metadata(log).ok().and_then(|m| Some((m.len(), m.modified().ok()?)));
    let log = (now != stamp).then(|| (now, read_log(log).map(|t| parse_log(&t)).unwrap_or_default()));
    Snapshot { sessions, log }
}

enum Outcome {
    Stopped(String),
    Failed(String),
}

/// How the session in the log ended.
fn outcome(log: &LogState) -> Option<Outcome> {
    log.pid?;
    // A fatal error says why; the stop that follows it ("could not start the stream") only says that.
    if let Some(e) = log.error.as_ref().filter(|_| log.fatal) {
        return Some(Outcome::Failed(e.clone()));
    }
    let reason = log.stopped.as_deref();
    if let Some(r) = reason.filter(|r| *r != "signal") {
        return Some(Outcome::Stopped(friendly(r)));
    }
    if let Some(e) = &log.error {
        return Some(Outcome::Failed(e.clone()));
    }
    if reason.is_some() {
        return Some(Outcome::Stopped("stopped from this PC".into()));
    }
    log.last.as_ref().map(|l| Outcome::Stopped(format!("ended ({l})")))
}

fn friendly(reason: &str) -> String {
    match reason {
        "the phone stopped screen sharing" => "stopped on the phone".into(),
        "the phone reports an ICE failure" => "the connection to the phone failed (ICE)".into(),
        "refused by the phone" => "the phone refused the session".into(),
        "daemon went away" => "smartconnectd stopped".into(),
        other => other.into(),
    }
}

// ── Outputs (niri) and other setup checks ───────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
struct Output {
    name: String,
    label: String,
}

impl Output {
    fn is_virtual(&self) -> bool {
        self.name.starts_with("Virtual-")
    }

    /// A laptop's own panel: never offered as an extend target (it would be switched off afterwards).
    fn is_internal(&self) -> bool {
        ["eDP", "LVDS", "DSI"].iter().any(|p| self.name.starts_with(p))
    }
}

fn list_outputs() -> Result<Vec<Output>, String> {
    let out = Command::new("niri")
        .args(["msg", "--json", "outputs"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run niri msg ({e})"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("niri msg failed").trim().to_owned());
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected niri output ({e})"))?;
    let map = v.as_object().ok_or("unexpected niri output")?;
    let mut outputs: Vec<Output> = map
        .iter()
        .map(|(name, o)| {
            let on = o.get("logical").is_some_and(Value::is_object);
            let text = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "Unknown");
            let what = match (text("make"), text("model")) {
                _ if name.starts_with("Virtual-") => "virtual".to_owned(),
                (Some(make), Some(model)) => format!("{make} {model}"),
                (Some(x), None) | (None, Some(x)) => x.to_owned(),
                (None, None) => "monitor".to_owned(),
            };
            let label = format!("{name} ({what}, {})", if on { "on" } else { "off" });
            Output { name: name.clone(), label }
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(outputs)
}

/// The phone's keyboard button runs `$SMARTCONNECT_OSK` or wvkbd.
fn osk_missing() -> bool {
    std::env::var_os("SMARTCONNECT_OSK").is_none_or(|v| v.is_empty())
        && on_path("wvkbd-deskintl").is_none()
        && on_path("wvkbd-mobintl").is_none()
}

struct Checks {
    outputs: Result<Vec<Output>, String>,
    osk_missing: bool,
    /// Set when the helper changed since `Flags::from`.
    flags: Option<Flags>,
}

fn run_checks(flags_from: Option<(PathBuf, SystemTime)>) -> Checks {
    let stamp = helper_stamp();
    let flags = (stamp != flags_from).then(|| stamp.map(read_flags).unwrap_or_default());
    Checks { outputs: list_outputs(), osk_missing: osk_missing(), flags }
}

// ── Page ────────────────────────────────────────────────────────────────────

enum Pending {
    /// Spawned, not seen in /proc yet.
    Starting { pid: u32, since: Instant },
    /// SIGINT sent.
    Stopping { since: Instant },
    /// SIGINT sent; start again once every session is gone (+ `RESTART_GAP`).
    Restarting { since: Instant, gone_at: Option<Instant> },
}

#[derive(Default)]
struct State {
    prefs: Prefs,
    connected: bool,
    /// `None` until listed once.
    outputs: Option<Result<Vec<Output>, String>>,
    outputs_at: Option<Instant>,
    /// Output row index → `--output` value ("" = automatic), and the labels shown.
    output_choices: Vec<String>,
    output_labels: Vec<String>,
    osk_missing: bool,
    flags: Option<Flags>,
    sessions: Vec<Running>,
    log: LogState,
    log_stamp: Option<Stamp>,
    pending: Option<Pending>,
    /// Pids already sent SIGINT: one signal each (the helper's handler is gone after the first).
    signalled: Vec<u32>,
    /// The session this window started last, with everything it was asked for (also what only
    /// `display.json` carries to an older helper).
    own: Option<(u32, Opts)>,
    /// Our session exited without us stopping it: say why once the log is read.
    announce: Option<u32>,
}

impl State {
    /// What a running session shows: its command line on top of `display.json` (a helper that reads
    /// it, unless started with `--no-config`) or on top of the helper's own defaults.
    fn running_opts(&self, r: &Running) -> Opts {
        let reads_config = self.flags.as_ref().is_some_and(|f| f.has(NO_CONFIG_FLAG)) && !r.args.iter().any(|a| a == NO_CONFIG_FLAG);
        let base = if reads_config { Opts::from_prefs(&self.prefs.display) } else { Opts::cli_defaults() };
        Opts::parse(&r.args, base)
    }
}

pub struct DisplayPage {
    toasts: adw::ToastOverlay,
    status: gtk::Label,
    detail: gtk::Label,
    start: gtk::Button,
    apply: gtk::Button,
    hints: adw::PreferencesGroup,
    vkms_hint: adw::ActionRow,
    output_hint: adw::ActionRow,
    osk_hint: adw::ActionRow,
    mode_row: adw::ActionRow,
    mode: adw::ToggleGroup,
    resolution: adw::ComboRow,
    width: adw::SpinRow,
    height: adw::SpinRow,
    scale: adw::ComboRow,
    osk: adw::SpinRow,
    audio: adw::SwitchRow,
    rotate: adw::SwitchRow,
    advanced: adw::ExpanderRow,
    output: adw::ComboRow,
    exited: async_channel::Sender<u32>,
    st: RefCell<State>,
    visible: Cell<bool>,
    polling: Cell<bool>,
    checking: Cell<bool>,
    /// Widgets are being set from the preferences: ignore their change signals.
    syncing: Cell<bool>,
}

fn hint_row(icon: &str, title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle_selectable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

fn copy_button(text: &'static str, toasts: &adw::ToastOverlay) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(format!("Copy “{text}”"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let toasts = toasts.clone();
    button.connect_clicked(move |b| {
        b.clipboard().set_text(text);
        toasts.add_toast(adw::Toast::new("Copied to the clipboard"));
    });
    button
}

fn refresh_button(tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}

fn spin_row(title: &str, subtitle: &str, (min, max, step): (u32, u32, u32)) -> adw::SpinRow {
    let adj = gtk::Adjustment::new(f64::from(min), f64::from(min), f64::from(max), f64::from(step), f64::from(step) * 4.0, 0.0);
    adw::SpinRow::builder().title(title).subtitle(subtitle).adjustment(&adj).numeric(true).build()
}

impl DisplayPage {
    pub fn build(toasts: &adw::ToastOverlay) -> (Rc<Self>, gtk::Widget) {
        let icon = gtk::Image::builder().icon_name("video-display-symbolic").pixel_size(32).build();
        let title = gtk::Label::builder().label("Phone as display").xalign(0.0).css_classes(["title-4"]).wrap(true).build();
        let status = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let detail = gtk::Label::builder()
            .css_classes(["dim-label"])
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .build();
        let start = gtk::Button::builder().css_classes(["suggested-action"]).label("Start").valign(gtk::Align::Center).build();
        let apply = gtk::Button::builder()
            .label("Apply")
            .tooltip_text("Restart the session with these settings")
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Center).build();
        buttons.append(&start);
        buttons.append(&apply);

        // Setup hints, shown only when something is missing.
        let hints = adw::PreferencesGroup::builder().visible(false).build();
        let vkms_hint = hint_row("dialog-warning-symbolic", "Extend needs a virtual output");
        vkms_hint.set_subtitle(
            "Run <tt>sudo modprobe vkms</tt> (adds Virtual-1), or plug in an HDMI dummy and choose it under Advanced. \
             To load vkms at every boot: <tt>echo vkms | sudo tee /etc/modules-load.d/vkms.conf</tt>",
        );
        vkms_hint.add_suffix(&copy_button("sudo modprobe vkms", toasts));
        let vkms_check = refresh_button("Check again");
        vkms_hint.add_suffix(&vkms_check);
        let output_hint = hint_row("dialog-warning-symbolic", "");
        let output_check = refresh_button("Check again");
        output_hint.add_suffix(&output_check);
        let osk_hint = hint_row("input-keyboard-symbolic", "No on-screen keyboard");
        osk_hint.set_subtitle(
            "The keyboard button on the phone opens wvkbd on the shared screen. Install it, e.g. <tt>yay -S wvkbd</tt>",
        );
        osk_hint.add_suffix(&copy_button("yay -S wvkbd", toasts));
        for row in [&vkms_hint, &output_hint, &osk_hint] {
            hints.add(row);
        }

        let mode = adw::ToggleGroup::builder().valign(gtk::Align::Center).homogeneous(true).can_shrink(false).build();
        mode.add(adw::Toggle::builder().name("extend").label("Extend").tooltip("The phone becomes a second monitor").build());
        mode.add(adw::Toggle::builder().name("mirror").label("Duplicate").tooltip("Show this PC's screen on the phone").build());
        let mode_row = adw::ActionRow::builder().title("Mode").build();
        mode_row.add_suffix(&mode);
        let resolution = adw::ComboRow::builder()
            .title("Resolution")
            .model(&gtk::StringList::new(&["Phone native", "Phone default", "Custom"]))
            .build();
        let width = spin_row("Width", "Pixels", (prefs::MIN_SIDE, prefs::MAX_SIDE, 8));
        let height = spin_row("Height", "Pixels", (prefs::MIN_SIDE, prefs::MAX_SIDE, 8));
        let scale_names: Vec<String> = SCALES.iter().map(|s| format!("{s} %")).collect();
        let scale_refs: Vec<&str> = scale_names.iter().map(String::as_str).collect();
        let scale = adw::ComboRow::builder()
            .title("Display scale")
            .subtitle("Size of text and windows on the phone; bigger means less room")
            .model(&gtk::StringList::new(&scale_refs))
            .build();
        let osk = spin_row(
            "On-screen keyboard height",
            "In % of the shared screen, for the keyboard button on the phone",
            (prefs::OSK_MIN, prefs::OSK_MAX, 5),
        );
        let audio = adw::SwitchRow::builder().title("Send PC audio to the phone").subtitle("Experimental").build();
        let rotate = adw::SwitchRow::builder()
            .title("Rotate with the phone")
            .subtitle("Experimental · Extend: turns the phone's monitor when you turn the phone")
            .build();
        let advanced = adw::ExpanderRow::builder().title("Advanced").subtitle("Which output to extend onto").build();
        let output = adw::ComboRow::builder().title("Extend onto").model(&gtk::StringList::new(&[])).build();
        let output_refresh = refresh_button("List the outputs again");
        output.add_suffix(&output_refresh);
        advanced.add_row(&output);

        // Layout, Experimental and Advanced groups (the phone-as-display card sits above them).
        let layout_group = adw::PreferencesGroup::builder()
            .title("Layout")
            .description(
                "Extend makes the phone a second monitor; Duplicate shows this PC's screen on it. \
                 To see the phone's screen on this PC instead, use Mirror Screen on the Phone page.",
            )
            .build();
        for row in [mode_row.upcast_ref::<gtk::Widget>(), resolution.upcast_ref(), width.upcast_ref(), height.upcast_ref(), scale.upcast_ref(), osk.upcast_ref()] {
            layout_group.add(row);
        }
        let experimental_group = adw::PreferencesGroup::builder().title("Experimental").build();
        experimental_group.add(&audio);
        experimental_group.add(&rotate);
        let advanced_group = adw::PreferencesGroup::new();
        advanced_group.add(&advanced);

        // ── Phone-as-display card (hero) ──────────────────────────────────────
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk::Align::Center).build();
        text.append(&title);
        text.append(&status);
        text.append(&detail);
        let head = gtk::Box::builder().spacing(12).build();
        head.append(&icon);
        head.append(&text);
        head.append(&buttons);
        let card = gtk::Box::builder().css_classes(["card", "sc-card"]).hexpand(true).build();
        card.append(&head);

        // Layout / Experimental / Advanced sit two-up on a wide window, stacked when narrow.
        let groups = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(1)
            .max_children_per_line(2)
            .column_spacing(18)
            .row_spacing(18)
            .valign(gtk::Align::Start)
            .hexpand(true)
            .build();
        for g in [&layout_group, &experimental_group, &advanced_group] {
            g.set_hexpand(true);
            g.set_width_request(360);
            groups.append(g);
        }

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&card);
        column.append(&hints);
        column.append(&groups);
        let clamp = adw::Clamp::builder().maximum_size(1400).tightening_threshold(1400).child(&column).build();
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build();

        let (exited, exits) = async_channel::unbounded();
        let page = Rc::new(Self {
            toasts: toasts.clone(),
            status,
            detail,
            start,
            apply,
            hints,
            vkms_hint,
            output_hint,
            osk_hint,
            mode_row,
            mode,
            resolution,
            width,
            height,
            scale,
            osk,
            audio,
            rotate,
            advanced,
            output,
            exited,
            st: RefCell::new(State { prefs: prefs::load(), osk_missing: osk_missing(), ..State::default() }),
            visible: Cell::new(false),
            polling: Cell::new(false),
            checking: Cell::new(false),
            syncing: Cell::new(false),
        });
        page.connect_signals(&[&vkms_check, &output_check, &output_refresh]);
        page.sync_widgets();

        let weak = Rc::downgrade(&page);
        glib::spawn_future_local(async move {
            while let Ok(pid) = exits.recv().await {
                let Some(page) = weak.upgrade() else { break };
                page.on_exit(pid);
            }
        });
        let weak = Rc::downgrade(&page);
        glib::timeout_add_local(POLL, move || {
            let Some(page) = weak.upgrade() else { return glib::ControlFlow::Break };
            page.tick();
            glib::ControlFlow::Continue
        });
        page.poll_now();
        (page, scroller.upcast())
    }

    fn connect_signals(self: &Rc<Self>, check_again: &[&gtk::Button]) {
        let weak = Rc::downgrade(self);
        self.start.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.start_or_stop();
            }
        });
        let weak = Rc::downgrade(self);
        self.apply.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.restart();
            }
        });
        for button in check_again {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.run_checks();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.mode.connect_active_name_notify(move |group| {
            let (Some(p), Some(name)) = (weak.upgrade(), group.active_name()) else { return };
            if !p.syncing.get() {
                p.update_prefs(|d| d.mode = name.to_string());
                p.sync_widgets(); // the Resolution row follows the mode
                if name == "extend" {
                    p.run_checks();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.resolution.connect_selected_notify(move |row| {
            let Some(p) = weak.upgrade() else { return };
            let key = RESOLUTIONS.get(row.selected() as usize).copied().unwrap_or("phone");
            p.update_prefs(|d| {
                let res = if d.mode == "mirror" { &mut d.mirror_resolution } else { &mut d.extend_resolution };
                *res = key.to_owned();
            });
        });
        let weak = Rc::downgrade(self);
        self.width.connect_value_notify(move |row| {
            if let Some(p) = weak.upgrade() {
                p.update_prefs(|d| d.custom_width = row.value() as u32);
            }
        });
        let weak = Rc::downgrade(self);
        self.height.connect_value_notify(move |row| {
            if let Some(p) = weak.upgrade() {
                p.update_prefs(|d| d.custom_height = row.value() as u32);
            }
        });
        let weak = Rc::downgrade(self);
        self.scale.connect_selected_notify(move |row| {
            if let (Some(p), Some(s)) = (weak.upgrade(), SCALES.get(row.selected() as usize)) {
                p.update_prefs(|d| d.scale = *s);
            }
        });
        let weak = Rc::downgrade(self);
        self.osk.connect_value_notify(move |row| {
            if let Some(p) = weak.upgrade() {
                p.update_prefs(|d| d.osk_height = row.value() as u32);
            }
        });
        let weak = Rc::downgrade(self);
        self.audio.connect_active_notify(move |row| {
            if let Some(p) = weak.upgrade() {
                p.update_prefs(|d| d.audio = row.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        self.rotate.connect_active_notify(move |row| {
            if let Some(p) = weak.upgrade() {
                p.update_prefs(|d| d.follow_rotation = row.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        self.output.connect_selected_notify(move |row| {
            let Some(p) = weak.upgrade() else { return };
            let name = p.st.borrow().output_choices.get(row.selected() as usize).cloned();
            if let Some(name) = name {
                p.update_prefs(|d| d.output = name);
            }
        });
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Applies a change made on the page, saves the preferences (and `display.json`) and updates the page.
    fn update_prefs(&self, change: impl FnOnce(&mut DisplayPrefs)) {
        if self.syncing.get() {
            return;
        }
        let prefs = {
            let mut st = self.st.borrow_mut();
            change(&mut st.prefs.display);
            st.prefs.clone()
        };
        if let Err(e) = prefs::save(&prefs) {
            eprintln!("smartconnect-gui: could not save the display settings: {e}");
        }
        self.refresh();
    }

    /// Sets the widgets from the preferences.
    fn sync_widgets(&self) {
        let d = self.st.borrow().prefs.display.clone();
        self.syncing.set(true);
        self.mode.set_active_name(Some(&d.mode));
        self.resolution.set_selected(RESOLUTIONS.iter().position(|r| *r == d.resolution()).unwrap_or(0) as u32);
        self.width.set_value(f64::from(d.custom_width));
        self.height.set_value(f64::from(d.custom_height));
        let nearest = (0..SCALES.len()).min_by_key(|&i| SCALES[i].abs_diff(d.scale)).unwrap_or(4);
        self.scale.set_selected(nearest as u32);
        self.osk.set_value(f64::from(d.osk_height));
        self.audio.set_active(d.audio);
        self.rotate.set_active(d.follow_rotation);
        self.syncing.set(false);
        self.rebuild_outputs();
        self.refresh();
    }

    pub fn set_connected(&self, connected: bool) {
        let changed = {
            let mut st = self.st.borrow_mut();
            std::mem::replace(&mut st.connected, connected) != connected
        };
        if changed {
            self.refresh();
        }
    }

    /// The page was shown or hidden.
    pub fn set_visible(self: &Rc<Self>, visible: bool) {
        self.visible.set(visible);
        if visible {
            // The Noctalia plugin may have changed the shared choices meanwhile.
            let changed = prefs::reload_shared(&mut self.st.borrow_mut().prefs.display);
            if changed {
                self.sync_widgets();
            }
            self.run_checks();
            self.poll_now();
        }
    }

    // ── Polling ─────────────────────────────────────────────────────────────

    fn tick(self: &Rc<Self>) {
        let (active, checks_due) = {
            let st = self.st.borrow();
            let active = st.pending.is_some() || !st.sessions.is_empty() || st.announce.is_some();
            (active, st.outputs_at.is_none_or(|t| t.elapsed() >= OUTPUTS_EVERY))
        };
        let visible = self.visible.get();
        if visible || active {
            self.poll_now();
        }
        if visible && checks_due {
            self.run_checks();
        }
    }

    fn poll_now(self: &Rc<Self>) {
        if self.polling.replace(true) {
            return;
        }
        let stamp = self.st.borrow().log_stamp;
        let (_, name) = program();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let snap = gio::spawn_blocking(move || snapshot(&name, &log_path(), stamp)).await;
            let Some(page) = weak.upgrade() else { return };
            page.polling.set(false);
            if let Ok(snap) = snap {
                page.apply_snapshot(snap);
            }
        });
    }

    fn apply_snapshot(self: &Rc<Self>, snap: Snapshot) {
        let mut toasts = Vec::new();
        let mut start_again = false;
        {
            let mut st = self.st.borrow_mut();
            let st = &mut *st;
            st.sessions = snap.sessions;
            if let Some((stamp, log)) = snap.log {
                st.log_stamp = stamp;
                st.log = log;
            }
            let pids: Vec<u32> = st.sessions.iter().map(|r| r.pid).collect();
            st.signalled.retain(|p| pids.contains(p));
            let now = Instant::now();
            match st.pending {
                Some(Pending::Starting { pid, since }) if pids.contains(&pid) || since.elapsed() > START_GRACE => st.pending = None,
                Some(Pending::Stopping { since }) => {
                    if pids.is_empty() {
                        st.pending = None;
                    } else if since.elapsed() > STOP_TIMEOUT {
                        st.pending = None;
                        toasts.push("The phone display is still running; press Stop again".to_owned());
                    }
                }
                Some(Pending::Restarting { since, gone_at }) => {
                    if !pids.is_empty() {
                        if since.elapsed() > STOP_TIMEOUT {
                            st.pending = None;
                            toasts.push("The old session did not stop, so nothing was restarted".to_owned());
                        }
                    } else if let Some(gone) = gone_at {
                        if gone.elapsed() >= RESTART_GAP {
                            st.pending = None;
                            start_again = true;
                        }
                    } else {
                        st.pending = Some(Pending::Restarting { since, gone_at: Some(now) });
                    }
                }
                _ => {}
            }
            // A session we started hit an error it cannot recover from: stop it, so the phone
            // does not wait on "Connecting…" forever.
            let own = st.own.as_ref().map(|(pid, _)| *pid);
            if let Some(own) = own.filter(|own| {
                st.log.fatal
                    && st.log.stopped.is_none() // a newer helper stops itself after a failed offer
                    && st.log.pid == Some(*own)
                    && pids.contains(own)
                    && !st.signalled.contains(own)
                    && st.pending.is_none()
            }) {
                interrupt(own);
                st.signalled.push(own);
                st.pending = Some(Pending::Stopping { since: now });
                toasts.push(format!("Phone display failed: {}", st.log.error.as_deref().unwrap_or("unknown error")));
            }
            if let Some(pid) = st.announce.filter(|pid| !pids.contains(pid)) {
                st.announce = None;
                if st.log.pid == Some(pid) {
                    match outcome(&st.log) {
                        Some(Outcome::Stopped(r)) => toasts.push(format!("Phone display: {r}")),
                        Some(Outcome::Failed(e)) => toasts.push(format!("Phone display failed: {e}")),
                        None => {}
                    }
                }
            }
        }
        for t in toasts {
            self.toast(&t);
        }
        if start_again {
            self.start();
        }
        self.refresh();
    }

    /// Our child exited (its reaper thread reports the pid).
    fn on_exit(self: &Rc<Self>, pid: u32) {
        {
            let mut st = self.st.borrow_mut();
            if matches!(st.pending, Some(Pending::Starting { pid: p, .. }) if p == pid) {
                st.pending = None;
            }
            if st.own.as_ref().is_some_and(|(own, _)| *own == pid) && !st.signalled.contains(&pid) {
                st.announce = Some(pid);
            }
        }
        self.poll_now();
    }

    /// Lists the outputs, looks for an on-screen keyboard and (when the helper changed) its flags.
    fn run_checks(self: &Rc<Self>) {
        if self.checking.replace(true) {
            return;
        }
        let flags_from = self.st.borrow().flags.as_ref().and_then(|f| f.from.clone());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let checks = gio::spawn_blocking(move || run_checks(flags_from)).await;
            let Some(page) = weak.upgrade() else { return };
            page.checking.set(false);
            if let Ok(checks) = checks {
                {
                    let mut st = page.st.borrow_mut();
                    st.outputs = Some(checks.outputs);
                    st.outputs_at = Some(Instant::now());
                    st.osk_missing = checks.osk_missing;
                    if let Some(flags) = checks.flags {
                        st.flags = Some(flags);
                    }
                }
                page.rebuild_outputs();
                page.refresh();
            }
        });
    }

    /// Fills the "Extend onto" row: Automatic, every output except the laptop's own panel, and the
    /// saved choice even while it is unplugged.
    fn rebuild_outputs(&self) {
        let (choices, labels, selected) = {
            let st = self.st.borrow();
            let saved = st.prefs.display.output.clone();
            let mut choices = vec![String::new()];
            let mut labels = vec!["Automatic (first virtual output)".to_owned()];
            if let Some(Ok(list)) = &st.outputs {
                for o in list.iter().filter(|o| !o.is_internal()) {
                    choices.push(o.name.clone());
                    labels.push(o.label.clone());
                }
            }
            if !choices.contains(&saved) {
                choices.push(saved.clone());
                labels.push(format!("{saved} (not connected)"));
            }
            let selected = choices.iter().position(|c| *c == saved).unwrap_or(0);
            if choices == st.output_choices && labels == st.output_labels && self.output.selected() as usize == selected {
                return;
            }
            (choices, labels, selected)
        };
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        {
            let mut st = self.st.borrow_mut();
            st.output_choices = choices;
            st.output_labels = labels.clone();
        }
        self.syncing.set(true);
        self.output.set_model(Some(&gtk::StringList::new(&refs)));
        self.output.set_selected(selected as u32);
        self.syncing.set(false);
    }

    // ── Actions ─────────────────────────────────────────────────────────────

    fn start_or_stop(self: &Rc<Self>) {
        if self.st.borrow().sessions.is_empty() {
            self.start();
        } else {
            self.stop(false);
        }
    }

    /// Why the current settings cannot start right now, if they cannot.
    fn blocked(st: &State, opts: &Opts) -> Option<String> {
        if !st.connected {
            return Some("Connect the phone first".into());
        }
        if opts.mode != Mode::Extend {
            return None;
        }
        let Some(Ok(outputs)) = &st.outputs else { return None };
        match &opts.output {
            None if !outputs.iter().any(Output::is_virtual) => Some("Extend needs a virtual output (see the hint below)".into()),
            Some(name) if !outputs.iter().any(|o| &o.name == name) => Some(format!("{name} is not connected")),
            _ => None,
        }
    }

    fn start(self: &Rc<Self>) {
        let (display, opts, blocked) = {
            let st = self.st.borrow();
            let opts = Opts::from_prefs(&st.prefs.display);
            let blocked = Self::blocked(&st, &opts);
            (st.prefs.display.clone(), opts, blocked)
        };
        if let Some(why) = blocked {
            self.toast(&why);
            self.refresh();
            return;
        }
        let (bin, _) = program();
        let Some(stamp) = helper_stamp() else {
            self.toast(&format!(
                "{BIN} was not found next to this app or on PATH (install it with packaging/install.sh, or set {})",
                crate::mirror::env_var(BIN)
            ));
            return;
        };
        // The flags of this very build (normally read already when the page was shown).
        let flags = {
            let cached = self.st.borrow().flags.clone().filter(|f| f.from.as_ref() == Some(&stamp));
            cached.unwrap_or_else(|| read_flags(stamp))
        };
        // A start from the phone later uses the same choices.
        if let Err(e) = prefs::save_shared(&display) {
            eprintln!("smartconnect-gui: could not write {}: {e}", prefs::display_path().display());
        }
        match spawn(&bin, &opts.args(&flags), &log_path()) {
            Ok(mut child) => {
                let pid = child.id();
                {
                    let mut st = self.st.borrow_mut();
                    st.flags = Some(flags);
                    st.own = Some((pid, opts));
                    st.announce = None;
                    st.pending = Some(Pending::Starting { pid, since: Instant::now() });
                    // The wrapper rewrites the log: forget the previous session's.
                    st.log = LogState::default();
                    st.log_stamp = None;
                }
                let exited = self.exited.clone();
                std::thread::spawn(move || {
                    let _ = child.wait();
                    let _ = exited.send_blocking(pid);
                });
            }
            Err(e) => self.toast(&e),
        }
        self.refresh();
        self.poll_now();
    }

    /// SIGINT to every running session; with `restart`, start again once they are gone.
    fn stop(self: &Rc<Self>, restart: bool) {
        {
            let mut st = self.st.borrow_mut();
            let st = &mut *st;
            for r in &st.sessions {
                if !st.signalled.contains(&r.pid) {
                    interrupt(r.pid);
                    st.signalled.push(r.pid);
                }
            }
            let since = Instant::now();
            st.pending = Some(if restart { Pending::Restarting { since, gone_at: None } } else { Pending::Stopping { since } });
        }
        self.refresh();
        self.poll_now();
    }

    /// Apply: restart the running session with the page's settings.
    fn restart(self: &Rc<Self>) {
        let blocked = {
            let st = self.st.borrow();
            Self::blocked(&st, &Opts::from_prefs(&st.prefs.display))
        };
        match blocked {
            Some(why) => self.toast(&why),
            None => self.stop(true),
        }
    }

    // ── View ────────────────────────────────────────────────────────────────

    fn refresh(&self) {
        let st = self.st.borrow();
        let d = &st.prefs.display;
        let desired = Opts::from_prefs(d);
        let extend = desired.mode == Mode::Extend;
        let running = st.sessions.first();
        let log = &st.log;
        // The log describes the running session when its pid matches (we or the Noctalia plugin started it).
        let logged = running.is_some() && log.pid.is_some_and(|p| st.sessions.iter().any(|r| r.pid == p));
        let blocked = Self::blocked(&st, &desired);

        let mut detail_error = false;
        let (status, detail): (String, String) = match (&st.pending, running) {
            (Some(Pending::Restarting { .. }), _) => ("Restarting…".into(), "Applying the new settings".into()),
            (Some(Pending::Stopping { .. }), Some(_)) => ("Stopping…".into(), "Telling the phone to stop".into()),
            (Some(Pending::Starting { .. }), _) if !logged => {
                ("Starting…".into(), "Asking the phone to open its display view".into())
            }
            (_, Some(r)) if logged => {
                let opts = st.running_opts(r);
                let target = log.output.clone().or_else(|| opts.output.clone());
                let (status, mut detail) = if log.stopped.is_some() {
                    ("Stopping…".to_owned(), String::new())
                } else if log.streaming {
                    let size = log.size.map_or(String::new(), |(w, h)| format!(" {w}×{h}"));
                    let what = match (opts.mode, target) {
                        (Mode::Extend, Some(o)) => format!("Extended onto {o}"),
                        (Mode::Extend, None) => "Extended onto a virtual output".to_owned(),
                        (Mode::Mirror, Some(o)) => format!("Showing {o}"),
                        (Mode::Mirror, None) => String::new(),
                    };
                    (format!("Streaming{size} to the phone"), what)
                } else if log.lost {
                    ("Connection interrupted".to_owned(), "Waiting for the phone…".to_owned())
                } else if log.size.is_some() {
                    ("Connecting…".to_owned(), "The phone answered; starting the stream".to_owned())
                } else {
                    ("Waiting for the phone…".to_owned(), "Smart Connect on the phone opens its display view".to_owned())
                };
                if let Some(e) = &log.error {
                    detail = e.clone();
                    detail_error = true;
                }
                (status, detail)
            }
            (_, Some(r)) => ("Running".into(), format!("Started outside Smart Connect: {}", st.running_opts(r).summary())),
            (_, None) => {
                let detail = if !st.connected {
                    "Connect the phone to start".to_owned()
                } else {
                    match outcome(log) {
                        Some(Outcome::Stopped(r)) => format!("Last session: {r}"),
                        Some(Outcome::Failed(e)) => {
                            detail_error = true;
                            format!("Last session failed: {e}")
                        }
                        None => String::new(),
                    }
                };
                ("Off".into(), detail)
            }
        };
        self.status.set_label(&status);
        self.detail.set_label(&detail);
        self.detail.set_visible(!detail.is_empty());
        if detail_error {
            self.detail.remove_css_class("dim-label");
            self.detail.add_css_class("error");
        } else {
            self.detail.remove_css_class("error");
            self.detail.add_css_class("dim-label");
        }

        // Start / Stop and Apply.
        let restarting = matches!(st.pending, Some(Pending::Restarting { .. }));
        let stopping = matches!(st.pending, Some(Pending::Stopping { .. })) || (logged && log.stopped.is_some());
        let starting = matches!(st.pending, Some(Pending::Starting { .. }));
        let (label, sensitive, destructive) = if restarting {
            ("Restarting…", false, false)
        } else if running.is_some() {
            (if stopping { "Stopping…" } else { "Stop" }, !stopping, true)
        } else if starting {
            ("Starting…", false, false)
        } else {
            ("Start", blocked.is_none(), false)
        };
        self.start.set_label(label);
        self.start.set_sensitive(sensitive);
        self.start.set_tooltip_text(if running.is_none() { blocked.as_deref() } else { None });
        if destructive {
            self.start.remove_css_class("suggested-action");
            self.start.add_css_class("destructive-action");
        } else {
            self.start.remove_css_class("destructive-action");
            self.start.add_css_class("suggested-action");
        }
        // Our own session is compared with everything it was asked for, others by their command line.
        let differs = running.is_some_and(|r| match &st.own {
            Some((pid, asked)) if *pid == r.pid => !asked.same_as(&desired),
            _ => !st.running_opts(r).same_as(&desired),
        });
        self.apply.set_visible(differs && st.pending.is_none() && !stopping);
        self.apply.set_sensitive(blocked.is_none());

        // Settings rows.
        self.mode_row.set_subtitle(if extend {
            "The phone becomes a second monitor next to this screen"
        } else {
            "The phone shows this PC's screen"
        });
        let custom = matches!(desired.size, Size::Fixed(..));
        self.resolution.set_subtitle(match desired.size {
            Size::Native => "Fills the phone's screen (its panel resolution, e.g. 2712×1220)",
            Size::Phone => "What the phone asks for, usually 1920×1080",
            Size::Fixed(..) => "Width and height below",
        });
        self.width.set_visible(custom);
        self.height.set_visible(custom);
        self.scale.set_sensitive(extend);
        self.rotate.set_sensitive(extend);
        self.advanced.set_sensitive(extend);
        self.output.set_subtitle(if d.output.is_empty() {
            "The first virtual (vkms) output, e.g. Virtual-1"
        } else {
            "Switched on at the stream size while sharing, off again afterwards"
        });

        // Setup hints.
        let outputs = if extend { st.outputs.as_ref() } else { None };
        let vkms = matches!(outputs, Some(Ok(list)) if d.output.is_empty() && !list.iter().any(Output::is_virtual));
        let missing = matches!(outputs, Some(Ok(list)) if !d.output.is_empty() && !list.iter().any(|o| o.name == d.output));
        let niri_error = match outputs {
            Some(Err(e)) => Some(e.clone()),
            _ => None,
        };
        self.vkms_hint.set_visible(vkms);
        self.output_hint.set_visible(missing || niri_error.is_some());
        if missing {
            self.output_hint.set_title(&glib::markup_escape_text(&format!("{} is not connected", d.output)));
            self.output_hint.set_subtitle("Plug it in, or choose another output under Advanced.");
        } else if let Some(e) = &niri_error {
            self.output_hint.set_title("Could not list the screens");
            self.output_hint.set_subtitle(&glib::markup_escape_text(&format!("{e}. Extend needs the niri compositor.")));
        }
        self.osk_hint.set_visible(st.osk_missing);
        self.hints.set_visible(vkms || missing || niri_error.is_some() || st.osk_missing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn flags(names: &[&str]) -> Flags {
        Flags { from: None, names: names.iter().map(|s| (*s).to_owned()).collect() }
    }

    /// Three generations of the helper: no keyboard height; `--osk-height`; reads display.json.
    fn oldest() -> Flags {
        flags(&["--mode", "--size", "--display-scale", "--output"])
    }

    fn old_helper() -> Flags {
        flags(&["--mode", "--size", "--display-scale", "--output", "--osk-height"])
    }

    fn new_helper() -> Flags {
        flags(&[
            "--mode", "--size", "--display-scale", "--output", "--osk-height", "--audio", "--no-audio", "--follow-rotation",
            "--no-follow-rotation", "--config", "--no-config", "--replace",
        ])
    }

    #[test]
    fn prefs_to_args_and_back() {
        let mut d = DisplayPrefs::default();
        let o = Opts::from_prefs(&d);
        assert_eq!(o.args(&oldest()), strings(&["--mode", "extend", "--size", "native", "--display-scale", "2"]));
        assert_eq!(o.args(&old_helper()), strings(&["--mode", "extend", "--size", "native", "--display-scale", "2", "--osk-height", "45"]));
        assert_eq!(
            o.args(&new_helper()),
            strings(&[
                "--mode", "extend", "--size", "native", "--display-scale", "2", "--output", "auto", "--osk-height", "45", "--no-audio",
                "--no-follow-rotation",
            ])
        );
        assert!(Opts::parse(&o.args(&oldest()), Opts::cli_defaults()).same_as(&o), "flags an older helper lacks don't count");
        assert!(Opts::parse(&o.args(&new_helper()), Opts::cli_defaults()).same_as(&o), "a newer helper's command line says it all");

        d.scale = 150;
        d.output = "HDMI-A-1".into();
        d.extend_resolution = "custom".into();
        d.custom_width = 1280;
        d.custom_height = 720;
        d.audio = true;
        let o = Opts::from_prefs(&d);
        assert_eq!(
            o.args(&old_helper()),
            strings(&["--mode", "extend", "--size", "1280x720", "--display-scale", "1.5", "--output", "HDMI-A-1", "--osk-height", "45"])
        );
        assert!(o.args(&new_helper()).contains(&AUDIO_FLAG.to_owned()));
        assert!(Opts::parse(&o.args(&new_helper()), Opts::cli_defaults()).same_as(&o));

        // Duplicate: scale and output don't apply; the phone's default size is implicit for older helpers.
        d.mode = "mirror".into();
        let o = Opts::from_prefs(&d);
        assert_eq!(o.args(&oldest()), strings(&["--mode", "mirror"]));
        assert_eq!(
            o.args(&new_helper()),
            strings(&["--mode", "mirror", "--size", "requested", "--osk-height", "45", "--audio", "--no-follow-rotation"])
        );
        assert!(Opts::parse(&o.args(&oldest()), Opts::cli_defaults()).same_as(&o));
        assert!(Opts::parse(&o.args(&new_helper()), Opts::cli_defaults()).same_as(&o));
    }

    #[test]
    fn running_sessions_compare() {
        // Without a settings file a bare start is a duplicate at the phone's size.
        let mut d = DisplayPrefs { mode: "mirror".into(), ..DisplayPrefs::default() };
        assert!(Opts::parse(&[], Opts::cli_defaults()).same_as(&Opts::from_prefs(&d)));
        // `--display-scale 150%` equals 1.5; a missing one is the default 200 %.
        d.mode = "extend".into();
        d.scale = 150;
        let want = Opts::from_prefs(&d);
        let cli = |a: &[&str]| Opts::parse(&strings(a), Opts::cli_defaults());
        assert!(cli(&["--mode", "extend", "--size", "native", "--display-scale", "150%"]).same_as(&want));
        assert!(!cli(&["--mode", "extend", "--size", "native"]).same_as(&want));
        // A different keyboard height on the command line differs; none at all does not.
        assert!(!cli(&["--mode", "extend", "--size", "native", "--display-scale", "1.5", "--osk-height", "60"]).same_as(&want));
        // How it was started is no difference; what it shows is.
        assert!(cli(&["--replace", "--config", "/x.json", "--mode", "extend", "--size", "native", "--display-scale", "1.5"]).same_as(&want));
        let test = cli(&["--mode", "extend", "--size", "native", "--display-scale", "1.5", "--test-pattern"]);
        assert!(!test.same_as(&want));
        assert!(test.summary().contains("--test-pattern"));
        // A newer helper fills the gaps from display.json, i.e. these very choices (a start from the phone).
        assert!(Opts::parse(&strings(&["--replace"]), want.clone()).same_as(&want));
        assert!(!Opts::parse(&strings(&["--mode", "mirror"]), want.clone()).same_as(&want));
        // What this window asked for is compared strictly.
        let mut asked = want.clone();
        asked.audio = Some(true);
        assert!(!asked.same_as(&want));
    }

    #[test]
    fn running_opts_follow_the_helper() {
        let mut st = State::default();
        st.prefs.display.mode = "extend".into();
        let bare = Running { pid: 1, args: Vec::new() };
        assert_eq!(st.running_opts(&bare).mode, Mode::Mirror, "older helper: its own default");
        st.flags = Some(new_helper());
        assert_eq!(st.running_opts(&bare).mode, Mode::Extend, "newer helper: display.json");
        assert_eq!(st.running_opts(&Running { pid: 2, args: strings(&["--no-config"]) }).mode, Mode::Mirror);
    }

    #[test]
    fn log_progress_and_stop_reason() {
        let log = "pid 4242\n\
            asking the phone to start (extend mode, session 243)\n\
            daemon: {\"ok\":false,\"message\":\"phone not connected\"}\n\
            phone panel 2712x1220, asks for 1920x1080@Some(60) (max height Some(1440)); sending 2712x1220\n\
            capturing Virtual-1 (2712x1220), geometry (1920, 0, 1356, 610)\n\
            peer connection: Connecting\n\
            peer connection: Connected\n\
            stream size → 1280x720\n";
        let s = parse_log(log);
        assert_eq!(s.pid, Some(4242));
        assert_eq!(s.size, Some((1280, 720)));
        assert_eq!(s.output.as_deref(), Some("Virtual-1"));
        assert!(s.streaming && !s.lost);
        assert_eq!(s.error, None, "an error before the stream came up is dropped");
        assert!(matches!(outcome(&s), Some(Outcome::Stopped(r)) if r.starts_with("ended")));

        let s = parse_log(&format!("{log}← phone cmd 52: {{\"stopFrom\":0}}\nstopping: the phone stopped screen sharing\n"));
        assert!(matches!(outcome(&s), Some(Outcome::Stopped(r)) if r == "stopped on the phone"));
        let s = parse_log(&format!("{log}stopping: signal\n"));
        assert!(matches!(outcome(&s), Some(Outcome::Stopped(r)) if r == "stopped from this PC"));
        let s = parse_log(&format!("{log}peer connection: Failed\n"));
        assert!(s.lost && !s.streaming);
    }

    #[test]
    fn log_errors() {
        let s = parse_log(
            "pid 7\nasking the phone to start (extend mode, session 8)\n\
             svc 202: no virtual output for extend mode: run `sudo modprobe vkms`\n",
        );
        assert!(s.fatal);
        assert!(matches!(outcome(&s), Some(Outcome::Failed(e)) if e.starts_with("no virtual output")));

        // A newer helper stops itself after a failed offer; the error is still the reason shown.
        let s = parse_log("pid 7\nsvc 202: no virtual output for extend mode\nstopping: could not start the stream\n");
        assert!(matches!(outcome(&s), Some(Outcome::Failed(e)) if e == "no virtual output for extend mode"));

        let s = parse_log("pid 7\nError: is smartconnectd running?\n\nCaused by:\n    No such file or directory (os error 2)\n");
        assert_eq!(s.error.as_deref(), Some("is smartconnectd running? (No such file or directory (os error 2))"));
        assert!(!s.fatal);

        let s = parse_log("pid 7\nError: unknown argument --osk-height\nusage: smartconnect-display [--mode mirror|extend]\n");
        assert_eq!(s.error.as_deref(), Some("unknown argument --osk-height"));

        let s = parse_log("pid 7\nsh: line 1: exec: /nowhere/smartconnect-display: not found\n");
        assert!(s.error.is_some());

        // Not written by the wrapper: ignored.
        assert_eq!(parse_log("stopping: signal\n"), LogState::default());
    }

    #[test]
    fn scales() {
        assert_eq!(parse_scale("1.75"), Some(175));
        assert_eq!(parse_scale("150%"), Some(150));
        assert_eq!(parse_scale("2"), Some(200));
        assert_eq!(parse_scale("x"), None);
    }

    /// The real wrapper, /proc detection, SIGINT and log parsing, against a stand-in helper with a
    /// unique name (so a real smartconnect-display that may be running is never matched or signalled).
    #[test]
    fn stub_session_start_detect_stop() {
        let dir = std::env::temp_dir().join(format!("smartconnect-gui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = format!("fake-display-{}", std::process::id());
        let bin = dir.join(&name);
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             trap 'echo \"stopping: signal\"; exit 0' INT\n\
             echo \"asking the phone to start ($2 mode, session 1)\"\n\
             echo \"phone panel 2712x1220, asks for 1920x1080@Some(60) (max height Some(1440)); sending 2712x1220\"\n\
             echo \"peer connection: Connected\"\n\
             while :; do sleep 0.05; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.join("display.log");
        let opts = Opts::from_prefs(&DisplayPrefs::default());
        let mut child = spawn(&bin, &opts.args(&oldest()), &log).unwrap();
        let pid = child.id();

        let deadline = Instant::now() + Duration::from_secs(5);
        let snap = loop {
            let snap = snapshot(&name, &log, None);
            let streaming = snap.log.as_ref().is_some_and(|(_, l)| l.streaming);
            if streaming && snap.sessions.iter().any(|r| r.pid == pid) {
                break snap;
            }
            assert!(Instant::now() < deadline, "the stand-in never showed up as streaming");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(snap.sessions.iter().find(|r| r.pid == pid).is_some_and(|r| Opts::parse(&r.args, Opts::cli_defaults()).same_as(&opts)));
        let (_, state) = snap.log.unwrap();
        assert_eq!((state.pid, state.size), (Some(pid), Some((2712, 1220))));

        interrupt(pid);
        assert!(child.wait().unwrap().success());
        let snap = snapshot(&name, &log, None);
        assert!(snap.sessions.is_empty());
        let (_, state) = snap.log.unwrap();
        assert!(matches!(outcome(&state), Some(Outcome::Stopped(r)) if r == "stopped from this PC"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_text_flags() {
        let names = flag_names("usage: x [--mode mirror|extend] [--osk-height PCT]\n  --size WxH  stream size; 'native'");
        assert!(names.contains("--osk-height") && names.contains("--mode") && names.contains("--size"));
        assert!(!names.contains(AUDIO_FLAG));
    }
}
