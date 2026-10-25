//! niri: pixels through niri's own `org.gnome.Mutter.ScreenCast` (no picker, by connector name),
//! input through the wlroots virtual pointer.
//!
//! niri has no virtual outputs of its own, so Extend switches on a spare connector: the first
//! `Virtual-*` (the vkms kernel module, `sudo modprobe vkms`) or a named one such as an HDMI dummy
//! plug. It goes through `niri msg output <name> on|custom-mode|mode|transform|scale|position|off`:
//! temporary changes that never touch the config file (which keeps `output "Virtual-1" { off }`).
//! Resizing or rotating keeps the output on and only swaps the cast (`reconfigure_extend`).
//! After a SIGKILL the output stays on: `niri msg output Virtual-1 off`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::mutter::{Cast, Source};
use super::{Backend, DMABUF_CONVERT, Mode};
use crate::desktop::{self, Output};

pub struct NiriCapture {
    width: u32,
    height: u32,
    output: String,
    geometry: (i32, i32, u32, u32),
    cast: Cast,
    /// Output we switched on for Extend; switched back off on Drop (after the cast stops).
    enabled_output: Option<String>,
    /// Extend: the niri scale we gave the output (kept when it is resized).
    scale: Option<f64>,
    /// Extend: the output is turned with `transform 90` (its connector had no mode of the wanted
    /// orientation); set back to `normal` before it is switched off.
    transformed: bool,
}

impl Backend for NiriCapture {
    fn source_description(&self) -> String {
        format!("pipewiresrc path={} do-timestamp=true keepalive-time=1000 ! {DMABUF_CONVERT}", self.cast.node_id)
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn geometry(&self) -> (i32, i32, u32, u32) {
        self.geometry
    }

    fn output_name(&self) -> String {
        self.output.clone()
    }

    fn is_extend(&self) -> bool {
        self.enabled_output.is_some()
    }

    fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        self.reconfigure(width, height, false)
    }

    fn reposition(&mut self) -> Result<bool> {
        let Some(name) = self.enabled_output.clone() else { return Ok(false) };
        let pos = spot(&name, (self.width, self.height), self.scale)?;
        niri_msg(&["output", &name, "position", "set", &pos.0.to_string(), &pos.1.to_string()])?;
        let out = wait_for_output(&name, (self.width, self.height), Some(pos), self.scale, Duration::from_secs(2))
            .ok_or_else(|| anyhow!("{name} did not move to {pos:?}"))?;
        self.geometry = out.logical.unwrap_or(self.geometry);
        Ok(true)
    }
}

impl NiriCapture {
    /// `force_transform` (tests): rotate with `transform 90` even if a portrait custom mode works.
    fn reconfigure(&mut self, width: u32, height: u32, force_transform: bool) -> Result<()> {
        let name = self.enabled_output.clone().ok_or_else(|| anyhow!("only an extend capture can be resized"))?;
        // Stop the current cast first; niri would otherwise keep rendering it at the new size.
        self.cast.stop();
        // The new size moves the spot along the edge (centred where the user put the phone).
        let pos = spot(&name, (width, height), self.scale).unwrap_or((self.geometry.0, self.geometry.1));
        niri_msg(&["output", &name, "position", "set", &pos.0.to_string(), &pos.1.to_string()])?;
        let (out, transformed) = match apply_extend_mode(&name, width, height, pos, self.scale, self.transformed, force_transform) {
            Ok(r) => r,
            Err(e) => {
                self.transformed = true; // unknown now: make Drop put `normal` back
                return Err(e);
            }
        };
        self.transformed = transformed;
        let fresh = record(&out).with_context(|| format!("recording {name} at {width}x{height}"))?;
        // The old (stopped) cast goes with `fresh`; it never switched an output on, so dropping
        // it leaves ours alone.
        self.cast = fresh.0;
        (self.width, self.height) = fresh.1;
        self.geometry = out.logical.unwrap_or(self.geometry);
        Ok(())
    }
}

impl Drop for NiriCapture {
    fn drop(&mut self) {
        self.cast.stop();
        if let Some(name) = &self.enabled_output {
            switch_off(name, self.transformed);
        }
    }
}

