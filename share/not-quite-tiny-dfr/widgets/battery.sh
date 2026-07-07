#!/bin/sh
# Bundled default widget for not-quite-tiny-dfr.
# Prints the battery percentage from the UPower daemon -- the same source the
# caelestia shell reads. UPower recalibrates the battery's learned "full"
# charge, so it stays accurate on degraded T2 Macs where the kernel's
# charge_full/capacity read wrong. (The built-in `Battery` indicator does its
# own /sys/class/power_supply math and can stick near 100% on those batteries.)
#
# Emits JSON with an `icon` so the widget shows the same stepped battery glyph
# the built-in indicator used, next to the percentage.
#
# Usage: battery.sh [-c] [-t] [-w] [device]
#   -c  color the label green while charging, red when low (< 10%)
#   -t  text only (no icon)
#   -w  watch UPower and block until it reports a change (plug/unplug/level)
#       before reading, so the poller repaints within a fraction of a second of
#       an event instead of waiting out the Interval. Pair with a small Interval
#       (0 -> the built-in 100ms floor) so the monitor is essentially always up.
#   -f N  seed the "full" ceiling at N%. The script auto-learns this: whenever
#       the firmware reports fully charged, it records that raw percentage as
#       the pack's real 100% and rescales 0..ceiling -> 0..100 (clamped), so a
#       degraded T2 cell that stops at ~97% reads a flat 100% like macOS and
#       glides down when unplugged instead of snapping to 97. -f just provides a
#       starting ceiling before the first full charge is seen; default 100.
# device is a UPower object-path leaf (e.g. battery_BAT0); it defaults to
# DisplayDevice, the aggregate battery UPower presents (what caelestia uses).
#
# Use it from your config (plain 30s poll, or -w for instant plug/unplug):
#   { Command = "sh /usr/share/not-quite-tiny-dfr/widgets/battery.sh", Interval = 30, Stretch = 1 }
#   { Command = "sh /usr/share/not-quite-tiny-dfr/widgets/battery.sh -w -f 96", Interval = 0, Stretch = 1 }

color=""
noicon=""
watch=""
fullat=100
while getopts ctwf: opt; do
    case $opt in
        c) color=1 ;;
        t) noicon=1 ;;
        w) watch=1 ;;
        f) fullat=$OPTARG ;;
        *) ;;
    esac
done
shift $((OPTIND - 1))

dev="${1:-DisplayDevice}"
path="/org/freedesktop/UPower/devices/$dev"

# Backstop for -w: cap the wait so a quiet battery still refreshes and we never
# run long enough to trip the widget's 30s command timeout (which would paint
# "timeout"). UPower emits on every level change anyway, so this rarely fires.
backstop=20

prop() {
    busctl --system get-property org.freedesktop.UPower "$path" \
        org.freedesktop.UPower.Device "$1" 2>/dev/null
}

# Block until UPower signals a change for this device, or the backstop elapses.
# gdbus subscribes as a normal client (no eavesdrop privilege, unlike `busctl
# monitor`). We read a single line through a fifo -- not a pipe straight to
# gdbus -- because the shell waits for every process in a pipeline, which would
# make us sit out the whole monitor instead of leaving the instant a signal
# lands; so we read one line, then stop the monitor ourselves.
wait_change() {
    command -v gdbus >/dev/null 2>&1 || { sleep "$backstop"; return; }
    fifo=$(mktemp -u) && mkfifo "$fifo" 2>/dev/null || { sleep "$backstop"; return; }
    gdbus monitor --system --dest org.freedesktop.UPower \
        --object-path "$path" >"$fifo" 2>/dev/null &
    gd=$!
    timeout "$backstop" head -n 1 "$fifo" >/dev/null 2>&1
    kill "$gd" 2>/dev/null
    wait "$gd" 2>/dev/null
    rm -f "$fifo"
}

# In watch mode, wait for the next change before reading, then fall through and
# emit the fresh state. (Read-then-block would emit the pre-change reading, and
# the widget only reads our stdout once we exit -- so we must block first.)
[ -n "$watch" ] && wait_change

