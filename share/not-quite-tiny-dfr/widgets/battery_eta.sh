#!/bin/sh
# Bundled expand-view widget for not-quite-tiny-dfr's battery.sh.
#
# Instant, smoothed battery time-to-empty / time-to-full. It combines UPower's
# recalibrated capacity (Energy / EnergyFull -- accurate on degraded T2 packs)
# with the *instantaneous* current and voltage from sysfs, so the estimate
# reacts to load right away instead of trailing UPower's own smoothed
# EnergyRate (which it only re-emits every ~30s):
#
#   discharging:  eta = Energy            / (current_now * voltage_now)
#   charging:     eta = (EnergyFull-Energy) / (current_now * voltage_now)
#
# current_now is spiky, so it is sampled several times and the median is taken
# (which ignores transient spikes outright); that value is then eased with an
# EMA persisted between runs, giving a steady number without losing response.
#
# Meant as the ExpandCommand of a battery.sh widget with OnClick = Expand -- the
# collapsed button shows the percentage, tapping expands it to this ETA. Uses
# the same UPower daemon as battery.sh, so it adds no dependency.
#
# Usage: battery_eta.sh [device]
#   device: a UPower object-path leaf (default DisplayDevice, the aggregate).
# Env knobs: NQTD_BAT (sysfs dir), NQTD_ETA_SAMPLES, NQTD_ETA_ALPHA.

dev="${1:-DisplayDevice}"
path="/org/freedesktop/UPower/devices/$dev"
bat="${NQTD_BAT:-/sys/class/power_supply/BAT0}"
# current_now is already a stable, slowly-updating reading on T2 hardware, so a
# short burst (median-filtered, ~0.1s total) is enough to shrug off a stray
# glitch read while keeping the widget's response to a tap effectively instant.
# alpha is the EMA weight of each new reading; low = heavily smoothed. The widget
# polls this script every couple of seconds in the background, so a small alpha
# gives a steady value that drifts rather than jumps as load changes.
samples="${NQTD_ETA_SAMPLES:-4}"
gap="${NQTD_ETA_GAP:-0.02}"
alpha="${NQTD_ETA_ALPHA:-0.2}"

prop() {
    busctl --system get-property org.freedesktop.UPower "$path" \
        org.freedesktop.UPower.Device "$1" 2>/dev/null
}
# "d 65.4585" -> 65.4585 ; "u 2" / "x 8130" -> 2 / 8130
val() { prop "$1" | awk '{print $2}'; }

energy=$(val Energy)
energy_full=$(val EnergyFull)
state=$(val State)

# UPower is local; if it's unreachable there's nothing to estimate from.
if [ -z "$energy" ] || [ -z "$state" ]; then
    echo "eta n/a"
    exit 0
fi

# --- instant power (W), spike-trimmed ---
# Sample current_now a handful of times and take the median. Voltage barely
# moves, so read it once.
volt_uv=$(cat "$bat/voltage_now" 2>/dev/null)
readings=""
i=0
while [ "$i" -lt "$samples" ]; do
    c=$(cat "$bat/current_now" 2>/dev/null)
    [ -n "$c" ] && readings="$readings $c"
    i=$((i + 1))
    [ "$i" -lt "$samples" ] && sleep "$gap"
done

# Median of |current| in µA (abs: some drivers sign the charge direction). The
# median shrugs off transient spikes -- a heavy sample or two never moves it.
cur_ua=$(printf '%s\n' $readings | awk '{ print ($1 < 0 ? -$1 : $1) }' | sort -n | awk '
    { a[NR] = $1 }
    END {
        if (NR == 0) { print 0; exit }
        printf "%d", a[int((NR + 1) / 2)]
    }')

# Instant power W = current(A) * voltage(V) = (µA/1e6) * (µV/1e6).
power=$(awk -v c="$cur_ua" -v v="$volt_uv" 'BEGIN { printf "%.4f", (c / 1e6) * (v / 1e6) }')

# --- EMA across runs (persisted). Reset only after a long gap (well past the
# widget's background poll interval), i.e. the daemon wasn't polling us, so the
# old power is stale; a normal run-to-run gap keeps smoothing. ---
state_file="${XDG_RUNTIME_DIR:-/tmp}/nqtd-battery-eta.${dev}.state"
now=$(date +%s)
prev_p=""
prev_t=0
[ -r "$state_file" ] && read prev_p prev_t < "$state_file" 2>/dev/null
if [ -z "$prev_p" ] || [ "$((now - prev_t))" -gt 30 ]; then
    smooth=$power
else
    smooth=$(awk -v n="$power" -v o="$prev_p" -v a="$alpha" \
        'BEGIN { printf "%.4f", a * n + (1 - a) * o }')
fi
printf '%s %s\n' "$smooth" "$now" > "$state_file" 2>/dev/null

# Below a floor the rate is basically idle/noise -> no meaningful ETA yet.
too_low=$(awk -v p="$smooth" 'BEGIN { print (p < 0.5) ? 1 : 0 }')

fmt() {  # seconds -> "Xh Ym" / "Ym" / "<1m"
    s=$1
    h=$((s / 3600))
    m=$(((s % 3600) / 60))
    if [ "$h" -gt 0 ]; then printf '%dh %dm' "$h" "$m"
    elif [ "$m" -gt 0 ]; then printf '%dm' "$m"
    else printf '<1m'; fi
}

# UPower Device.State 2 = discharging; anything else is on the charger.
if [ "$state" = "2" ]; then
    if [ "$too_low" = "1" ]; then
        echo "estimating…"
    else
        secs=$(awk -v e="$energy" -v p="$smooth" 'BEGIN { printf "%d", (e / p) * 3600 }')
        echo "$(fmt "$secs") left"
    fi
else
    remain=$(awk -v f="$energy_full" -v e="$energy" \
        'BEGIN { r = f - e; printf "%.4f", (r < 0 ? 0 : r) }')
    full=$(awk -v r="$remain" 'BEGIN { print (r < 0.05) ? 1 : 0 }')
    if [ "$state" = "4" ] || [ "$full" = "1" ]; then
        # Fully charged (State 4, or there's essentially nothing left to add).
        echo "full"
    elif [ "$too_low" = "1" ]; then
        # Just plugged in; UPower has no rate yet.
        echo "charging"
    else
        secs=$(awk -v r="$remain" -v p="$smooth" 'BEGIN { printf "%d", (r / p) * 3600 }')
        echo "$(fmt "$secs") to full"
    fi
fi
