# Shared by install.sh and uninstall.sh (sourced, not run). Bash.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_ID="dev.smartconnect.Linux"
BINS=(smartconnectd smartconnect-mirror smartconnect-crosscontrol smartconnect-display smartconnect-webcam smartconnect-files smartconnect-fex smartconnect-gui smartconnect-share)
PLUGIN_ID="smart-connect"
DRY=0

if [ -t 1 ]; then
  C_SAY=$'\033[1;34m' C_OK=$'\033[1;32m' C_WARN=$'\033[1;33m' C_ERR=$'\033[1;31m' C_OFF=$'\033[0m'
else
  C_SAY='' C_OK='' C_WARN='' C_ERR='' C_OFF=''
fi
say()  { printf '%s==>%s %s\n' "$C_SAY" "$C_OFF" "$*"; }
ok()   { printf '%s[ok]%s %s\n' "$C_OK" "$C_OFF" "$*"; }
warn() { printf '%s[!]%s %s\n' "$C_WARN" "$C_OFF" "$*"; }
die()  { printf '%s[x]%s %s\n' "$C_ERR" "$C_OFF" "$*" >&2; exit 1; }
# Reports something done; silent with --dry-run, where `run` already printed it.
did()  { [ "$DRY" = 1 ] || ok "$@"; }

# Runs a command, or only prints it with --dry-run.
run() {
  if [ "$DRY" = 1 ]; then
    printf '    would run:'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

# Every install location, from PREFIX (default ~/.local) and the XDG variables.
set_paths() {
  PREFIX="${PREFIX:-$HOME/.local}"
  case "$PREFIX" in /*) ;; *) PREFIX="$PWD/$PREFIX" ;; esac
  PREFIX="${PREFIX%/}"
  CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
  BIN_DIR="$PREFIX/bin"
  SHARE_DIR="$PREFIX/share"
  APPS_DIR="$SHARE_DIR/applications"
  ICON_DIR="$SHARE_DIR/icons/hicolor/scalable/apps"
  DOC_DIR="$SHARE_DIR/doc/smartconnect-linux"
  # A per-user install puts the unit where `systemctl --user` looks first; any other prefix uses
  # the prefix's own lib/systemd/user (/usr/local/lib/systemd/user is in systemd's search path).
  if [ "$PREFIX" = "$HOME/.local" ]; then
    UNIT_DIR="$CONFIG_HOME/systemd/user"
  else
    UNIT_DIR="$PREFIX/lib/systemd/user"
  fi
  UNIT="$UNIT_DIR/smartconnectd.service"
  PLUGIN_SRC="$ROOT/noctalia-plugin/$PLUGIN_ID"
  PLUGIN_DST="$CONFIG_HOME/noctalia/plugins/$PLUGIN_ID"
}

# A real directory (not a symlink) holding the Smart Connect Noctalia plugin.
is_our_plugin_dir() {
  [ -d "$1" ] && [ ! -L "$1" ] && [ -f "$1/manifest.json" ] \
    && grep -q "\"id\": *\"$PLUGIN_ID\"" "$1/manifest.json"
}

# A reachable systemd user manager (systemctl --user works).
have_user_manager() {
  command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1
}

refuse_root() {
  if [ "$(id -u)" -eq 0 ]; then
    die "run this as your normal user, not as root: it works on your home directory and your systemd user manager. For a system-wide install, build a package (see packaging/README.md)."
  fi
}
