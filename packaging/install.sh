#!/usr/bin/env bash
# Builds the smartconnect-linux workspace and installs it for the current user. Never runs sudo:
# what needs root (firewall, kernel modules, AUR packages) is printed at the end, not done.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

usage() {
  cat <<'EOF'
Usage: packaging/install.sh [options]

Builds the Cargo workspace in release mode and installs, for the current user:
  binaries            $PREFIX/bin: smartconnectd, smartconnect-mirror, smartconnect-crosscontrol,
                      smartconnect-display, smartconnect-webcam, smartconnect-files,
                      smartconnect-fex, smartconnect-gui, smartconnect-share
  systemd user unit   ~/.config/systemd/user/smartconnectd.service ($PREFIX/lib/systemd/user
                      for a prefix other than ~/.local); enabled only with --enable
  GTK app launcher    $PREFIX/share/applications/dev.smartconnect.Linux.desktop + icon
  display helper      $PREFIX/share/applications/dev.smartconnect.Display.desktop (hidden; lets
                      KDE Plasma trust smartconnect-display with screen casting and input)
  Noctalia plugin     ~/.config/noctalia/plugins/smart-connect (a copy; --link for a symlink)
  short guide         $PREFIX/share/doc/smartconnect-linux/

Options:
  --prefix DIR   install under DIR instead of $PREFIX or ~/.local
  --enable       enable the systemd user service and start it (restart it if it is running)
  --link         symlink the Noctalia plugin to this checkout instead of copying it (development)
  --no-plugin    leave the Noctalia plugin out
  --no-build     don't run cargo; install what was built before
  --dry-run      only print what would be done
  -h, --help     show this help

Re-running it updates an installation; the plugin's settings are kept.
EOF
}

ENABLE=0 LINK=0 PLUGIN=1 BUILD=1
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX="$2"; shift 2 ;;
    --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
    --enable) ENABLE=1; shift ;;
    --link) LINK=1; shift ;;
    --no-plugin) PLUGIN=0; shift ;;
    --no-build) BUILD=0; shift ;;
    --dry-run|-n) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
refuse_root
set_paths
CARGO="${CARGO:-cargo}"
[ "$DRY" = 1 ] && say "Dry run: nothing is changed"

