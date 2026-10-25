//! Options: defaults from the shared settings file, then command-line flags on top.
//!
//! The GUI writes `$XDG_CONFIG_HOME/smartconnect-linux/display.json` (usually
//! `~/.config/smartconnect-linux/display.json`); every key is optional:
//!
//! ```json
//! {"mode":"extend","size":"native","displayScale":2.0,"oskHeight":45,"output":null,
//!  "audio":false,"followRotation":false}
//! ```
//!
//! - `mode`: `"extend"` or `"mirror"`; `size`: `"native"`, `"requested"` (what the phone asks for) or
//!   `"WxH"`; `displayScale`: a factor (0.5–4) or a percentage (50–400, or `"150%"`); `oskHeight`:
//!   15–80 (% of the shared screen); `output`: `null` (a new virtual screen; on niri the first
//!   `Virtual-*` output) or a connector name;
//!   `audio`, `followRotation`: booleans; `bitrate`: kbit/s (500–50000); `fps`: 10–60.
//! - Unknown keys are ignored and a bad value is reported and skipped, so a file written by a newer
//!   or older GUI never stops a phone-initiated start.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub const USAGE: &str = "usage: smartconnect-display [--mode mirror|extend] [--side SIDE] [--output NAME|auto]
                            [--size WxH|native|requested] [--display-scale N] [--osk-height PCT]
                            [--audio|--no-audio] [--follow-rotation|--no-follow-rotation]
                            [--test-pattern [--pattern NAME]] [--scale N] [--x264] [--bitrate KBPS] [--fps N]
                            [--config PATH|--no-config] [--replace]
       smartconnect-display --list-outputs
  Shows this PC's screen on the phone (Smart Connect svc 202). Needs smartconnectd running and the phone connected.
  Works on GNOME, KDE Plasma, Hyprland, sway and niri ($SMARTCONNECT_DESKTOP overrides the detection).
  Defaults come from ~/.config/smartconnect-linux/display.json (written by the GUI); flags override it.
  --mode          mirror (default: the focused output) or extend (the phone becomes a second monitor:
                  a virtual screen on GNOME/KDE/Hyprland/sway, a spare output such as vkms Virtual-1 on niri)
  --side SIDE     extend: left, right, top or bottom of the main screen (default: where the phone
                  was put on the GTK Cross Control page, ~/.config/smartconnect-linux/crosscontrol.json,
                  else right; a running session follows changes to that file)
  --output NAME   extend: use this connected output instead (e.g. an HDMI dummy plug; default/auto:
                  a new virtual screen, or on niri the first Virtual-* output)
  --size WxH      stream size (default/requested: what the phone asks for, usually 1920x1080);
                  'native' = the phone's own panel, landscape (e.g. 2712x1220, no letterboxing)
  --display-scale N  extend: scale of the phone's monitor, e.g. 1, 1.5, 2 or 150%
                  (default 2 = 200%, like Windows; bigger = larger text, less space; GNOME rounds it
                  to a scale it supports for the size)
  --osk-height PCT  height of the on-screen keyboard shown by the phone's keyboard button,
                  in % of the shared screen (default 45, like a phone keyboard; needs wvkbd)
  --audio         also send what this PC plays (default output's monitor, Opus) to the phone;
                  the phone's audio switch then mutes/unmutes it (experimental)
  --follow-rotation  extend: turn the phone's monitor portrait/landscape when the phone is
                  rotated (experimental)
  --test-pattern  send a moving test pattern instead of capturing the screen (no input injection)
  --pattern NAME  videotestsrc pattern for --test-pattern (default ball; smpte shows the frame edges)
  --scale N       setScale sent to the phone (Windows DPI %, default 100)
  --x264          force the software encoder (default: vah264enc when available)
  --bitrate KBPS  stream quality: video bitrate, 500-50000 (default 8000); lower = less data per
                  frame, which can cut the delay on a busy Wi-Fi
  --fps N         most frames per second sent, 10-60 (default 60); fewer = less to encode and send
  --config PATH   read defaults from PATH instead; --no-config ignores the settings file
  --replace       stop a running smartconnect-display first (default: refuse to start a second one)
  --list-outputs  print this desktop's outputs and how Extend works here as JSON, then exit";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    Mirror,
    Extend,
}

