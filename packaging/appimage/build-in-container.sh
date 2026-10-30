#!/bin/sh
# Builds the Smart Connect AppImage inside an Arch Linux container (run by make-appimage.sh).
#
# Arch has the newest GTK, libadwaita and GStreamer; quick-sharun (from pkgforge-dev's Anylinux
# AppImages) bundles every library including glibc and runs the programs through sharun, so the
# result does not depend on the host's libraries and runs on older distributions too.
#
# Inputs: the checkout at /src (read-only), cargo caches at /cache, output to /out.
set -eu

QUICK_SHARUN_COMMIT=c28bb80743106fdf08cb10fb27d19d9932eb0865
PROGRAMS="smartconnectd smartconnect-gui smartconnect-mirror smartconnect-crosscontrol smartconnect-display smartconnect-webcam smartconnect-files smartconnect-fex"
# Host tools the programs run by name, bundled so they match the bundled libraries.
TOOLS="wl-copy wl-paste avahi-publish avahi-browse xkbcli notify-send"
# GStreamer plugins the programs use (everything else is left out: gst-plugins-bad alone pulls in
# dozens of libraries). webrtcbin needs nice, dtls, srtp, sctp, rtp and rtpmanager.
GST_PLUGINS="coreelements app typefindfunctions playback autodetect debug audioconvert audioresample
	audiotestsrc volume videoconvertscale videorate videotestsrc videoparsersbad rtp rtpmanager webrtc
	nice dtls srtp sctp opus x264 libav va pipewire pulseaudio opengl waylandsink video4linux2"

echo "==> Packages"
pacman -Syu --noconfirm --needed \
	base-devel git curl patchelf strace file zsync rustup \
	gtk4 libadwaita dbus fuse3 openssl \
	gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav \
	gst-plugin-pipewire gst-plugin-va libnice pipewire libpulse \
	mesa intel-media-driver libva \
	wl-clipboard avahi libxkbcommon libnotify \
	adwaita-icon-theme >/dev/null

echo "==> Rust"
export CARGO_HOME=/cache/cargo RUSTUP_HOME=/cache/rustup CARGO_TARGET_DIR=/cache/target
rustup default stable >/dev/null
rustup update stable >/dev/null 2>&1 || :

echo "==> Building the workspace"
rm -rf /build && mkdir -p /build
# Copy the checkout without build output or personal settings (the live Noctalia settings).
(cd /src && tar --exclude=./target --exclude='*/target' --exclude=./.git \
	--exclude=./noctalia-plugin/smart-connect/settings.json -cf - .) | tar -xf - -C /build
cd /build
cargo build --release --workspace --locked
for p in $PROGRAMS; do
	install -Dm755 "$CARGO_TARGET_DIR/release/$p" "/usr/bin/$p"
done

echo "==> Trimming GStreamer to the plugins in use"
gst_dir=/usr/lib/gstreamer-1.0
for f in "$gst_dir"/libgst*.so; do
	name=${f##*/libgst}
	name=${name%.so}
	keep=0
	for p in $GST_PLUGINS; do
		[ "$p" = "$name" ] && keep=1
	done
	[ "$keep" = 1 ] || rm -f "$f"
done
ls "$gst_dir"

echo "==> quick-sharun"
mkdir -p /tools
curl -fsSL -o /tools/quick-sharun \
	"https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/$QUICK_SHARUN_COMMIT/useful-tools/quick-sharun.sh"
chmod +x /tools/quick-sharun

rm -rf /appimage && mkdir -p /appimage && cd /appimage
export APPDIR=/appimage/AppDir OUTPATH=/out
mkdir -p "$APPDIR"
cp /src/packaging/appimage/AppRun.sh "$APPDIR"/AppRun.sh
chmod +x "$APPDIR"/AppRun.sh

# Files the --install step writes out.
share="$APPDIR/share/smartconnect-linux"
mkdir -p "$share"
cp /build/packaging/dev.smartconnect.Linux.desktop /build/packaging/dev.smartconnect.Linux.svg \
	/build/packaging/dev.smartconnect.Display.desktop /build/packaging/systemd/smartconnectd.service "$share"/
cp -a /build/noctalia-plugin/smart-connect "$share"/noctalia-plugin
rm -f "$share"/noctalia-plugin/settings.json

# Tells the app whether the installed copy is this build (setup.rs: update or open it).
echo "$(date -u +%Y%m%d%H%M%S)-$(cd /build && sha256sum Cargo.lock | cut -c1-12)" > "$share"/build-id

VA_DRIVERS=$(ls /usr/lib/dri/*_drv_video.so)
version=$(awk -F'"' '/^version/ {print $2; exit}' /build/Cargo.toml)
export VERSION="$version"
export DESKTOP=/build/packaging/dev.smartconnect.Linux.desktop
export ICON=/build/packaging/dev.smartconnect.Linux.svg
export MAIN_BIN=smartconnect-gui
export DEPLOY_GTK=1 DEPLOY_GSTREAMER=1 DEPLOY_OPENGL=1 DEPLOY_PIPEWIRE=1 DEPLOY_PULSE=1 DEPLOY_P11KIT=1
export UPINFO="gh-releases-zsync|vemru|linuxsmartconnect|latest|*x86_64.AppImage.zsync"
export OUTPUT_APPIMAGE=1
# The programs need a phone, a session bus and a compositor; there is none here, so only a short
# trace run each (the GStreamer plugins and VA drivers are passed explicitly instead).
export STRACE_TIME=3

tool_paths=""
for t in $TOOLS; do
	tool_paths="$tool_paths $(command -v "$t")"
done
prog_paths=""
for p in $PROGRAMS; do
	prog_paths="$prog_paths /usr/bin/$p"
done

# shellcheck disable=SC2086
/tools/quick-sharun $prog_paths $tool_paths $VA_DRIVERS

# The container runs as root: hand the output back to whoever ran make-appimage.sh.
chown -R "${HOST_UID:-0}:${HOST_GID:-0}" /out
ls -la /out