# Escapes a string for the replacement side of a sed s|||.
sed_escape() { printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'; }

# install_filtered MODE SRC DST SED-SCRIPT: installs SRC run through sed.
install_filtered() {
  if [ "$DRY" = 1 ]; then
    printf '    would write: %s (from %s)\n' "$3" "${2#"$ROOT"/}"
    return
  fi
  mkdir -p "$(dirname "$3")"
  local tmp
  tmp="$(mktemp "$3.XXXXXX")"
  sed -e "$4" "$2" >"$tmp"
  chmod "$1" "$tmp"
  mv -f "$tmp" "$3"
}

# ---- prerequisites (hints only) ----
say "Checking the desktop side (hints only)"
hint_cmd() { # command, what is missing without it, package hint
  if command -v "$1" >/dev/null 2>&1; then ok "$1"; else warn "$1 not found: $2 ($3)"; fi
}
hint_cmd wl-copy "no clipboard sync" "wl-clipboard"
hint_cmd avahi-publish "no Wi-Fi reconnect or Connect button" "avahi"
hint_cmd avahi-browse "no Wi-Fi reconnect or Connect button" "avahi"
hint_cmd gst-inspect-1.0 "mirroring, app streaming, phone display and webcam need GStreamer" "gstreamer"
hint_cmd qs "no quick-reply window (Noctalia ships its own Quickshell)" "quickshell"
hint_cmd fusermount3 "no browsing of the phone's files" "fuse3"
if command -v systemctl >/dev/null 2>&1; then
  systemctl is-active --quiet avahi-daemon 2>/dev/null && ok "avahi-daemon running" \
    || warn "avahi-daemon not running: sudo systemctl enable --now avahi-daemon"
  systemctl is-active --quiet bluetooth 2>/dev/null && ok "bluetooth running" \
    || warn "bluetooth not running (only the Bluetooth wake needs it): sudo systemctl enable --now bluetooth"
fi

# ---- build ----
if [ "$BUILD" = 1 ]; then
  command -v "$CARGO" >/dev/null 2>&1 || die "cargo not found: install Rust 1.88 or newer (rustup, or your distribution's rust package)"
  # The build needs these libraries' development files; say which package is missing before cargo
  # stops halfway with a pkg-config error.
  if command -v pkg-config >/dev/null 2>&1; then
    for m in gtk4 libadwaita-1 gstreamer-webrtc-1.0 dbus-1; do
      pkg-config --exists "$m" 2>/dev/null \
        || warn "pkg-config can't find $m: the build will fail (packages: docs/installation.md#packages)"
    done
  else
    warn "pkg-config not found: the build needs it (pkgconf or pkg-config package)"
  fi
  say "Building the workspace (release)"
  # From the checkout, so a .cargo/config.toml there (e.g. a target-dir) applies.
  if [ "$DRY" = 1 ]; then
    printf '    would run: (cd %q && %q build --release --workspace --locked)\n' "$ROOT" "$CARGO"
  else
    (cd "$ROOT" && "$CARGO" build --release --workspace --locked)
  fi
fi

# Where cargo put the binaries: CARGO_TARGET_DIR, a configured target-dir, or ./target.
TARGET_DIR=""
if command -v "$CARGO" >/dev/null 2>&1 \
  && meta="$(cd "$ROOT" && "$CARGO" metadata --no-deps --format-version 1 --offline 2>/dev/null)"; then
  TARGET_DIR="$(printf '%s' "$meta" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
fi
REL="${TARGET_DIR:-${CARGO_TARGET_DIR:-$ROOT/target}}/release"
missing=()
for b in "${BINS[@]}"; do
  [ -x "$REL/$b" ] || [ -x "$ROOT/packaging/$b" ] || missing+=("$b")
done
if [ ${#missing[@]} -gt 0 ]; then
  if [ "$DRY" = 1 ]; then
    warn "not built yet in $REL: ${missing[*]}"
  else
    die "missing in $REL: ${missing[*]} (build them first, or run without --no-build)"
  fi
fi

# ---- binaries ----
say "Installing binaries to $BIN_DIR"
run mkdir -p "$BIN_DIR"
for b in "${BINS[@]}"; do
  # install(1) replaces the file instead of writing into it, so a running binary is not disturbed.
  if [ -f "$REL/$b" ]; then
    run install -m755 "$REL/$b" "$BIN_DIR/$b"
  elif [ -f "$ROOT/packaging/$b" ]; then
    run install -m755 "$ROOT/packaging/$b" "$BIN_DIR/$b"
  fi
done

# ---- systemd user unit ----
say "Installing the systemd user unit to $UNIT_DIR"
unit_script=""
if [ "$PREFIX" != "$HOME/.local" ]; then
  # The unit names %h/.local/bin/smartconnectd and the guide in %h/.local/share/doc.
  unit_script="s|%h/\\.local/|$(sed_escape "$PREFIX")/|g"
fi
install_filtered 644 "$ROOT/packaging/systemd/smartconnectd.service" "$UNIT" "$unit_script"
run install -Dm644 "$ROOT/packaging/README.md" "$DOC_DIR/README.md"
run install -Dm644 "$ROOT/LICENSE" "$DOC_DIR/LICENSE"

# ---- GTK app launcher ----
say "Installing the GTK app launcher and icon"
desktop_script=""
display_script=""
if printf '%s' "$BIN_DIR" | grep -Eq '^[A-Za-z0-9._/+-]+$'; then
  # An absolute Exec= works even when $BIN_DIR is not on the launcher's PATH.
  desktop_script="s|^Exec=smartconnect-gui|Exec=$(sed_escape "$BIN_DIR")/smartconnect-gui|"
  # KWin matches this Exec= against the running binary's path, so it has to be absolute.
  display_script="s|^Exec=smartconnect-display|Exec=$(sed_escape "$BIN_DIR")/smartconnect-display|"
  share_script="s|^Exec=smartconnect-share|Exec=$(sed_escape "$BIN_DIR")/smartconnect-share|"
else
  warn "$BIN_DIR has characters a .desktop Exec= line can't hold unquoted; the launcher relies on PATH"
  warn "and on KDE Plasma the phone display will not get screen casting (KWin needs an absolute Exec=)"
fi
run install -Dm644 "$ROOT/packaging/$APP_ID.svg" "$ICON_DIR/$APP_ID.svg"
install_filtered 644 "$ROOT/packaging/$APP_ID.desktop" "$APPS_DIR/$APP_ID.desktop" "$desktop_script"
install_filtered 644 "$ROOT/packaging/dev.smartconnect.Share.desktop" "$APPS_DIR/dev.smartconnect.Share.desktop" "${share_script:-}"
if [ -n "$display_script" ]; then
  install_filtered 644 "$ROOT/packaging/dev.smartconnect.Display.desktop" "$APPS_DIR/dev.smartconnect.Display.desktop" "$display_script"
  # KDE reads .desktop files through its sycoca cache; refresh it so KWin sees the new entry.
  if command -v kbuildsycoca6 >/dev/null 2>&1; then
    run kbuildsycoca6 2>/dev/null || true
  fi
fi
# Older instructions installed the launcher under this name.
old_desktop="$APPS_DIR/smartconnect-gui.desktop"
if [ -f "$old_desktop" ] && grep -q '^Exec=.*smartconnect-gui' "$old_desktop"; then
  run rm -f "$old_desktop"
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  run update-desktop-database -q "$APPS_DIR" || true
fi

# ---- Nautilus & file manager context menu ----
nautilus_scripts="${XDG_DATA_HOME:-$HOME/.local/share}/nautilus/scripts"
if [ -d "$nautilus_scripts" ] || command -v nautilus >/dev/null 2>&1; then
  run mkdir -p "$nautilus_scripts"
  if [ "$DRY" = 1 ]; then
    printf '    would write: %s/Send to Phone (Smart Connect)\n' "$nautilus_scripts"
  else
    cat <<'NS' > "$nautilus_scripts/Send to Phone (Smart Connect)"
#!/usr/bin/env bash
BIN="$(command -v smartconnect-share 2>/dev/null || echo "$HOME/.local/bin/smartconnect-share")"
exec "$BIN" "$@"
NS
    chmod 755 "$nautilus_scripts/Send to Phone (Smart Connect)"
  fi
  did "Nautilus context menu script installed: $nautilus_scripts/Send to Phone (Smart Connect)"
fi

dolphin_dir="${XDG_DATA_HOME:-$HOME/.local/share}/kio/servicemenus"
if [ -d "$dolphin_dir" ] || command -v dolphin >/dev/null 2>&1; then
  run mkdir -p "$dolphin_dir"
  install_filtered 644 "$ROOT/packaging/smartconnect-share.desktop" "$dolphin_dir/smartconnect-share.desktop" "${share_script:-}"
fi
if [ -f "$SHARE_DIR/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
  run gtk-update-icon-cache -q -t -f "$SHARE_DIR/icons/hicolor" || true
fi

# ---- Noctalia plugin ----
# Copies every plugin file except settings.json (Noctalia's per-user settings) into $1.
copy_plugin_files() {
  run find "$PLUGIN_SRC" -mindepth 1 -maxdepth 1 ! -name settings.json -exec cp -R {} "$1/" ';'
  # QML and JSON only: no file needs the execute bit some checkouts (e.g. on NTFS) give them.
  run find "$1" -type f -exec chmod 644 {} +
}

install_plugin() {
  local src_real carry=""
  src_real="$(readlink -f "$PLUGIN_SRC")"
  if [ -L "$PLUGIN_DST" ]; then
    local target
    target="$(readlink -f "$PLUGIN_DST" || true)"
    if [ "$target" != "$src_real" ]; then
      warn "$PLUGIN_DST links to ${target:-a missing path}, not to this checkout: left alone (remove it and re-run to install)"
      return
    fi
    if [ "$LINK" = 1 ]; then
      ok "Noctalia plugin already linked to this checkout"
      return
    fi
    # Development link -> copy. While linked, Noctalia kept the settings in the checkout.
    [ -f "$PLUGIN_SRC/settings.json" ] && carry="$PLUGIN_SRC/settings.json"
    run rm "$PLUGIN_DST" # the link only
  elif [ -e "$PLUGIN_DST" ]; then
    if ! is_our_plugin_dir "$PLUGIN_DST"; then
      warn "$PLUGIN_DST exists and is not the Smart Connect plugin: left alone"
      return
    fi
    if [ "$LINK" = 1 ]; then
      # Copy -> link: the settings move into the checkout, where git ignores them.
      if [ -f "$PLUGIN_DST/settings.json" ] && [ ! -f "$PLUGIN_SRC/settings.json" ]; then
        run cp "$PLUGIN_DST/settings.json" "$PLUGIN_SRC/settings.json"
      fi
      run rm -rf "$PLUGIN_DST"
    else
      # Update the copy in place; its settings.json stays.
      run find "$PLUGIN_DST" -mindepth 1 -maxdepth 1 ! -name settings.json -exec rm -rf {} +
      copy_plugin_files "$PLUGIN_DST"
      did "Noctalia plugin updated in $PLUGIN_DST (settings kept)"
      return
    fi
  fi
  run mkdir -p "$(dirname "$PLUGIN_DST")"
  if [ "$LINK" = 1 ]; then
    run ln -s "$src_real" "$PLUGIN_DST"
    did "Noctalia plugin linked: $PLUGIN_DST -> $src_real"
  else
    run mkdir "$PLUGIN_DST"
    copy_plugin_files "$PLUGIN_DST"
    if [ -n "$carry" ]; then
      run cp "$carry" "$PLUGIN_DST/settings.json"
      did "Noctalia plugin copied to $PLUGIN_DST, replacing the link to this checkout (settings kept)"
    else
      did "Noctalia plugin copied to $PLUGIN_DST"
    fi
  fi
}

if [ "$PLUGIN" = 1 ]; then
  say "Installing the Noctalia plugin"
  install_plugin
fi

# ---- systemd ----
if have_user_manager; then
  say "Updating the systemd user manager"
  run systemctl --user daemon-reload
  # A unit outside systemd's search path (an unusual --prefix) is enabled by its full path.
  unit_ref="smartconnectd.service"
  if command -v systemd-analyze >/dev/null 2>&1 \
    && ! systemd-analyze --user unit-paths 2>/dev/null | grep -qxF "$UNIT_DIR"; then
    unit_ref="$UNIT"
    warn "$UNIT_DIR is not in systemd's user unit search path; --enable links $UNIT"
  fi
  if [ "$ENABLE" = 1 ]; then
    if systemctl --user is-active --quiet smartconnectd.service; then
      run systemctl --user enable "$unit_ref"
      run systemctl --user restart smartconnectd.service
      did "service enabled and restarted with the new build"
    elif pgrep -u "$(id -u)" -x smartconnectd >/dev/null 2>&1; then
      run systemctl --user enable "$unit_ref"
      warn "a smartconnectd you started by hand is running and holds port 52020: stop it, then run"
      echo "      systemctl --user start smartconnectd"
    else
      run systemctl --user enable --now "$unit_ref"
      did "service enabled and started"
    fi
  elif systemctl --user is-active --quiet smartconnectd.service; then
    warn "the service is running the previous build: systemctl --user restart smartconnectd"
  fi
else
  warn "no systemd user manager reachable: start smartconnectd from your session's autostart instead"
fi

# ---- environment checks ----
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on PATH: the Noctalia plugin starts the helpers by name, so add it to PATH (or set full paths in the plugin settings)" ;;
esac
if have_user_manager; then
  if ! systemctl --user show-environment 2>/dev/null | grep -Eq '^(WAYLAND_DISPLAY|DISPLAY)='; then
    warn "neither WAYLAND_DISPLAY nor DISPLAY is in the systemd user environment: run 'systemctl --user import-environment WAYLAND_DISPLAY DISPLAY' from your compositor's autostart (niri does this itself)"
  fi
fi

# ---- optional setup: printed, never run ----
cat <<EOF

${C_SAY}Optional setup that needs root (not done by this script):${C_OFF}

  Firewall: the phone connects to TCP 52020 and finds this PC over mDNS (UDP 5353).
    sudo firewall-cmd --permanent --add-port=52020/tcp
    sudo firewall-cmd --permanent --add-service=mdns
    sudo firewall-cmd --reload
    # or with ufw (use your LAN's range):
    sudo ufw allow from 192.168.1.0/24 to any port 52020 proto tcp
    sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp
    # Only for sharing a PC folder with the phone (smartconnect-files serve): TCP 2049,
    # or the --port you pass (2049 clashes with a running nfs-server).
    sudo firewall-cmd --permanent --add-port=2049/tcp

  Phone as a second monitor (Extend), niri only (GNOME, KDE, Hyprland and sway need nothing):
  the vkms virtual output.
    sudo modprobe vkms
    echo vkms | sudo tee /etc/modules-load.d/vkms.conf
    # niri: keep the virtual output off while it isn't used (~/.config/niri/config.kdl):
    output "Virtual-1" {
        off
    }

  Phone as a webcam: v4l2loopback.
    sudo pacman -S --needed v4l2loopback-dkms linux-headers   # or your distribution's package
    sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1
    echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
    echo 'options v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1' | sudo tee /etc/modprobe.d/v4l2loopback.conf

  Keyboard button on the phone display: wvkbd, from the AUR.
    yay -S wvkbd     # or: paru -S wvkbd

EOF

say "Done"
if [ "$ENABLE" != 1 ]; then
  echo "    systemctl --user enable --now smartconnectd   # start the daemon now and at every login"
fi
echo "    smartconnectd pair                            # show a pairing QR to scan in Smart Connect"
if [ "$PLUGIN" = 1 ] && { command -v qs >/dev/null 2>&1 || [ -d "$CONFIG_HOME/noctalia" ]; }; then
  echo "    Noctalia: enable Smart Connect under Settings > Plugins"
fi
echo "    Full documentation: docs/installation.md and docs/usage.md in this checkout"
