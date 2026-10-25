#!/usr/bin/env bash
# Removes what install.sh put in place. Pairing and identity (~/.local/share/smartconnect-linux),
# preferences and caches stay unless --purge is given. Firewall rules and kernel module settings
# are never touched.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

usage() {
  cat <<'EOF'
Usage: packaging/uninstall.sh [options]

  --prefix DIR   the prefix install.sh used (default: $PREFIX, else ~/.local)
  --purge        also delete pairings and identity, preferences and caches
                 (the phone then has to pair again)
  --dry-run      only print what would be removed
  -h, --help     show this help
EOF
}

PURGE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX="$2"; shift 2 ;;
    --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
    --purge) PURGE=1; shift ;;
    --dry-run|-n) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
refuse_root
set_paths
[ "$DRY" = 1 ] && say "Dry run: nothing is changed"

remove() { # removes each existing file or symlink
  local f
  for f in "$@"; do
    if [ -e "$f" ] || [ -L "$f" ]; then run rm -f "$f"; did "removed $f"; fi
  done
}

# ---- service ----
if [ -f "$UNIT" ]; then
  if have_user_manager; then
    run systemctl --user disable --now smartconnectd.service || true
  fi
  remove "$UNIT"
  if have_user_manager; then run systemctl --user daemon-reload; fi
fi

# ---- files ----
for b in "${BINS[@]}"; do
  remove "$BIN_DIR/$b"
done
remove "$APPS_DIR/$APP_ID.desktop" "$APPS_DIR/dev.smartconnect.Display.desktop" "$ICON_DIR/$APP_ID.svg"
old_desktop="$APPS_DIR/smartconnect-gui.desktop"
if [ -f "$old_desktop" ] && grep -q '^Exec=.*smartconnect-gui' "$old_desktop"; then
  remove "$old_desktop"
fi
if [ -d "$DOC_DIR" ]; then run rm -rf "$DOC_DIR"; did "removed $DOC_DIR"; fi
if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$APPS_DIR" ]; then
  run update-desktop-database -q "$APPS_DIR" || true
fi

# ---- Noctalia plugin ----
if [ -L "$PLUGIN_DST" ]; then
  target="$(readlink -f "$PLUGIN_DST" || true)"
  if [ -n "$target" ] && { [ "$target" = "$(readlink -f "$PLUGIN_SRC")" ] || is_our_plugin_dir "$target"; }; then
    run rm "$PLUGIN_DST" # the link only; the checkout is untouched
    did "removed the Noctalia plugin link $PLUGIN_DST"
  else
    warn "$PLUGIN_DST links to ${target:-a missing path}: left alone"
  fi
elif is_our_plugin_dir "$PLUGIN_DST"; then
  run rm -rf "$PLUGIN_DST"
  did "removed the Noctalia plugin $PLUGIN_DST (with its settings)"
elif [ -e "$PLUGIN_DST" ]; then
  warn "$PLUGIN_DST is not the Smart Connect plugin: left alone"
fi

# ---- user data ----
STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/smartconnect-linux"
PREFS_DIR="$CONFIG_HOME/smartconnect-linux"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/smartconnect"
if [ "$PURGE" = 1 ]; then
  for d in "$STATE_DIR" "$PREFS_DIR" "$CACHE_DIR"; do
    if [ -d "$d" ]; then run rm -rf "$d"; did "removed $d"; fi
  done
  echo "Also remove this PC from the Smart Connect app on the phone."
else
  echo "Kept $STATE_DIR (pairings, identity), $PREFS_DIR and $CACHE_DIR: --purge deletes them."
fi
say "Uninstalled. Firewall rules and kernel module settings, if you added any, are unchanged."
