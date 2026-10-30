//! Hyprland: wlr-screencopy for the pixels (`screencopy.rs`), the wlroots virtual pointer for input,
//! and `hyprctl` for outputs.
//!
//! Extend creates a headless output (`hyprctl output create headless SMARTCONNECT-1`), sets its
//! mode, position (the side the user chose, `desktop::Placement`) and scale with a runtime monitor rule (`hyprctl keyword monitor
//! NAME,WxH@60,XxY,SCALE`; Hyprland takes any size for headless outputs, portrait included) and
//! removes it again on Drop (`hyprctl output remove NAME`). A named real output (`--output`, e.g. a
//! dummy plug) gets the same rule and is disabled afterwards. Hyprland applies a rule to a headless
//! output only when it next renders it, which the running screencopy makes happen. After a SIGKILL
//! the output stays: `hyprctl output remove SMARTCONNECT-1`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::screencopy::{Feeder, Plain};
use super::{Backend, Mode};
use crate::desktop::{self, Output};

/// Name of the headless output we create.
pub const HEADLESS: &str = "SMARTCONNECT-1";

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    /// Mode size in pixels (before the transform).
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: u32,
    pub focused: bool,
    pub disabled: bool,
}

impl Monitor {
    /// Logical rect: pixels / scale, turned by 90°/270° transforms.
    fn logical(&self) -> Option<(i32, i32, u32, u32)> {
        if self.disabled {
            return None;
        }
        let (w, h) = if self.transform % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        Some((
            self.x,
            self.y,
            (f64::from(w) / s).round() as u32,
            (f64::from(h) / s).round() as u32,
        ))
    }

    pub fn output(&self) -> Output {
        Output {
            name: self.name.clone(),
            description: self.description.clone(),
            logical: self.logical(),
            focused: self.focused,
            is_virtual: self.name.starts_with("HEADLESS") || self.name == HEADLESS,
        }
    }
}

pub fn outputs() -> Result<Vec<Monitor>> {
    parse_monitors(&desktop::run_tool(&["hyprctl", "-j", "monitors", "all"])?)
}

fn parse_monitors(json: &str) -> Result<Vec<Monitor>> {
    let v: Value = serde_json::from_str(json).context("parsing `hyprctl -j monitors all`")?;
    let list = v
        .as_array()
        .ok_or_else(|| anyhow!("hyprctl monitors: not a list"))?;
    Ok(list
        .iter()
        .map(|m| {
            let i = |k: &str| m.get(k).and_then(Value::as_i64).unwrap_or(0);
            let s = |k: &str| {
                m.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let b = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
            Monitor {
                name: s("name"),
                description: s("description"),
                x: i("x") as i32,
                y: i("y") as i32,
                width: i("width").max(0) as u32,
                height: i("height").max(0) as u32,
                scale: m.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                transform: i("transform").max(0) as u32,
                focused: b("focused"),
                disabled: b("disabled"),
            }
        })
        .collect())
}

fn hyprctl(args: &[&str]) -> Result<()> {
    let mut argv = vec!["hyprctl"];
    argv.extend_from_slice(args);
    let out = desktop::run_tool(&argv)?;
    // hyprctl exits 0 even when a command is refused; it answers "ok" when it was accepted.
    if !out.trim().eq_ignore_ascii_case("ok") {
        bail!("`{}` answered: {}", argv.join(" "), out.trim());
    }
    Ok(())
}

pub struct HyprCapture {
    feeder: Feeder,
    output: String,
    geometry: (i32, i32, u32, u32),
    extend: Option<Extend>,
}

struct Extend {
    pos: (i32, i32),
    scale: f64,
    /// We created the headless output (remove it), or it is a real one we enabled (disable it).
    created: bool,
}

impl Backend for HyprCapture {
    fn source_description(&self) -> String {
        self.feeder.source_description()
    }

    fn size(&self) -> (u32, u32) {
        self.feeder
            .size()
            .unwrap_or((self.geometry.2, self.geometry.3))
    }

    fn geometry(&self) -> (i32, i32, u32, u32) {
        self.geometry
    }

    fn output_name(&self) -> String {
        self.output.clone()
    }

    fn is_extend(&self) -> bool {
        self.extend.is_some()
    }

    fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        let Some(e) = &mut self.extend else {
            bail!("only an extend capture can be resized")
        };
        // The new size moves the spot along the edge (centred where the user put the phone).
        let outs: Vec<Output> = outputs()?.iter().map(Monitor::output).collect();
        e.pos = desktop::spot(
            &outs,
            Some(&self.output),
            desktop::logical_size((width, height), e.scale),
        );
        self.geometry = apply_rule(&self.feeder, &self.output, (width, height), e.pos, e.scale)?;
        Ok(())
    }

    fn reposition(&mut self) -> Result<bool> {
        let size = self.size();
        self.reconfigure_extend(size.0, size.1)?;
        Ok(true)
    }

    fn bind(&self, bin: &gst::Element) {
        self.feeder.bind(bin);
    }
}