# "d 78.3342" -> 78.33 ; "u 2" -> 2
raw=$(prop Percentage | awk '{printf "%.2f", $2}')
state=$(prop State | awk '{print $2}')

# UPower is local, so a failure means the daemon is down or the device is gone
# -- there is nothing to fall back to, so just say so.
if [ -z "$raw" ]; then
    echo "battery n/a"
    exit 0
fi

# Auto-learn the "full" ceiling. Whenever the firmware reports the battery fully
# charged (State 4), the raw percentage at that moment is this pack's real 100%
# -- a degraded T2 cell stops a few percent short and the SMC won't trickle the
# rest. Persist it (the process is short-lived, and the value has to survive the
# whole discharge), then rescale 0..cap -> 0..100 so the top reads a flat 100
# and unplugging glides down instead of snapping to the raw ~97. -f seeds the
# ceiling before the first full charge of the session is seen.
cap_file="${XDG_RUNTIME_DIR:-/tmp}/nqtd-battery-fullat.${dev}"
cap=""
[ -r "$cap_file" ] && cap=$(cat "$cap_file" 2>/dev/null)
if [ "$state" = "4" ]; then
    # Ignore an implausible reading rather than poison the ceiling with it.
    if [ "$(awk -v r="$raw" 'BEGIN { print (r >= 50 && r <= 100) ? 1 : 0 }')" = "1" ]; then
        printf '%s\n' "$raw" > "$cap_file" 2>/dev/null
        cap="$raw"
    fi
fi
[ -z "$cap" ] && cap="$fullat"

# Rescale to the learned/seed ceiling, clamped at 100; and force 100 while the
# firmware says fully charged (covers a raw level sitting just under the cap).
pct=$(awk -v r="$raw" -v c="$cap" \
    'BEGIN { if (c <= 0) c = 100; v = r * 100.0 / c; if (v > 100) v = 100; printf "%.0f", v }')
[ "$state" = "4" ] && pct=100

# UPower Device.State: 1=charging, 4=fully charged, 5=pending charge.
charging=""
case "$state" in
    1 | 4 | 5) charging=1 ;;
esac

# Pick the icon the same way the built-in indicator stepped its SVGs.
icon=""
if [ -z "$noicon" ]; then
    if [ -n "$charging" ]; then
        if   [ "$pct" -le 20 ]; then icon="battery_charging_20"
        elif [ "$pct" -le 30 ]; then icon="battery_charging_30"
        elif [ "$pct" -le 50 ]; then icon="battery_charging_50"
        elif [ "$pct" -le 60 ]; then icon="battery_charging_60"
        elif [ "$pct" -le 80 ]; then icon="battery_charging_80"
        elif [ "$pct" -le 99 ]; then icon="battery_charging_90"
        else                         icon="battery_charging_full"
        fi
    else
        if   [ "$pct" -le 0 ];  then icon="battery_0_bar"
        elif [ "$pct" -le 20 ]; then icon="battery_1_bar"
        elif [ "$pct" -le 30 ]; then icon="battery_2_bar"
        elif [ "$pct" -le 50 ]; then icon="battery_3_bar"
        elif [ "$pct" -le 60 ]; then icon="battery_4_bar"
        elif [ "$pct" -le 80 ]; then icon="battery_5_bar"
        elif [ "$pct" -le 99 ]; then icon="battery_6_bar"
        else                         icon="battery_full"
        fi
    fi
fi

# Mirror the built-in indicator's default state colors (BatteryChargingColor /
# BatteryLowColor) when asked; otherwise let the label inherit the button color.
hex=""
if [ -n "$color" ]; then
    if [ -n "$charging" ]; then
        hex="#00b300"
    elif [ "$pct" -lt 10 ]; then
        hex="#b30000"
    fi
fi

# Build the JSON object from whichever fields apply.
json="\"text\":\"$pct%\""
[ -n "$icon" ] && json="$json,\"icon\":\"$icon\""
[ -n "$hex" ] && json="$json,\"color\":\"$hex\""
printf '{%s}\n' "$json"
