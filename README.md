# not-quite-tiny-dfr
Not quite the most basic dynamic function row daemon possible.

A customizable Touch Bar daemon for Apple T2 and Silicon Macs, forked from
[tiny-dfr](https://github.com/AsahiLinux/tiny-dfr). It adds full theming
(colors, spacing, corners, fonts, background images), per-user configuration in
`~/.config/not-quite-tiny-dfr/`, and programmable **command widgets**.

Config is merged from, in increasing precedence:
`/usr/share/not-quite-tiny-dfr/config.toml` → `/etc/not-quite-tiny-dfr/config.toml`
→ `~/.config/not-quite-tiny-dfr/config.toml`. All are live-reloaded.

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
| `FontFamily` | string | `""` | Font family for text labels; `""` uses the system default sans. |
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
| `CpuTemp` | string | unset | Make this a CPU temperature indicator; value is the unit: `"celsius"` or `"fahrenheit"`. |
| `Action` | string or array | none | Linux key name (`"F1"`, `"VolumeUp"`, `"IllumUp"` = keyboard backlight, …) or internal action `"TouchBarBrightnessUp"`/`"TouchBarBrightnessDown"` (the bar's own brightness, 10 levels, hold to repeat). An array sends a chord. |
| `Pinned` | bool | `false` | Keep this leading button outside the scrolling band and still during layer swipes. Pinned slots must match across all layers, or the config is rejected. |
| `Stretch` | int | `1` | How many button slots this button spans. |
| `IconWidth` / `IconHeight` | int | `IconSize` | Per-button icon size in px. |
| `Color` | hex string | `ButtonColor` | Per-button idle fill color. |
| `ColorActive` | hex string | `ButtonColorActive` | Per-button pressed fill color. |
| `TextColor` | hex string | `TextColor` | Per-button label color. |
| `Command` | string | unset | Make this a command widget (see [Custom widgets](#custom-widgets)). Takes precedence over `Text`/`Icon`. |
| `Interval` | float | `2.0` | Seconds between `Command` runs (min 0.1). |

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
| `CpuTempCoolColor` | color | `#8ec07c` | CPU temperature text color below 70 °C. |
| `CpuTempWarmColor` | color | `#fabd2f` | CPU temperature text color from 70 °C. |
| `CpuTempHotColor` | color | `#fb4934` | CPU temperature text color from 85 °C. |

## Custom widgets

A widget runs a shell command every `Interval` seconds and shows its output:

```toml
{ Command = "sh ~/.config/not-quite-tiny-dfr/weather.sh", Interval = 300, Stretch = 2 }
```

The command's stdout is read as **JSON** if it looks like JSON, otherwise as
**plain text** (first line):

```
echo "42%"                                  ->  shows: 42%
echo '{"text":"42°C","color":"#ff5555"}'    ->  shows red: 42°C
```

Fields: `text` (label) and `color` (hex, colors the label). `Interval` defaults
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

## CPU temperature widget

The CPU temperature indicator is built into the daemon:

```toml
{ CpuTemp = "celsius", Stretch = 2 }
```

The value picks the unit: `"celsius"` or `"fahrenheit"`. It reads the
`x86_pkg_temp` thermal zone when present (Intel), and falls back to the hottest
zone under `/sys/class/thermal` otherwise (e.g. Apple Silicon). A background
thread polls sysfs every 2 seconds, and the button redraws only when the
reading changes.

The label (e.g. `CPU 62°C`) is color-coded by temperature:
`CpuTempCoolColor` below 70 °C, `CpuTempWarmColor` from 70 °C, and
`CpuTempHotColor` from 85 °C (see the `[Style]` table). If no thermal zone is
readable the button shows `CPU n/a`.

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