impl Drop for HyprCapture {
    fn drop(&mut self) {
        let Some(e) = &self.extend else { return };
        let r = if e.created {
            hyprctl(&["output", "remove", &self.output])
        } else {
            hyprctl(&["keyword", "monitor", &format!("{},disable", self.output)])
        };
        if let Err(err) = r {
            eprintln!("capture: could not remove {}: {err:#}", self.output);
        }
    }
}

pub fn start(mode: &Mode) -> Result<Box<dyn Backend>> {
    let mons = outputs()?;
    let outs: Vec<Output> = mons.iter().map(Monitor::output).collect();
    let (width, height, wanted, scale) = match mode {
        Mode::Mirror => {
            let out = desktop::primary(&outs, None)
                .ok_or_else(|| anyhow!("no enabled monitor to mirror"))?;
            return Ok(Box::new(Plain::start(out)?));
        }
        Mode::Extend {
            width,
            height,
            output,
            scale,
        } => (*width, *height, output.clone(), scale.unwrap_or(1.0)),
    };
    let (name, created) = match wanted {
        Some(n) if mons.iter().any(|m| m.name == n) => (n, false),
        Some(n) => {
            let names: Vec<_> = mons.iter().map(|m| m.name.as_str()).collect();
            bail!(
                "output {n:?} is not connected (Hyprland sees: {})",
                names.join(", ")
            )
        }
        None => {
            // A headless output left behind by a killed session is ours to reuse.
            if !mons.iter().any(|m| m.name == HEADLESS) {
                hyprctl(&["output", "create", "headless", HEADLESS])
                    .context("creating a headless output")?;
            }
            (HEADLESS.to_owned(), true)
        }
    };
    let pos = desktop::spot(
        &outs,
        Some(&name),
        desktop::logical_size((width, height), scale),
    );
    let undo = |name: &str| {
        let _ = if created {
            hyprctl(&["output", "remove", name])
        } else {
            hyprctl(&["keyword", "monitor", &format!("{name},disable")])
        };
    };
    // The first rule before the capture starts (the output must be enabled to be captured); the
    // running capture then lets Hyprland apply it.
    let feeder = match hyprctl(&[
        "keyword",
        "monitor",
        &rule_text(&name, (width, height), pos, scale),
    ])
    .and_then(|_| Feeder::start(&name))
    {
        Ok(f) => f,
        Err(e) => {
            undo(&name);
            return Err(e);
        }
    };
    // From here on, dropping `cap` removes the output again.
    let mut cap = HyprCapture {
        feeder,
        output: name,
        geometry: (pos.0, pos.1, width, height),
        extend: Some(Extend {
            pos,
            scale,
            created,
        }),
    };
    cap.geometry = apply_rule(&cap.feeder, &cap.output, (width, height), pos, scale)?;
    Ok(Box::new(cap))
}

