#!/bin/sh
# Builds dist/Smart_Connect-<version>-x86_64.AppImage in a throwaway Arch Linux container.
#
#   packaging/appimage/make-appimage.sh
#
# Needs Docker (or Podman: DOCKER=podman). The checkout is mounted read-only; Rust, cargo and the
# build output are kept in the volume "smartconnect-appimage-cache" so rebuilds are quick
# (`docker volume rm smartconnect-appimage-cache` frees the space). See docs/installation.md.
set -eu

DOCKER=${DOCKER:-docker}
root=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$root/dist"

exec "$DOCKER" run --rm \
	-e CI=1 -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
	-v "$root":/src:ro \
	-v "$root/dist":/out \
	-v smartconnect-appimage-cache:/cache \
	archlinux:latest \
	sh /src/packaging/appimage/build-in-container.sh
