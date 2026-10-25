# Installation

This page covers what to install, how to build and install everything, how to run the daemon as a
systemd user service, which firewall ports to open, and how to set up the optional front-ends. The
[README quick start](../README.md#quick-start) is the short version.

- [AppImage](#appimage): the easiest way, any distribution, nothing to build
- [What you need](#what-you-need)
- [Packages](#packages): [Arch Linux](#arch-linux), [Debian and Ubuntu](#debian-and-ubuntu-untested), [Fedora](#fedora-untested)
- [System services](#system-services)
- [Build and install](#build-and-install): [`install.sh`](#packaginginstallsh) · [by hand](#building-by-hand) · [Arch package](#arch-linux-package) · [how the programs find each other](#how-the-programs-find-each-other)
- [The systemd user service](#the-systemd-user-service)
- [Firewall](#firewall)
- [Noctalia plugin](#noctalia-plugin) · [GTK app](#gtk-app) · [Quick-reply window](#quick-reply-window)
- [Optional: vkms and v4l2loopback](#optional-kernel-modules)
- [Updating and uninstalling](#updating-and-uninstalling)

## AppImage

One file with every program and library inside (GTK, libadwaita, GStreamer with the WebRTC,
VA-API and x264 plugins, wl-clipboard, avahi's tools, even glibc), so it doesn't depend on your
distribution's versions. Built on Arch Linux with
[quick-sharun](https://github.com/pkgforge-dev/Anylinux-AppImages); x86_64 only.

1. Download `Smart_Connect-<version>-anylinux-x86_64.AppImage` from the
   [Releases page](https://github.com/rudrahnpatel/smartconnect-linux/releases), make it executable
   (`chmod +x Smart_Connect-*.AppImage`, or *Executable as program* in the file manager's
   properties) and open it.
2. The first time, a welcome window offers **Set up**. That is the same as running
   `./Smart_Connect-*.AppImage --install --enable` and needs no password:
   - unpacks itself to `~/.local/opt/smartconnect-linux` (about 600 MB) and links the programs into
     `~/.local/bin`;
   - adds *Smart Connect* to your applications, the systemd user service `smartconnectd`
     (enabled and started), the hidden `dev.smartconnect.Display.desktop` entry KDE Plasma needs
     to trust the display helper, and the Noctalia plugin when `~/.config/noctalia` exists (its
     settings are kept);
   - then opens the installed app. You can delete the AppImage file afterwards.

   **Try without installing** runs it straight from the file instead. The daemon then only runs
   while the app is open.
3. Pair: **Pair new phone…** in the device menu, then scan the code with Smart Connect on the phone.
   If firewalld runs and its default zone doesn't allow port 52020, the pairing window says so;
   on GNOME and KDE its **Allow** button opens the port (firewalld asks for your password),
   elsewhere it gives the commands to copy. With ufw it shows the commands to run.

Updating: open the newer AppImage; the welcome window then offers **Update** (or run it with
`--install`). Uninstalling: `./Smart_Connect-*.AppImage --uninstall` (or
`~/.local/opt/smartconnect-linux/AppRun --uninstall`) removes the programs, the launcher and the
service and keeps your pairing and settings. Every bundled program can be run through the file:
`./Smart_Connect-*.AppImage smartconnectd status`.

Still needed from the system: a Wayland desktop with a systemd user session, `avahi-daemon`
running (most distributions run it; Arch: `sudo systemctl enable --now avahi-daemon`), a VA-API
driver for hardware video is optional (the AppImage ships Intel's and Mesa's; NVIDIA falls back to
software), `fusermount3` for browsing the phone's files, and, only if you use them, the firewall
rules, `v4l2loopback` for the webcam and `vkms` for Extend on niri (see below).

Building it yourself: `packaging/appimage/make-appimage.sh` (needs Docker or Podman; about 15
minutes the first time) writes `dist/Smart_Connect-<version>-anylinux-x86_64.AppImage` and its
`.zsync` file.

## What you need

| | |
|---|---|
| Phone | A Motorola phone with the Smart Connect app. Tested: motorola edge 60 stylus with Smart Connect 09.0.3.013. Nothing is installed on the phone. |
| Network | Phone and PC on the same Wi-Fi network and subnet (discovery uses mDNS), able to reach each other. Video goes directly between them over UDP, on the address the phone reached the daemon on (see [Troubleshooting](troubleshooting.md#mirroring-and-app-streaming) for networks where that isn't enough). |
| Desktop | A Wayland compositor started as part of a systemd user session (`graphical-session.target`), plus a notification daemon. Developed on niri 26.04 with Noctalia Shell on Arch Linux. |
| Rust | 1.88 or newer. The crates use edition 2024 and let-chains. |

What each feature needs on the desktop side:

| Feature | Needs |
|---|---|
| Reconnect over Wi-Fi, Connect button | avahi: `avahi-daemon` running, plus `avahi-publish` and `avahi-browse` on `PATH`. Inbound mDNS in the firewall. |
| Bluetooth LE wake | BlueZ (`bluetooth.service`) and a Bluetooth LE adapter that is already powered on. The daemon never switches the adapter on for this. |
| Clipboard | `wl-copy`/`wl-paste` from wl-clipboard. Linux-to-phone sync uses `wl-paste --watch`, which needs a compositor with the data-control protocol (check with `wl-paste --watch echo changed`); where it is missing, as on GNOME, the daemon follows the clipboard through Xwayland instead (`DISPLAY` must be set, Xwayland enabled). |
| Notifications | A notification daemon on the session bus (`org.freedesktop.Notifications`): Noctalia, mako, dunst, swaync, … |
| Quick-reply window | Quickshell's `qs` on `PATH`. |
| Mirroring and app streaming | GStreamer 1.22 or newer with `webrtcbin`, the libnice GStreamer plugin and an H.264 decoder (`vah264dec` or `avdec_h264`). Keyboard input in the video windows needs XWayland (`DISPLAY` set; on niri that is `xwayland-satellite`). |
| Cross Control | Either compositor support for `wlr-layer-shell`, with `pointer-constraints` and `relative-pointer` (strongly recommended) and `keyboard-shortcuts-inhibit` (lets your shortcuts reach the phone), tested on niri; or, on GNOME 45 or newer, xdg-desktop-portal with InputCapture (not yet tested on a device). |
| PC lock tile | systemd-logind (`loginctl`) and a screen locker that reacts to logind's Lock signal (GNOME needs GDM as the display manager for its lock screen), or Noctalia, or a locker passed with `--lock-command`. |
| Hotspot tile | NetworkManager (`nmcli`). |
| GTK app | GTK 4.18 or newer and libadwaita 1.7 or newer. |
| Noctalia plugin | Noctalia Shell 4.4.0 or newer. |
| Phone as display | PipeWire with `pipewiresrc`; `vah264enc` (VA-API) is the default encoder, `x264enc` the fallback. GNOME: nothing more (Mutter's own screencast, RemoteDesktop and virtual monitors). KDE Plasma: the `dev.smartconnect.Display.desktop` file `install.sh` installs, so KWin trusts the helper. Hyprland and sway: `wlr-screencopy` and the virtual-pointer/virtual-keyboard protocols (both built in). niri: `vapostproc` (`gst-plugin-va`) to read its DMA-BUF frames, and for *Extend* the `vkms` module or an HDMI dummy plug. The keyboard button needs `wvkbd` (not on GNOME). `--audio` needs PulseAudio or PipeWire's Pulse server (`pulsesrc`) and `opusenc`. |
| Phone as webcam | The `v4l2loopback` module and `v4l2sink` for a camera every app sees, or PipeWire (`pipewiresink`) as a fallback. An H.264 decoder, as for mirroring. |
| Browsing the phone's files | `fuse3` (`fusermount3` on `PATH`). Nothing at build time: the FUSE code doesn't link libfuse. |
| Sharing a PC folder with the phone | Inbound TCP 2049 in the [firewall](#firewall), or the port you pass to `smartconnect-files serve --port` (2049 is also the port of a running `nfs-server`). `gio` (from GLib, already there with GTK) for delete-to-Trash. Optional: `ffmpeg` for video thumbnails in the phone's Files tab. |

## Packages

### Arch Linux

```sh
# Build tools
sudo pacman -S --needed git base-devel pkgconf rust    # or rustup

# Daemon
sudo pacman -S --needed avahi bluez dbus wl-clipboard

# Mirroring and app streaming
sudo pacman -S --needed gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
  libnice gst-libav gst-plugin-va
# VA-API driver for hardware video (gst-plugin-va needs one): mesa (AMD),
# intel-media-driver (Intel), libva-nvidia-driver (NVIDIA); check with vainfo (libva-utils)

# Browsing the phone's files
sudo pacman -S --needed fuse3

# GTK app
sudo pacman -S --needed gtk4 libadwaita

# Quick-reply window. Skip this if you use Noctalia: it ships its own Quickshell build.
sudo pacman -S --needed quickshell

# Optional
sudo pacman -S --needed networkmanager                     # Hotspot tile
sudo pacman -S --needed pipewire gst-plugin-pipewire gst-plugin-va gst-plugins-ugly   # phone as display
yay -S wvkbd                                               # phone display's keyboard button (AUR)
sudo pacman -S --needed v4l2loopback-dkms linux-headers gst-plugin-pipewire   # phone as webcam
```

`libnice` is easy to miss: `webrtcbin` needs its `nicesrc`/`nicesink` elements for ICE, and
`gst-plugins-bad` doesn't pull it in. Noctalia Shell itself comes from the AUR (`noctalia-shell`).

The GStreamer elements the code uses, and the Arch package that provides each one (found with
`gst-inspect-1.0 <element>` and `pacman -Qo` on the plugin file):

| Element | Used by | Arch package |
|---|---|---|
| `webrtcbin`, `dtlssrtpenc`, `srtpenc`, `sctpenc` | mirror, display, webcam | `gst-plugins-bad` |
| `nicesrc`, `nicesink` | mirror, display, webcam | `libnice` |
| `rtpbin`, `rtph264depay`, `rtph264pay` | mirror, display, webcam | `gst-plugins-good` |
| `h264parse` | mirror, display, webcam | `gst-plugins-bad-libs` (pulled in by `gst-plugins-bad`) |
| `vah264dec`, `vah264enc`, `vapostproc` | mirror and webcam (hardware decode), display | `gst-plugin-va` |
| `avdec_h264` | mirror and webcam (software decode) | `gst-libav` |
| `x264enc` | display (software encode) | `gst-plugins-ugly` |
| `videoconvert`, `videoscale`, `videorate`, `videotestsrc`, `glimagesink` | mirror, display, webcam | `gst-plugins-base-libs` (pulled in by `gst-plugins-base`) |
| `taginject`, `autovideosink` | mirror | `gst-plugins-good` |
| `pipewiresrc`, `pipewiresink` | display (capture), webcam (PipeWire camera fallback) | `gst-plugin-pipewire` |
| `v4l2sink` | webcam | `gst-plugins-good` |
| `pulsesrc`, `rtpopuspay` | display (`--audio`) | `gst-plugins-good` |
| `opusenc` | display (`--audio`) | `gst-plugins-base` |
| `audioconvert`, `audioresample`, `volume` | display (`--audio`) | `gst-plugins-base-libs` (pulled in by `gst-plugins-base`) |

Check what is missing:

```sh
# mirroring, app streaming and webcam
for e in webrtcbin nicesrc rtph264depay h264parse vah264dec avdec_h264 videoconvert glimagesink taginject \
         vapostproc vah264enc x264enc pipewiresrc \
         v4l2sink pipewiresink; do
  gst-inspect-1.0 --exists "$e" && echo "ok       $e" || echo "MISSING  $e"
done
```

The second line is for the phone as a display and the third for the webcam. You need at least one
of `vah264dec` and `avdec_h264`; the viewer prefers the VA-API decoder when it exists. The display
needs `vapostproc`, `pipewiresrc` and one of `vah264enc` and `x264enc`; the webcam needs `v4l2sink`
or `pipewiresink`. `gst-plugin-va` only registers its `vah264dec`, `vah264enc` and `vapostproc`
elements when a VA-API driver is installed.

Build-time libraries, through pkg-config: `dbus-1` for the daemon's Bluetooth support;
`gstreamer-1.0`, `gstreamer-webrtc-1.0`, `gstreamer-sdp-1.0`, `gstreamer-video-1.0` and
`gstreamer-gl-1.0` for the viewer (the display and webcam helpers need the first three);
`wayland-client` for Cross Control and the display helper; `gtk4` and `libadwaita-1` for the GTK
app. On Arch the headers ship in the packages above.

### Debian and Ubuntu (untested)

Package names for the same pieces. You need a release that has GTK 4.18 and libadwaita 1.7 for the
GTK app (Debian 13, Ubuntu 25.04 or newer). Install Rust with [rustup](https://rustup.rs) if the
distribution's `rustc` is older than 1.88.

```sh
sudo apt install git build-essential pkg-config libdbus-1-dev libwayland-dev \
  avahi-daemon avahi-utils bluez wl-clipboard network-manager fuse3 \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-gl gstreamer1.0-nice gstreamer1.0-libav \
  libgtk-4-dev libadwaita-1-dev
```

### Fedora (untested)

```sh
sudo dnf install git gcc pkgconf-pkg-config dbus-devel wayland-devel \
  avahi avahi-tools bluez wl-clipboard NetworkManager fuse3 \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel \
  gstreamer1-plugins-good gstreamer1-plugins-bad-free libnice-gstreamer1 \
  gtk4-devel libadwaita-devel
```

Fedora's default repositories may not ship a working H.264 decoder. Use the loop above to check that
`vah264dec` or `avdec_h264` exists. If neither does, the RPM Fusion GStreamer and VA driver packages
provide one.

## System services

```sh
sudo systemctl enable --now avahi-daemon bluetooth
```

NetworkManager is usually running already. The daemon talks to avahi and BlueZ but doesn't require
them. Without avahi you lose Wi-Fi reconnect, and without BlueZ you lose the Bluetooth wake.

## Build and install

The repository is one Cargo workspace: the daemon is the root package, and `mirror/`,
`crosscontrol/`, `display/`, `webcam/`, `files/`, `fex/` and `gui/` are members. They share one
`Cargo.lock` and one target directory, so a build puts all eight binaries next to each other.

### `packaging/install.sh`

```sh
git clone <repository-url> smartconnect-linux
cd smartconnect-linux
./packaging/install.sh            # add --enable to also start the daemon as a service
```

It builds the workspace (`cargo build --release --workspace --locked`) and installs it for your
user. `PREFIX` is `~/.local` unless you pass `--prefix DIR` or set `PREFIX=DIR`:

| What | Where |
|---|---|
| `smartconnectd`, `smartconnect-mirror`, `smartconnect-crosscontrol`, `smartconnect-display`, `smartconnect-webcam`, `smartconnect-files`, `smartconnect-fex`, `smartconnect-gui` | `$PREFIX/bin` |
| The systemd user unit `smartconnectd.service` | `~/.config/systemd/user/`, or `$PREFIX/lib/systemd/user/` for any other prefix, with `ExecStart=` pointing into it. Not enabled unless you pass `--enable`. |
| The GTK app's launcher and icon | `$PREFIX/share/applications/dev.smartconnect.Linux.desktop` (its `Exec=` is the full path of `smartconnect-gui`) and `$PREFIX/share/icons/hicolor/scalable/apps/dev.smartconnect.Linux.svg` |
| The Noctalia plugin | `~/.config/noctalia/plugins/smart-connect`, a copy (see [Noctalia plugin](#noctalia-plugin)) |
| `packaging/README.md` and the license | `$PREFIX/share/doc/smartconnect-linux/` |

| Option | Effect |
|---|---|
| `--prefix DIR` | Install under `DIR`. |
| `--enable` | Enable the user service and start it, or restart it when it is already running. If a `smartconnectd` you started by hand is running, it only enables the service and tells you to stop that one first (both can't own port 52020). |
| `--link` | Symlink the Noctalia plugin to the checkout instead of copying it, for development. |
| `--no-plugin` | Leave the Noctalia plugin out. |
| `--no-build` | Don't run cargo; install what was built before. |
| `--dry-run` | Print every step and change nothing. |

The script never runs `sudo`. Before building it checks with pkg-config that the build libraries'
headers are there. It checks the desktop side (wl-clipboard, avahi, GStreamer, Quickshell, fuse3,
the avahi and bluetooth services, `WAYLAND_DISPLAY` in the systemd user environment, `$PREFIX/bin`
on `PATH`) and only prints hints. At the end it prints, without running them, the optional steps
that need root: the [firewall](#firewall) rules, [vkms](#vkms-for-extending-the-desktop-onto-the-phone)
with the niri snippet, [v4l2loopback](#v4l2loopback-for-the-phone-as-a-webcam), and `wvkbd` from
the AUR. Run it as your normal user; it refuses to run as root.

Re-run it after `git pull` to update. `install(1)` replaces each binary rather than writing into
it, so a running daemon is not disturbed; restart it to use the new build (`--enable` does that).

### Building by hand

```sh
cargo build --release --workspace           # all eight binaries
cargo build --release -p smartconnect-gui   # or one package
cargo test --release --workspace
```

The binaries land in `target/release/`, or in the directory `CARGO_TARGET_DIR` or a
`.cargo/config.toml` `target-dir` names. To install by hand, copy all eight into one directory on your
`PATH`, plus the unit, launcher and icon from `packaging/`.

### Arch Linux package

`packaging/arch/PKGBUILD` builds a package from the checkout it sits in; nothing but the crates is
downloaded:

```sh
cd packaging/arch
makepkg -si
```

It installs the binaries to `/usr/bin`, the unit to `/usr/lib/systemd/user/`, the launcher and icon,
and the Noctalia plugin to `/usr/share/smartconnect-linux/noctalia-plugin/smart-connect` for each
user to copy (the install message shows the commands). Each user then runs
`systemctl --user enable --now smartconnectd`. Use either the package or `install.sh`, not both.
Set `CARGO_TARGET_DIR` to reuse an existing build directory. The PKGBUILD turns off makepkg's `lto`
option: with it, ring's C code is compiled as GCC LTO bitcode and the link fails with undefined
`ring_core_*` symbols. It is for a local checkout only; it isn't an AUR package.

### How the programs find each other

The daemon and the front-ends start helper programs. Nothing looks in a build directory by name;
this is the order:

| Who starts the helper | Where it looks |
|---|---|
| `smartconnectd`: the stream host, plus Cross Control, the webcam helper and the phone display when the phone asks for them | `$SMARTCONNECT_<NAME>_BIN`, then the directory `smartconnectd` runs from, then `PATH` |
| GTK app: Mirror button, Apps page, Cross Control switch, Display page, Files page, and `smartconnectd` itself when the service isn't running | `$SMARTCONNECT_<NAME>_BIN`, then the directory `smartconnect-gui` runs from, then `PATH` |
| Noctalia plugin: Mirror and Display buttons, Phone files tile | the *Screen mirroring viewer*, *Phone-as-display helper* and *Phone files helper* settings: a command name looked up on `PATH` (default `smartconnect-mirror`, `smartconnect-display`, `smartconnect-files`) or a full path (`~` is expanded) |
| Noctalia plugin: Cross Control button | `smartconnect-crosscontrol` on `PATH` |

The override variables hold the full path of one program; an empty value counts as unset. The name
is the program's name without `smartconnect-`, in capitals:

| Variable | Program |
|---|---|
| `SMARTCONNECT_MIRROR_BIN` | `smartconnect-mirror` |
| `SMARTCONNECT_CROSSCONTROL_BIN` | `smartconnect-crosscontrol` |
| `SMARTCONNECT_DISPLAY_BIN` | `smartconnect-display` |
| `SMARTCONNECT_WEBCAM_BIN` | `smartconnect-webcam` |
| `SMARTCONNECT_FILES_BIN` | `smartconnect-files` (GTK app) |
| `SMARTCONNECT_SMARTCONNECTD_BIN` | `smartconnectd` (GTK app, when it starts the daemon itself) |

So an installation (all binaries in one directory) always finds its helpers, whatever the service's
`PATH` is. A daemon or GTK app started from a build directory uses the helpers built next to it.
The Noctalia plugin is the one place that needs `$PREFIX/bin` on the `PATH` Noctalia runs with, or
full paths in its settings. If the daemon can't find a helper, it logs `helpers: can't start …`
with the variable to set.

`smartconnect-display` itself starts an on-screen keyboard for the phone's keyboard button:
`$SMARTCONNECT_OSK` (a command line), else `wvkbd` from `PATH`.

## The systemd user service

```sh
systemctl --user enable --now smartconnectd
systemctl --user status smartconnectd
journalctl --user -u smartconnectd -f
```

The unit (`packaging/systemd/smartconnectd.service`):

- starts with your graphical session (`WantedBy=`/`PartOf=graphical-session.target`) and restarts on
  failure after 3 s;
- runs `~/.local/bin/smartconnectd --token-ttl 600 --no-ble --name "Linux PC"` with
  `RUST_LOG=info` (`install.sh --prefix DIR` writes `DIR/bin/smartconnectd` instead, the Arch
  package `/usr/bin/smartconnectd`). The flags mean pairing QRs stay valid for 10 minutes, the
  experimental Bluetooth peripheral is off (pairing doesn't need it, and the Bluetooth wake still
  works), and the phone shows the PC as "Linux PC".

The daemon needs `WAYLAND_DISPLAY`, and ideally `DISPLAY`, in the systemd user manager's environment.
Without `WAYLAND_DISPLAY` there is no Linux-to-phone clipboard sync. Without either variable the
daemon doesn't start the stream host, Cross Control or the phone display, so the App streaming and
Cross Control tiles (and starting the PC screen from the phone) do nothing.
niri imports these variables on its own. For other compositors:

```sh
systemctl --user show-environment | grep -E '^(WAYLAND_DISPLAY|DISPLAY)='
systemctl --user import-environment WAYLAND_DISPLAY DISPLAY   # e.g. from your compositor's autostart
systemctl --user restart smartconnectd
```

If your session never reaches `graphical-session.target`
(`systemctl --user is-active graphical-session.target`), the service won't start at login. Start
`smartconnectd` from your compositor's autostart instead.

To change the flags or the environment, use a drop-in instead of editing the installed unit:

```sh
systemctl --user edit smartconnectd
```

```ini
[Service]
# The empty ExecStart= clears the original line.
ExecStart=
ExecStart=%h/.local/bin/smartconnectd --token-ttl 600 --no-ble --name "My Laptop"
Environment=RUST_LOG=debug
Environment=SMARTCONNECT_CROSS_EDGE=left
```

The daemon options and environment variables are listed in [Usage](usage.md#command-line-reference).
Only one daemon can run at a time because it owns TCP port 52020. Stop the service before you start
`smartconnectd` by hand.

## Firewall

The phone has to reach the PC on:

| Port | Why |
|---|---|
| TCP 52020 (inbound; change it with `--port`) | The TLS WebSocket the phone connects to, for pairing, sessions and Connect-button triggers. |
| UDP 5353 (inbound, mDNS) | Lets the phone find the PC and the PC find the phone, for reconnecting without a QR. |
| TCP 2049 (inbound; only while sharing a PC folder, `smartconnect-files serve` or the GTK app's *Share with the phone*; change it with `--port`) | The phone browses a shared PC folder over NFS in mutual TLS. A running `nfs-server` also uses 2049; pass another `--port` then. Not needed for browsing the phone's files, where the PC connects out. |

Outgoing traffic needs no rules on a typical stateful firewall. That covers the PC dialing the
phone's discovery port and the WebRTC video, where ICE starts from the PC side. In testing with
firewalld, mirroring worked with only the two rules above.

firewalld:

```sh
firewall-cmd --get-active-zones    # find your Wi-Fi interface's zone, e.g. "home" (no sudo needed)
sudo firewall-cmd --permanent --zone=home --add-port=52020/tcp
sudo firewall-cmd --permanent --zone=home --add-service=mdns
sudo firewall-cmd --permanent --zone=home --add-port=2049/tcp    # only for sharing a PC folder (serve)
sudo firewall-cmd --reload
```

Use the zone your Wi-Fi interface is in. Without `--zone` the rules go to the *default* zone, which
is often not the Wi-Fi's (for example the default is `public` while the Wi-Fi is in `home`), and
the phone then gets "No route to host". Runtime-only rules (without `--permanent`) are gone
after a reboot, with the same symptom.

ufw:

```sh
sudo ufw allow from 192.168.1.0/24 to any port 52020 proto tcp   # use your LAN's range
sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp
sudo ufw allow from 192.168.1.0/24 to any port 2049 proto tcp    # only for sharing a PC folder (serve)
```

## Noctalia plugin

`install.sh` copies `noctalia-plugin/smart-connect` to `~/.config/noctalia/plugins/smart-connect`.
Noctalia keeps the plugin's settings in that directory (`settings.json`); re-running `install.sh`
updates the plugin files and keeps them.

For development, `install.sh --link` makes that path a symlink to the checkout instead, so editing
the QML is enough (then reload the plugin). Noctalia then writes `settings.json` into the checkout,
where git ignores it. Switching between a copy and a link keeps the settings. Anything else at that
path, such as a link to another checkout, is left alone.

By hand:

```sh
mkdir -p ~/.config/noctalia/plugins
cp -r noctalia-plugin/smart-connect ~/.config/noctalia/plugins/
# or, for development: ln -s "$PWD/noctalia-plugin/smart-connect" ~/.config/noctalia/plugins/smart-connect
```

Then enable **Smart Connect** in Noctalia's settings under *Plugins*, and add its bar widget or
control-center button where you want it. Three settings name programs:

- **Screen mirroring viewer**: `smartconnect-mirror` (the default), looked up on `PATH`, or a full
  path to it (`~` is expanded).
- **Phone-as-display helper**: the same for `smartconnect-display`.
- **Phone files helper**: the same for `smartconnect-files` (the Phone files tile).

The plugin starts these and `smartconnect-crosscontrol` by name, so the directory they are installed
in (`~/.local/bin` with the defaults) has to be on the `PATH` Noctalia runs with. Otherwise, enter
full paths in the settings.

The panel talks to the daemon's control socket and reconnects on its own when the daemon restarts.

## GTK app

`install.sh` installs the launcher (`dev.smartconnect.Linux.desktop`, named after the app's
Wayland app ID so docks show its icon) and the icon. Start **Smart Connect** from your application
launcher, or run `smartconnect-gui`. The app shows a "Smart Connect service isn't running" page (with a **Start service** button) until the
daemon is up, then connects on its own.

## Quick-reply window

When you click **Reply** on a phone notification, the daemon writes a small QML file to
`$XDG_RUNTIME_DIR/smartconnectd/reply.qml` and runs it with `qs -p`. Quickshell has to be on `PATH`
in the daemon's environment. It is there if you use Noctalia; otherwise install the `quickshell`
package.

## Optional kernel modules

### vkms, for extending the desktop onto the phone

Only for niri, which can't create a virtual screen itself. GNOME, KDE Plasma, Hyprland and sway
need nothing here.

```sh
sudo modprobe vkms                                      # adds a virtual connector (Virtual-1)
echo vkms | sudo tee /etc/modules-load.d/vkms.conf      # load it at every boot
```

On niri, keep the virtual output off while it isn't in use, so windows and the cursor can't get lost
on an invisible screen. Add this to the niri config:

```kdl
output "Virtual-1" {
    off
}
```

`smartconnect-display --mode extend` switches it on only for the length of a session, using
temporary `niri msg output` changes; your config file is never edited. An HDMI dummy plug works
instead of vkms: pass its connector name, as in `--output HDMI-A-1`.

### v4l2loopback, for the phone as a webcam

Without a loopback device the webcam helper offers a PipeWire camera instead, which only PipeWire-aware apps see. For a camera every app sees:

```sh
sudo pacman -S --needed v4l2loopback-dkms linux-headers    # or your distribution's v4l2loopback package
sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1

# load it at every boot with the same options
echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
echo 'options v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1' \
  | sudo tee /etc/modprobe.d/v4l2loopback.conf
```

`smartconnect-webcam --check` shows which output it would use, and prints the same setup hint.

## Updating and uninstalling

Updating:

```sh
git pull
./packaging/install.sh --enable     # rebuilds, reinstalls everything and restarts the service
```

Without `--enable`, restart the service yourself: `systemctl --user restart smartconnectd`.

Uninstalling:

```sh
./packaging/uninstall.sh            # keeps pairing and identity, preferences and caches
./packaging/uninstall.sh --purge    # also deletes them; the phone has to pair again
```

Pass the same `--prefix` you installed with; `--dry-run` lists what would be removed.
`uninstall.sh` disables and stops the service and removes the unit, the eight binaries, the launcher
and icon, the guide, and the Noctalia plugin: a copy together with its settings, a link only when
it points to a Smart Connect plugin (the checkout itself is never touched). `--purge` also deletes
`~/.local/share/smartconnect-linux` (pairings, identity), `~/.config/smartconnect-linux`
(preferences) and `~/.cache/smartconnect`. Firewall rules and kernel module settings stay as they
are. Also remove the PC from the Smart Connect app on the phone.

Both scripts also remove `smartconnect-gui.desktop`, the launcher name earlier versions of these
instructions used.
