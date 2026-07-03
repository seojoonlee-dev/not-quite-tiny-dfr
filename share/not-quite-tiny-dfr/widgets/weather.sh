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

out=$(curl -sf --max-time 10 "wttr.in/$1?format=$cond+%t$extra" 2>/dev/null)
if [ -z "$out" ]; then
    echo "weather n/a"
    exit 0
fi

# wttr.in writes positive temperatures as "+11°C" and pads some fields;
# drop the sign and squeeze the whitespace.
echo "$out" | sed 's/+//' | tr -s ' '