pub fn start(mode: &Mode) -> Result<NiriCapture> {
    match mode {
        Mode::Mirror => {
            let outs = niri_outputs()?;
            let out = primary_output(&outs, None).ok_or_else(|| anyhow!("no enabled output to mirror"))?.clone();
            let (cast, (width, height)) = record(&out)?;
            Ok(NiriCapture {
                width,
                height,
                output: out.name.clone(),
                geometry: out.logical.unwrap_or_default(),
                cast,
                enabled_output: None,
                scale: None,
                transformed: false,
            })
        }
        Mode::Extend { width, height, output, scale } => {
            let (out, transformed) = enable_extend_output(*width, *height, output.as_deref(), *scale)?;
            let name = out.name.clone();
            match record(&out) {
                Ok((cast, (width, height))) => Ok(NiriCapture {
                    width,
                    height,
                    output: name.clone(),
                    geometry: out.logical.unwrap_or_default(),
                    cast,
                    enabled_output: Some(name),
                    scale: *scale,
                    transformed,
                }),
                Err(e) => {
                    switch_off(&name, transformed);
                    Err(e.context(format!("recording extend output {name}")))
                }
            }
        }
    }
}

/// Records `out` and returns the cast with its size in buffer pixels. niri casts at the output's
/// mode size, but its stream `Parameters.size` is the logical size (they differ when the output
/// is scaled), so the mode wins.
fn record(out: &NiriOutput) -> Result<(Cast, (u32, u32))> {
    let geometry = out.logical.ok_or_else(|| anyhow!("output {} is off", out.name))?;
    let cast = Cast::start(Source::Monitor(&out.name), false)?;
    let size = match (out.pixel_size(), cast.params_size) {
        (Some(px), _) => px,
        (None, Some((w, h))) => (w as u32, h as u32),
        _ => (geometry.2, geometry.3),
    };
    Ok((cast, size))
}

/// Turns an extend output back off (and upright first, so the next session starts unrotated).
fn switch_off(name: &str, transformed: bool) {
    if transformed {
        let _ = niri_msg(&["output", name, "transform", "normal"]);
    }
    if let Err(e) = niri_msg(&["output", name, "off"]) {
        eprintln!("capture: failed to turn {name} back off: {e:#}");
    }
}

// ---------------------------------------------------------------------------------------------
// niri output handling

#[derive(Debug, Clone)]
pub struct NiriOutput {
    name: String,
    make_model: String,
    /// Logical rect (x, y, w, h); None when the output is off.
    logical: Option<(i32, i32, u32, u32)>,
    /// Index into `modes` of the current mode.
    current: Option<usize>,
    /// (width, height, refresh mHz).
    modes: Vec<(u32, u32, u32)>,
    rotated: bool,
    /// niri output scale (1.0 when off).
    scale: f64,
}

impl NiriOutput {
    fn is_virtual(&self) -> bool {
        self.name.starts_with("Virtual-")
    }

    /// Current mode size, rotated like the logical rect.
    fn pixel_size(&self) -> Option<(u32, u32)> {
        let (w, h, _) = *self.modes.get(self.current?)?;
        Some(if self.rotated { (h, w) } else { (w, h) })
    }
}

fn niri_msg(args: &[&str]) -> Result<String> {
    let mut argv = vec!["niri", "msg"];
    argv.extend_from_slice(args);
    desktop::run_tool(&argv)
}

fn niri_outputs() -> Result<Vec<NiriOutput>> {
    parse_outputs(&niri_msg(&["--json", "outputs"])?)
}

/// For `desktop::outputs`.
pub fn outputs() -> Result<Vec<Output>> {
    let focused = focused_output();
    Ok(niri_outputs()?
        .into_iter()
        .map(|o| Output {
            focused: Some(&o.name) == focused.as_ref(),
            is_virtual: o.is_virtual(),
            description: o.make_model,
            logical: o.logical,
            name: o.name,
        })
        .collect())
}

