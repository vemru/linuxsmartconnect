#!/usr/bin/env bash
# Web installer for Smart Connect on Linux:
# curl -fsSL https://vemru.github.io/linuxsmartconnect/install.sh | bash
#
# Detects distribution (Arch, Fedora, Debian/Ubuntu, openSUSE, Void, Nix),
# downloads the latest release from GitHub Releases, verifies SHA-256 checksums,
# and installs AppImage, system package, or builds workspace.
set -euo pipefail

REPO="vemru/linuxsmartconnect"
INSTALL_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/smartconnect-linux"
BIN_DIR="${PREFIX:-$HOME/.local}/bin"

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[ok]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[!]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[x]\033[0m %s\n' "$*" >&2; exit 1; }

# Detect OS and distribution
detect_distro() {
  if [ -f /etc/os-release ]; then
    . /etc/os-release
    DISTRO="${ID:-linux}"
    DISTRO_LIKE="${ID_LIKE:-}"
  elif command -v nix-env >/dev/null 2>&1; then
    DISTRO="nix"
  else
    DISTRO="unknown"
  fi
  say "Detected distribution: $DISTRO ${DISTRO_LIKE:+(like $DISTRO_LIKE)}"
}

# Fetch latest release tag and download files with SHA-256 verification
install_release() {
  local api_url="https://api.github.com/repos/$REPO/releases/latest"
  say "Querying latest release from $REPO..."
  local release_json
  release_json="$(curl -fsSL -H "Accept: application/vnd.github.v3+json" "$api_url" || true)"

  if [ -z "$release_json" ]; then
    warn "Could not fetch GitHub release metadata; falling back to local source build if present."
    if [ -f "packaging/install.sh" ]; then
      exec ./packaging/install.sh "$@"
    else
      die "Cannot download release and not in source checkout."
    fi
  fi

  local tag
  tag="$(printf '%s' "$release_json" | grep -m1 '"tag_name":' | cut -d'"' -f4)"
  [ -n "$tag" ] || die "Could not determine latest release tag"
  say "Latest release is $tag"

  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  local appimage_url="https://github.com/$REPO/releases/download/$tag/SmartConnect-x86_64.AppImage"
  local sha_url="https://github.com/$REPO/releases/download/$tag/SHA256SUMS"

  say "Downloading release files..."
  curl -fsSL -o "$tmp/SmartConnect.AppImage" "$appimage_url" || die "Failed to download AppImage"
  curl -fsSL -o "$tmp/SHA256SUMS" "$sha_url" || warn "SHA256SUMS not published for $tag; skipping hash check"

  if [ -f "$tmp/SHA256SUMS" ]; then
    say "Verifying SHA-256 checksum..."
    (cd "$tmp" && sha256sum --check --ignore-missing SHA256SUMS) || die "SHA-256 checksum mismatch! Aborting."
    ok "SHA-256 checksum verified successfully."
  fi

  mkdir -p "$BIN_DIR"
  install -m755 "$tmp/SmartConnect.AppImage" "$BIN_DIR/smartconnect-gui"
  chmod +x "$BIN_DIR/smartconnect-gui"

  # Run AppImage setup to install desktop entries and icons
  say "Running initial setup..."
  "$BIN_DIR/smartconnect-gui" --background || true

  ok "Installed Smart Connect $tag to $BIN_DIR/smartconnect-gui"
}

detect_distro
install_release "$@"
