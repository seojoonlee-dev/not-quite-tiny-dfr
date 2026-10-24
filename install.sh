#!/usr/bin/env bash
# Build and install not-quite-tiny-dfr. Remove it with ./uninstall.sh.
#
# Usage: ./install.sh [--no-build]
#   --no-build   install the existing target/release binary without building
#
# Run as your normal user; the script asks for sudo when it needs it. Your
# configs (~/.config/not-quite-tiny-dfr, /etc/not-quite-tiny-dfr) are never
# touched.

set -euo pipefail

NAME=not-quite-tiny-dfr
BIN=/usr/bin/$NAME
SHARE=/usr/share/$NAME
UNIT=/etc/systemd/system/$NAME.service
RULES_DIR=/etc/udev/rules.d
RULES=(99-touchbar-$NAME.rules 99-touchbar-seat.rules)
PKG_MODULES=(cairo pango pangocairo librsvg-2.0 libinput libudev fontconfig freetype2)

cd "$(dirname "$(readlink -f "$0")")"

build=1
for arg in "$@"; do
    case $arg in
        --no-build) build=0 ;;
        -h | --help)
            sed -n '2,9s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg (see --help)" >&2
            exit 1
            ;;
    esac
done

info() { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
    printf '\033[1;31merror:\033[0m %s\n' "$*" >&2
    exit 1
}

if [[ $EUID -eq 0 ]]; then
    die "run this as your normal user, not root; it uses sudo where needed"
fi
command -v sudo >/dev/null || die "sudo is required"

if ((build)); then
    command -v cargo >/dev/null || die "cargo not found; install Rust from https://rustup.rs or your package manager"
    command -v pkg-config >/dev/null || die "pkg-config not found"
    missing=()
    for module in "${PKG_MODULES[@]}"; do
        pkg-config --exists "$module" || missing+=("$module")
    done
    if ((${#missing[@]})); then
        die "missing development libraries: ${missing[*]}"
    fi
    info "Building (release)"
    cargo build --release --locked
fi
[[ -x target/release/$NAME ]] || die "target/release/$NAME not found; run without --no-build"

# Stock tiny-dfr drives the same display, so both can't run at once. Its own
# udev rule starts it even when it isn't enabled, so masking is the only
# reliable way to keep it off. uninstall.sh unmasks it again.
if [[ $(systemctl is-enabled tiny-dfr.service 2>/dev/null) != masked ]] &&
    systemctl cat tiny-dfr.service >/dev/null 2>&1; then
    warn "tiny-dfr is installed and would conflict with $NAME"
    read -rp "Stop and mask tiny-dfr.service? (./uninstall.sh restores it) [Y/n] " reply
    if [[ ! $reply =~ ^[Nn] ]]; then
        sudo systemctl disable --now tiny-dfr.service 2>/dev/null || true
        sudo systemctl mask tiny-dfr.service
    else
        warn "leaving tiny-dfr.service as is; mask it before using $NAME"
    fi
fi

info "Installing files"
# install, not cp: it replaces the binary even while the daemon is running.
sudo install -Dm755 "target/release/$NAME" "$BIN"
sudo install -d "$SHARE"
sudo cp -rT "share/$NAME" "$SHARE"
sudo install -Dm644 "etc/systemd/system/$NAME.service" "$UNIT"
for rule in "${RULES[@]}"; do
    sudo install -Dm644 "etc/udev/rules.d/$rule" "$RULES_DIR/$rule"
done

sudo systemctl daemon-reload
sudo udevadm control --reload
sudo systemctl enable "$NAME.service" >/dev/null 2>&1

# The Touch Bar's display only shows up once the udev rules have configured
# it, which on a first install usually takes a reboot.
touchbar_ready=0
for card in /sys/class/drm/card*; do
    driver=$(basename "$(readlink -f "$card/device/driver" 2>/dev/null)" 2>/dev/null) || continue
    if [[ $driver == appletbdrm || $driver == adp ]]; then
        touchbar_ready=1
        break
    fi
done

if ((touchbar_ready)); then
    info "Restarting $NAME.service"
    sudo systemctl restart "$NAME.service"
    info "Installed. Edit ~/.config/$NAME/config.toml to customize; changes apply on save."
else
    info "Installed. Reboot to bring up the Touch Bar."
fi
