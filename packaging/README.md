# Smart Connect for Linux — packaging

This directory installs the whole project: the `smartconnectd` daemon as a systemd **user** service
tied to the graphical session, its helpers (`smartconnect-mirror`, `smartconnect-crosscontrol`,
`smartconnect-display`, `smartconnect-webcam`), the GTK app `smartconnect-gui` with its launcher,
and the Noctalia (Quickshell) panel plugin. `install.sh` copies this file to
`~/.local/share/doc/smartconnect-linux/README.md`.

| File | What it is |
|---|---|
| `install.sh`, `uninstall.sh` (+ `common.sh`) | Per-user install from the checkout |
| `systemd/smartconnectd.service` | The user unit |
| `dev.smartconnect.Linux.desktop`, `dev.smartconnect.Linux.svg` | GTK app launcher and icon (named after the app ID) |
| `arch/PKGBUILD` | Arch Linux package built from the checkout |

## Prerequisites

- Rust 1.88 or newer, and the libraries listed in the installation docs (`docs/installation.md`):
  `dbus`, GStreamer (with gst-plugins-bad and libnice), GTK 4.18 and libadwaita 1.7.
- At run time: `wl-clipboard` (clipboard sync), `avahi` with `avahi-daemon` running (Wi-Fi
  reconnect), `bluez` with `bluetooth` running (Bluetooth wake).
- The service needs `WAYLAND_DISPLAY` (and ideally `DISPLAY`) in the systemd user manager's
  environment. niri imports it; check with `systemctl --user show-environment | grep DISPLAY`. If
  missing, run `systemctl --user import-environment WAYLAND_DISPLAY DISPLAY` from your compositor's
  autostart. Without `WAYLAND_DISPLAY` there is no clipboard sync from the PC, and without either
  variable the daemon doesn't start the stream host or Cross Control.

## Install

```sh
./packaging/install.sh --enable     # build, install to ~/.local, enable and start the service
```

| Option | Effect |
|---|---|
| `--prefix DIR` | Install under `DIR` instead of `~/.local` (also `PREFIX=DIR`) |
| `--enable` | Enable the user service and start it, or restart it if it runs |
| `--link` | Symlink the Noctalia plugin to the checkout instead of copying it (development) |
| `--no-plugin` | Leave the Noctalia plugin out |
| `--no-build` | Install what was built before |
| `--dry-run` | Print every step, change nothing |

It never runs `sudo` and refuses to run as root. Re-run it after pulling changes; the plugin's
settings are kept. Remove everything with `./packaging/uninstall.sh` (same `--prefix`;
`--purge` also deletes pairings, identity, preferences and caches; `--dry-run` lists what would go).

Arch Linux: `cd packaging/arch && makepkg -si` builds a package from this checkout instead (binaries
in `/usr/bin`, the plugin in `/usr/share/smartconnect-linux/` for each user to copy).

## How the programs find each other

`smartconnectd` and the GTK app start helpers from, in order: `$SMARTCONNECT_MIRROR_BIN`,
`$SMARTCONNECT_CROSSCONTROL_BIN`, `$SMARTCONNECT_DISPLAY_BIN` or `$SMARTCONNECT_WEBCAM_BIN` (the
full path of that helper), then their own directory, then `PATH`. The Noctalia plugin starts them
by name from `PATH`, unless its settings name a path, so the install's `bin` directory has to be on
the `PATH` Noctalia runs with.

## Setup that needs root

`install.sh` prints these at the end; it doesn't run them.

- Firewall: the phone connects to TCP 52020 and finds the PC over mDNS (UDP 5353).

  ```sh
  sudo firewall-cmd --permanent --add-port=52020/tcp
  sudo firewall-cmd --permanent --add-service=mdns
  sudo firewall-cmd --reload
  ```

  Sharing a PC folder with the phone (`smartconnect-files serve`) also needs inbound TCP 2049, or
  the port you pass with `--port` (pick another one when an `nfs-server` already holds 2049):
  `sudo firewall-cmd --permanent --add-port=2049/tcp`.

- Phone as a second monitor (Extend): the `vkms` module, and on niri the virtual output off while
  unused (`smartconnect-display` switches it on for a session).

  ```sh
  sudo modprobe vkms
  echo vkms | sudo tee /etc/modules-load.d/vkms.conf
  ```

  ```kdl
  output "Virtual-1" {
      off
  }
  ```

- Phone as a webcam: `v4l2loopback`.

  ```sh
  sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1
  echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
  echo 'options v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1' | sudo tee /etc/modprobe.d/v4l2loopback.conf
  ```

- Keyboard button on the phone display: `wvkbd` (AUR: `yay -S wvkbd`).

## First pairing

```sh
smartconnectd pair
```

Writes a fresh QR to `~/.local/share/smartconnect-linux/pairing-qr.svg`; open it with
`xdg-open`. The daemon also prints the QR on its own output, which is the journal when it runs as a
service. **Pair new phone…** in the GTK app's device menu and the QR button in the Noctalia panel show the same code. Scan it
from Smart Connect on the phone (**+**, then **Scan QR code**).

The full documentation (installation, usage, troubleshooting, security) is in the `docs/`
directory of the repository.

## CLI (talks to the running daemon via `$XDG_RUNTIME_DIR/smartconnectd.sock`)

| Command | Effect |
|---|---|
| `smartconnectd status` | phone status as JSON |
| `smartconnectd subscribe` | status changes as JSON lines |
| `smartconnectd connect` | connect to the paired phone now |
| `smartconnectd disconnect` | close the current session |
| `smartconnectd pair` | show a new pairing QR |

## Front-ends

- GTK app: **Smart Connect** in your application launcher, or `smartconnect-gui`.
- Noctalia: enable **Smart Connect** under Settings > Plugins.
- `smartconnect-mirror` on its own shows the phone screen while the phone is connected.
- `smartconnect-display --mode extend` makes the phone a second monitor (also the GTK app's
  *Display* page and the Noctalia panel).

## Troubleshooting

```sh
journalctl --user -u smartconnectd -f      # live logs (RUST_LOG=info; override with systemctl --user edit)
systemctl --user status smartconnectd
```

- Phone can't find/connect: check the firewall rules above and that `avahi-daemon` is active.
- Clipboard not syncing: `WAYLAND_DISPLAY` missing from the user manager env (see above), then restart the service.
- A phone tile does nothing: the daemon couldn't start the helper; the journal says
  `helpers: can't start …`, and each helper logs to `$XDG_RUNTIME_DIR/smartconnectd/<name>.log`.
- Panel says "smartconnectd is not running": the service is stopped or crashed — see the journal.
