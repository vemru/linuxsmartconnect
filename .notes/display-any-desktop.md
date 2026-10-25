# Phone as display on any desktop (GNOME, KDE, Hyprland, sway, niri)

## What

`smartconnect-display` (Extend and Duplicate) no longer needs niri. `desktop.rs` detects the
desktop (`$SMARTCONNECT_DESKTOP`, then `NIRI_SOCKET` / `HYPRLAND_INSTANCE_SIGNATURE` / `SWAYSOCK`, then
`XDG_CURRENT_DESKTOP`) and `capture.rs` picks one backend per desktop. `--list-outputs` prints the
desktop, how Extend works there and its outputs as JSON; the GTK Display page uses it instead of
`niri msg`.

| Desktop  | Extend screen                              | Pixels                       | Input                    | Status |
|----------|--------------------------------------------|------------------------------|--------------------------|--------|
| niri     | spare output switched on (vkms Virtual-1)  | Mutter ScreenCast (niri)     | wlr virtual pointer      | unchanged code, moved to `capture/niri.rs` |
| GNOME    | Mutter virtual monitor (`RecordVirtual`)   | Mutter ScreenCast            | Mutter RemoteDesktop     | verified live (GNOME 51) |
| Hyprland | `hyprctl output create headless`           | wlr-screencopy → appsrc      | wlr virtual pointer      | verified live (nested 0.56) |
| sway     | `swaymsg create_output`                    | wlr-screencopy → appsrc      | wlr virtual pointer      | untested (not installed) |
| KDE      | KWin virtual output (`zkde_screencast`)    | KWin screencast → PipeWire   | `org_kde_kwin_fake_input`| untested (nested KWin would not start here) |
| other    | none (`--output NAME` only)                | wlr-screencopy → appsrc      | wlr virtual pointer      | untested |

Mirror falls back to the portal picker when the direct path fails.

## How

- `capture.rs`: `Capture` wraps a `Box<dyn Backend>`; new `bind(bin)` (the session calls it after
  building the source bin, so screencopy can push into its appsrc) and `input_target()`.
- `capture/gnome.rs`: the virtual monitor's mode is whatever the consumer negotiates, so the source
  description pins `video/x-raw,width=W,height=H`. A new consumer with other caps resizes the same
  monitor in place (verified 1920×1080 → 1080×1920 → 1280×720). A layout thread waits for the new
  `Meta-N` and applies position + nearest supported scale with `ApplyMonitorsConfig` method 1
  (temporary, no dialog, every other monitor passed back unchanged, layout-mode passed through).
- `capture/screencopy.rs`: one thread per capture; first frame of a size with `copy`, then
  `copy_with_damage`; keepalive re-push every 1 s in both wait loops; EAGAIN tolerated (it killed the
  thread before). Hyprland applies a headless output's new mode only when it renders it, which the
  running screencopy causes.
- `input.rs`: `Injector` = `Translator` + `Box<dyn Sink>` (`input/wlr.rs`, `input/mutter.rs`,
  `input/kwin.rs`); held buttons/keys are released by the Injector for every sink.
- OSK: `desktop::focus_output` per desktop; on GNOME the keyboard button toggles GNOME's own screen
  keyboard (`org.gnome.desktop.a11y.applications screen-keyboard-enabled`), restored on hide.

## Why / pitfalls found

- **GNOME absolute coordinates differ per stream type** (measured with a GTK probe window that
  logs pointer motion on the virtual monitor): `RecordMonitor` streams take stream pixels (divided
  by the monitor scale); `RecordVirtual` streams take logical px. The translator maps into stream
  pixels and the Mutter sink divides by the virtual monitor's *current* scale (read live: the layout
  thread rescales it after the stream starts). At 200 % GNOME draws no cursor into the virtual
  monitor's stream (Mutter); touches still land correctly.
- KDE needs a trusted `.desktop` file (`X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input`,
  absolute `Exec`): KWin hides those globals otherwise. KWin's virtual-output size is logical, so we
  ask for (W/scale)×(H/scale). **Not yet shipped in `packaging/`** (see TODO below).
- A portal picker can't target a just-created output, which is why Hyprland/sway use screencopy.

## Verified

- GNOME 51: `gnome_smoke_extend` / `gnome_smoke_mirror` / `capture_smoke_*` / `input_smoke_extend`
  (touch at 29.5 %/41 % → pointer exactly there at 200 %, checked with the probe window).
- Hyprland 0.56 nested: `hyprland_smoke_extend` (create, rotate in place, remove),
  `hyprland_smoke_frames` (~50 buffers/3 s), `input_smoke_extend` (`hyprctl cursorpos` exact).
- Not verified: a real phone session on GNOME/Hyprland, sway, KDE, other wlroots, niri regression
  (code moved but not re-run on niri).

## Packaging

`packaging/dev.smartconnect.Display.desktop` (NoDisplay, `X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input`):
`install.sh` writes it with the absolute `$BIN_DIR/smartconnect-display` in `Exec=` (KWin compares
the canonical path of `Exec=` with the client's `/proc/<pid>/exe`) and runs `kbuildsycoca6`;
`uninstall.sh` removes it. Without an installable absolute path it is skipped with a warning.

## Also changed

- `mirror/src/main.rs` `raise_window`: Hyprland (`hyprctl dispatch focuswindow address:…`) and sway
  (`[title="^…$"] focus`) besides niri. Untested on Hyprland/sway.
- GUI `DisplayPage::{start_mode, stop_session, running_mode}`: a small API asked for by the Cross
  Control work so the Phone page can offer Duplicate/Extend; not called yet (`allow(dead_code)`).

## Placement (side of the main screen)

`desktop::Placement` reads the Cross Control page's `crosscontrol.json`
(`{"edge","start","end"}`, fractions along the primary's edge) and `Placement::position` puts the
phone's screen on that side, centred on `(start+end)/2`, clamped so it touches the primary along
the longest overlap it can have, and pushed past another monitor already on that side.
`desktop::spot` does it from a list of outputs; every backend except KDE uses it (GNOME also shifts
the whole layout so it starts at 0,0, which Mutter requires; niri, Hyprland and sway take negative
positions). `--side` overrides the edge. The session checks the file's mtime once a second and
calls `Capture::reposition` (GNOME: re-run the layout; niri: `position set`; Hyprland/sway: the
monitor rule / output command), then re-targets input. Verified live on GNOME
(`gnome_smoke_placement`: left, then moved below while running); the others only by the shared
math's unit tests.

## Still open

- A real phone session on GNOME and Hyprland; KDE and sway at all; niri regression run.