impl Share {
    pub fn as_str(self) -> &'static str {
        match self {
            Share::Mirror => "mirror",
            Share::Extend => "extend",
        }
    }

    pub fn parse(s: &str) -> Option<Share> {
        match s.trim() {
            "mirror" => Some(Share::Mirror),
            "extend" => Some(Share::Extend),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizePref {
    /// What the phone asks for in cmd 5 (`widthReq x heightReq`).
    Requested,
    /// The phone's panel, landscape.
    Native,
    Fixed(u32, u32),
}

impl SizePref {
    pub fn parse(v: &str) -> Result<SizePref> {
        match v.trim() {
            "native" => Ok(SizePref::Native),
            // "phone" is how the GUI calls it.
            "requested" | "phone" | "auto" => Ok(SizePref::Requested),
            s => {
                let (w, h) = s.split_once(['x', 'X']).context("size: use WxH, native or requested")?;
                let (w, h): (u32, u32) = (w.trim().parse()?, h.trim().parse()?);
                if !(16..=8192).contains(&w) || !(16..=8192).contains(&h) {
                    bail!("size {s}: each side must be 16–8192");
                }
                Ok(SizePref::Fixed(w, h))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Opts {
    pub mode: Share,
    pub output: Option<String>,
    pub size: SizePref,
    pub test_pattern: Option<String>,
    /// Extend: scale of the phone's output (compositor scale, e.g. 2.0 = 200 %).
    pub display_scale: f64,
    /// On-screen keyboard height, % of the shared screen.
    pub osk_height: u32,
    /// `setScale` sent to the phone.
    pub scale: i64,
    pub force_x264: bool,
    pub bitrate: u32,
    /// Most frames per second sent (`--fps`).
    pub fps: u32,
    /// Send the PC's audio (opt-in).
    pub audio: bool,
    /// Extend: follow the phone's rotation (cmd 7) with a portrait/landscape output (opt-in).
    pub follow_rotation: bool,
    /// Stop a running instance instead of refusing to start.
    pub replace: bool,
    /// Extend: which side of the main screen (`--side`; else the Cross Control placement file).
    pub side: Option<crate::desktop::Edge>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            mode: Share::Mirror,
            output: None,
            size: SizePref::Requested,
            test_pattern: None,
            display_scale: 2.0,
            osk_height: 45,
            scale: 100,
            force_x264: false,
            bitrate: 8000,
            fps: 60,
            audio: false,
            follow_rotation: false,
            replace: false,
            side: None,
        }
    }
}

/// `$XDG_CONFIG_HOME/smartconnect-linux/display.json`, else `~/.config/smartconnect-linux/display.json`.
pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("smartconnect-linux")
        .join("display.json")
}

/// "1.5", "150%" or 150 → 1.5 (compositors accept roughly 0.5–10; keep it to sane display scales).
pub fn parse_display_scale(v: &str) -> Result<f64> {
    let s = match v.trim().strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>()? / 100.0,
        None => {
            let n = v.trim().parse::<f64>()?;
            // A bare 50–400 can only be a percentage (factors stop at 4).
            if (50.0..=400.0).contains(&n) { n / 100.0 } else { n }
        }
    };
    if !(0.5..=4.0).contains(&s) {
        bail!("display scale {v}: use a value between 0.5 and 4 (or 50%–400%)");
    }
    Ok(s)
}

pub fn parse_bitrate(v: &str) -> Result<u32> {
    let kbps: u32 = v.trim().parse()?;
    if !(500..=50_000).contains(&kbps) {
        bail!("bitrate {v}: use 500–50000 (kbit/s)");
    }
    Ok(kbps)
}

pub fn parse_fps(v: &str) -> Result<u32> {
    let fps: u32 = v.trim().parse()?;
    if !(10..=60).contains(&fps) {
        bail!("frame rate {v}: use 10–60");
    }
    Ok(fps)
}

pub fn parse_osk_height(v: &str) -> Result<u32> {
    let pct: u32 = v.trim().trim_end_matches('%').trim().parse()?;
    if !(15..=80).contains(&pct) {
        bail!("on-screen keyboard height {v}: use 15–80 (% of the screen)");
    }
    Ok(pct)
}

/// Applies the settings file onto `o`. Returns one warning per value it could not use.
pub fn apply_file(o: &mut Opts, v: &Value) -> Vec<String> {
    let mut warn = Vec::new();
    let Some(map) = v.as_object() else {
        return vec!["not a JSON object; ignoring it".into()];
    };
    let mut bad = |k: &str, v: &Value, why: String| warn.push(format!("ignoring {k}: {v} ({why})"));
    for (k, val) in map {
        match k.as_str() {
            "mode" => match val.as_str().and_then(Share::parse) {
                Some(m) => o.mode = m,
                None => bad(k, val, "use \"extend\" or \"mirror\"".into()),
            },
            "size" => match val.as_str().map(SizePref::parse) {
                Some(Ok(s)) => o.size = s,
                Some(Err(e)) => bad(k, val, format!("{e:#}")),
                None => bad(k, val, "use \"native\", \"requested\" or \"WxH\"".into()),
            },
            "displayScale" => {
                let parsed = match val {
                    Value::Number(n) => n.as_f64().map(|f| parse_display_scale(&f.to_string())),
                    Value::String(s) => Some(parse_display_scale(s)),
                    _ => None,
                };
                match parsed {
                    Some(Ok(s)) => o.display_scale = s,
                    Some(Err(e)) => bad(k, val, format!("{e:#}")),
                    None => bad(k, val, "use a number such as 2.0".into()),
                }
            }
            "oskHeight" => {
                let parsed = match val {
                    Value::Number(n) => Some(parse_osk_height(&n.to_string())),
                    Value::String(s) => Some(parse_osk_height(s)),
                    _ => None,
                };
                match parsed {
                    Some(Ok(p)) => o.osk_height = p,
                    Some(Err(e)) => bad(k, val, format!("{e:#}")),
                    None => bad(k, val, "use a number such as 45".into()),
                }
            }
            "output" => match val {
                Value::Null => o.output = None,
                Value::String(s) if s.trim().is_empty() || s.trim() == "auto" => o.output = None,
                Value::String(s) => o.output = Some(s.trim().to_owned()),
                _ => bad(k, val, "use null or an output name".into()),
            },
            "bitrate" => match val.as_u64().map(|n| parse_bitrate(&n.to_string())) {
                Some(Ok(b)) => o.bitrate = b,
                Some(Err(e)) => bad(k, val, format!("{e:#}")),
                None => bad(k, val, "use a number of kbit/s such as 8000".into()),
            },
            "fps" => match val.as_u64().map(|n| parse_fps(&n.to_string())) {
                Some(Ok(f)) => o.fps = f,
                Some(Err(e)) => bad(k, val, format!("{e:#}")),
                None => bad(k, val, "use a number such as 30".into()),
            },
            "audio" => match val.as_bool() {
                Some(b) => o.audio = b,
                None => bad(k, val, "use true or false".into()),
            },
            "followRotation" => match val.as_bool() {
                Some(b) => o.follow_rotation = b,
                None => bad(k, val, "use true or false".into()),
            },
            _ => {} // newer GUI fields this binary does not know
        }
    }
    warn
}

/// Where the defaults come from, picked out of the arguments before anything else.
enum Source {
    Default,
    Path(PathBuf),
    None,
}

fn config_source(args: &[String]) -> Result<Source> {
    let mut src = Source::Default;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => src = Source::Path(PathBuf::from(it.next().context("--config PATH")?)),
            "--no-config" => src = Source::None,
            _ => {}
        }
    }
    Ok(src)
}