fn parse_outputs(json: &str) -> Result<Vec<NiriOutput>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing niri outputs")?;
    let map = v.as_object().ok_or_else(|| anyhow!("niri outputs: not an object"))?;
    let mut res: Vec<NiriOutput> = map
        .iter()
        .map(|(name, o)| {
            let logical = o.get("logical").filter(|l| l.is_object()).map(|l| {
                let g = |k: &str| l.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                (g("x") as i32, g("y") as i32, g("width") as u32, g("height") as u32)
            });
            let rotated = o
                .pointer("/logical/transform")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("90") || t.contains("270"));
            let modes = o
                .get("modes")
                .and_then(|m| m.as_array())
                .map(|a| {
                    a.iter()
                        .map(|m| {
                            let g = |k: &str| m.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                            (g("width"), g("height"), g("refresh_rate"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let current = o.get("current_mode").and_then(|c| c.as_u64()).map(|c| c as usize);
            let scale = o.pointer("/logical/scale").and_then(|s| s.as_f64()).unwrap_or(1.0);
            let text = |k: &str| o.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty() && *s != "Unknown");
            let make_model = [text("make"), text("model")].into_iter().flatten().collect::<Vec<_>>().join(" ");
            NiriOutput { name: name.clone(), make_model, logical, current, modes, rotated, scale }
        })
        .collect();
    res.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(res)
}

fn focused_output() -> Option<String> {
    niri_msg(&["--json", "focused-output"])
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned))
}

/// Focused output if it is a real, enabled one; else the first enabled non-virtual output;
/// else any enabled output. `exclude` skips the output being set up for Extend.
fn primary_output<'a>(outs: &'a [NiriOutput], exclude: Option<&str>) -> Option<&'a NiriOutput> {
    let usable = |o: &&NiriOutput| o.logical.is_some() && Some(o.name.as_str()) != exclude;
    let focused = focused_output();
    outs.iter()
        .filter(usable)
        .find(|o| !o.is_virtual() && Some(&o.name) == focused.as_ref())
        .or_else(|| outs.iter().filter(usable).find(|o| !o.is_virtual()))
        .or_else(|| outs.iter().find(usable))
}

/// Mode closest to the request: same orientation first, then smallest size difference,
/// then closest to 60 Hz.
fn nearest_mode(modes: &[(u32, u32, u32)], w: u32, h: u32) -> Option<(u32, u32, u32)> {
    let landscape = w >= h;
    modes.iter().copied().min_by_key(|&(mw, mh, r)| {
        let orient = u64::from((mw >= mh) != landscape);
        let diff = (i64::from(mw) - i64::from(w)).unsigned_abs() + (i64::from(mh) - i64::from(h)).unsigned_abs();
        let rate = (i64::from(r) - 60_000).unsigned_abs();
        (orient, diff, rate)
    })
}

/// Poll niri until `name` is on with the given pixel size (and position and scale, if given).
fn wait_for_output(name: &str, size: (u32, u32), pos: Option<(i32, i32)>, scale: Option<f64>, timeout: Duration) -> Option<NiriOutput> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = niri_outputs().ok().and_then(|outs| outs.into_iter().find(|o| o.name == name)).filter(|o| {
            o.logical.is_some()
                && o.pixel_size() == Some(size)
                && pos.is_none_or(|p| o.logical.map(|l| (l.0, l.1)) == Some(p))
                && scale.is_none_or(|s| (o.scale - s).abs() < 0.02)
        });
        if found.is_some() || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Where the extend output `name` goes at `size` pixels and `scale` (`desktop::Placement`).
fn spot(name: &str, size: (u32, u32), scale: Option<f64>) -> Result<(i32, i32)> {
    Ok(desktop::spot(&outputs()?, Some(name), desktop::logical_size(size, scale.unwrap_or(1.0))))
}

