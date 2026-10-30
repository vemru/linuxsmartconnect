#!/usr/bin/env bash
# setup-v4l2loopback.sh — Configure virtual webcam device for Smart Connect
#
# Creates a permanent virtual V4L2 loopback device (/dev/video10 labeled "Phone Camera")
# so that every web browser (Google Chrome, Firefox, Chromium, Brave, Edge), video call
# service (Google Meet, Zoom, Teams, Discord), and desktop application sees the phone
# camera as a standard hardware webcam.
#
# Must be run with root privileges (e.g., sudo ./packaging/setup-v4l2loopback.sh).

set -euo pipefail

C_GREEN="\033[1;32m"
C_BLUE="\033[1;34m"
C_YELLOW="\033[1;33m"
C_RED="\033[1;31m"
C_OFF="\033[0m"

say() { echo -e "${C_BLUE}==>${C_OFF} $*"; }
ok()  { echo -e "${C_GREEN}✓${C_OFF} $*"; }
warn(){ echo -e "${C_YELLOW}!${C_OFF} $*"; }
err() { echo -e "${C_RED}✗${C_OFF} $*" >&2; exit 1; }

if [ "$(id -u)" -ne 0 ]; then
    err "This script must be run as root. Please run: sudo $0"
fi

say "Detecting Linux distribution..."
ID="unknown"
ID_LIKE=""
if [ -f /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
fi

say "Detected OS: ${NAME:-$ID} ($ID)"

# 1. Install v4l2loopback package if not present
if ! modinfo v4l2loopback >/dev/null 2>&1; then
    say "Installing v4l2loopback kernel module..."
    case "$ID" in
        fedora|rhel|centos)
            say "Running dnf install for akmod-v4l2loopback..."
            dnf install -y akmod-v4l2loopback kernel-devel
            say "Building akmods module..."
            akmods --force || true
            ;;
        arch|manjaro|endeavouros)
            say "Running pacman for v4l2loopback-dkms..."
            pacman -S --needed --noconfirm v4l2loopback-dkms linux-headers
            ;;
        debian|ubuntu|pop|mint)
            say "Running apt-get for v4l2loopback-dkms..."
            apt-get update -y
            apt-get install -y v4l2loopback-dkms "linux-headers-$(uname -r)"
            ;;
        opensuse*|suse)
            say "Running zypper for v4l2loopback-kmp-default..."
            zypper install -y v4l2loopback-kmp-default
            ;;
        *)
            if [[ "$ID_LIKE" == *"fedora"* ]] || [[ "$ID_LIKE" == *"rhel"* ]]; then
                dnf install -y akmod-v4l2loopback kernel-devel && akmods --force || true
            elif [[ "$ID_LIKE" == *"arch"* ]]; then
                pacman -S --needed --noconfirm v4l2loopback-dkms linux-headers
            elif [[ "$ID_LIKE" == *"debian"* ]] || [[ "$ID_LIKE" == *"ubuntu"* ]]; then
                apt-get install -y v4l2loopback-dkms "linux-headers-$(uname -r)"
            else
                warn "Unsupported distribution $ID. Please install v4l2loopback for your distribution manually."
            fi
            ;;
    esac
else
    ok "v4l2loopback kernel module is already installed."
fi

# 2. Configure persistence: modprobe options and modules-load
say "Configuring /etc/modprobe.d/v4l2loopback.conf..."
mkdir -p /etc/modprobe.d /etc/modules-load.d

# NOTE: exclusive_caps=1 is required for Chromium, Google Chrome, and Google Meet
# to recognize the loopback device as a capture camera.
cat <<'EOF' > /etc/modprobe.d/v4l2loopback.conf
# Virtual camera loopback device for Smart Connect
# exclusive_caps=1 ensures Chromium/Chrome and WebRTC apps see it as a webcam
options v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1
EOF
ok "Saved /etc/modprobe.d/v4l2loopback.conf"

say "Configuring /etc/modules-load.d/v4l2loopback.conf..."
cat <<'EOF' > /etc/modules-load.d/v4l2loopback.conf
# Load v4l2loopback on boot
v4l2loopback
EOF
ok "Saved /etc/modules-load.d/v4l2loopback.conf"

# 3. Reload kernel module with new parameters
say "Loading v4l2loopback module..."
if lsmod | grep -q "^v4l2loopback\s"; then
    say "Unloading existing v4l2loopback module..."
    modprobe -r v4l2loopback || true
fi

modprobe v4l2loopback devices=1 video_nr=10 card_label="Phone Camera" exclusive_caps=1

# 4. Verify device node
if [ -e /dev/video10 ]; then
    ok "Virtual camera device /dev/video10 is ready!"
    echo ""
    echo -e "${C_GREEN}Success!${C_OFF} Virtual webcam is configured and active:"
    echo "  Device:     /dev/video10"
    echo "  Label:      Phone Camera"
    echo "  Status:     Persistent across reboots"
    echo ""
    echo "Next steps:"
    echo "  1. Start the phone webcam in Smart Connect (GUI button or: smartconnect-webcam --start)"
    echo "  2. In Google Meet or any browser/app, select 'Phone Camera' as your camera."
else
    # Check if another video node was assigned
    VIRT_DEV=$(ls -d /sys/devices/virtual/video4linux/video* 2>/dev/null | head -n 1 || true)
    if [ -n "$VIRT_DEV" ]; then
        DEV_NAME=$(basename "$VIRT_DEV")
        ok "Virtual camera device /dev/$DEV_NAME is ready!"
        echo ""
        echo "Next steps:"
        echo "  1. Start the phone webcam in Smart Connect"
        echo "  2. Select 'Phone Camera' (/dev/$DEV_NAME) in your browser/app."
    else
        warn "v4l2loopback module loaded, but virtual device node was not found immediately."
        warn "You may need to reboot or build the module with: sudo akmods --force"
    fi
fi