/// Reads the settings file (silently nothing when it doesn't exist).
fn load_file(path: &std::path::Path, explicit: bool) -> Option<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("{}: not valid JSON ({e}); using the built-in defaults", path.display());
                None
            }
        },
        Err(e) if explicit || e.kind() != std::io::ErrorKind::NotFound => {
            eprintln!("{}: {e}; using the built-in defaults", path.display());
            None
        }
        Err(_) => None,
    }
}

/// Parses the process arguments (with the settings file underneath). `None` = `--help` was shown.
pub fn parse_args() -> Result<Option<Opts>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = match config_source(&args)? {
        Source::Default => {
            let p = config_path();
            load_file(&p, false).map(|v| (p, v))
        }
        Source::Path(p) => load_file(&p, true).map(|v| (p, v)),
        Source::None => None,
    };
    let (opts, warnings) = parse_with(&args, file.as_ref().map(|(_, v)| v))?;
    if let (Some((p, _)), Some(_)) = (&file, &opts) {
        println!("defaults from {}", p.display());
        for w in warnings {
            eprintln!("{}: {w}", p.display());
        }
    }
    Ok(opts)
}

/// `args` (without the program name) over the settings file `file`.
pub fn parse_with(args: &[String], file: Option<&Value>) -> Result<(Option<Opts>, Vec<String>)> {
    let mut o = Opts::default();
    let warnings = file.map(|v| apply_file(&mut o, v)).unwrap_or_default();
    let mut pattern = "ball".to_owned();
    let mut test = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| it.next().cloned().with_context(|| format!("{a} {what}"));
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok((None, warnings));
            }
            "--list-outputs" => {
                println!("{}", crate::desktop::list_json());
                return Ok((None, warnings));
            }
            "--mode" => o.mode = Share::parse(&val("mirror|extend")?).context("--mode needs mirror|extend")?,
            "--output" => {
                let v = val("NAME")?;
                o.output = (v != "auto" && !v.trim().is_empty()).then(|| v.trim().to_owned());
            }
            "--size" => o.size = SizePref::parse(&val("WxH|native|requested")?)?,
            "--test-pattern" => test = true,
            "--pattern" => pattern = val("NAME")?,
            "--display-scale" => o.display_scale = parse_display_scale(&val("N")?)?,
            "--scale" => o.scale = val("N")?.parse()?,
            "--x264" => o.force_x264 = true,
            "--bitrate" => o.bitrate = parse_bitrate(&val("KBPS")?)?,
            "--fps" => o.fps = parse_fps(&val("N")?)?,
            "--osk-height" => o.osk_height = parse_osk_height(&val("PCT")?)?,
            "--audio" => o.audio = true,
            "--no-audio" => o.audio = false,
            "--follow-rotation" => o.follow_rotation = true,
            "--no-follow-rotation" => o.follow_rotation = false,
            "--replace" => o.replace = true,
            "--side" => o.side = Some(crate::desktop::Edge::parse(&val("left|right|top|bottom")?).context("--side needs left, right, top or bottom")?),
            "--config" => {
                val("PATH")?; // read by parse_args before the file is loaded
            }
            "--no-config" => {}
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    if test {
        o.test_pattern = Some(pattern);
    }
    Ok((Some(o), warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn display_scale_accepts_factors_and_percentages() {
        assert_eq!(parse_display_scale("1.5").unwrap(), 1.5);
        assert_eq!(parse_display_scale("175%").unwrap(), 1.75);
        assert_eq!(parse_display_scale("200").unwrap(), 2.0);
        assert!(parse_display_scale("12").is_err());
        assert!(parse_display_scale("0.2").is_err());
    }

    #[test]
    fn file_values_become_defaults() {
        let file = json!({"mode":"extend","size":"native","displayScale":1.5,"oskHeight":40,
                          "output":"HDMI-A-1","audio":true,"followRotation":true,"somethingNew":1});
        let (o, warn) = parse_with(&[], Some(&file)).unwrap();
        let o = o.unwrap();
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(o.mode, Share::Extend);
        assert_eq!(o.size, SizePref::Native);
        assert_eq!(o.display_scale, 1.5);
        assert_eq!(o.osk_height, 40);
        assert_eq!(o.output.as_deref(), Some("HDMI-A-1"));
        assert!(o.audio && o.follow_rotation);
    }

    #[test]
    fn flags_override_the_file() {
        let file = json!({"mode":"extend","size":"1280x720","audio":true,"followRotation":true,"output":"HDMI-A-1"});
        let a = args(&["--mode", "mirror", "--size", "requested", "--no-audio", "--no-follow-rotation", "--output", "auto"]);
        let o = parse_with(&a, Some(&file)).unwrap().0.unwrap();
        assert_eq!(o.mode, Share::Mirror);
        assert_eq!(o.size, SizePref::Requested);
        assert!(!o.audio && !o.follow_rotation);
        assert_eq!(o.output, None);
        let o = parse_with(&args(&["--size", "1920x1080", "--audio", "--display-scale", "150%"]), None).unwrap().0.unwrap();
        assert_eq!(o.size, SizePref::Fixed(1920, 1080));
        assert!(o.audio);
        assert_eq!(o.display_scale, 1.5);
    }

    #[test]
    fn bad_file_values_are_skipped_with_a_warning() {
        let file = json!({"mode":"sideways","size":"huge","displayScale":"x","oskHeight":5,"output":3,
                          "audio":"yes","followRotation":null,"displayScale2":1});
        let (o, warn) = parse_with(&[], Some(&file)).unwrap();
        assert_eq!(o.unwrap(), Opts::default());
        assert_eq!(warn.len(), 7, "{warn:?}");
        // GUI spellings and empties.
        let o = parse_with(&[], Some(&json!({"size":"phone","output":"","displayScale":200}))).unwrap().0.unwrap();
        assert_eq!((o.size, o.output, o.display_scale), (SizePref::Requested, None, 2.0));
        assert_eq!(apply_file(&mut Opts::default(), &json!([1])).len(), 1);
    }

    #[test]
    fn cli_errors_and_help() {
        assert!(parse_with(&args(&["--mode", "sideways"]), None).is_err());
        assert!(parse_with(&args(&["--osk-height", "90"]), None).is_err());
        assert!(parse_with(&args(&["--size"]), None).is_err());
        assert!(parse_with(&args(&["--bogus"]), None).is_err());
        assert!(parse_with(&args(&["--help"]), None).unwrap().0.is_none());
        let o = parse_with(&args(&["--test-pattern", "--pattern", "smpte", "--config", "/x.json", "--replace"]), None)
            .unwrap()
            .0
            .unwrap();
        assert_eq!(o.test_pattern.as_deref(), Some("smpte"));
        assert!(o.replace);
    }

    #[test]
    fn config_path_follows_xdg() {
        // Only the shape: the env is shared by parallel tests, so don't modify it here.
        assert!(config_path().ends_with("smartconnect-linux/display.json"));
    }
}
