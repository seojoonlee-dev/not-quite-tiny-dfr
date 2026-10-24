#!/usr/bin/env bash
# Remove not-quite-tiny-dfr and hand the Touch Bar back to stock tiny-dfr.
#
# Usage: ./uninstall.sh
#
# Run as your normal user; the script asks for sudo when it needs it. Your
# configs (~/.config/not-quite-tiny-dfr, /etc/not-quite-tiny-dfr) and the cache
# in /var/cache/not-quite-tiny-dfr are left in place.

set -euo pipefail

NAME=not-quite-tiny-dfr
BIN=/usr/bin/$NAME
SHARE=/usr/share/$NAME
UNIT=/etc/systemd/system/$NAME.service
RULES_DIR=/etc/udev/rules.d
RULES=(99-touchbar-$NAME.rules 99-touchbar-seat.rules)

for arg in "$@"; do
    case $arg in
        -h | --help)
            sed -n '2,8s/^# \{0,1\}//p' "$0"
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

info "Stopping and disabling $NAME.service"
sudo systemctl disable --now "$NAME.service" 2>/dev/null || true

info "Removing installed files"
sudo rm -f "$BIN" "$UNIT"
# 99-touchbar-seat.rules shares its name with tiny-dfr's own rule, so removing
# this copy lets tiny-dfr's version in /usr/lib/udev/rules.d apply again.
for rule in "${RULES[@]}"; do
    sudo rm -f "$RULES_DIR/$rule"
done
sudo rm -rf "$SHARE"
sudo systemctl daemon-reload
sudo udevadm control --reload

info "Restoring tiny-dfr"
if [[ $(systemctl is-enabled tiny-dfr.service 2>/dev/null) == masked ]]; then
    sudo systemctl unmask tiny-dfr.service
    sudo systemctl daemon-reload
fi

if systemctl cat tiny-dfr.service >/dev/null 2>&1; then
    # Some packages start tiny-dfr only from udev and have no [Install]
    # section, so enabling it may be a no-op.
    sudo systemctl enable tiny-dfr.service >/dev/null 2>&1 || true

    touchbar_ready=0
    for card in /sys/class/drm/card*; do
        driver=$(basename "$(readlink -f "$card/device/driver" 2>/dev/null)" 2>/dev/null) || continue
        if [[ $driver == appletbdrm || $driver == adp ]]; then
            touchbar_ready=1
            break
        fi
    done

    if ((touchbar_ready)) && sudo systemctl restart tiny-dfr.service; then
        info "Uninstalled. tiny-dfr is running again."
    else
        info "Uninstalled. Reboot to bring the Touch Bar up with tiny-dfr."
    fi
else
    info "Uninstalled."
    warn "tiny-dfr is not installed, so the Touch Bar will stay blank until it is."
    warn "Install it from your distribution (e.g. 'sudo pacman -S tiny-dfr' with the"
    warn "arch-mact2 repository) or from https://github.com/AsahiLinux/tiny-dfr, then reboot."
fi
