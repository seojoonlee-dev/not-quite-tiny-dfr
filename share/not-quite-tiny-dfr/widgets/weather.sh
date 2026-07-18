#!/bin/sh
# Bundled default widget for not-quite-tiny-dfr.
# Prints the current weather from wttr.in.
#
# Usage: weather.sh [-e] [-H] [-w] [location]
#   -e  show the condition as an emoji instead of text
#   -H  include humidity
#   -w  include wind
# The location defaults to IP geolocation.
#
# Use it from your config:
#   { Command = "sh /usr/share/not-quite-tiny-dfr/widgets/weather.sh -e -H Seoul", Interval = 900, Stretch = 3 }

cond="%C"
extra=""
while getopts eHw opt; do
    case $opt in
        e) cond="%c" ;;
        H) extra="$extra+%h" ;;
        w) extra="$extra+%w" ;;
        *) ;;
    esac
done
shift $((OPTIND - 1))

# wttr.in is frequently rate-limited or briefly unreachable. Rather than blank
# the widget to "weather n/a" on every transient hiccup, cache the last good
# reading (keyed by the requested format+location) and fall back to it. Only
# show "weather n/a" when a fetch fails and we have never had a good reading.
# Under the systemd sandbox the home directory is read-only; CACHE_DIRECTORY
# (from the unit's CacheDirectory=) is the writable spot there.
cache_dir="${CACHE_DIRECTORY:-${XDG_CACHE_HOME:-$HOME/.cache}/not-quite-tiny-dfr}"
cache_key=$(printf '%s' "$cond+%t$extra|$1" | tr -c 'A-Za-z0-9' '_')
cache_file="$cache_dir/weather_$cache_key"

out=$(curl -sf --max-time 10 "wttr.in/$1?format=$cond+%t$extra" 2>/dev/null)
if [ -z "$out" ]; then
    if [ -s "$cache_file" ]; then
        cat "$cache_file"
    else
        echo "weather n/a"
    fi
    # Non-zero exit = no fresh data this run; the daemon retries with a short
    # backoff instead of sitting on this output for a full interval.
    exit 1
fi

# wttr.in writes positive temperatures as "+11°C" and pads some fields;
# drop the sign and squeeze the whitespace.
formatted=$(echo "$out" | sed 's/+//' | tr -s ' ')
mkdir -p "$cache_dir" 2>/dev/null && printf '%s\n' "$formatted" > "$cache_file"
printf '%s\n' "$formatted"
