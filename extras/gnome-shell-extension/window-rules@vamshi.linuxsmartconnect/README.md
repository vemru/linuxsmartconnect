# Window Rules (GNOME Shell extension)

Optional GNOME Shell 51 extension. The GTK app works without it — it only adds per-window
positioning and always-on-top, which GNOME Wayland does not expose to normal applications.

## Install

```sh
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r extras/gnome-shell-extension/window-rules@vamshi.linuxsmartconnect \
      ~/.local/share/gnome-shell/extensions/
gnome-extensions enable window-rules@vamshi.linuxsmartconnect
```

If the shell does not pick it up immediately, log out and back in (Wayland cannot reload the shell
in place). `metadata.json` declares shell version 51, from `gnome-shell --version`.

## How rules are applied

Signal-driven, no 500 ms polling:

- `global.window_manager` `map`: apply when a window is created.
- `notify::title`, `notify::app-id`, `notify::wm-class` on each window: apply when identifying
  properties change (the title is often set after mapping).
- The actor's `first-frame` signal: apply once more after mutter's first frame, which can move the
  window. If that signal is absent, one `GLib.idle_add` plus one short timeout are used instead.
- A quiet 5 s scan only registers windows that were already mapped before the extension loaded;
  it does not log per window and does not fight a user who later moves a window.

Signals and timeouts are disconnected on `disable()`, and always-on-top is reverted.

## Rules

Edit `rules.json` in the extension directory. Matching is **AND across fields**, **OR within a
list**. The first matching rule wins.

```json
{
  "rules": [
    {
      "name": "example",
      "match": {
        "appId": ["dev.smartconnect.Linux"],
        "wmClass": ["dev.smartconnect.Linux"],
        "role": ["call"],
        "titleFile": "call-token.txt"
      },
      "actions": {
        "alwaysOnTop": true,
        "position": { "corner": "top-right", "marginX": 16, "marginY": 48 },
        "size": { "width": 380, "height": 360 }
      }
    }
  ]
}
```

- `appId` / `wmClass` / `role`: exact string lists. `role` is the X11 window role; Wayland windows
  have none. **Verified on GNOME 51 Wayland:** `Meta.Window.get_app_id()` returns `null` even for
  the call window, while `get_wm_class()` carries `dev.smartconnect.Linux`. The shipped rule therefore
  matches on `wmClass` only — it deliberately has no `appId` field.
- `titleFile`: exact match against a language-neutral token read from that file in the extension
  directory. No substring matching. The shipped rule uses `wmClass` + `titleFile`
  (`call-token.txt`). The same file is compiled into the GUI (`gui/src/callwindow.rs`), so the token
  is defined once.
- `position.corner`: `top-left`, `top-right`, `bottom-left`, `bottom-right`; `marginX` / `marginY`
  are in pixels. `marginY: 48` clears the GNOME top bar.
- `size`: optional. Without it the window keeps its current size.
- `alwaysOnTop`: optional. Windows changed by the extension are restored when it is disabled.

## Debug output

Debug output is off by default: there is no `debug.txt` file in the extension directory, so there
is **no per-window logging**; errors are always logged. To enable it, put a file named `debug.txt`
containing `1` in the extension directory and run
`gnome-extensions disable window-rules@vamshi.linuxsmartconnect && gnome-extensions enable window-rules@vamshi.linuxsmartconnect`
— no shell restart needed. Remove the file and disable/enable again to turn it off. With it on you
get one line per window with app id, wm class, title, role and the match result, plus a
`placing at x,y WxH` line when a rule fires. Example:

```
window-rules: map title="sc-call" appId="null" wmClass="dev.smartconnect.Linux" role="null" match=smartconnect-call
window-rules: placing at 1524,48 380x275
```

Read it with `journalctl -f -o cat /usr/bin/gnome-shell` (or your shell's debug log).

## Verify the rule applies to new call windows

The 5 s scan covers windows that predate the extension, but the normal path is for windows created
after it is loaded:

1. Log out and back in so the shell loads the extension (Wayland cannot reload in place).
2. With `debug.txt` in place, watch the shell log, then start a call in the GTK app.
3. Look for `match=smartconnect-call` followed by `placing at ...`; visually the call window should
   be pinned top-right, below the top bar. Without the flag, skip the log and check visually.

Not included: panel indicator, quick settings toggle. Those are possible future work; the GUI and
the daemon do not depend on either.