fn rule_text(name: &str, (w, h): (u32, u32), (x, y): (i32, i32), scale: f64) -> String {
    format!("{name},{w}x{h}@60,{x}x{y},{scale}")
}

/// Applies the monitor rule and waits until frames (and hyprctl) show the new size. Returns the
/// output's logical rect.
fn apply_rule(
    feeder: &Feeder,
    name: &str,
    size: (u32, u32),
    pos: (i32, i32),
    scale: f64,
) -> Result<(i32, i32, u32, u32)> {
    hyprctl(&["keyword", "monitor", &rule_text(name, size, pos, scale)])?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mon = outputs()?.into_iter().find(|m| m.name == name);
        if let Some(m) = mon.as_ref().filter(|m| (m.width, m.height) == size)
            && feeder.wait_for_size(size, Duration::from_millis(500))
        {
            if (m.scale - scale).abs() > 0.01 {
                eprintln!(
                    "capture: Hyprland gave {name} scale {} instead of {scale}",
                    m.scale
                );
            }
            return m.logical().ok_or_else(|| anyhow!("{name} is disabled"));
        }
        if Instant::now() >= deadline {
            let now = mon
                .map(|m| format!("{}x{}", m.width, m.height))
                .unwrap_or_else(|| "gone".into());
            bail!(
                "{name} did not change to {}x{} (it is {now})",
                size.0,
                size.1
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::tests::run_bound;

    #[test]
    fn parses_hyprctl_monitors() {
        let json = r#"[{"id":0,"name":"eDP-1","description":"HKC 0x9051","width":1920,"height":1080,"x":0,"y":0,
            "scale":1.25,"transform":0,"focused":true,"disabled":false},
            {"id":1,"name":"SMARTCONNECT-1","description":"","width":2712,"height":1220,"x":1536,"y":0,
            "scale":2,"transform":1,"focused":false,"disabled":false}]"#;
        let m = parse_monitors(json).unwrap();
        assert_eq!(m[0].output().logical, Some((0, 0, 1536, 864)));
        // transform 1 = 90°: portrait logical rect.
        assert_eq!(m[1].output().logical, Some((1536, 0, 610, 1356)));
        assert!(m[1].output().is_virtual && !m[0].output().is_virtual);
        assert_eq!(
            rule_text("X", (2712, 1220), (1536, 0), 2.0),
            "X,2712x1220@60,1536x0,2"
        );
    }

    /// Live (Hyprland, also nested): cargo test --release -p smartconnect-display hyprland_smoke -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn hyprland_smoke_extend() {
        let mut cap = start(&Mode::Extend {
            width: 2712,
            height: 1220,
            output: None,
            scale: Some(2.0),
        })
        .expect("start");
        eprintln!(
            "{} {:?} {:?}",
            cap.output_name(),
            cap.size(),
            cap.geometry()
        );
        assert_eq!(cap.size(), (2712, 1220));
        assert_eq!((cap.geometry().2, cap.geometry().3), (1356, 610));
        let check = |cap: &dyn Backend, (w, h): (u32, u32)| {
            let desc = cap.source_description();
            assert!(desc.contains(&format!("width={w},height={h}")), "{desc}");
        };
        check(cap.as_ref(), (2712, 1220));
        cap.reconfigure_extend(1220, 2712).expect("portrait");
        check(cap.as_ref(), (1220, 2712));
        assert_eq!((cap.geometry().2, cap.geometry().3), (610, 1356));
        drop(cap);
        assert!(
            !outputs().unwrap().iter().any(|m| m.name == HEADLESS),
            "headless output removed"
        );
    }

    /// Live (Hyprland): frames reach a pipeline through the appsrc.
    #[test]
    #[ignore]
    fn hyprland_smoke_frames() {
        let cap = start(&Mode::Extend {
            width: 1280,
            height: 720,
            output: None,
            scale: Some(1.0),
        })
        .expect("start");
        let (n, caps) = run_bound(cap.as_ref());
        eprintln!("{n} buffers; {caps}");
        assert!(n > 0);
    }
}
