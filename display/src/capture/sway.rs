//! sway: wlr-screencopy for the pixels (`screencopy.rs`), the wlroots virtual pointer for input,
//! and `swaymsg` for outputs.
//!
//! Extend creates a headless output (`swaymsg create_output`, which names it `HEADLESS-N`), sets
//! `mode --custom WxH@60Hz position X Y scale S` and unplugs it again on Drop (`swaymsg output
//! HEADLESS-N unplug`). A named real output (`--output`, e.g. a dummy plug) gets the same settings
//! and is disabled afterwards. After a SIGKILL the output stays: `swaymsg output HEADLESS-N unplug`.
//! Not verified on a real sway session yet.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::screencopy::{Feeder, Plain};
use super::{Backend, Mode};
use crate::desktop::{self, Output};

pub fn outputs() -> Result<Vec<Output>> {
    parse_outputs(&desktop::run_tool(&["swaymsg", "-t", "get_outputs", "-r"])?)
}

fn parse_outputs(json: &str) -> Result<Vec<Output>> {
    let v: Value = serde_json::from_str(json).context("parsing `swaymsg -t get_outputs`")?;
    let list = v
        .as_array()
        .ok_or_else(|| anyhow!("swaymsg outputs: not a list"))?;
    Ok(list
        .iter()
        .map(|o| {
            let s = |k: &str| {
                o.get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty() && *s != "Unknown")
            };
            let active = o.get("active").and_then(Value::as_bool).unwrap_or(false);
            let r = |k: &str| {
                o.pointer(&format!("/rect/{k}"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
            };
            let name = s("name").unwrap_or_default().to_owned();
            Output {
                description: [s("make"), s("model")]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
                logical: (active && r("width") > 0).then(|| {
                    (
                        r("x") as i32,
                        r("y") as i32,
                        r("width") as u32,
                        r("height") as u32,
                    )
                }),
                focused: o.get("focused").and_then(Value::as_bool).unwrap_or(false),
                is_virtual: name.starts_with("HEADLESS-"),
                name,
            }
        })
        .collect())
}

fn swaymsg(args: &[&str]) -> Result<()> {
    let mut argv = vec!["swaymsg"];
    argv.extend_from_slice(args);
    desktop::run_tool(&argv).map(|_| ())
}

pub struct SwayCapture {
    feeder: Feeder,
    output: String,
    geometry: (i32, i32, u32, u32),
    pos: (i32, i32),
    scale: f64,
    /// We created the headless output (unplug it), or it is a real one we enabled (disable it).
    created: bool,
}

impl Backend for SwayCapture {
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
        true
    }

    fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        self.pos = desktop::spot(
            &outputs()?,
            Some(&self.output),
            desktop::logical_size((width, height), self.scale),
        );
        configure(&self.output, (width, height), self.pos, self.scale)?;
        self.geometry = wait_for(&self.feeder, &self.output, (width, height))?;
        Ok(())
    }

    fn bind(&self, bin: &gst::Element) {
        self.feeder.bind(bin);
    }

    fn reposition(&mut self) -> Result<bool> {
        let size = self.size();
        self.reconfigure_extend(size.0, size.1)?;
        Ok(true)
    }
}

impl Drop for SwayCapture {
    fn drop(&mut self) {
        let r = swaymsg(&[
            "output",
            &self.output,
            if self.created { "unplug" } else { "disable" },
        ]);
        if let Err(e) = r {
            eprintln!("capture: could not remove {}: {e:#}", self.output);
        }
    }
}

fn configure(name: &str, (w, h): (u32, u32), (x, y): (i32, i32), scale: f64) -> Result<()> {
    swaymsg(&[
        "output",
        name,
        "enable",
        "mode",
        "--custom",
        &format!("{w}x{h}@60Hz"),
        "position",
        &x.to_string(),
        &y.to_string(),
        "scale",
        &scale.to_string(),
    ])
}

/// Waits until frames come in at `size`; returns the output's logical rect.
fn wait_for(feeder: &Feeder, name: &str, size: (u32, u32)) -> Result<(i32, i32, u32, u32)> {
    if !feeder.wait_for_size(size, Duration::from_secs(3)) {
        bail!("{name} did not change to {}x{}", size.0, size.1);
    }
    outputs()?
        .into_iter()
        .find(|o| o.name == name)
        .and_then(|o| o.logical)
        .ok_or_else(|| anyhow!("{name} is off"))
}

pub fn start(mode: &Mode) -> Result<Box<dyn Backend>> {
    let outs = outputs()?;
    let (width, height, wanted, scale) = match mode {
        Mode::Mirror => {
            let out = desktop::primary(&outs, None)
                .ok_or_else(|| anyhow!("no enabled output to mirror"))?;
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
        Some(n) if outs.iter().any(|o| o.name == n) => (n, false),
        Some(n) => {
            let names: Vec<_> = outs.iter().map(|o| o.name.as_str()).collect();
            bail!(
                "output {n:?} is not connected (sway sees: {})",
                names.join(", ")
            )
        }
        None => {
            swaymsg(&["create_output"]).context("creating a headless output")?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let new = outputs()?
                    .into_iter()
                    .find(|o| o.is_virtual && !outs.iter().any(|b| b.name == o.name));
                match new {
                    Some(o) => break (o.name, true),
                    None if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100))
                    }
                    None => bail!("`swaymsg create_output` made no new HEADLESS output"),
                }
            }
        }
    };
    let pos = desktop::spot(
        &outs,
        Some(&name),
        desktop::logical_size((width, height), scale),
    );
    let undo = |name: &str| {
        let _ = swaymsg(&["output", name, if created { "unplug" } else { "disable" }]);
    };
    let feeder =
        match configure(&name, (width, height), pos, scale).and_then(|_| Feeder::start(&name)) {
            Ok(f) => f,
            Err(e) => {
                undo(&name);
                return Err(e);
            }
        };
    // From here on, dropping `cap` removes the output again.
    let mut cap = SwayCapture {
        feeder,
        output: name,
        geometry: (pos.0, pos.1, width, height),
        pos,
        scale,
        created,
    };
    cap.geometry = wait_for(&cap.feeder, &cap.output, (width, height))?;
    Ok(Box::new(cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_swaymsg_outputs() {
        let json = r#"[{"name":"eDP-1","make":"HKC","model":"Unknown","active":true,"focused":true,
              "rect":{"x":0,"y":0,"width":1536,"height":864},"scale":1.25},
            {"name":"HEADLESS-1","make":"headless","model":"headless","active":true,"focused":false,
              "rect":{"x":1536,"y":0,"width":1356,"height":610},"scale":2.0},
            {"name":"HDMI-A-1","make":"Dummy","model":"","active":false,"rect":{"x":0,"y":0,"width":0,"height":0}}]"#;
        let o = parse_outputs(json).unwrap();
        assert_eq!(o[0].description, "HKC");
        assert_eq!(o[1].logical, Some((1536, 0, 1356, 610)));
        assert!(o[1].is_virtual && o[2].logical.is_none());
        assert_eq!(desktop::primary(&o, None).unwrap().name, "eDP-1");
    }
}
