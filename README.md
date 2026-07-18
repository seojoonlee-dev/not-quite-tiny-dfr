# not-quite-tiny-dfr
Not quite the most basic dynamic function row daemon possible.

A customizable Touch Bar daemon for Apple T2 and Silicon Macs, forked from
[tiny-dfr](https://github.com/AsahiLinux/tiny-dfr). It adds full theming
(colors, spacing, corners, fonts, background images), per-user configuration in
`~/.config/not-quite-tiny-dfr/`, and programmable **command widgets**.

Config is merged from, in increasing precedence:
`/usr/share/not-quite-tiny-dfr/config.toml` → `/etc/not-quite-tiny-dfr/config.toml`
→ `~/.config/not-quite-tiny-dfr/config.toml`. All are live-reloaded.

## NixOS

This flake exposes `nixosModules.default`, a drop-in replacement for
nixpkgs' `hardware.apple.touchBar` module -- same option interface
(`enable`/`package`/`settings`), but pointed at this fork's actual config
path and systemd unit name instead of the mainline module's hardcoded
`tiny-dfr` literals (which otherwise silently no-op when `package` is set to
a differently-named fork; see the module's own header comment for details).

```nix
{
  inputs.not-quite-tiny-dfr.url = "github:seojoonlee-dev/not-quite-tiny-dfr";

  outputs = { nixpkgs, not-quite-tiny-dfr, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        not-quite-tiny-dfr.nixosModules.default
        {
          hardware.apple.touchBar = {
            enable = true;
            # package defaults to this flake's own package output; override
            # only if you need a different revision/build.
            settings = {
              MediaLayerDefault = true;
              EnablePixelShift = true;
              # ... see "Configuration options" below
            };
          };
        }
      ];
    };
  };
}
```

If any `settings` command widget (or a `Slider{Get,Set}`) needs something on
`PATH` beyond a shell -- e.g. a package referenced by bare name rather than
absolute store path -- add it via `hardware.apple.touchBar.extraPath`.

## Configuration options

Every key is optional; unset keys fall back to the defaults below. Top-level
keys must come before the `[Style]` table. A parse error or invalid config
shows a red banner on the bar (with Esc still usable) instead of silently
misbehaving.

### Top-level keys

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `MediaLayerDefault` | bool | `false` | Show the media layer (instead of F-keys) by default. Ignored when `Layers` is set. |
| `ShowButtonOutlines` | bool | `true` | Draw the filled rounded rectangle behind each button. |
| `EnablePixelShift` | bool | `false` | Periodically shift the image by a pixel to reduce OLED burn-in. |
| `FontFamily` | string | `""` | Font family for text labels; `""` uses the system default sans. Rendering falls back across system fonts per glyph, so symbols/emoji outside the family still show. |
| `FontBold` | bool | `true` | Render labels in bold. |
| `AdaptiveBrightness` | bool | `true` | Follow the ambient light sensor for bar brightness. |
| `ActiveBrightness` | u32 | `128` | Fixed brightness (0–255) when `AdaptiveBrightness = false`. |
| `DoublePressSwitchLayers` | u32 | `0` | Double-press window in ms for Fn to swap the two layers persistently; `0` disables. |
| `DimTimeout` | u32 | `30` | Seconds of inactivity before the bar dims; `0` disables dimming. |
| `OffTimeout` | u32 | `60` | Seconds of inactivity before the bar turns off; `0` disables it. |
| `VisibleButtons` | int | `0` | Button slots shown at once; layers with more become scrollable. `0` = fit everything, no scrolling. |
| `ScrollLoop` | bool | `true` | Scrollable layers wrap around like a band instead of stopping at the ends. |
| `ScrollRubberBand` | bool | `true` | Overscroll stretches and springs back at the ends (when `ScrollLoop = false`). |
| `LayerSwipe` | bool | `true` | Two-finger horizontal swipe slides between layers. |
| `PinnedIgnoreScroll` | bool | `true` | Pinned buttons hold still while the rest of the layer scrolls. |
| `PinnedIgnoreLayerSwipe` | bool | `true` | Pinned buttons hold still during layer swipes. |
| `LyricOffset` | float | `0.0` | Seconds to shift synced lyrics against the audio. Positive shows each line earlier (compensates for audio output latency); negative shows it later. |
| `MediaCoverBlur` | bool | `false` | Blur the album cover behind the media panel. |
| `PrimaryLayerKeys` | array of buttons | Esc + F1–F12 | The primary layer. Ignored when `Layers` is set. |
| `MediaLayerKeys` | array of buttons | Esc + media keys | The media layer. Ignored when `Layers` is set. |
| `Layers` | array of button arrays | unset | Any number of layers; swiping cycles through them in order. When set, wins over `PrimaryLayerKeys`/`MediaLayerKeys`/`MediaLayerDefault`. |

### Button keys

Each entry in `PrimaryLayerKeys`, `MediaLayerKeys`, or `Layers` is a table with:

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `Icon` (alias `Svg`) | string | unset | Icon name, looked up in the config dirs (`~/.config`, `/etc`, `/usr/share`). |
| `Text` | string | unset | Text label. |
| `Theme` | string | unset | Icon theme to look the icon up in. |
| `Time` | string | unset | Make this a clock: a strftime format string, e.g. `"%H:%M"`. |
| `Locale` | string | unset | Locale used for the `Time` format. |
| `Battery` | string | unset | Make this a battery indicator; value is the display mode: `"icon"`, `"percentage"`, or `"both"`. |
| `Cpu` (alias `CpuTemp`) | string | unset | Make this a CPU indicator; space-separated list of what to show: `"celsius"`, `"fahrenheit"`, `"watts"` (e.g. `"celsius watts"`). |
| `CpuLabel` | bool | `true` | Show the leading `CPU` label on a `Cpu` widget. |
| `Gpu` | string | unset | Make this a GPU indicator; same component list as `Cpu` (`"celsius"`, `"fahrenheit"`, `"watts"`). Vendor (AMD/NVIDIA/Intel) is detected automatically. |
| `GpuLabel` | bool | `true` | Show the leading vendor label (e.g. `AMD`) on a `Gpu` widget. |
| `Action` | string or array | none | Linux key name (`"F1"`, `"VolumeUp"`, `"IllumUp"` = keyboard backlight, …) or internal action `"TouchBarBrightnessUp"`/`"TouchBarBrightnessDown"` (the bar's own brightness, 10 levels, hold to repeat). An array sends a chord. |
| `Pinned` | bool | `false` | Keep this leading button outside the scrolling band and still during layer swipes. Pinned slots must match across all layers, or the config is rejected. |
| `Stretch` | int | `1` | How many button slots this button spans. |
| `IconWidth` / `IconHeight` | int | `IconSize` | Per-button icon size in px. |
| `Color` | hex string | `ButtonColor` | Per-button idle fill color. |
| `ColorActive` | hex string | `ButtonColorActive` | Per-button pressed fill color. |
| `TextColor` | hex string | `TextColor` | Per-button label color. |
| `Command` | string | unset | Make this a command widget (see [Custom widgets](#custom-widgets)). Takes precedence over `Text`/`Icon`. |
| `Interval` | float | `2.0` | Seconds between `Command` (or `SliderGet`) runs (min 0.1). |
| `SliderGet` / `SliderSet` | string | unset | Make this a slider widget (see [Slider widgets](#slider-widgets)): `SliderGet` prints the value 0–100 (optionally followed by `muted`), `SliderSet` runs with `{}` replaced by the new value. Both required. |
| `SliderMute` | string | unset | Mute command for a slider: runs with `{}` replaced by `toggle` (tapping the expanded slider's icon) or `0` (a drag unmutes). |
| `SliderStretch` | int | `2` | Slots the slider expands to when tapped (collapsed it uses `Stretch`). |
| `OnClick` | string | unset | What a tap does. `"Action"` runs the button's `Action`/keys; `"Expand"` expands the button in place — reusing the slider's animation — and shows `ExpandCommand`'s output until it idles (see [Expandable widgets](#expandable-widgets)). Only `"Action"` **lights up on tap** (the pressed fill); `"Expand"` (which has its own animation) and buttons with no `OnClick` stay flat when tapped. |
| `ExpandCommand` | string | unset | For `OnClick = "Expand"`: the shell command whose stdout fills the expanded view (same JSON/plain-text protocol as `Command`). Required to expand. |
| `ExpandStretch` | int | `2` | Slots the button expands to when tapped (collapsed it uses `Stretch`). |

### `Action` values

An `Action` is one of three things:

1. **A Linux input key name.** Any variant name of the `input_linux` crate's
   [`Key` enum](https://docs.rs/input-linux/0.7.1/input_linux/enum.Key.html) —
   the kernel's `KEY_*` constants in CamelCase, ~550 in total. The daemon emits
   the key through uinput like a real keyboard: press on touch-down, release on
   lift, and holding your finger keeps the key held (autorepeat works). Common
   names:

   | Category | Examples |
   | --- | --- |
   | Function/basics | `Esc`, `F1`–`F24`, `Tab`, `Enter`, `Backspace`, `Space`, `Delete` |
   | Letters | `A`–`Z` (just the letter) |
   | Digits | `Num1`–`Num9`, `Num0` |
   | Modifiers | `LeftCtrl`, `RightCtrl`, `LeftShift`, `LeftAlt`, `LeftMeta`, … |
   | Media | `PlayPause`, `NextSong`, `PreviousSong`, `StopCd`, `Mute`, `VolumeDown`, `VolumeUp`, `MicMute` |
   | Display/backlight | `BrightnessDown`/`BrightnessUp` (screen), `IllumDown`/`IllumUp` (keyboard backlight) |
   | Navigation | `Left`, `Right`, `Up`, `Down`, `Home`, `End`, `PageUp`, `PageDown` |
   | Misc | `Search`, `Sleep`, `Camera`, `Calc`, `Www`, `Mail`, and hundreds more |

   The docs.rs page above lists every accepted name exactly as it is written in
   the config. The underlying kernel constants (with the same set of keys) are in
   [input-event-codes.h](https://github.com/torvalds/linux/blob/master/include/uapi/linux/input-event-codes.h).

2. **A daemon-internal action.** Exactly two exist: `"TouchBarBrightnessUp"`
   and `"TouchBarBrightnessDown"`. They never leave the daemon — they step the
   Touch Bar's own backlight through its 10 software dimming levels, and holding
   repeats.

3. **An array of the above** — e.g. `Action = ["LeftCtrl", "C"]`. All entries
   are pressed together in listed order on touch-down and released on lift, so
   the array acts as a chord (list modifiers first).

Omitting `Action` (or `Action = []`) makes the button inert: it draws but sends
nothing, which is the usual choice for display-only widgets like clocks. A name
that matches neither a key nor an internal action is a config error — red
banner on the bar.

### `[Style]` table

Colors are hex strings: `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `Background` | color | `#000000` | Bar background color. |
| `BackgroundImage` | string | unset | PNG background, scaled/center-cropped to the bar (`cover`). Absolute path, or relative to the config dirs. Whichever of `Background`/`BackgroundImage` is declared **later** wins. |
| `BackgroundImageBlur` | bool | `false` | Blur the background image (applied once when it's loaded). |
| `ButtonColor` | color | `#333333` | Idle button fill. Setting it explicitly draws the fill even with `ShowButtonOutlines = false`, so buttons can be tinted over a background image. |
| `ButtonColorActive` | color | `#666666` | Pressed button fill. |
| `TextColor` | color | `#ffffff` | Label and icon color. |
| `ButtonSpacing` | number | `16` | Gap between buttons in px; `0` makes a seamless strip. |
| `EdgePadding` | number | `0` | Padding between the screen edges and the first/last button, in px. |
| `CornerRadius` | number | `8` | Button corner radius in px. |
| `FontSize` | number | `32` | Label font size in px. |
| `IconSize` | number | `48` | Default icon size in px (per-button `IconWidth`/`IconHeight` override it). |
| `HeightPercent` | number | `90` | Button height as a percentage (0–100) of the bar height. |
| `BatteryChargingColor` | color | `#00b300` | Battery indicator color while charging. |
| `BatteryLowColor` | color | `#b30000` | Battery indicator color when low. |

## Custom widgets

A widget runs a shell command every `Interval` seconds and shows its output:

```toml
{ Command = "sh ~/.config/not-quite-tiny-dfr/my_widget.sh", Interval = 5, Stretch = 2 }
```

The command's stdout is read as **JSON** if it looks like JSON, otherwise as
**plain text** (first line):

```
echo "42%"                                          ->  shows: 42%
echo '{"text":"42°C","color":"#ff5555"}'            ->  shows red: 42°C
echo '{"text":"84%","icon":"battery_5_bar"}'        ->  shows the icon + 84%
```

Fields: `text` (label), `color` (hex, colors the label), and `icon` (an icon
name resolved like a button's `Icon`, drawn to the left of the label).
`Interval` defaults
to 2s and is clamped to a 0.1s minimum. Commands run asynchronously with a
timeout, so a slow or hung script never freezes the bar. Widgets redraw only
when their output changes. The command can be any executable — a shell one-liner,
a Python script, a compiled binary.

> ⚠️ **Security / permissions.** Widgets execute **arbitrary commands as your
> user**. Only use scripts you trust — a malicious config or script can do
> anything you can. The systemd unit sandboxes the daemon: your home directory
> is mounted **read-only** (scripts can read but not write it), `/tmp` is
> **private and writable** (use it for caches), and network access
> (`AF_INET`/`AF_INET6`) is **enabled** so widgets like weather scripts work. If
> you don't want network-capable widgets, remove `AF_INET AF_INET6` from
> `RestrictAddressFamilies=` in the unit.

Event-driven / streaming widgets (a long-lived process that pushes updates
instantly, rather than polling) are planned but not yet implemented.

### Bundled widget scripts

Scripts shipped with the app are installed to
`/usr/share/not-quite-tiny-dfr/widgets/`:

- **`weather.sh`** — shows the current condition and temperature (e.g.
  `Partly Cloudy 29°C`) from [wttr.in](https://wttr.in). The location is
  geolocated by IP unless passed as an argument, and flags pick the fields:
  `-e` renders the condition as an emoji instead of text (e.g. `☀️ 29°C`),
  `-H` appends humidity, `-w` appends wind:

  ```toml
  { Command = "sh /usr/share/not-quite-tiny-dfr/widgets/weather.sh -e -H Seoul", Interval = 900, Stretch = 3 }
  ```

  If the network or wttr.in is unavailable it shows `weather n/a` and retries
  on the next interval.

  Text is rendered with pango, which falls back across system fonts per
  glyph — color emoji (and `-w`'s wind arrows) render as long as an emoji
  font (e.g. `noto-fonts-emoji`) is installed system-wide.

- **`battery.sh`** — shows the battery percentage read from the **UPower**
  daemon (the same source the caelestia shell uses), next to the same stepped
  battery icon the built-in indicator draws, e.g. 🔋 `84%`. Unlike the built-in
  `Battery` indicator, which does its own `/sys/class/power_supply` math, UPower
  recalibrates the battery's learned "full" charge — so on degraded T2 Macs,
  where the kernel's `charge_full`/`capacity` read wrong and the built-in
  indicator can stick near 100%, this reports the true level. `-c` colors the
  label (green while charging, red below 10%) and `-t` drops the icon for
  text-only. The percentage **auto-calibrates to a flat 100%** like macOS: each
  time the firmware reports the pack fully charged, its raw level (e.g. ~97% on
  a degraded T2 cell that the SMC won't trickle past) is learned as the ceiling
  and the reading is rescaled `0…ceiling` → `0…100`, so it shows `100%` when
  full and glides down smoothly when unplugged instead of snapping to 97. `-f N`
  just seeds that ceiling before the first full charge of the session. An
  optional trailing argument selects a UPower device leaf (default
  `DisplayDevice`):

  ```toml
  { Command = "sh /usr/share/not-quite-tiny-dfr/widgets/battery.sh -w -f 96", Interval = 0, Stretch = 1 }
  ```

  It shows `battery n/a` if UPower isn't running or the device is absent.
  Requires `busctl` (from systemd) to reach the system bus.

- **`battery_eta.sh`** — the expand-view companion to `battery.sh`. Prints the
  estimated **time to empty** while discharging or **time to full** while
  charging, e.g. `2h 15m left` or `1h 30m to full` (and `full` at the top). It
  combines UPower's recalibrated `Energy`/`EnergyFull` with the *instantaneous*
  current×voltage from sysfs, so it reacts to load right away instead of
  trailing UPower's smoothed rate; a median-of-samples plus an EMA keep it
  steady. Use it as the `ExpandCommand` of a `battery.sh` widget with
  `OnClick = "Expand"` so the collapsed button shows the percentage and a tap
  expands it to the ETA (see [Expandable widgets](#expandable-widgets)). Pass
  the **same `-f N`** as `battery.sh` so "to full" targets the same ceiling and
  reads `full` exactly when the collapsed view shows 100% (it also picks up
  `battery.sh`'s learned ceiling automatically). Adds no dependency beyond
  UPower; a trailing argument selects a UPower device leaf.

## Expandable widgets

Any command widget can be made **tap-to-expand** by setting `OnClick =
"Expand"` and an `ExpandCommand`. Collapsed, the button shows `Command`'s
output at `Stretch` slots; a tap widens it — with the same spring animation the
volume slider uses — to `ExpandStretch` slots and shows `ExpandCommand`'s
output, then auto-collapses after a few idle seconds. The expand script uses
the same JSON/plain-text protocol as `Command`.

The expand script **polls in the background**, so a smoothed value is always
ready to show the instant you tap — no wait to compute it. Once open, that value
is **frozen** until the button collapses, so it never shifts under you; it only
takes on a fresh background reading while out of sight, ready for the next open.
The collapsed reading and the expand view **crossfade** into
each other as the button animates open and closed. For a value that would
otherwise jump around (an ETA, a rate), smooth it in the script too — the
bundled `battery_eta.sh` keeps an EMA of the power draw so the estimate drifts
instead of flickering.

The bundled battery pair is the worked example: percentage collapsed, ETA
expanded.

```toml
{ Command = "sh /usr/share/not-quite-tiny-dfr/widgets/battery.sh -t", Interval = 30,
  OnClick = "Expand",
  ExpandCommand = "sh /usr/share/not-quite-tiny-dfr/widgets/battery_eta.sh",
  ExpandStretch = 3 }
```

`OnClick = "Expand"` suppresses the button's `Action` while it's the expand
trigger. The built-in [slider](#slider-widgets) is the same mechanism with a
draggable track instead of a second script.

## Slider widgets

A slider is an interactive widget that reads and writes a 0–100 value through
shell commands — volume being the classic use:

```toml
{ Icon = "volume_up", SliderGet = "wpctl get-volume @DEFAULT_AUDIO_SINK@ | awk '{print int($2*100) ($3==\"[MUTED]\" ? \" muted\" : \"\")}'", SliderSet = "wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ {}%", SliderMute = "wpctl set-mute @DEFAULT_AUDIO_SINK@ {}", SliderStretch = 2 }
```

Collapsed, it sits at its normal `Stretch` width showing just the icon.
**Tapping it expands** it to `SliderStretch` slots — a springy ease-out that
slightly overshoots — revealing a track with a round drag handle at the
current value; **dragging (or tapping) the track moves the handle and sets
the value**, and after ~3 seconds without interaction it slides shut again.
While a finger is on the slider, band scrolling and layer swipes stay out of
its way.

`SliderGet` is polled every `Interval` seconds (its stdout is parsed as a
number, optionally followed by the word `muted`), so changes made elsewhere —
volume keys, a mixer — show up on the bar. `SliderSet` runs with `{}` replaced
by the integer value; rapid drags coalesce so only the latest value runs,
never a backlog. Anything with a get/set command pair works: screen brightness
(`brightnessctl`), keyboard backlight, media position.

With `SliderMute` set, **tapping the expanded slider's icon toggles mute**
(the fill and handle dim while muted), and **dragging the track unmutes**
before applying the new volume — moving the slider always makes sound
changes audible.

## CPU widget

The CPU indicator is built into the daemon:

```toml
{ Cpu = "celsius watts", Stretch = 2 }
```

The `Cpu` value is a **space-separated list** of what to show, any of
`celsius`, `fahrenheit`, and `watts` (alias `power`) — so `"celsius watts"`
shows both, e.g. `CPU 62°C 15W`. (`CpuTemp` is still accepted as an alias for
`Cpu`.) `CpuLabel = false` drops the leading `CPU`. A background thread polls
every 2 seconds and the button redraws only when a reading changes.

- **Temperature** (`celsius`/`fahrenheit`) reads the `x86_pkg_temp` thermal zone
  when present (Intel), else the hottest zone under `/sys/class/thermal` (e.g.
  Apple Silicon).

- **Power** (`watts`) shows CPU package draw (e.g. `15W`), derived from the
  Intel RAPL energy counter. That counter is root-only, so the daemon opens it
  while it still has privileges at startup; if RAPL isn't present it shows
  `n/a`.

Any component that can't be read shows `n/a` in its place.

## GPU widget

The GPU indicator is built into the daemon and mirrors the CPU widget:

```toml
{ Gpu = "celsius watts", Stretch = 2 }
```

The `Gpu` value is the same **space-separated list** as `Cpu` — any of
`celsius`, `fahrenheit`, and `watts` (alias `power`) — so `"celsius watts"`
shows both, e.g. `AMD 54°C 22W`. At startup the daemon **detects the GPU
vendor** and uses it as the label prefix (`AMD`, `NVIDIA`, or `Intel`);
`GpuLabel = false` drops it. A background thread polls every 2 seconds and the
button redraws only when a reading changes.

- **Temperature** (`celsius`/`fahrenheit`) and **power** (`watts`) come from the
  card's `amdgpu`/`i915` hwmon sysfs (`temp1_input`, `power1_average`) on
  AMD/Intel, or from `nvidia-smi` on NVIDIA. All are readable unprivileged, so —
  unlike CPU RAPL — nothing is opened as root.

A component the detected GPU doesn't expose (e.g. power on some integrated
chips) shows `n/a`, as does everything when no supported GPU is found.

## Battery widget

The battery indicator is built into the daemon (no script involved):

```toml
{ Battery = "both", Stretch = 2 }
```

The value picks the display mode: `"icon"`, `"percentage"`, or `"both"`. The
first device of type `Battery` under `/sys/class/power_supply` is used
automatically; if none exists the button shows `Battery N/A`. A background
thread polls sysfs once per second, and the button redraws only when the
reading changes.

> On degraded T2 batteries the kernel's `charge_full`/`capacity` values are
> unreliable, so this sysfs-based percentage can read wrong (often stuck near
> 100%). If you hit that, use the bundled **`battery.sh`** widget instead — it
> reads UPower's recalibrated percentage and shows the same stepped icon. See
> [Bundled widget scripts](#bundled-widget-scripts).

The icon steps through the bundled `battery_0_bar` … `battery_full` SVGs
(charging variants get a bolt overlay). The button fill also signals state —
`BatteryChargingColor` while charging, `BatteryLowColor` when discharging below
10% — and is drawn even with `ShowButtonOutlines = false`. Like any button it
accepts `Theme`, `Stretch`, and even an `Action`.

## Scrollable layers

Want more buttons than fit on the bar? Set

```toml
VisibleButtons = 12
```

and add as many buttons to a layer as you like. The layer shows 12 slots at a
time; **flick horizontally** to scroll through the rest, with momentum, and the
strip **wraps around like a band** — scrolling past the last button loops back
to the first (set `ScrollLoop = false` to stop at the ends instead). The
auto-added **Esc key stays pinned** on the left and never scrolls.

On a scrollable layer, a quick **tap** presses a button, **holding** your
finger still for a moment holds the key down (key repeat for volume/brightness
still works), and a horizontal **drag** scrolls. Layers that fit within
`VisibleButtons` behave exactly as before.

## Performance and T2 reliability

This fork adds scrolling, momentum, rubber-band overscroll, and layer-swipe
animations that [tiny-dfr](https://github.com/AsahiLinux/tiny-dfr) does not have.
Those recomposite the whole bar every frame at 60 Hz, so a couple of caches keep
each full-bar frame cheap:

- **Rasterized SVG icons are cached.** librsvg's `render_document` re-rasterizes
  an icon from scratch on every call; under scrolling that would re-rasterize
  every visible icon 60×/s. Each icon is instead rasterized once per size and the
  bitmap is blitted thereafter (icons never change at runtime), keeping full-bar
  draw time to a few milliseconds.

- **Album art is cached** into a cairo surface once per track, rather than being
  rebuilt from raw bytes every frame.

**Full-bar redraws (Apple T2).** On T2 Macs the `appletbdrm` display stream
desyncs and stalls — ~1 s per flush, occasionally wedging until reboot — when it
is sent a *partial single-widget* dirty rectangle of certain sizes. The size that
trips it depends on the slot width, i.e. on the layout, which is why some configs
froze and others did not. To avoid it, a widget update repaints the **whole bar**
instead of emitting a partial clip; full-bar flushes are the safe path (and the
SVG cache above is what makes doing so on every widget tick cheap).

Set `NQTD_FRAME_LOG=1` in the daemon's environment to log per-frame
`draw`/`flush`/`period` timings to the journal; `NQTD_TOUCH_LOG=1` logs touch
events.
