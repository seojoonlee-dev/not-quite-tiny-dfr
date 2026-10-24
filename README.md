<h1 align="center">not-quite-tiny-dfr</h1>

<p align="center">
  <img src="docs/touchbar.png" alt="not-quite-tiny-dfr running on a MacBook Pro Touch Bar: Esc, weather, a now-playing media panel, volume, battery, CPU and GPU widgets" width="100%">
</p>

A customizable Touch Bar daemon for Apple T2 and Apple Silicon Macs on Linux.

not-quite-tiny-dfr is a fork of [tiny-dfr](https://github.com/AsahiLinux/tiny-dfr),
the minimal dynamic function row daemon from Asahi Linux. It keeps tiny-dfr's
small footprint and adds:

- **Theming**: colors, spacing, corner radius, fonts, and background images.
- **Per-user configuration** in `~/.config/not-quite-tiny-dfr/`, live-reloaded
  on save.
- **Built-in widgets**: clock, battery, CPU, GPU, and a now-playing media panel
  with album art and synced lyrics.
- **Custom widgets** driven by any shell command, including sliders and
  tap-to-expand buttons.
- **Scrollable layers** with momentum, pinned buttons, and swipe gestures to
  switch between any number of layers.

## Contents

- [Hardware support](#hardware-support)
- [Installation](#installation)
- [Uninstallation](#uninstallation)
- [Configuration](#configuration)
- [Built-in widgets](#built-in-widgets)
- [Custom widgets](#custom-widgets)
- [Scrollable layers](#scrollable-layers)
- [Performance and T2 reliability](#performance-and-t2-reliability)
- [License](#license)

## Hardware support

not-quite-tiny-dfr talks to the Touch Bar through the same kernel drivers and
udev setup as tiny-dfr, so it should run on any Mac where tiny-dfr works. That
covers the T2 MacBook Pros from 2018 to 2020 running a
[t2linux](https://t2linux.org) kernel, and the 13-inch M1 and M2 MacBook Pros
running [Asahi Linux](https://asahilinux.org).

These models have been tested directly:

| Model | Identifier |
| --- | --- |
| MacBook Pro (15-inch, 2019) | `MacBookPro15,1` |
| MacBook Pro (16-inch, 2019) | `MacBookPro16,1` |

Development happens on the 15-inch model running Arch Linux, so other
distributions may need small adjustments. If you run it on a model that isn't
listed, please open an issue or email developer.seojoonlee@gmail.com with your
model identifier, whether it worked or not. On a T2 Mac,
`cat /sys/class/dmi/id/product_name` prints the identifier; on Apple Silicon,
use `cat /proc/device-tree/model`. Bug reports and pull requests are welcome.

## Installation

### Requirements

- A Mac with a Touch Bar, running a kernel with the Touch Bar drivers
  (`appletbdrm` and `hid-appletb-*` on T2, `adp` on Apple Silicon).
- Rust (stable) and `pkg-config`.
- Development files for cairo, pango, librsvg, libinput, fontconfig, and
  freetype.

Optional runtime tools:

| Tool | Used by |
| --- | --- |
| `playerctl` | Media widget |
| `nvidia-smi` | GPU widget on NVIDIA cards |
| `curl` | Bundled `weather.sh` widget |
| `busctl` (systemd) and UPower | Bundled `battery.sh` and `battery_eta.sh` widgets |

### Building and installing

Make sure the [requirements](#requirements) are installed, then follow these
steps as your normal user. The install script asks for sudo when it needs it.

**1. Clone the repository.**

```sh
git clone https://github.com/seojoonlee-dev/not-quite-tiny-dfr.git
cd not-quite-tiny-dfr
```

**2. Run the install script.**

```sh
./install.sh
```

The script checks the build dependencies, builds a release binary, and
installs:

| What | Where |
| --- | --- |
| The daemon | `/usr/bin/not-quite-tiny-dfr` |
| Icons, widget scripts, and the base config | `/usr/share/not-quite-tiny-dfr/` |
| The systemd unit | `/etc/systemd/system/not-quite-tiny-dfr.service` |
| The udev rules | `/etc/udev/rules.d/` |

If stock tiny-dfr is installed, the script offers to stop and mask it, since
both daemons drive the same display. Masking keeps tiny-dfr's own udev rule
from starting it, and `./uninstall.sh` undoes it. It then enables the service, and starts it
right away if the Touch Bar is already available.

**3. Reboot.**

On a first install, reboot so the udev rules can bring up the Touch Bar. The
daemon starts on its own from then on.

**4. Customize.**

Copy the base config to your config directory and edit it. Changes apply as
soon as you save.

```sh
mkdir -p ~/.config/not-quite-tiny-dfr
cp /usr/share/not-quite-tiny-dfr/config.toml ~/.config/not-quite-tiny-dfr/
```

See [Configuration](#configuration) for every option.

#### Updating

Pull the latest changes and run the script again:

```sh
cd not-quite-tiny-dfr
git pull
./install.sh
```

`./install.sh --no-build` installs an existing build without rebuilding. Your
config files are never modified.

<details>
<summary>Installing manually</summary>

```sh
git clone https://github.com/seojoonlee-dev/not-quite-tiny-dfr.git
cd not-quite-tiny-dfr
cargo build --release
sudo install -Dm755 target/release/not-quite-tiny-dfr /usr/bin/not-quite-tiny-dfr
sudo install -d /usr/share/not-quite-tiny-dfr
sudo cp -rT share/not-quite-tiny-dfr /usr/share/not-quite-tiny-dfr
sudo install -Dm644 etc/systemd/system/not-quite-tiny-dfr.service /etc/systemd/system/not-quite-tiny-dfr.service
sudo install -Dm644 -t /etc/udev/rules.d etc/udev/rules.d/*.rules
sudo systemctl daemon-reload
sudo systemctl enable not-quite-tiny-dfr.service
```

Disable stock tiny-dfr first if it's installed, then reboot.
`/usr/share/not-quite-tiny-dfr/config.toml` must be present for the daemon to
start.

</details>

### NixOS

The flake exposes `nixosModules.default`, a drop-in replacement for nixpkgs'
`hardware.apple.touchBar` module. It has the same options (`enable`, `package`,
`settings`), but writes the config path and systemd unit this fork actually
uses. The upstream module hard-codes `tiny-dfr` for both, so pointing its
`package` option at this fork silently does nothing. See the module's header
comment for details.

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
            # `package` defaults to this flake's own package.
            settings = {
              MediaLayerDefault = true;
              EnablePixelShift = true;
              # See "Configuration" for every available key.
            };
          };
        }
      ];
    };
  };
}
```

If a command widget or slider needs a program on `PATH` beyond a shell (for
example, a package referenced by name rather than by store path), add it with
`hardware.apple.touchBar.extraPath`.

## Uninstallation

From your clone of the repository, run:

```sh
cd not-quite-tiny-dfr
./uninstall.sh
```

The script stops and disables the service and removes everything
`install.sh` installed. It then hands the Touch Bar back to stock tiny-dfr:

- tiny-dfr is unmasked, and its own udev rules apply again.
- If tiny-dfr is installed, it is enabled and started, so the Touch Bar comes
  back without a reboot. Otherwise the script tells you how to install it; the
  Touch Bar stays blank until a daemon is running.

Your config files in `~/.config/not-quite-tiny-dfr/` and
`/etc/not-quite-tiny-dfr/`, and the cache in `/var/cache/not-quite-tiny-dfr/`,
are kept in case you reinstall. Delete them by hand to remove every trace.

On NixOS, remove the module from your configuration and rebuild instead.

## Configuration

Configuration is merged from three files, each overriding the one before:

1. `/usr/share/not-quite-tiny-dfr/config.toml` (shipped defaults)
2. `/etc/not-quite-tiny-dfr/config.toml` (system-wide)
3. `~/.config/not-quite-tiny-dfr/config.toml` (per-user)

All three are watched, and changes apply to the Touch Bar as soon as a file is
saved. Every key is optional; unset keys use the defaults listed below. In
TOML, top-level keys must come before the `[Style]` table.

An invalid configuration does not fail silently: the bar shows a red error
banner instead, and the Esc key keeps working.

### Top-level keys

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `MediaLayerDefault` | bool | `false` | Show the media layer instead of the F-keys by default. Ignored when `Layers` is set. |
| `ShowButtonOutlines` | bool | `true` | Draw the filled rounded rectangle behind each button. |
| `EnablePixelShift` | bool | `false` | Periodically shift the image by a pixel to reduce OLED burn-in. |
| `FontFamily` | string | `""` | Font family for labels. `""` uses the system default sans-serif. Glyphs missing from the family (symbols, emoji) fall back to other installed fonts. |
| `FontBold` | bool | `true` | Render labels bold (weight 700) instead of regular (400). Ignored when `FontWeight` is set. |
| `FontWeight` | int | unset | Numeric weight from 1 to 1000 (400 regular, 500 medium, 600 semibold, 700 bold). Selects the matching static face, or the `wght` axis of a variable font such as SF Pro. Overrides `FontBold`. |
| `FontVariations` | string | `""` | OpenType variation axes for variable fonts, e.g. `"opsz=18"` or `"opsz=18,GRAD=-20"`. Pango sets `opsz` to the point size by default; set it explicitly to match toolkits such as Qt that leave it at the font's default. |
| `AdaptiveBrightness` | bool | `true` | Follow the ambient light sensor for bar brightness. |
| `ActiveBrightness` | int | `128` | Fixed brightness (0–255) when `AdaptiveBrightness = false`. |
| `DoublePressSwitchLayers` | int | `0` | Double-press window for Fn, in ms, that swaps the two layers persistently. `0` disables it. |
| `DimTimeout` | int | `30` | Seconds of inactivity before the bar dims. `0` disables dimming. |
| `OffTimeout` | int | `60` | Seconds of inactivity before the bar turns off. `0` disables it. |
| `VisibleButtons` | int | `0` | Button slots shown at once; layers with more buttons become scrollable. `0` fits every button with no scrolling. |
| `ScrollLoop` | bool | `true` | Scrollable layers wrap around instead of stopping at the ends. |
| `ScrollRubberBand` | bool | `true` | Overscrolling past an end stretches and springs back (when `ScrollLoop = false`). |
| `LayerSwipe` | bool | `true` | A two-finger horizontal swipe slides between layers. |
| `PinnedIgnoreScroll` | bool | `true` | Pinned buttons stay still while the rest of the layer scrolls. |
| `PinnedIgnoreLayerSwipe` | bool | `true` | Pinned buttons stay still during layer swipes. |
| `LyricOffset` | float | `0.0` | Seconds to shift synced lyrics relative to the audio. Positive values show each line earlier (to compensate for output latency); negative values show it later. |
| `MediaCoverBlur` | bool | `false` | Blur the album cover behind the media panel. |
| `MediaArtCache` | bool | `true` | Cache downloaded album covers in the service's cache directory. Disabling stops reads and writes but keeps existing entries. |
| `MediaLyricsCache` | bool | `true` | Cache downloaded lyrics on disk. Behaves like `MediaArtCache` when disabled. |
| `PrimaryLayerKeys` | array of buttons | Esc + F1–F12 | The primary layer. Ignored when `Layers` is set. |
| `MediaLayerKeys` | array of buttons | Esc + media keys | The media layer. Ignored when `Layers` is set. |
| `Layers` | array of button arrays | unset | Any number of layers; swiping cycles through them in order. Overrides `PrimaryLayerKeys`, `MediaLayerKeys`, and `MediaLayerDefault`. |

### Button keys

Each entry in `PrimaryLayerKeys`, `MediaLayerKeys`, or `Layers` is a table with
the following keys.

**Content.** A button shows the first of these that is set:
`SliderGet`/`SliderSet`, `Media`, `Command`, `Text`, `Icon`, `Time`, `Battery`,
`Cpu`, or `Gpu`. A button with none of them is an empty spacer.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `Icon` (alias `Svg`) | string | unset | Icon name. Looked up as `<name>.svg` or `<name>.png` in `~/.config/not-quite-tiny-dfr/`, then `/etc/not-quite-tiny-dfr/`, then `/usr/share/not-quite-tiny-dfr/`. |
| `Theme` | string | unset | Look `Icon` up in this freedesktop icon theme instead of the config directories. |
| `Text` | string | unset | Text label. |
| `Time` | string | unset | Make this a clock, using a strftime format string, e.g. `"%H:%M"`. |
| `Locale` | string | `POSIX` | Locale for the `Time` format, e.g. `"ko_KR"`. |
| `Battery` | string | unset | Make this a [battery indicator](#battery). Value is the display mode: `"icon"`, `"percentage"`, or `"both"`. |
| `Cpu` (alias `CpuTemp`) | string | unset | Make this a [CPU indicator](#cpu). Value is a space-separated list of `"celsius"`, `"fahrenheit"`, and `"watts"`. |
| `CpuLabel` | bool | `true` | Show the leading `CPU` label on a `Cpu` widget. |
| `Gpu` | string | unset | Make this a [GPU indicator](#gpu). Takes the same list as `Cpu`. |
| `GpuLabel` | bool | `true` | Show the leading vendor label (e.g. `AMD`) on a `Gpu` widget. |
| `IgpuText` | string | `"iGPU"` | What a `Gpu` widget shows while the discrete GPU isn't in use. |
| `Media` | bool | unset | Make this a [media widget](#media). |
| `Command` | string | unset | Make this a [command widget](#custom-widgets). |
| `Interval` | float | `2.0` | Seconds between `Command` (or `SliderGet`) runs. Minimum `0.1`. |
| `SliderGet` / `SliderSet` | string | unset | Make this a [slider](#sliders). `SliderGet` prints a value from 0 to 100 (optionally followed by `muted`); `SliderSet` runs with `{}` replaced by the new value. Both are required. |
| `SliderMute` | string | unset | Mute command for a slider. Runs with `{}` replaced by `toggle` when the expanded slider's icon is tapped, or by `0` when a drag unmutes. |
| `SliderMuteIcon` | string | unset | Icon shown instead of `Icon` while the slider is muted. |
| `SliderLowIcon` | string | unset | Icon shown instead of `Icon` while the value is below 50. |
| `SliderStretch` | int | `2` | Slots the slider expands to when tapped. |

**Behavior and appearance.**

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `Action` | string or array | none | Key or internal action sent on press. See [`Action` values](#action-values). |
| `OnClick` | string | unset | What a tap does. See [`OnClick` values](#onclick-values). |
| `ExpandCommand` | string | unset | For `OnClick = "Expand"`: the command whose output fills the expanded view. Uses the same output format as `Command`. Required to expand. |
| `ExpandStretch` | int | `2` | Slots the button expands to when tapped. |
| `Pinned` | bool | `false` | Keep this button fixed at the left while the layer scrolls or swipes. Only leading buttons can be pinned, and every layer must pin the same number of slots or the config is rejected. |
| `Stretch` | int | `1` | Number of slots the button spans. |
| `IconWidth` / `IconHeight` | int | `IconSize` | Icon size in px. |
| `Color` | color | `ButtonColor` | Idle fill color. |
| `ColorActive` | color | `ButtonColorActive` | Pressed fill color. |
| `TextColor` | color | `TextColor` | Label and icon color. |

### `Action` values

An `Action` is one of the following:

1. **A Linux input key name.** Any variant of the `input_linux` crate's
   [`Key` enum](https://docs.rs/input-linux/0.7.1/input_linux/enum.Key.html),
   which mirrors the kernel's `KEY_*` constants in CamelCase. The key is sent
   through uinput like a real keyboard: pressed on touch-down and released on
   lift, so holding a button holds the key and autorepeat works.

   | Category | Examples |
   | --- | --- |
   | Function and editing | `Esc`, `F1`–`F24`, `Tab`, `Enter`, `Backspace`, `Space`, `Delete` |
   | Letters | `A`–`Z` |
   | Digits | `Num1`–`Num9`, `Num0` |
   | Modifiers | `LeftCtrl`, `RightCtrl`, `LeftShift`, `LeftAlt`, `LeftMeta`, … |
   | Media | `PlayPause`, `NextSong`, `PreviousSong`, `StopCd`, `Mute`, `VolumeDown`, `VolumeUp`, `MicMute` |
   | Brightness | `BrightnessDown`/`BrightnessUp` (screen), `IllumDown`/`IllumUp` (keyboard backlight) |
   | Navigation | `Left`, `Right`, `Up`, `Down`, `Home`, `End`, `PageUp`, `PageDown` |
   | Other | `Search`, `Sleep`, `Camera`, `Calc`, `Www`, `Mail`, and many more |

   The docs.rs page lists every accepted name exactly as written in the config.
   The corresponding kernel constants are in
   [input-event-codes.h](https://github.com/torvalds/linux/blob/master/include/uapi/linux/input-event-codes.h).

2. **An internal action**: `"TouchBarBrightnessUp"` or
   `"TouchBarBrightnessDown"`. These step the Touch Bar's own brightness through
   10 levels and repeat while held. They are never sent to the system.

3. **An array of the above**, e.g. `Action = ["LeftCtrl", "C"]`. All entries
   are pressed in order on touch-down and released on lift, so the array acts
   as a chord. List modifiers first.

Omitting `Action` (or setting `Action = []`) makes the button display-only,
which is the usual choice for clocks and indicators. A name that matches
neither a key nor an internal action is a config error.

### `OnClick` values

| Value | Effect |
| --- | --- |
| unset | The button sends its `Action` and does not highlight when tapped. |
| `"Action"` | The button sends its `Action` and shows the pressed fill while touched. |
| `"Expand"` | The button expands in place to show `ExpandCommand`'s output. See [Expandable widgets](#expandable-widgets). |
| any other string | A shell command, run when the button is pressed. On a `Media` widget it runs when the panel is tapped outside the transport controls. |

Commands that need your login session, such as suspending or launching an app,
should go through `systemd-run --user`, because the daemon runs outside it:

```toml
{ Icon = "sleep", OnClick = "systemd-run --user --collect systemctl suspend" }
{ Media = true, Stretch = 4, OnClick = "systemd-run --user gtk-launch tidal" }
```

### `[Style]` table

Colors are hex strings: `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `Background` | color | `#000000` | Bar background color. |
| `BackgroundImage` | string | unset | Background image (PNG, JPEG, or WebP, detected from the file contents), scaled and center-cropped to fill the bar. Absolute path, or relative to the config directories. If both `Background` and `BackgroundImage` are set, the one declared later wins. |
| `BackgroundImageBlur` | bool | `false` | Blur the background image. |
| `ButtonColor` | color | `#333333` | Idle button fill. Setting it explicitly draws the fill even with `ShowButtonOutlines = false`, so buttons can be tinted over a background image. |
| `ButtonColorActive` | color | `#666666` | Pressed button fill. |
| `TextColor` | color | `#ffffff` | Label and icon color. |
| `ButtonSpacing` | number | `16` | Gap between buttons in px. `0` makes a seamless strip. |
| `EdgePadding` | number | `0` | Space between the screen edges and the outermost buttons, in px. |
| `CornerRadius` | number | `8` | Button corner radius in px. |
| `RoundDisplayCorners` | bool | `false` | Round the corners of the whole display at `CornerRadius`, blacking out everything outside. |
| `FontSize` | number | `32` | Label font size in px. |
| `LetterSpacing` | number | `0` | Extra space between glyphs in px; negative values tighten. Pango ignores a font's built-in tracking, so use this to match, for example, SF Pro's tighter display spacing. |
| `IconSize` | number | `48` | Default icon size in px. |
| `HeightPercent` | number | `90` | Button height as a percentage of the bar height. |
| `BatteryChargingColor` | color | `#00b300` | Battery indicator fill while charging. |
| `BatteryLowColor` | color | `#b30000` | Battery indicator fill while discharging below 10%. |

Icons are drawn as single-color shapes in the button's text color, whatever
color the image file uses. Text uses grayscale antialiasing, slight hinting,
and fractional glyph positioning, the same combination Qt uses with
fontconfig's default `hintslight`.

## Built-in widgets

These indicators are part of the daemon and need no scripts. Each polls its
data on a background thread and redraws only when the reading changes.

### Battery

```toml
{ Battery = "both", Stretch = 2 }
```

The value selects the display mode: `"icon"`, `"percentage"`, or `"both"`. The
first `Battery` device under `/sys/class/power_supply` is used; if there is
none, the button shows `Battery N/A`. The reading is refreshed once per second,
but only while a layer contains a `Battery` button.

The icon steps through the bundled `battery_0_bar` … `battery_full` images,
with charging variants. The button fill also reflects the state:
`BatteryChargingColor` while charging and `BatteryLowColor` while discharging
below 10%, drawn even when `ShowButtonOutlines = false`.

> [!TIP]
> On degraded T2 batteries the kernel's `charge_full` and `capacity` values are
> unreliable, and this indicator can stick near 100%. The bundled
> [`battery.sh`](#bundled-widget-scripts) widget reads UPower's recalibrated
> level instead and shows the same icons.

### CPU

```toml
{ Cpu = "celsius watts", Stretch = 2 }
```

The value is a space-separated list of `celsius`, `fahrenheit`, and `watts`
(alias `power`), so `"celsius watts"` shows e.g. `CPU 62°C 15W`.
`CpuLabel = false` removes the `CPU` prefix. Readings refresh every 2 seconds.

- **Temperature** comes from the `x86_pkg_temp` thermal zone on Intel, or the
  hottest zone under `/sys/class/thermal` otherwise (e.g. Apple Silicon).
- **Power** is the CPU package draw from the Intel RAPL energy counter. The
  counter is readable only by root, so the daemon opens it at startup before
  dropping privileges.

Any value that can't be read shows `n/a`.

### GPU

```toml
{ Gpu = "celsius watts", Stretch = 2 }
```

The value takes the same list as `Cpu`, e.g. `AMD 54°C 22W`. The vendor is
detected automatically and used as the prefix (`AMD`, `NVIDIA`, or `Intel`);
`GpuLabel = false` removes it. Readings refresh every 2 seconds.

Temperature and power come from the card's hwmon sysfs files (`temp1_input`,
`power1_average`) on AMD and Intel, or from `nvidia-smi` on NVIDIA. None of
these require root.

On hybrid laptops the discrete GPU sleeps (PCI runtime suspend) while the
integrated GPU does the work. While it sleeps, or when no readable GPU is
found, the widget shows `iGPU`; set `IgpuText` to change this. The daemon
checks the card's `power/runtime_status` before reading, so it never wakes the
card, and spaces its reads beyond the card's autosuspend delay so polling
can't keep it awake. Detection retries until a card appears, for drivers that
load after the daemon starts.

A value the card doesn't expose (e.g. power on some models) shows `n/a`.

### Media

```toml
{ Media = true, Stretch = 4 }
```

Shows previous, play/pause, and next controls for the active MPRIS player, via
`playerctl`. While something is playing, the controls join a now-playing panel
with the album cover, track title, and artist. When synced lyrics are
available (from lrclib, with NetEase as a fallback), the current line replaces
the controls, which return during instrumental gaps.

Related settings: `LyricOffset`, `MediaCoverBlur`, `MediaArtCache`, and
`MediaLyricsCache` in [Top-level keys](#top-level-keys), and `OnClick` to run a
command when the panel is tapped.

## Custom widgets

A command widget runs a shell command every `Interval` seconds and displays its
output:

```toml
{ Command = "sh ~/.config/not-quite-tiny-dfr/my_widget.sh", Interval = 5, Stretch = 2 }
```

Standard output is parsed as **JSON** when it looks like JSON, and otherwise
the first line is shown as **plain text**:

```sh
echo "42%"                                     # shows 42%
echo '{"text":"42°C","color":"#ff5555"}'       # shows 42°C in red
echo '{"text":"84%","icon":"battery_5_bar"}'   # shows the icon, then 84%
```

JSON fields:

| Field | Description |
| --- | --- |
| `text` | The label. |
| `color` | Label and icon color (hex), overriding `TextColor`. |
| `icon` | Icon name, resolved like a button's `Icon` and drawn left of the label. |

The command can be any executable: a shell one-liner, a Python script, or a
compiled program. Commands run asynchronously, so a slow script never freezes
the bar; one that runs longer than 30 seconds is stopped. The widget redraws
only when the output changes.

If a command exits non-zero or times out, it is retried sooner than its
`Interval`: after 15 seconds, then doubling with each consecutive failure up to
the interval. Scripts that fetch external data should exit non-zero on failure,
so that, for example, a fetch that failed because the network wasn't up yet at
boot recovers within seconds.

A script can also wait for an event before printing, for instant updates
without frequent polling. Pair it with `Interval = 0` (clamped to the 0.1 s
minimum) so it is restarted as soon as it exits. The bundled `battery.sh -w` is
an example. Long-running streaming widgets, where one process prints a stream of
updates, are not yet supported.

> [!WARNING]
> Widgets execute **arbitrary commands as your user**. Only use scripts you
> trust. The systemd unit sandboxes the daemon:
>
> - Your home directory is mounted **read-only**.
> - `/tmp` is **private** and cleared on restart.
> - `$CACHE_DIRECTORY` (`/var/cache/not-quite-tiny-dfr`, owned by the logged-in
>   user) is **writable and persistent**; use it for caches.
> - Network access (`AF_INET`/`AF_INET6`) is **enabled** so widgets such as
>   weather can fetch data. To disable it, remove `AF_INET AF_INET6` from
>   `RestrictAddressFamilies=` in the unit.

### Sliders

A slider reads and writes a value from 0 to 100 through shell commands. Volume
is the typical use:

```toml
{ Icon = "volume_up", SliderGet = "wpctl get-volume @DEFAULT_AUDIO_SINK@ | awk '{print int($2*100) ($3==\"[MUTED]\" ? \" muted\" : \"\")}'", SliderSet = "wpctl set-volume -l 1.0 @DEFAULT_AUDIO_SINK@ {}%", SliderMute = "wpctl set-mute @DEFAULT_AUDIO_SINK@ {}", SliderStretch = 2 }
```

Collapsed, the slider shows only its icon at its `Stretch` width. Tapping it
expands it to `SliderStretch` slots, revealing a track with a handle at the
current value. Dragging or tapping the track sets the value, and the slider
collapses after 3 seconds without interaction. While a finger is on the
slider, scrolling and layer swipes are suspended.

`SliderGet` runs every `Interval` seconds, so changes made elsewhere (volume
keys, a mixer) appear on the bar. `SliderSet` runs with `{}` replaced by the
integer value. During a fast drag only the latest value is applied, so commands
never queue up. Any get/set command pair works, for example screen brightness
with `brightnessctl`.

With `SliderMute` set, tapping the expanded slider's icon toggles mute (the
track dims while muted), and dragging the track unmutes before applying the new
value.

### Expandable widgets

Any command widget becomes **tap-to-expand** with `OnClick = "Expand"` and an
`ExpandCommand`. Collapsed, it shows `Command`'s output at `Stretch` slots. A
tap widens it to `ExpandStretch` slots, with the same animation as the slider,
and shows `ExpandCommand`'s output. It collapses again after 3 idle seconds,
and the two views crossfade during the animation.

`ExpandCommand` runs every 5 seconds in the background, so a value is ready the
moment you tap. While the button is open the value is held fixed so it doesn't
change under you; the next background reading appears on the following open.
For values that fluctuate, such as an ETA or a rate, smooth them in the script.

The bundled battery scripts work as a pair, showing the percentage collapsed
and the time remaining expanded:

```toml
{ Command = "sh /usr/share/not-quite-tiny-dfr/widgets/battery.sh -t", Interval = 30,
  OnClick = "Expand",
  ExpandCommand = "sh /usr/share/not-quite-tiny-dfr/widgets/battery_eta.sh",
  ExpandStretch = 3 }
```

### Bundled widget scripts

These scripts are installed to `/usr/share/not-quite-tiny-dfr/widgets/`.

#### `weather.sh`

Shows the current condition and temperature from [wttr.in](https://wttr.in),
e.g. `Partly Cloudy 29°C`. The location is detected from your IP address
unless given as an argument.

| Flag | Effect |
| --- | --- |
| `-e` | Show the condition as an emoji, e.g. `☀️ 29°C`. |
| `-H` | Append humidity. |
| `-w` | Append wind. |

```toml
{ Command = "sh /usr/share/not-quite-tiny-dfr/widgets/weather.sh -e -H Seoul", Interval = 900, Stretch = 3 }
```

When wttr.in can't be reached, the script shows the last cached reading (or
`weather n/a` if there is none) and exits non-zero so the daemon retries soon.
Emoji and wind arrows require an emoji font such as `noto-fonts-emoji`.

#### `battery.sh`

Shows the battery level reported by **UPower** with the same stepped icon as
the built-in indicator, e.g. `84%`. UPower recalibrates the battery's learned
full charge, so this stays accurate on degraded T2 batteries where the built-in
indicator can stick near 100%.

The percentage is rescaled so a full charge reads 100%, like macOS. Each time
the firmware reports the battery full, the raw level at that point (e.g. 97% on
a worn cell) is recorded as the ceiling and readings are scaled from
`0–ceiling` to `0–100`.

| Flag | Effect |
| --- | --- |
| `-c` | Color the label green while charging and red below 10%. |
| `-t` | Text only, without the icon. |
| `-w` | Wait for UPower to report a change before printing, so plugging in or unplugging shows up immediately. Use with `Interval = 0`. |
| `-f N` | Starting ceiling, in percent, until the first full charge is observed. Default `100`. |

An optional final argument selects a UPower device (default `DisplayDevice`).

```toml
{ Command = "sh /usr/share/not-quite-tiny-dfr/widgets/battery.sh -w -f 96", Interval = 0, Stretch = 1 }
```

Shows `battery n/a` if UPower isn't running or the device doesn't exist.

#### `battery_eta.sh`

The expanded-view companion to `battery.sh`. Shows the estimated time to empty
while discharging or time to full while charging, e.g. `2h 15m left` or
`1h 30m to full`, and `full` when charged. It combines UPower's energy values
with the instantaneous power draw from sysfs, smoothed over several samples, so
it responds to load changes without flickering.

Pass the same `-f N` as `battery.sh` so `full` appears exactly when the
collapsed view shows 100%; it also reads the ceiling `battery.sh` has learned.
An optional final argument selects a UPower device.

## Scrollable layers

To fit more buttons than the bar can show, set:

```toml
VisibleButtons = 12
```

and add as many buttons to a layer as you like. The layer shows 12 slots at a
time, and a horizontal flick scrolls through the rest with momentum. By default
the strip wraps around, so scrolling past the last button returns to the first;
set `ScrollLoop = false` to stop at the ends instead.

The default layers start with a pinned Esc key, which stays in place while the
rest scrolls. In your own layers, set `Pinned = true` on any leading buttons
that should do the same.

On a scrollable layer, a quick tap presses a button, holding your finger still
holds the key (so key repeat for volume and brightness still works), and a
horizontal drag scrolls. Layers that fit within `VisibleButtons` behave as
usual.

## Performance and T2 reliability

Scrolling, momentum, rubber-banding, and layer swipes redraw the whole bar at
60 Hz. Two caches keep each frame cheap:

- **Icons** are rasterized once per size and reused, instead of being re-rendered
  by librsvg every frame. This keeps a full-bar frame to a few milliseconds.
- **Album art** is converted to a cairo surface once per track.

**Full-bar redraws on T2.** On T2 Macs the `appletbdrm` display stream can
desync and stall (about one second per update, occasionally until reboot)
when sent a partial update rectangle of certain sizes. The size that triggers
it depends on the button width, which is why only some layouts were affected.
To avoid this, every widget update redraws the whole bar, which the icon cache
makes inexpensive.

### Debugging

Set these environment variables on the service to log to the journal:

| Variable | Logs |
| --- | --- |
| `NQTD_FRAME_LOG=1` | Per-frame draw, flush, and period timings. |
| `NQTD_TOUCH_LOG=1` | Touch events. |

## License

MIT, as inherited from tiny-dfr (see [`LICENSE`](LICENSE)). The bundled
Material Symbols icons are licensed under Apache 2.0 (see
[`LICENSE.material`](LICENSE.material)).
