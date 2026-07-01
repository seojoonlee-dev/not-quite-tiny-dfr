#!/bin/sh
# Bundled default widget for not-quite-tiny-dfr.
# Prints CPU/SoC temperature as JSON, color-coded warm/cool.
#
# Use it from your config:
#   { Command = "sh /usr/share/not-quite-tiny-dfr/widgets/cpu_temp.sh", Interval = 2, Stretch = 2 }

read_temp() {
    # Prefer the x86 package temperature.
    zone=$(grep -l x86_pkg_temp /sys/class/thermal/thermal_zone*/type 2>/dev/null | head -n1)
    if [ -n "$zone" ]; then
        cat "${zone%/type}/temp" 2>/dev/null
        return
    fi
    # Fallback (e.g. Apple Silicon): the hottest thermal zone.
    max=""
    for f in /sys/class/thermal/thermal_zone*/temp; do
        v=$(cat "$f" 2>/dev/null) || continue
        [ -z "$v" ] && continue
        if [ -z "$max" ] || [ "$v" -gt "$max" ]; then
            max=$v
        fi
    done
    echo "$max"
}

t=$(read_temp)
if [ -z "$t" ]; then
    echo '{"text":"CPU n/a"}'
    exit 0
fi

c=$((t / 1000))
if [ "$c" -ge 85 ]; then
    color="#fb4934"   # hot: red
elif [ "$c" -ge 70 ]; then
    color="#fabd2f"   # warm: yellow
else
    color="#8ec07c"   # cool: green
fi

printf '{"text":"CPU %d°C","color":"%s"}\n' "$c" "$color"