/// Switches the extend output on at `width`x`height` (see `apply_extend_mode`). Returns it and
/// whether it had to be turned with a transform.
fn enable_extend_output(width: u32, height: u32, wanted: Option<&str>, scale: Option<f64>) -> Result<(NiriOutput, bool)> {
    let outs = niri_outputs()?;
    let target = match wanted {
        Some(n) => outs.iter().find(|o| o.name == n).ok_or_else(|| {
            let names: Vec<_> = outs.iter().map(|o| o.name.as_str()).collect();
            anyhow!("output {n:?} is not connected (niri sees: {})", names.join(", "))
        })?,
        None => outs.iter().find(|o| o.is_virtual()).ok_or_else(|| {
            anyhow!(
                "no virtual output for extend mode: run `sudo modprobe vkms` (creates \
                 \"Virtual-1\"; add `vkms` to /etc/modules-load.d/ to keep it), or plug an \
                 HDMI dummy and pass its connector name"
            )
        })?,
    }
    .clone();
    let name = target.name.as_str();

    // Next to the primary output, on the side the user chose (niri takes negative positions).
    let (x, y) = spot(name, (width, height), scale)?;
    let (xs, ys) = (x.to_string(), y.to_string());

    // niri lays out explicitly positioned outputs first and auto-places the rest after them, so
    // positioning only the virtual output would push an unpositioned laptop panel to its right.
    // Pin every other output where it is now (temporary, like the rest of this).
    for o in outs.iter().filter(|o| o.name != name) {
        if let Some((ox, oy, ..)) = o.logical {
            let _ = niri_msg(&["output", &o.name, "position", "set", &ox.to_string(), &oy.to_string()]);
        }
    }

    // `on` first: it overrides an `output "…" { off }` in the config (temporary, until Drop).
    niri_msg(&["output", name, "on"])?;
    // A transform left behind by a killed session would turn the picture: start upright.
    let _ = niri_msg(&["output", name, "transform", "normal"]);
    // The scale changes the logical size, so set it before the geometry is read for input mapping.
    if let Some(s) = scale {
        niri_msg(&["output", name, "scale", &s.to_string()])?;
    }
    niri_msg(&["output", name, "position", "set", &xs, &ys])?;
    match apply_extend_mode(name, width, height, (x, y), scale, false, false) {
        Ok(r) => Ok(r),
        Err(e) => {
            switch_off(name, true);
            Err(e)
        }
    }
}

/// Gives the (on, positioned) output `name` the pixel size `width`x`height` (`height > width` =
/// portrait): a custom mode of exactly that size first (vkms takes any), else the connector's
/// nearest listed mode, turned with `transform 90` when no listed mode has the wanted
/// orientation. `pos` is where the output sits (unchanged). Returns the output once niri reports
/// the new size and whether it is transformed (`was_transformed`: it is now, undo that first).
fn apply_extend_mode(
    name: &str,
    width: u32,
    height: u32,
    pos: (i32, i32),
    scale: Option<f64>,
    was_transformed: bool,
    force_transform: bool,
) -> Result<(NiriOutput, bool)> {
    let portrait = height > width;
    let set_transform = |rotate: bool| niri_msg(&["output", name, "transform", if rotate { "90" } else { "normal" }]);
    if force_transform {
        // Tests: a landscape custom mode turned to portrait by the compositor.
        let (lw, lh) = (width.max(height), width.min(height));
        niri_msg(&["output", name, "custom-mode", &format!("{lw}x{lh}@60")])?;
        set_transform(portrait)?;
        return match wait_for_output(name, (width, height), Some(pos), scale, Duration::from_secs(3)) {
            Some(o) => Ok((o, portrait)),
            None => bail!("{name} did not come up as {width}x{height} with a transform"),
        };
    }
    if was_transformed {
        set_transform(false)?;
    }
    if niri_msg(&["output", name, "custom-mode", &format!("{width}x{height}@60")]).is_ok()
        && let Some(o) = wait_for_output(name, (width, height), Some(pos), scale, Duration::from_secs(2))
    {
        return Ok((o, false));
    }

    // Custom mode refused (real connectors often are picky): use the nearest listed mode. A
    // connector without a mode of the wanted orientation (EDIDs rarely list portrait modes) gets
    // the nearest one of the other orientation, turned by 90°.
    let modes = niri_outputs()?.into_iter().find(|o| o.name == name).map(|o| o.modes).unwrap_or_default();
    let (mw, mh, mr, rotate) = pick_fallback_mode(&modes, width, height)
        .ok_or_else(|| anyhow!("{name}: custom mode {width}x{height} failed and no modes are listed"))?;
    let turned = if rotate { ", turned by 90°" } else { "" };
    eprintln!("capture: {name}: custom mode {width}x{height} not applied; using {mw}x{mh}{turned}");
    let (xs, ys) = (pos.0.to_string(), pos.1.to_string());
    let set = niri_msg(&["output", name, "mode", &format!("{mw}x{mh}@{:.3}", f64::from(mr) / 1000.0)])
        .and_then(|_| set_transform(rotate))
        .and_then(|_| niri_msg(&["output", name, "position", "set", &xs, &ys]));
    let size = if rotate { (mh, mw) } else { (mw, mh) };
    match set.ok().and_then(|_| wait_for_output(name, size, None, scale, Duration::from_secs(3))) {
        Some(o) => Ok((o, rotate)),
        None => bail!("{name} did not come up with mode {mw}x{mh}{turned}"),
    }
}

/// The listed mode to use when a custom `width`x`height` is refused: (w, h, refresh mHz, turn by
/// 90°). The nearest mode of the same orientation, else the nearest of the other, turned.
fn pick_fallback_mode(modes: &[(u32, u32, u32)], width: u32, height: u32) -> Option<(u32, u32, u32, bool)> {
    let portrait = height > width;
    let same = modes.iter().any(|&(mw, mh, _)| (mh > mw) == portrait);
    let (rw, rh, rotate) = if same || modes.is_empty() { (width, height, false) } else { (height, width, true) };
    nearest_mode(modes, rw, rh).map(|(mw, mh, mr)| (mw, mh, mr, rotate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::tests::run_pipeline;

    const OUTS: &str = r#"{
      "eDP-1":{"name":"eDP-1","make":"HKC","model":"Unknown","modes":[{"width":1920,"height":1080,"refresh_rate":60003,"is_preferred":true}],
               "current_mode":0,"logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":"Normal"}},
      "Virtual-1":{"name":"Virtual-1","modes":[{"width":1024,"height":768,"refresh_rate":60004,"is_preferred":true},
               {"width":2560,"height":1440,"refresh_rate":59951,"is_preferred":false},
               {"width":2560,"height":1440,"refresh_rate":60000,"is_preferred":false},
               {"width":1080,"height":1920,"refresh_rate":60000,"is_preferred":false}],
               "current_mode":null,"logical":null}
    }"#;

    #[test]
    fn parse_and_match() {
        let outs = parse_outputs(OUTS).unwrap();
        assert_eq!(outs.len(), 2);
        let edp = outs.iter().find(|o| o.name == "eDP-1").unwrap();
        assert_eq!(edp.make_model, "HKC");
        assert_eq!(edp.pixel_size(), Some((1920, 1080)));
        let v = outs.iter().find(|o| o.is_virtual()).unwrap();
        assert!(v.logical.is_none() && v.current.is_none());
        assert_eq!(nearest_mode(&v.modes, 2712, 1220), Some((2560, 1440, 60000)));
        assert_eq!(nearest_mode(&v.modes, 1220, 2712), Some((1080, 1920, 60000)));
    }

    #[test]
    fn fallback_mode_turns_when_no_portrait_mode_is_listed() {
        let with_portrait = [(1024, 768, 60004), (2560, 1440, 60000), (1080, 1920, 60000)];
        assert_eq!(pick_fallback_mode(&with_portrait, 1220, 2712), Some((1080, 1920, 60000, false)));
        let hdmi = [(1920, 1080, 60000), (1280, 720, 60000), (1920, 1080, 50000)];
        // Portrait wanted, only landscape modes: the nearest to the turned size, rotated.
        assert_eq!(pick_fallback_mode(&hdmi, 1080, 1920), Some((1920, 1080, 60000, true)));
        assert_eq!(pick_fallback_mode(&hdmi, 720, 1280), Some((1280, 720, 60000, true)));
        assert_eq!(pick_fallback_mode(&hdmi, 1280, 720), Some((1280, 720, 60000, false)));
        assert_eq!(pick_fallback_mode(&[], 1280, 720), None);
    }

    fn describe(cap: &NiriCapture) {
        eprintln!(
            "capture {} {}x{} node={} geometry={:?}\n  src: {}",
            cap.output,
            cap.width,
            cap.height,
            cap.cast.node_id,
            cap.geometry,
            cap.source_description()
        );
    }

    /// Live (niri): cargo test --release -p smartconnect-display niri_smoke -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn niri_smoke_mirror() {
        let cap = start(&Mode::Mirror).expect("start mirror capture");
        describe(&cap);
        let (n, _) = run_pipeline(&cap.source_description(), None);
        eprintln!("mirror frames in 3 s: {n}");
        assert!(n > 0);
    }

    /// Live (niri): turns the vkms output on at 2712x1220 and 200 %, records it, turns it off again.
    #[test]
    #[ignore]
    fn niri_smoke_extend() {
        let cap = start(&Mode::Extend { width: 2712, height: 1220, output: None, scale: Some(2.0) }).expect("start extend capture");
        describe(&cap);
        assert_eq!((cap.width, cap.height), (2712, 1220));
        assert_eq!((cap.geometry.2, cap.geometry.3), (1356, 610), "logical size at scale 2");
        let (n, caps) = run_pipeline(&cap.source_description(), None);
        eprintln!("negotiated: {caps}");
        assert!(caps.contains("width=(int)2712") && caps.contains("height=(int)1220"), "full-resolution buffers");
        let name = cap.output.clone();
        drop(cap);
        let off = niri_outputs().unwrap().into_iter().find(|o| o.name == name).unwrap();
        eprintln!("extend frames in 3 s: {n}; {name} off after drop: {}", off.logical.is_none());
        assert!(off.logical.is_none());
        assert!(n > 0);
    }

    /// Live (niri): Virtual-1 on at 1920x1080 and 200 %, turned portrait IN PLACE (custom mode, then
    /// the `transform 90` fallback), back to landscape, then off. The output is never switched off
    /// in between, so windows on it stay there.
    #[test]
    #[ignore]
    fn niri_smoke_extend_rotate() {
        let mut cap = start(&Mode::Extend { width: 1920, height: 1080, output: None, scale: Some(2.0) }).expect("start extend capture");
        let name = cap.output.clone();
        // (PipeWire reuses freed node ids, so a new cast may get the old id.)
        let check = |cap: &NiriCapture, what: &str, (w, h): (u32, u32)| {
            describe(cap);
            let (n, caps) = run_pipeline(&cap.source_description(), None);
            eprintln!("{what}: {n} frames in 3 s; negotiated: {caps}");
            assert_eq!((cap.width, cap.height), (w, h), "{what}: capture size");
            assert_eq!((cap.geometry.2, cap.geometry.3), (w / 2, h / 2), "{what}: logical size at 200 %");
            assert!(caps.contains(&format!("width=(int){w}")) && caps.contains(&format!("height=(int){h}")), "{what}: buffers");
            assert!(n > 0, "{what}: no frames");
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cap.reconfigure_extend(1080, 1920).expect("portrait (custom mode)");
            assert!(!cap.transformed);
            check(&cap, "portrait custom mode", (1080, 1920));
            cap.reconfigure(1080, 1920, true).expect("portrait (transform)");
            assert!(cap.transformed);
            check(&cap, "portrait via transform 90", (1080, 1920));
            cap.reconfigure_extend(1920, 1080).expect("back to landscape");
            assert!(!cap.transformed);
            check(&cap, "landscape again", (1920, 1080));
        }));
        drop(cap); // always switch the output off, even after a failed assertion
        let off = niri_outputs().unwrap().into_iter().find(|o| o.name == name).unwrap();
        eprintln!("{name} off after drop: {}", off.logical.is_none());
        assert!(off.logical.is_none());
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }
}
