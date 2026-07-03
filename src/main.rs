use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface, Surface};
use chrono::{
    format::{Item as ChronoItem, StrftimeItems},
    Local, Locale, Timelike,
};
use drm::control::ClipRect;
use freedesktop_icons::lookup;
use input::{
    event::{
        device::DeviceEvent,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot, TouchEventTrait},
        Event, EventTrait,
    },
    Device as InputDevice, Libinput, LibinputInterface,
};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use libc::{c_char, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        time::TimeSpec,
        timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags},
    },
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod config;
mod display;
mod pixel_shift;
mod style;
mod user;
mod widget;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonAction, ButtonConfig, Config};
use display::DrmBackend;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_HEIGHT_PX, PIXEL_SHIFT_WIDTH_PX};
use style::Color;
use widget::{WidgetRuntime, WidgetSpec};

const DEFAULT_ICON_SIZE: i32 = 48;
/// Gap in px between the battery icon and its percentage text ("both" mode).
const BATTERY_ICON_TEXT_GAP: f64 = 8.0;

/// The user's `~/.config/not-quite-tiny-dfr` directory, if a target user was resolved.
/// Icons named in the config are looked up here first. Set once — either at
/// startup if a user is already logged in, or later (from the main loop) the
/// moment one logs in, when the daemon came up before anyone was logged in.
static USER_ICON_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
const TIMEOUT_MS: i32 = 10 * 1000;

/// While no user is logged in yet, how often to re-check logind for a login (and
/// how tightly to cap the event loop's idle wait) so a login is picked up
/// promptly rather than after the full idle timeout.
const USER_POLL_INTERVAL: Duration = Duration::from_secs(1);

// Gestures on scrollable layers (see `VisibleButtons` in the config).
/// Finger travel in px before a touch on the band becomes a scroll drag.
const SCROLL_SLOP_PX: f64 = 20.0;
/// How long a stationary touch sits before its button activates as a real key
/// hold (so key repeat still works); anything shorter is a tap on release.
const HOLD_ACTIVATE_MS: u64 = 150;
/// Minimum release velocity (px/s) for a drag to keep coasting as a fling.
const FLING_MIN_VELOCITY: f64 = 300.0;
/// Hard cap on fling velocity, so one glitchy touch event (a jump, or a
/// mis-batched delta) can't launch the band at warp speed.
const FLING_MAX_VELOCITY: f64 = 8000.0;
/// A finger that rested this long (µs) before lifting is placing the band,
/// not flicking it: release without momentum.
const FLING_STALE_US: u64 = 80_000;
/// How often the battery poller thread re-reads sysfs.
const BATTERY_POLL: Duration = Duration::from_secs(1);
/// How often the CPU temperature poller thread re-reads sysfs.
const CPU_TEMP_POLL: Duration = Duration::from_secs(2);
/// CpuTemp widget color thresholds, in °C.
const CPU_TEMP_WARM_C: i32 = 70;
const CPU_TEMP_HOT_C: i32 = 85;
/// A fling decelerating below this (px/s) stops.
const FLING_STOP_VELOCITY: f64 = 40.0;
/// Exponential-decay time constant of fling friction, in seconds.
const FLING_FRICTION_TAU: f64 = 0.3;
/// The Touch Bar panel refreshes at 60 Hz. All drawing is paced to this
/// budget, VRR-style: frames render at the full panel rate while something
/// is moving and not at all while nothing changes. Drawing faster than the
/// panel (e.g. chasing the ~110 Hz digitizer during a drag) is wasted work
/// and presents unevenly.
const FRAME_PERIOD: Duration = Duration::from_micros(16_667);
/// How early a frame may render ahead of its deadline. Covers timer wake-up
/// latency, so a wake landing just short of the boundary draws now instead of
/// slipping a whole extra millisecond.
const FRAME_SLACK: Duration = Duration::from_micros(500);
/// A flush this slow is not congestion, it is appletbdrm waiting out (part
/// of) its 1 s response timeout: the T2's display stream is desyncing. A
/// healthy frame is single-digit ms of draw and tens of ms of flush.
const FLUSH_STALL_MIN: Duration = Duration::from_millis(200);
/// Cool-down after a stalled flush, doubling per consecutive stall (capped
/// via FLUSH_STALL_MAX_DOUBLINGS). Feeding more frames into a desyncing
/// stream is what escalates a glitchy panel into a permanently wedged one,
/// so the daemon goes quiet and only probes occasionally.
const FLUSH_COOLDOWN_BASE: Duration = Duration::from_secs(2);
/// Cap on the cool-down doubling (2 s * 2^4 = 32 s between probes at worst).
const FLUSH_STALL_MAX_DOUBLINGS: u32 = 4;
/// Time constant of the post-scroll snap glide (to the nearest slot boundary).
const SNAP_TAU: f64 = 0.08;
/// The snap glide is finished once within this many px of its target.
const SNAP_EPSILON: f64 = 0.5;
/// Rubber-band overscroll (non-looping bands only): hard cap in px on how far
/// past an end the band can be pulled. Drag resistance grows asymptotically
/// toward it, so the cap is approached but never reached.
const RUBBER_BAND_RANGE: f64 = 160.0;
/// Time constant of the critically damped spring that returns a fling
/// overshooting past an end: one continuous out-and-back bounce, no
/// friction phase to wait out.
const RUBBER_SPRING_TAU: f64 = 0.08;
/// Cap on the momentum handed to that spring when a fling crosses an end,
/// keeping the bounce peak (~130 px) under the drag stretch cap.
const RUBBER_MAX_BOUNCE_VELOCITY: f64 = 3000.0;
/// Hard cap on the animation timestep, in seconds. The step is real elapsed
/// time between loop iterations, and an iteration can stall well past a
/// second (USB flush backlog, scheduling); integrating a gap like that in
/// one go teleports flings across the band. Capped, a stall just plays the
/// animation out slower.
const MAX_ANIM_DT: f64 = 0.05;
/// Minimum release velocity (px/s) for a two-finger layer swipe to commit
/// the switch regardless of how far it has slid. Layer swiping is a
/// two-finger HORIZONTAL fling: the digitizer never reports Y movement
/// (verified with evtest -- the axis is declared but silent), so vertical
/// gestures cannot exist on this hardware.
const LAYER_SWIPE_MIN_VELOCITY: f64 = 300.0;

/// What one finger on the bar is currently doing.
#[derive(Clone, Copy)]
enum TouchState {
    /// Holding an activated button (its key is down until release).
    Held { layer: usize, btn: usize },
    /// Not yet disambiguated between tap, hold, scroll, and layer swipe.
    /// `btn` is `None` when the touch only caught a moving band (or hit
    /// a gap) and so should never press anything.
    Pending {
        layer: usize,
        btn: Option<usize>,
        start_x: f64,
        x: f64,
        at: Instant,
    },
    /// Dragging the scrollable band. `last_t_us` is the previous touch event's
    /// hardware timestamp — velocity must be computed from event time, not
    /// wall-clock processing time (events arrive in batches).
    Scroll {
        layer: usize,
        last_x: f64,
        last_t_us: u64,
        velocity: f64,
    },
    /// Two-finger horizontal swipe switching layers: the whole bar slides
    /// sideways with the fingers (`layer_shift` in the main loop).
    LayerSwipe {
        last_x: f64,
        last_t_us: u64,
        velocity: f64,
    },
}

impl TouchState {
    /// Short label for NQTD_TOUCH_LOG diagnostics.
    fn name(&self) -> &'static str {
        match self {
            TouchState::Held { .. } => "held",
            TouchState::Pending { .. } => "pending",
            TouchState::Scroll { .. } => "scroll",
            TouchState::LayerSwipe { .. } => "swipe",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
}

/// Latest battery reading (capacity %, state), updated by a poller thread.
/// The power_supply sysfs attributes go through the SMC on T2 Macs and can
/// take tens of ms to read, so they must never be touched on the render path
/// -- that was a visible frame hitch on every battery refresh.
static BATTERY_STATE: Mutex<(u32, BatteryState)> = Mutex::new((100, BatteryState::NotCharging));

/// Latest CPU temperature in whole °C, updated by a poller thread (same
/// never-read-sysfs-on-the-render-path rule as BATTERY_STATE). `None` means no
/// readable thermal zone.
static CPU_TEMP_STATE: Mutex<Option<i32>> = Mutex::new(None);

#[derive(Copy, Clone, Eq, PartialEq)]
enum CpuTempUnit {
    Celsius,
    Fahrenheit,
}

struct BatteryImages {
    plain: Vec<Handle>,
    charging: Vec<Handle>,
    bolt: Handle,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum BatteryIconMode {
    Percentage,
    Icon,
    Both,
}

impl BatteryIconMode {
    fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(BatteryIconMode, BatteryImages),
    CpuTemp(CpuTempUnit),
    /// A command widget: `text`/`color` are updated from its script's output.
    Command {
        id: usize,
        text: String,
        color: Option<Color>,
    },
    Spacer,
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Vec<ButtonAction>,
    icon_width: f64,
    icon_height: f64,
    // Per-button style overrides; fall back to the global Style when None.
    bg_color: Option<Color>,
    bg_color_active: Option<Color>,
    text_color: Option<Color>,
}

/// Copy the latest widget outputs into their buttons, marking changed ones for
/// redraw. Cheap enough to call every loop iteration (the results map is small).
fn apply_widget_results(layers: &mut [FunctionLayer], rt: &WidgetRuntime) {
    let map = rt.results();
    for layer in layers.iter_mut() {
        for (_, button) in layer.buttons.iter_mut() {
            if let ButtonImage::Command { id, text, color } = &mut button.image {
                match map.get(id) {
                    Some(out) if *text != out.text || *color != out.color => {
                        *text = out.text.clone();
                        *color = out.color;
                    }
                    _ => continue,
                }
            } else {
                continue;
            }
            button.changed = true;
        }
    }
}

/// Set the cairo source to the background image (positioned to fill the bar) if
/// one is configured, otherwise the solid background color. `shift` is the
/// pixel-shift offset: the image is loaded PIXEL_SHIFT_* px larger than the bar
/// so it can slide around without exposing its edges.
fn set_background_source(c: &Context, style: &style::Style, shift: (f64, f64)) {
    if let Some(img) = &style.background_image {
        c.set_source_surface(
            img,
            shift.0 - (PIXEL_SHIFT_WIDTH_PX / 2) as f64,
            shift.1 - (PIXEL_SHIFT_HEIGHT_PX / 2) as f64,
        )
        .unwrap();
    } else {
        style.background.set_source(c);
    }
}

/// Lay `text` out in the bar font. Pango shapes with per-glyph font fallback
/// (and color emoji), which the cairo toy API's single font face could not.
fn text_layout(c: &Context, style: &style::Style, text: &str) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(c);
    layout.set_font_description(Some(&style.font));
    layout.set_text(text);
    layout
}

/// Draw `layout` horizontally centered in the button and vertically centered
/// in the bar, with the cairo source as the text color.
fn show_layout_centered(
    c: &Context,
    layout: &pango::Layout,
    height: i32,
    button_left_edge: f64,
    button_width: u64,
    y_shift: f64,
) {
    let (tw, th) = layout.pixel_size();
    c.move_to(
        button_left_edge + (button_width as f64 / 2.0 - tw as f64 / 2.0).round(),
        y_shift + ((height as f64 - th as f64) / 2.0).round(),
    );
    pangocairo::functions::show_layout(c, layout);
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    ))
}

fn try_load_png(path: impl AsRef<Path>, icon_width: i32, icon_height: i32) -> Result<ButtonImage> {
    let mut file = File::open(path)?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == icon_height && surf.width() == icon_width {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, icon_width, icon_height).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(
        icon_width as f64 / surf.width() as f64,
        icon_height as f64 / surf.height() as f64,
    );
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    Ok(ButtonImage::Bitmap(resized))
}

fn try_load_image(
    name: impl AsRef<str>,
    theme: Option<impl AsRef<str>>,
    icon_width: i32,
    icon_height: i32,
) -> Result<ButtonImage> {
    let name = name.as_ref();
    let locations;

    // Load list of candidate locations
    if let Some(theme) = theme {
        // Freedesktop icons
        let theme = theme.as_ref();
        let candidates = vec![
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .with_size(icon_height as u16)
                .force_svg()
                .find(),
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .force_svg()
                .find(),
        ];

        // .flatten() removes `None` and unwraps `Some` values
        locations = candidates.into_iter().flatten().collect();
    } else {
        // Standard file icons, searched most-specific first: the user's
        // ~/.config/not-quite-tiny-dfr, then the system /etc, then the shipped /usr/share.
        let mut candidates = Vec::new();
        if let Some(Some(dir)) = USER_ICON_DIR.get() {
            candidates.push(dir.join(format!("{name}.svg")));
            candidates.push(dir.join(format!("{name}.png")));
        }
        candidates.push(PathBuf::from(format!("/etc/not-quite-tiny-dfr/{name}.svg")));
        candidates.push(PathBuf::from(format!("/etc/not-quite-tiny-dfr/{name}.png")));
        candidates.push(PathBuf::from(format!(
            "/usr/share/not-quite-tiny-dfr/{name}.svg"
        )));
        candidates.push(PathBuf::from(format!(
            "/usr/share/not-quite-tiny-dfr/{name}.png"
        )));
        locations = candidates;
    };

    // Try to load each candidate
    let mut last_err = anyhow!("no suitable icon path was found"); // in case locations is empty

    for location in locations {
        let result = match location.extension().and_then(|s| s.to_str()) {
            Some("png") => try_load_png(&location, icon_width, icon_height),
            Some("svg") => try_load_svg(
                location
                    .to_str()
                    .ok_or(anyhow!("image path is not unicode"))?,
            ),
            _ => Err(anyhow!("invalid file extension")),
        };

        match result {
            Ok(image) => return Ok(image),
            Err(err) => {
                last_err = err.context(format!("while loading path {}", location.display()));
            }
        };
    }

    // if function hasn't returned by now, all sources have been exhausted
    Err(last_err.context(format!("failed loading all possible paths for icon {name}")))
}

fn find_battery_device() -> Option<String> {
    let power_supply_path = "/sys/class/power_supply";
    if let Ok(entries) = fs::read_dir(power_supply_path) {
        for entry in entries.flatten() {
            let dev_path = entry.path();
            let type_path = dev_path.join("type");
            if let Ok(typ) = fs::read_to_string(&type_path) {
                if typ.trim() == "Battery" {
                    if let Some(name) = dev_path.file_name().and_then(|n| n.to_str()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

fn get_battery_state(battery: &str) -> (u32, BatteryState) {
    let status_path = format!("/sys/class/power_supply/{}/status", battery);
    let status = fs::read_to_string(&status_path).unwrap_or_else(|_| "Unknown".to_string());

    let capacity = {
        #[cfg(target_arch = "x86_64")]
        {
            let charge_now_path = format!("/sys/class/power_supply/{}/charge_now", battery);
            let charge_full_path = format!("/sys/class/power_supply/{}/charge_full", battery);
            let charge_now = fs::read_to_string(&charge_now_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            let charge_full = fs::read_to_string(&charge_full_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            match (charge_now, charge_full) {
                (Some(now), Some(full)) if full > 0.0 => ((now / full) * 100.0).round() as u32,
                _ => 100,
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let capacity_path = format!("/sys/class/power_supply/{}/capacity", battery);
            fs::read_to_string(&capacity_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(100)
        }
    };

    let status = match status.trim() {
        "Charging" | "Full" => BatteryState::Charging,
        "Discharging" if capacity < 10 => BatteryState::Low,
        _ => BatteryState::NotCharging,
    };
    (capacity, status)
}

/// The `temp` file of the x86 package-temperature thermal zone, when one
/// exists. Its absence selects the hottest-zone fallback in `read_cpu_temp`.
fn find_cpu_temp_zone() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        let path = entry.path();
        if fs::read_to_string(path.join("type")).is_ok_and(|t| t.trim() == "x86_pkg_temp") {
            return Some(path.join("temp"));
        }
    }
    None
}

/// Read the CPU/SoC temperature in whole °C: the x86 package sensor when
/// present (`zone`), otherwise the hottest thermal zone (e.g. Apple Silicon).
fn read_cpu_temp(zone: Option<&Path>) -> Option<i32> {
    fn read_millideg(path: &Path) -> Option<i64> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }
    let millideg = match zone {
        Some(path) => read_millideg(path),
        None => fs::read_dir("/sys/class/thermal")
            .ok()?
            .flatten()
            .filter_map(|e| read_millideg(&e.path().join("temp")))
            .max(),
    }?;
    Some((millideg / 1000) as i32)
}

/// The CpuTemp widget's label for the current cached reading.
fn cpu_temp_text(unit: CpuTempUnit) -> String {
    match *CPU_TEMP_STATE.lock().unwrap() {
        Some(c) => match unit {
            CpuTempUnit::Celsius => format!("CPU {c}\u{00b0}C"),
            CpuTempUnit::Fahrenheit => format!("CPU {}\u{00b0}F", c * 9 / 5 + 32),
        },
        None => "CPU n/a".to_string(),
    }
}

impl Button {
    fn with_config(cfg: ButtonConfig, default_icon_size: i32) -> Button {
        let (bg_color, bg_color_active, text_color) = (cfg.color, cfg.color_active, cfg.text_color);
        let mut button = if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if let Some(icon) = cfg.icon {
            Button::new_icon(
                &icon,
                cfg.theme,
                cfg.action,
                cfg.icon_width.unwrap_or(default_icon_size),
                cfg.icon_height.unwrap_or(default_icon_size),
            )
        } else if let Some(time) = cfg.time {
            Button::new_time(cfg.action, &time, cfg.locale.as_deref())
        } else if let Some(battery_mode) = cfg.battery {
            if find_battery_device().is_some() {
                Button::new_battery(cfg.action, battery_mode, cfg.theme)
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
        } else if let Some(unit) = cfg.cpu_temp {
            Button::new_cpu_temp(cfg.action, &unit)
        } else {
            Button::new_spacer()
        };
        button.bg_color = bg_color;
        button.bg_color_active = bg_color_active;
        button.text_color = text_color;
        button
    }
    fn new_spacer() -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Spacer,
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }
    fn new_text(text: String, action: Vec<ButtonAction>) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }
    fn new_command(id: usize, action: Vec<ButtonAction>) -> Button {
        Button {
            action,
            active: false,
            changed: true, // draw the placeholder until the first result arrives
            image: ButtonImage::Command {
                id,
                text: "\u{2026}".to_string(),
                color: None,
            },
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }
    fn new_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        action: Vec<ButtonAction>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let image =
            try_load_image(path, theme, icon_width, icon_height).expect("failed to load icon");
        Button {
            action,
            image,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            active: false,
            changed: false,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }
    fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Handle {
        if let ButtonImage::Svg(svg) =
            try_load_image(icon, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap()
        {
            return svg;
        }
        panic!("failed to load icon");
    }
    fn new_battery(
        action: Vec<ButtonAction>,
        battery_mode: String,
        theme: Option<impl AsRef<str>>,
    ) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar",
            "battery_1_bar",
            "battery_2_bar",
            "battery_3_bar",
            "battery_4_bar",
            "battery_5_bar",
            "battery_6_bar",
            "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20",
            "battery_charging_30",
            "battery_charging_50",
            "battery_charging_60",
            "battery_charging_80",
            "battery_charging_90",
            "battery_charging_full",
        ] {
            charging.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        let battery_mode = match battery_mode.as_str() {
            "icon" => BatteryIconMode::Icon,
            "percentage" => BatteryIconMode::Percentage,
            "both" => BatteryIconMode::Both,
            _ => panic!("invalid battery mode, accepted modes: icon, percentage, both"),
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Battery(
                battery_mode,
                BatteryImages {
                    plain,
                    bolt,
                    charging,
                },
            ),
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }

    fn new_cpu_temp(action: Vec<ButtonAction>, unit: &str) -> Button {
        // An unknown unit is only worth a journal line, not a daemon abort:
        // this also runs on live config reloads.
        let unit = match unit {
            "celsius" => CpuTempUnit::Celsius,
            "fahrenheit" => CpuTempUnit::Fahrenheit,
            other => {
                eprintln!("not-quite-tiny-dfr: unknown CpuTemp unit {other:?}, using \"celsius\"");
                CpuTempUnit::Celsius
            }
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::CpuTemp(unit),
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }

    fn new_time(action: Vec<ButtonAction>, format: &str, locale_str: Option<&str>) -> Button {
        let format_str = if format == "24hr" {
            "%H:%M    %a %-e %b"
        } else if format == "12hr" {
            "%-l:%M %p    %a %-e %b"
        } else {
            format
        };

        let format_items = match StrftimeItems::new(format_str).parse_to_owned() {
            Ok(s) => s,
            Err(e) => panic!("Invalid time format, consult the configuration file for examples of correct ones: {e:?}"),
        };

        let locale = locale_str
            .and_then(|l| Locale::try_from(l).ok())
            .unwrap_or(Locale::POSIX);
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Time(format_items, locale),
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
        }
    }
    fn needs_faster_refresh(&self) -> bool {
        match &self.image {
            ButtonImage::Time(items, _) => items.iter().any(|item| {
                use chrono::format::{Item, Numeric};
                match item {
                    Item::Numeric(Numeric::Second, _)
                    | Item::Numeric(Numeric::Nanosecond, _)
                    | Item::Numeric(Numeric::Timestamp, _) => true,
                    _ => false,
                }
            }),
            _ => false,
        }
    }
    fn render(
        &self,
        c: &Context,
        style: &style::Style,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                let layout = text_layout(c, style, text);
                show_layout_centered(c, &layout, height, button_left_edge, button_width, y_shift);
            }
            ButtonImage::Svg(svg) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();

                svg.render_document(c, &Rectangle::new(x, y, self.icon_width, self.icon_height))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, self.icon_width, self.icon_height);
                c.fill().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let formatted_time = current_time
                    .format_localized_with_items(format.iter(), *locale)
                    .to_string();
                let layout = text_layout(c, style, &formatted_time);
                show_layout_centered(c, &layout, height, button_left_edge, button_width, y_shift);
            }
            ButtonImage::Battery(battery_mode, icons) => {
                let (capacity, state) = *BATTERY_STATE.lock().unwrap();
                let icon = if battery_mode.should_draw_icon() {
                    Some(match state {
                        BatteryState::Charging => match capacity {
                            0..=20 => &icons.charging[0],
                            21..=30 => &icons.charging[1],
                            31..=50 => &icons.charging[2],
                            51..=60 => &icons.charging[3],
                            61..=80 => &icons.charging[4],
                            81..=99 => &icons.charging[5],
                            _ => &icons.charging[6],
                        },
                        _ => match capacity {
                            0 => &icons.plain[0],
                            1..=20 => &icons.plain[1],
                            21..=30 => &icons.plain[2],
                            31..=50 => &icons.plain[3],
                            51..=60 => &icons.plain[4],
                            61..=80 => &icons.plain[5],
                            81..=99 => &icons.plain[6],
                            _ => &icons.plain[7],
                        },
                    })
                } else if state == BatteryState::Charging {
                    Some(&icons.bolt)
                } else {
                    None
                };
                let percent_str = format!("{:.0}%", capacity);
                let layout = text_layout(c, style, &percent_str);
                let (text_width, text_height) = layout.pixel_size();
                let mut width = text_width as f64;
                let mut text_offset = 0.0;
                if let Some(svg) = icon {
                    if !battery_mode.should_draw_text() {
                        width = DEFAULT_ICON_SIZE as f64;
                    } else {
                        width += DEFAULT_ICON_SIZE as f64 + BATTERY_ICON_TEXT_GAP;
                        text_offset = BATTERY_ICON_TEXT_GAP;
                    }
                    text_offset += DEFAULT_ICON_SIZE as f64;
                    let x = button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
                    let y = y_shift + ((height as f64 - DEFAULT_ICON_SIZE as f64) / 2.0).round();

                    svg.render_document(
                        c,
                        &Rectangle::new(x, y, DEFAULT_ICON_SIZE as f64, DEFAULT_ICON_SIZE as f64),
                    )
                    .unwrap();
                }
                if battery_mode.should_draw_text() {
                    c.move_to(
                        button_left_edge
                            + (button_width as f64 / 2.0 - width / 2.0 + text_offset).round(),
                        y_shift + ((height as f64 - text_height as f64) / 2.0).round(),
                    );
                    pangocairo::functions::show_layout(c, &layout);
                }
            }
            ButtonImage::CpuTemp(unit) => {
                let layout = text_layout(c, style, &cpu_temp_text(*unit));
                show_layout_centered(c, &layout, height, button_left_edge, button_width, y_shift);
            }
            ButtonImage::Command { text, .. } => {
                let layout = text_layout(c, style, text);
                show_layout_centered(c, &layout, height, button_left_edge, button_width, y_shift);
            }
            ButtonImage::Spacer => (),
        }
    }
    /// The color to draw this button's text in, letting a command widget's own
    /// JSON `color` -- or a CpuTemp widget's cool/warm/hot coding -- override
    /// the configured/default text color.
    fn effective_text_color(&self, style: &style::Style) -> Color {
        if let ButtonImage::Command {
            color: Some(color), ..
        } = &self.image
        {
            return *color;
        }
        if let ButtonImage::CpuTemp(_) = &self.image {
            match *CPU_TEMP_STATE.lock().unwrap() {
                Some(c) if c >= CPU_TEMP_HOT_C => return style.cpu_temp_hot_color,
                Some(c) if c >= CPU_TEMP_WARM_C => return style.cpu_temp_warm_color,
                Some(_) => {
                    return self.text_color.unwrap_or(style.cpu_temp_cool_color);
                }
                // No sensor: "CPU n/a" in the ordinary text color.
                None => {}
            }
        }
        self.text_color.unwrap_or(style.text_color)
    }
    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        if self.active != active {
            self.active = active;
            self.changed = true;

            toggle_keys(uinput, &self.action, active as i32);
        }
    }
    /// Set only the pressed look (no key events). Used while a touch on a
    /// scrollable band is still ambiguous between tap, hold, and scroll, so the
    /// button lights up immediately without committing to a key press.
    fn set_visual_active(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.changed = true;
        }
    }
    /// Emit this button's key events without touching the visual state.
    fn emit_keys<F>(&self, uinput: &mut UInputHandle<F>, pressed: bool)
    where
        F: AsRawFd,
    {
        toggle_keys(uinput, &self.action, pressed as i32);
    }
    /// Resolve the fill color for this button's rounded rectangle, or `None`
    /// if it should not be drawn (outlines disabled and button is inactive).
    /// Battery buttons signal charge state via color and are always drawn.
    fn fill_color(&self, style: &style::Style, show_outlines: bool) -> Option<Color> {
        if let ButtonImage::Battery(_, _) = &self.image {
            let (_, state) = *BATTERY_STATE.lock().unwrap();
            match state {
                BatteryState::Charging => return Some(style.battery_charging_color),
                BatteryState::Low => return Some(style.battery_low_color),
                BatteryState::NotCharging => {}
            }
        }
        if self.active {
            Some(self.bg_color_active.unwrap_or(style.button_color_active))
        } else if show_outlines || style.button_color_set || self.bg_color.is_some() {
            // Draw the idle fill when outlines are on, or when the user set an
            // explicit ButtonColor (globally or per-button) -- so a tint over a
            // background image works even with ShowButtonOutlines = false.
            Some(self.bg_color.unwrap_or(style.button_color))
        } else {
            None
        }
    }
}

/// Paint one button (rounded-rect fill plus label/icon) at the given geometry.
/// `radius` must already be capped against the button size.
#[allow(clippy::too_many_arguments)]
fn paint_button(
    c: &Context,
    button: &Button,
    style: &style::Style,
    show_outlines: bool,
    left_edge: f64,
    button_width: f64,
    radius: f64,
    bot: f64,
    top: f64,
    height: i32,
    y_shift: f64,
) {
    let fill = if matches!(button.image, ButtonImage::Spacer) {
        None
    } else {
        button.fill_color(style, show_outlines)
    };
    if let Some(fill) = fill {
        fill.set_source(c);
        // draw box with rounded corners
        c.new_sub_path();
        let left = left_edge + radius;
        let right = (left_edge + button_width.ceil()) - radius;
        // Inset the corner centers by the radius so the rounding stays
        // inside the button band [bot, top]. Centering them on bot/top
        // makes the corners overhang past the band -- off the top/bottom
        // of the short panel -- leaving only the straight edges visible.
        let cy_top = bot + radius;
        let cy_bot = top - radius;
        c.arc(
            right,
            cy_top,
            radius,
            (-90.0f64).to_radians(),
            (0.0f64).to_radians(),
        );
        c.arc(
            right,
            cy_bot,
            radius,
            (0.0f64).to_radians(),
            (90.0f64).to_radians(),
        );
        c.arc(
            left,
            cy_bot,
            radius,
            (90.0f64).to_radians(),
            (180.0f64).to_radians(),
        );
        c.arc(
            left,
            cy_top,
            radius,
            (180.0f64).to_radians(),
            (270.0f64).to_radians(),
        );
        c.close_path();
        c.fill().unwrap();
    }
    button.effective_text_color(style).set_source(c);
    button.render(c, style, height, left_edge, button_width.ceil() as u64, y_shift);
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    displays_battery: bool,
    displays_cpu_temp: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
    faster_refresh: bool,
    /// Leading buttons declared `Pinned` in the config (the Esc), when
    /// PinnedIgnoreScroll applies them; they never scroll with the band.
    pinned_count: usize,
    /// Virtual slots occupied by the pinned buttons.
    pinned_slots: usize,
    /// Whether the pinned buttons also hold still during a layer swipe
    /// (PinnedIgnoreLayerSwipe).
    pin_swipe: bool,
    /// How many slots the scrolling region shows at once; 0 disables scrolling.
    visible_slots: usize,
    /// Whether the band wraps around like a loop, or stops at its ends.
    scroll_loop: bool,
    /// When not looping, whether overscroll past an end stretches out with
    /// rubber-band resistance and springs back, instead of clamping dead.
    scroll_rubber_band: bool,
    /// Scroll position along the virtual strip, in px; wraps modulo the period.
    scroll_offset: f64,
    /// Fling momentum in px/s (in finger direction); 0 when not coasting.
    scroll_velocity: f64,
    /// Offset the band is gliding to after a scroll, so it never rests with a
    /// button cut off; `None` when settled or being dragged.
    scroll_snap: Option<f64>,
    /// Friction time constant of the current fling. Per-fling: it is stretched
    /// or shrunk at release so the natural landing point is slot-aligned while
    /// the release velocity stays continuous (a velocity jump reads as a hitch).
    fling_tau: f64,
}

/// For a layer slide in the given direction on `layers[active]`: which
/// neighbor slides in, how far the slide travels, and whether the pinned
/// prefix holds still. The prefix can only hold still when BOTH sides of the
/// transition pin the same slots -- with one side unpinned there is nothing
/// coherent to hold, so the whole bar slides and a layer whose Esc is pinned
/// simply carries it along for that transition.
fn slide_params(
    layers: &[FunctionLayer],
    active: usize,
    dir_positive: bool,
    width: f64,
    style: &style::Style,
) -> (usize, f64, bool) {
    let n = layers.len();
    let incoming = if dir_positive {
        (active + n - 1) % n
    } else {
        (active + 1) % n
    };
    let a = &layers[active];
    let stay = a.swipe_pinned_slots() > 0
        && a.swipe_pinned_slots() == layers[incoming].swipe_pinned_slots();
    let travel = if stay {
        a.slide_travel(width, style)
    } else {
        width
    };
    (incoming, travel, stay)
}

/// A layer-swap rotation renumbers `layers`; touch states hold layer
/// indices, so they must rotate the same way or a finger keeps acting on
/// whichever layer slid into its old index (e.g. releasing its held key on
/// the wrong layer's button, or scrolling the wrong band).
fn rotate_touch_layers<K>(touches: &mut HashMap<K, TouchState>, n: usize, left: bool) {
    for state in touches.values_mut() {
        match state {
            TouchState::Held { layer, .. }
            | TouchState::Pending { layer, .. }
            | TouchState::Scroll { layer, .. } => {
                *layer = if left {
                    (*layer + n - 1) % n
                } else {
                    (*layer + 1) % n
                };
            }
            TouchState::LayerSwipe { .. } => {}
        }
    }
}

/// Layout of a scrollable layer: a pinned region on the left (Esc) and a
/// wrapping band of the remaining buttons filling the rest of the bar.
struct ScrollGeometry {
    /// Width of one virtual button slot, in px.
    slot_width: f64,
    /// Distance from one slot's left edge to the next (slot plus gap).
    pitch: f64,
    /// Left edge of the scrolling region (right of the pinned buttons).
    region_left: f64,
    /// Width of the scrolling region.
    region_width: f64,
    /// Total length of the virtual strip (each slot plus one gap); the scroll
    /// offset wraps modulo this, which is what makes the band loop around.
    period: f64,
    /// Largest resting offset when not looping: the last button flush with the
    /// window's right edge.
    max_offset: f64,
}

impl ScrollGeometry {
    /// Map a raw offset (tracking the finger 1:1) to the displayed one:
    /// overshoot past either end is compressed asymptotically toward
    /// RUBBER_BAND_RANGE, which is what makes the band feel elastic.
    fn rubber_display(&self, raw: f64) -> f64 {
        let compress = |x: f64| RUBBER_BAND_RANGE * x / (x + RUBBER_BAND_RANGE);
        if raw < 0.0 {
            -compress(-raw)
        } else if raw > self.max_offset {
            self.max_offset + compress(raw - self.max_offset)
        } else {
            raw
        }
    }

    /// Inverse of `rubber_display`, so drags and flings can integrate in raw
    /// (finger) space and stay path-independent: the same travel back always
    /// returns the band to where it started stretching.
    fn rubber_raw(&self, displayed: f64) -> f64 {
        // The compression never actually reaches the cap; the min() only
        // guards the division against float dust at extreme offsets.
        let expand = |d: f64| {
            let d = d.min(RUBBER_BAND_RANGE - 1e-6);
            RUBBER_BAND_RANGE * d / (RUBBER_BAND_RANGE - d)
        };
        if displayed < 0.0 {
            -expand(-displayed)
        } else if displayed > self.max_offset {
            self.max_offset + expand(displayed - self.max_offset)
        } else {
            displayed
        }
    }
}

impl FunctionLayer {
    /// Whether overscroll on this layer rubber-bands (only meaningful without
    /// looping: a looping band has no ends to overshoot).
    fn rubber_bands(&self) -> bool {
        !self.scroll_loop && self.scroll_rubber_band
    }

    /// Leading buttons that hold still during a layer slide.
    fn swipe_pinned_count(&self) -> usize {
        if self.pin_swipe {
            self.pinned_count
        } else {
            0
        }
    }

    /// Virtual slots those buttons occupy.
    fn swipe_pinned_slots(&self) -> usize {
        if self.pin_swipe {
            self.pinned_slots
        } else {
            0
        }
    }

    /// How far a layer slide travels before the swap commits. With buttons
    /// held still at the left, only the region right of them slides, so the
    /// travel is that region's width plus one button gap -- the incoming
    /// content then abuts the outgoing content seamlessly instead of towing
    /// an Esc-sized hole behind it. With nothing held still it is the full
    /// bar width.
    fn slide_travel(&self, width: f64, style: &style::Style) -> f64 {
        if self.swipe_pinned_slots() == 0 {
            return width;
        }
        let spacing = style.button_spacing;
        let edge = style.edge_padding;
        if let Some(geo) = self.scroll_geometry(width, style) {
            return geo.region_width + spacing;
        }
        let n = self.virtual_button_count as f64;
        let vbw = (width - 2.0 * edge - spacing * (n - 1.0)) / n;
        let guard = edge + self.swipe_pinned_slots() as f64 * (vbw + spacing);
        (width - edge - guard) + spacing
    }

    /// The scroll layout for this layer, or `None` when it doesn't scroll
    /// (scrolling disabled, or all the buttons already fit).
    fn scroll_geometry(&self, width: f64, style: &style::Style) -> Option<ScrollGeometry> {
        let scroll_slots = self.virtual_button_count - self.pinned_slots;
        if self.visible_slots == 0 || scroll_slots <= self.visible_slots {
            return None;
        }
        let spacing = style.button_spacing;
        let usable = width - 2.0 * style.edge_padding;
        let total = (self.visible_slots + self.pinned_slots) as f64;
        let slot_width = (usable - spacing * (total - 1.0)) / total;
        if slot_width <= 0.0 {
            return None;
        }
        let pitch = slot_width + spacing;
        let region_left = style.edge_padding + self.pinned_slots as f64 * pitch;
        Some(ScrollGeometry {
            slot_width,
            pitch,
            region_left,
            region_width: width - style.edge_padding - region_left,
            period: scroll_slots as f64 * pitch,
            max_offset: (scroll_slots - self.visible_slots) as f64 * pitch,
        })
    }

    /// Normalize a scroll offset: wrap around the band when looping, clamp to
    /// the ends when not.
    fn normalize_offset(&self, geo: &ScrollGeometry, offset: f64) -> f64 {
        if self.scroll_loop {
            offset.rem_euclid(geo.period)
        } else {
            offset.clamp(0.0, geo.max_offset)
        }
    }

    /// The offset the band should come to rest at, nearest to `offset`: a
    /// position where neither window edge cuts through a button. Plain slot
    /// alignment isn't enough -- a button stretched across several slots must
    /// not straddle an edge either, so only offsets whose left AND right edges
    /// land on real button boundaries qualify. A button wider than the whole
    /// window can never fit, so left-aligning it is accepted for that one.
    fn snap_target(&self, geo: &ScrollGeometry, offset: f64) -> f64 {
        let scroll_slots = self.virtual_button_count - self.pinned_slots;
        // Band buttons' start slots, strip-relative and sorted.
        let starts: Vec<usize> = self.buttons[self.pinned_count..]
            .iter()
            .map(|(start, _)| start - self.pinned_slots)
            .collect();
        let is_start = |slot: usize| starts.binary_search(&(slot % scroll_slots)).is_ok();
        // Fallback only for degenerate layouts with no valid position at all.
        let mut best = (offset / geo.pitch).round() * geo.pitch;
        if !self.scroll_loop {
            best = best.clamp(0.0, geo.max_offset);
        }
        let mut best_dist = f64::INFINITY;
        for (j, &s) in starts.iter().enumerate() {
            let end = starts.get(j + 1).copied().unwrap_or(scroll_slots);
            if !is_start(s + self.visible_slots) && end - s < self.visible_slots {
                continue;
            }
            let cand = s as f64 * geo.pitch;
            if !self.scroll_loop && cand > geo.max_offset + 0.5 {
                // Without looping the window cannot rest past the last button.
                continue;
            }
            // When looping, also compare against the candidate's wrapped
            // copies, so "nearest" works across the band seam.
            let copies = if self.scroll_loop {
                [cand - geo.period, cand, cand + geo.period]
            } else {
                [cand, f64::INFINITY, f64::INFINITY]
            };
            for c in copies {
                let d = (offset - c).abs();
                if d < best_dist {
                    best_dist = d;
                    best = c;
                }
            }
        }
        best
    }

    /// The index in `buttons` of the button covering virtual slot `slot`.
    fn button_at_slot(&self, slot: usize) -> Option<usize> {
        if slot >= self.virtual_button_count {
            return None;
        }
        let idx = self
            .buttons
            .iter()
            .position(|(start, _)| *start > slot)
            .unwrap_or(self.buttons.len())
            - 1;
        Some(idx)
    }

    #[allow(clippy::too_many_arguments)]
    fn with_config(
        cfg: Vec<ButtonConfig>,
        widgets: &mut Vec<WidgetSpec>,
        next_id: &mut usize,
        default_icon_size: i32,
        visible_buttons: usize,
        scroll_loop: bool,
        scroll_rubber_band: bool,
        pin_scroll: bool,
        pin_swipe: bool,
    ) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }
        // The pinned region is the leading run of buttons marked Pinned in
        // the config (the declared Esc); PinnedIgnoreScroll turns the whole
        // mechanism off for scrolling.
        let declared_pinned = cfg.iter().take_while(|c| c.pinned.unwrap_or(false)).count();
        let pinned_count = if pin_scroll { declared_pinned } else { 0 };

        let mut virtual_button_count = 0;
        let displays_time = cfg.iter().any(|cfg| cfg.time.is_some());
        let displays_battery = cfg.iter().any(|cfg| cfg.battery.is_some());
        let displays_cpu_temp = cfg.iter().any(|cfg| cfg.cpu_temp.is_some());
        let buttons = cfg
            .into_iter()
            .scan(&mut virtual_button_count, |state, mut cfg| {
                let i = **state;
                let mut stretch = cfg.stretch.unwrap_or(1);
                if stretch < 1 {
                    println!("Stretch value must be at least 1, setting to 1.");
                    stretch = 1;
                }
                **state += stretch;
                let button = if let Some(command) = cfg.command.take() {
                    let id = *next_id;
                    *next_id += 1;
                    widgets.push(WidgetSpec {
                        id,
                        command,
                        interval: WidgetSpec::interval_from_secs(cfg.interval),
                    });
                    Button::new_command(id, std::mem::take(&mut cfg.action))
                } else {
                    Button::with_config(cfg, default_icon_size)
                };
                Some((i, button))
            })
            .collect::<Vec<_>>();
        let faster_refresh = buttons.iter().any(|(_, b)| b.needs_faster_refresh());
        let pinned_slots = if pinned_count == 0 {
            0
        } else if pinned_count < buttons.len() {
            buttons[pinned_count].0
        } else {
            virtual_button_count
        };
        FunctionLayer {
            displays_time,
            displays_battery,
            displays_cpu_temp,
            buttons,
            virtual_button_count,
            faster_refresh,
            pinned_count,
            pinned_slots,
            pin_swipe,
            visible_slots: visible_buttons,
            scroll_loop,
            scroll_rubber_band,
            scroll_offset: 0.0,
            scroll_velocity: 0.0,
            scroll_snap: None,
            fling_tau: FLING_FRICTION_TAU,
        }
    }
    fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
        // Layer-swipe slide: buttons draw shifted sideways by this many px
        // along the bar. When `slide_pins` (both sides of the transition pin
        // the same slots -- see slide_params), the pinned Esc stays put like
        // it does for band scrolling; otherwise it slides along with the
        // rest. `base_pass` is false for the incoming layer of a sliding
        // composite: it must not repaint the background or stack a second
        // Esc on top of a held-still one.
        slide_offset: f64,
        slide_pins: bool,
        base_pass: bool,
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let style = &config.style;
        // The buttons that hold still for THIS slide (0 when the transition
        // partner's pinning doesn't match).
        let static_count = if slide_pins {
            self.swipe_pinned_count()
        } else {
            0
        };
        let static_slots = if slide_pins {
            self.swipe_pinned_slots()
        } else {
            0
        };
        // With a background image, pixel shift slides the image instead of the
        // buttons: the layout stays put and the panel still gets its burn-in
        // relief from the image pixels moving underneath.
        let shift_background = style.background_image.is_some();
        let (pixel_shift, bg_shift) = if shift_background {
            ((0.0, 0.0), pixel_shift)
        } else {
            (pixel_shift, (0.0, 0.0))
        };
        let pixel_shift_width = if config.enable_pixel_shift && !shift_background {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let spacing = style.button_spacing;
        let edge = style.edge_padding;
        let effective_width = (width - pixel_shift_width as i32) as f64;
        let margin = (1.0 - style.height_percent / 100.0) / 2.0;
        let bot = (height as f64) * margin;
        let top = (height as f64) * (1.0 - margin);
        // Cap the radius at half the button height, otherwise the rounded-corner
        // arcs overlap into a degenerate shape that stops responding to changes.
        let radius = style.corner_radius.clamp(0.0, (top - bot) / 2.0);
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if let Some(geo) = self.scroll_geometry(effective_width, style) {
            // Band movement (scroll/fling/snap) arrives as complete_redraw --
            // the whole band moves as one piece. Without it, only the changed
            // buttons are cleared and repainted in place (e.g. the battery's
            // periodic refresh), so a widget tick never costs a full-bar
            // recomposite.
            let shift_x = pixel_shift_x + (pixel_shift_width / 2) as f64;
            let h = height as f64;
            let w = width as f64;
            let mut modified_regions = if complete_redraw {
                vec![ClipRect::new(0, 0, height as u16, width as u16)]
            } else {
                Vec::new()
            };
            if complete_redraw && base_pass {
                set_background_source(&c, style, bg_shift);
                c.paint().unwrap();
            }
            // Pinned buttons hold still during a layer slide when the
            // transition keeps them static; otherwise they ride along (and
            // the incoming layer draws its own copy sliding in instead of
            // skipping it).
            let pinned_stay = static_count > 0;
            c.save().unwrap();
            if !pinned_stay {
                c.translate(slide_offset, 0.0);
            }
            for i in 0..if base_pass || !pinned_stay {
                self.pinned_count
            } else {
                0
            } {
                let end = if i + 1 < self.buttons.len() {
                    self.buttons[i + 1].0
                } else {
                    self.virtual_button_count
                };
                let (start, button) = &mut self.buttons[i];
                if !button.changed && !complete_redraw {
                    continue;
                }
                let span = (end - *start) as f64;
                let button_width = span * geo.slot_width + (span - 1.0) * spacing;
                let radius = radius.min(button_width / 2.0);
                let left_edge = edge + *start as f64 * geo.pitch + shift_x;
                if !complete_redraw {
                    set_background_source(&c, style, bg_shift);
                    c.rectangle(
                        left_edge,
                        bot - radius,
                        button_width,
                        top - bot + radius * 2.0,
                    );
                    c.fill().unwrap();
                    modified_regions.push(ClipRect::new(
                        (h - top - radius).clamp(0.0, h) as u16,
                        left_edge.clamp(0.0, w) as u16,
                        (h - bot + radius).clamp(0.0, h) as u16,
                        (left_edge + button_width).clamp(0.0, w) as u16,
                    ));
                }
                paint_button(
                    &c,
                    button,
                    style,
                    config.show_button_outlines,
                    left_edge,
                    button_width,
                    radius,
                    bot,
                    top,
                    height,
                    pixel_shift_y,
                );
                button.changed = false;
            }
            c.restore().unwrap();
            // The band, clipped to its region so wrapped copies (and partial
            // clears) never bleed over the pinned Esc or off the bar. During
            // a layer slide the window travels with the layer (second clip,
            // in slid space) but stays inside the fixed band area (first
            // clip), so it can never cover a held-still Esc; with nothing
            // held still the fixed clip opens up to the whole bar.
            let region_left = geo.region_left + shift_x;
            let fixed_left = if pinned_stay { region_left } else { 0.0 };
            c.save().unwrap();
            c.rectangle(
                fixed_left,
                0.0,
                region_left + geo.region_width - fixed_left,
                h,
            );
            c.clip();
            c.translate(slide_offset, 0.0);
            c.rectangle(region_left, 0.0, geo.region_width, h);
            c.clip();
            for i in self.pinned_count..self.buttons.len() {
                let end = if i + 1 < self.buttons.len() {
                    self.buttons[i + 1].0
                } else {
                    self.virtual_button_count
                };
                let (start, button) = &mut self.buttons[i];
                if !button.changed && !complete_redraw {
                    continue;
                }
                let span = (end - *start) as f64;
                let button_width = span * geo.slot_width + (span - 1.0) * spacing;
                let radius = radius.min(button_width / 2.0);
                let strip_x = (*start - self.pinned_slots) as f64 * geo.pitch;
                let x0 = if self.scroll_loop {
                    (strip_x - self.scroll_offset).rem_euclid(geo.period)
                } else {
                    strip_x - self.scroll_offset
                };
                // The button, plus (when looping) a copy one period to the
                // left when it straddles the wrap seam.
                let wrap_copy = if self.scroll_loop {
                    x0 - geo.period
                } else {
                    f64::INFINITY
                };
                for base in [x0, wrap_copy] {
                    if base >= geo.region_width || base + button_width <= 0.0 {
                        continue;
                    }
                    let left_edge = region_left + base;
                    if !complete_redraw {
                        set_background_source(&c, style, bg_shift);
                        c.rectangle(
                            left_edge,
                            bot - radius,
                            button_width,
                            top - bot + radius * 2.0,
                        );
                        c.fill().unwrap();
                        // Dirty rect kept inside the band region: the paint is
                        // clipped there, and the pinned Esc must not be flushed
                        // with stale pixels.
                        modified_regions.push(ClipRect::new(
                            (h - top - radius).clamp(0.0, h) as u16,
                            left_edge.max(region_left).clamp(0.0, w) as u16,
                            (h - bot + radius).clamp(0.0, h) as u16,
                            (left_edge + button_width)
                                .min(region_left + geo.region_width)
                                .clamp(0.0, w) as u16,
                        ));
                    }
                    paint_button(
                        &c,
                        button,
                        style,
                        config.show_button_outlines,
                        left_edge,
                        button_width,
                        radius,
                        bot,
                        top,
                        height,
                        pixel_shift_y,
                    );
                }
                button.changed = false;
            }
            c.restore().unwrap();
            return modified_regions;
        }

        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        let virtual_button_width =
            (effective_width - 2.0 * edge - spacing * (self.virtual_button_count - 1) as f64)
                / self.virtual_button_count as f64;

        if complete_redraw && base_pass {
            set_background_source(&c, style, bg_shift);
            c.paint().unwrap();
        }

        c.save().unwrap();
        for i in 0..self.buttons.len() {
            if i == static_count {
                // Everything after the held-still buttons slides with a
                // layer swipe, behind a clip that keeps it off their area.
                if static_slots > 0 {
                    let guard = (static_slots as f64 * (virtual_button_width + spacing)).floor()
                        + edge
                        + pixel_shift_x
                        + (pixel_shift_width / 2) as f64;
                    c.rectangle(guard, 0.0, width as f64 - guard, height as f64);
                    c.clip();
                }
                c.translate(slide_offset, 0.0);
            }
            // The incoming layer of a sliding composite skips its held-still
            // buttons: the outgoing layer's identical ones are already there.
            if i < static_count && !base_pass {
                continue;
            }
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = (start as f64 * (virtual_button_width + spacing)).floor()
                + edge
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + spacing)).floor();
            // Also cap against the button width so narrow buttons stay valid.
            let radius = radius.min(button_width / 2.0);

            if !complete_redraw {
                set_background_source(&c, style, bg_shift);
                c.rectangle(
                    left_edge,
                    bot - radius,
                    button_width,
                    top - bot + radius * 2.0,
                );
                c.fill().unwrap();
            }
            paint_button(
                &c,
                button,
                style,
                config.show_button_outlines,
                left_edge,
                button_width,
                radius,
                bot,
                top,
                height,
                pixel_shift_y,
            );

            button.changed = false;

            if !complete_redraw {
                // Clamp to the framebuffer bounds: a large CornerRadius or
                // HeightPercent can otherwise push these past 0/height and, via
                // the u16 casts, wrap into an invalid rect that makes the
                // drm.dirty() call below fail and panic the daemon.
                let h = height as f64;
                let w = width as f64;
                modified_regions.push(ClipRect::new(
                    (h - top - radius).clamp(0.0, h) as u16,
                    left_edge.clamp(0.0, w) as u16,
                    (h - bot + radius).clamp(0.0, h) as u16,
                    (left_edge + button_width).clamp(0.0, w) as u16,
                ));
            }
        }
        c.restore().unwrap();

        modified_regions
    }

    fn hit(
        &self,
        style: &style::Style,
        width: u16,
        height: u16,
        x: f64,
        y: f64,
        i: Option<usize>,
    ) -> Option<usize> {
        let spacing = style.button_spacing;
        let edge = style.edge_padding;
        if y < 0.1 * height as f64 || y > 0.9 * height as f64 {
            return None;
        }

        if let Some(geo) = self.scroll_geometry(width as f64, style) {
            let pitch = geo.pitch;
            let target = if x < geo.region_left {
                // Pinned (Esc) region.
                let rel = x - edge;
                let slot = (rel.max(0.0) / pitch) as usize;
                if rel >= 0.0
                    && slot < self.pinned_slots
                    && rel - slot as f64 * pitch <= geo.slot_width
                {
                    self.button_at_slot(slot)
                } else {
                    None
                }
            } else {
                // The band: translate into strip coordinates (wrapped when
                // looping; negative only while rubber-banded past the start,
                // where the finger is left of the first button).
                let sx = if self.scroll_loop {
                    (x - geo.region_left + self.scroll_offset).rem_euclid(geo.period)
                } else {
                    x - geo.region_left + self.scroll_offset
                };
                let slot = (sx / pitch) as usize;
                if sx >= 0.0 && sx - slot as f64 * pitch <= geo.slot_width {
                    self.button_at_slot(slot + self.pinned_slots)
                } else {
                    None // in the gap between buttons
                }
            };
            // For motion tracking (`i` set), report a hit only while the finger
            // is still over that same button.
            return match i {
                Some(i) => (target == Some(i)).then_some(i),
                None => target,
            };
        }

        let usable = width as f64 - 2.0 * edge;
        let virtual_button_width = (usable - spacing * (self.virtual_button_count - 1) as f64)
            / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i =
                ((x - edge).max(0.0) / (usable / self.virtual_button_count as f64)) as usize;
            self.buttons
                .iter()
                .position(|(start, _)| *start > virtual_i)
                .unwrap_or(self.buttons.len())
                - 1
        });
        if i >= self.buttons.len() {
            return None;
        }

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge = (start as f64 * (virtual_button_width + spacing)).floor() + edge;

        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + spacing)).floor();

        if x < left_edge || x > (left_edge + button_width) {
            return None;
        }

        Some(i)
    }
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    uinput
        .write(&[input_event {
            value,
            type_: ty as u16,
            code,
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }])
        .unwrap();
}

fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<ButtonAction>, value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for action in codes {
        match action {
            // Handled inside the daemon; no input event leaves it.
            ButtonAction::TouchBarBrightnessUp | ButtonAction::TouchBarBrightnessDown => {
                if value <= 1 {
                    let delta = if *action == ButtonAction::TouchBarBrightnessUp {
                        1
                    } else {
                        -1
                    };
                    backlight::dim_button(delta, value == 1);
                }
            }
            ButtonAction::Key(kc) => emit(uinput, EventKind::Key, *kc as u16, value),
        }
    }
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}

/// Drop root down to `user`, keeping the supplementary `groups` (input/video)
/// needed for device access. Privilege dropping is one-way, so this is only
/// called once we actually know which user to serve.
fn drop_privileges(user: &str, groups: &[&str]) {
    PrivDrop::default()
        .user(user)
        .group_list(groups)
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));
}

/// Set up the virtual keyboard. Created in main(), before the panic boundary:
/// /dev/uinput is only openable as root, and by the time real_main panics the
/// privileges are long dropped -- but the emergency Esc key still needs it.
fn create_uinput() -> UInputHandle<File> {
    let uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    uinput.set_evbit(EventKind::Key).unwrap();
    for k in Key::iter() {
        uinput.set_keybit(k).unwrap();
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();
    uinput
}

/// Landscape width (px) of the emergency Esc touch region; must match the Esc
/// button baked into crash_bitmap.raw.
const CRASH_ESC_WIDTH_PX: f64 = 140.0;

/// Invert the emergency Esc button's pixels, as press feedback.
fn invert_crash_esc(drm: &mut DrmBackend, height: u16) {
    let row_bytes = drm.fb_info().map(|i| i.size().0 as usize).unwrap_or(64) * 4;
    if let Ok(mut map) = drm.map() {
        let data = map.as_mut();
        let n = (CRASH_ESC_WIDTH_PX as usize * row_bytes).min(data.len());
        for b in &mut data[..n] {
            *b ^= 0xFF;
        }
    }
    let _ = drm.dirty(&[ClipRect::new(0, 0, height, CRASH_ESC_WIDTH_PX as u16)]);
}

/// After a crash: show the crash message and keep serving a bare-bones Esc key,
/// so a Mac without a physical Esc isn't left stuck (e.g. in a fullscreen app).
/// Everything here is best-effort -- we are already in a failure state.
fn emergency_mode(drm: &mut DrmBackend, uinput: &mut UInputHandle<File>) -> ! {
    let (height, width) = drm.mode().size();
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    if let Ok(mut map) = drm.map() {
        let data = map.as_mut();
        let mut wptr = 0;
        for byte in crash_bitmap {
            for i in 0..8 {
                let bit = ((byte >> i) & 0x1) == 0;
                let color = if bit { 0xFF } else { 0x0 };
                if wptr + 3 < data.len() {
                    data[wptr] = color;
                    data[wptr + 1] = color;
                    data[wptr + 2] = color;
                    data[wptr + 3] = color;
                }
                wptr += 4;
            }
        }
    }
    let _ = drm.dirty(&[ClipRect::new(0, 0, height, width)]);
    // The normal backlight management died with real_main; make sure the
    // message (and the Esc key) is actually visible.
    backlight::force_on();

    let mut input_tb = Libinput::new_with_udev(Interface);
    if input_tb.udev_assign_seat("seat-touchbar").is_err() {
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    let epoll = match Epoll::new(EpollCreateFlags::empty()) {
        Ok(e) => e,
        Err(_) => loop {
            thread::sleep(Duration::from_secs(3600));
        },
    };
    let _ = epoll.add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0));
    let esc_action = vec![ButtonAction::Key(Key::Esc)];
    let mut esc_slots = HashSet::new();
    loop {
        let _ = epoll.wait(&mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)], 60_000u16);
        if input_tb.dispatch().is_err() {
            continue;
        }
        for event in &mut input_tb.clone() {
            match event {
                Event::Touch(TouchEvent::Down(dn)) => {
                    if dn.x_transformed(width as u32) <= CRASH_ESC_WIDTH_PX
                        && esc_slots.insert(dn.seat_slot())
                        && esc_slots.len() == 1
                    {
                        toggle_keys(uinput, &esc_action, 1);
                        invert_crash_esc(drm, height);
                    }
                }
                Event::Touch(TouchEvent::Up(up)) => {
                    if esc_slots.remove(&up.seat_slot()) && esc_slots.is_empty() {
                        toggle_keys(uinput, &esc_action, 0);
                        invert_crash_esc(drm, height);
                    }
                }
                _ => {}
            }
        }
    }
}

fn main() {
    let mut drm = DrmBackend::open_card().unwrap();
    let mut uinput = create_uinput();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm, &mut uinput)));
    emergency_mode(&mut drm, &mut uinput);
}

fn real_main(drm: &mut DrmBackend, uinput: &mut UInputHandle<File>) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut backlight = BacklightManager::new();

    // Work out whose config we serve (and whose privileges we drop to). We do
    // NOT block waiting for a login and never fall back to `nobody`: if no one
    // is logged in yet (e.g. the daemon started at boot, before the greeter) we
    // come up on system + default config, stay root, and poll for a login in the
    // main loop below -- dropping to the user and loading their ~/.config the
    // moment they log in. Privilege dropping is one-way, so staying root until
    // then is exactly what lets a late login still take effect.
    let groups = ["input", "video"];
    let target_user = user::resolve_target_user();

    // Config override layers, lowest precedence first: system /etc, then (once we
    // know who to serve) the per-user ~/.config. Both are merged on load and
    // watched for live-reload; the user layer is attached later if not known yet.
    let mut cfg_paths = vec![PathBuf::from("/etc/not-quite-tiny-dfr/config.toml")];
    if let Some(u) = &target_user {
        let dir = u.home.join(".config/not-quite-tiny-dfr");
        println!(
            "not-quite-tiny-dfr: serving user {:?}, config dir {}",
            u.name,
            dir.display()
        );
        // Icons named in the config are looked up in the user's config dir first.
        let _ = USER_ICON_DIR.set(Some(dir.clone()));
        cfg_paths.push(dir.join("config.toml"));
    } else {
        println!("not-quite-tiny-dfr: no logged-in user yet; starting on system config, will load ~/.config on login");
    }

    let mut cfg_mgr = ConfigManager::new(cfg_paths, width, height);
    let (mut cfg, mut layers, initial_widgets) = cfg_mgr.load_config();
    let mut pixel_shift = PixelShiftManager::new();
    let mut last = Instant::now();
    // Last time fling momentum was integrated (see the top of the main loop).
    let mut fling_tick = Instant::now();
    // Vertical layer-swipe slide: how far the visible layer is slid off the
    // bar (px, signed), and the slide's animation target -- +-height commits
    // the swap, 0 aborts back to the current layer.
    let mut layer_shift: f64 = 0.0;
    let mut layer_slide_target: Option<f64> = None;
    // The 60 Hz pacing gate: absolute deadline of the next frame, and when
    // the previous frame started (only for the frame log's period readout).
    let mut next_frame = Instant::now();
    let mut last_frame_start = Instant::now();
    // Consecutive stalled flushes (see the cool-down at the flush site).
    let mut flush_stalls: u32 = 0;
    // NQTD_FRAME_LOG=1 prints per-frame timings to the journal, for chasing
    // pacing problems on real hardware.
    let frame_log = std::env::var_os("NQTD_FRAME_LOG").is_some_and(|v| v != "0");
    let touch_log = std::env::var_os("NQTD_TOUCH_LOG").is_some_and(|v| v != "0");
    // The battery reading whose rendering is currently on screen; battery
    // buttons only redraw when the poller's cache moves away from this.
    let mut last_battery_drawn = *BATTERY_STATE.lock().unwrap();
    // Same, for CpuTemp buttons.
    let mut last_cpu_temp_drawn = *CPU_TEMP_STATE.lock().unwrap();

    // If we already know the user, drop to them now. Otherwise stay root and
    // defer the drop until someone logs in (handled at the top of the loop).
    let mut privileges_dropped = false;
    if let Some(u) = &target_user {
        drop_privileges(&u.name, &groups);
        privileges_dropped = true;
    }

    // Widget worker threads are only spawned once privileges have been dropped,
    // so scripts never run as root: until a user is resolved the runtime is
    // empty, and the real widgets come up when we reload after login.
    let (wake_read, wake_write) = nix::unistd::pipe().unwrap();
    widget::set_nonblocking(wake_read.as_raw_fd());
    let wake_write = Arc::new(wake_write);

    // Battery polling runs on its own thread (see BATTERY_STATE): one seed
    // read now, then a 1 Hz loop that updates the cache and wakes the epoll
    // loop through the pipe whenever the reading actually changed.
    if let Some(battery) = find_battery_device() {
        *BATTERY_STATE.lock().unwrap() = get_battery_state(&battery);
        let wake = wake_write.clone();
        thread::spawn(move || loop {
            let reading = get_battery_state(&battery);
            let changed = {
                let mut shared = BATTERY_STATE.lock().unwrap();
                let changed = *shared != reading;
                *shared = reading;
                changed
            };
            if changed {
                let byte = [1u8];
                unsafe {
                    libc::write(wake.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
                }
            }
            thread::sleep(BATTERY_POLL);
        });
    }
    // CPU temperature polling mirrors the battery poller: reading thermal
    // sysfs can be slow on T2 (SMC-backed), so it stays off the render path.
    {
        let cpu_zone = find_cpu_temp_zone();
        if let Some(seed) = read_cpu_temp(cpu_zone.as_deref()) {
            *CPU_TEMP_STATE.lock().unwrap() = Some(seed);
            let wake = wake_write.clone();
            thread::spawn(move || loop {
                let reading = read_cpu_temp(cpu_zone.as_deref());
                let changed = {
                    let mut shared = CPU_TEMP_STATE.lock().unwrap();
                    let changed = *shared != reading;
                    *shared = reading;
                    changed
                };
                if changed {
                    let byte = [1u8];
                    unsafe {
                        libc::write(wake.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
                    }
                }
                thread::sleep(CPU_TEMP_POLL);
            });
        }
    }
    let mut widget_rt = WidgetRuntime::new(
        if privileges_dropped {
            initial_widgets
        } else {
            Vec::new()
        },
        wake_write.clone(),
    );
    let mut last_user_poll = Instant::now();

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut active_layer = 0;
    let mut needs_complete_redraw = true;
    let mut last_soft_dim = 1.0;

    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();
    let udev_monitor = MonitorBuilder::new()
        .unwrap()
        .match_subsystem("power_supply")
        .unwrap()
        .listen()
        .unwrap();
    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .unwrap();
    epoll
        .add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 1))
        .unwrap();
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .unwrap();
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .unwrap();
    epoll
        .add(&wake_read, EpollEvent::new(EpollFlags::EPOLLIN, 4))
        .unwrap();
    // Frame-deadline timer for the pacing gate: epoll's millisecond timeout is
    // too coarse for the panel's 16.667 ms period -- the rounding made frames
    // ~58 fps, beating against the 60 Hz panel as a periodic stutter.
    let frame_timer = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_NONBLOCK).unwrap();
    epoll
        .add(&frame_timer, EpollEvent::new(EpollFlags::EPOLLIN, 5))
        .unwrap();
    let mut frame_timer_armed = false;

    let mut digitizer: Option<InputDevice> = None;
    let mut touches = HashMap::new();
    let mut last_redraw_ts = if layers[active_layer].faster_refresh {
        Local::now().second()
    } else {
        Local::now().minute()
    };
    loop {
        // Deferred startup: if we came up before anyone was logged in, poll
        // logind (throttled) for a login. When one appears, attach the user's
        // ~/.config layer, drop to them, reload, and bring their widgets up
        // (now running as that user).
        if !privileges_dropped && last_user_poll.elapsed() >= USER_POLL_INTERVAL {
            last_user_poll = Instant::now();
            if let Some(u) = user::resolve_target_user() {
                let dir = u.home.join(".config/not-quite-tiny-dfr");
                println!(
                    "not-quite-tiny-dfr: {:?} logged in, loading config dir {}",
                    u.name,
                    dir.display()
                );
                let _ = USER_ICON_DIR.set(Some(dir.clone()));
                cfg_mgr.add_path(dir.join("config.toml"));
                drop_privileges(&u.name, &groups);
                privileges_dropped = true;
                let (new_cfg, new_layers, new_widgets) = cfg_mgr.load_config();
                cfg = new_cfg;
                layers = new_layers;
                active_layer = 0;
                needs_complete_redraw = true;
                widget_rt = WidgetRuntime::new(new_widgets, wake_write.clone());
            }
        }
        if let Some(new_widgets) = cfg_mgr.update_config(&mut cfg, &mut layers) {
            active_layer = 0;
            needs_complete_redraw = true;
            // Replacing the runtime drops the old one, stopping its threads.
            widget_rt = WidgetRuntime::new(new_widgets, wake_write.clone());
        }
        // Pull in any widget script output and clear the wake pipe.
        widget::drain(wake_read.as_raw_fd());
        apply_widget_results(&mut layers, &widget_rt);

        // Promote stationary touches on a scrollable band into real key holds
        // once they have sat still long enough to be a hold rather than a tap
        // or the start of a scroll.
        let mut hold_wait_ms: Option<u64> = None;
        for state in touches.values_mut() {
            let TouchState::Pending {
                layer,
                btn: Some(btn),
                start_x,
                x,
                at,
            } = *state
            else {
                continue;
            };
            if (x - start_x).abs() > SCROLL_SLOP_PX {
                continue;
            }
            let elapsed = at.elapsed().as_millis() as u64;
            if elapsed < HOLD_ACTIVATE_MS {
                let wait = HOLD_ACTIVATE_MS - elapsed;
                hold_wait_ms = Some(hold_wait_ms.map_or(wait, |w| w.min(wait)));
                continue;
            }
            if btn < layers[layer].buttons.len() {
                // The button already lights up on touch; a promotion to a hold
                // sends the actual key press (visual state stays as-is, so
                // set_active would see no change and skip the keys).
                let button = &mut layers[layer].buttons[btn].1;
                button.set_visual_active(true);
                button.emit_keys(uinput, true);
            }
            *state = TouchState::Held { layer, btn };
        }

        // Advance scroll animations, wrapping around the band: first fling
        // momentum (exponential friction), then a smooth snap glide so the
        // band never rests with a button cut off mid-slot. Inactive layers
        // keep animating too, so they settle.
        let anim_dt = fling_tick.elapsed().as_secs_f64().min(MAX_ANIM_DT);
        fling_tick = Instant::now();
        let mut scroll_animating = false;
        // Advance the sideways layer slide. The layers form a wrapping
        // carousel: committing at -width rotates to the next layer, +width
        // to the previous one, and 0 aborts back to the current one.
        if let Some(target) = layer_slide_target {
            let delta = target - layer_shift;
            if delta.abs() <= SNAP_EPSILON {
                if target < 0.0 {
                    layers.rotate_left(1);
                    rotate_touch_layers(&mut touches, layers.len(), true);
                } else if target > 0.0 {
                    layers.rotate_right(1);
                    rotate_touch_layers(&mut touches, layers.len(), false);
                }
                layer_shift = 0.0;
                layer_slide_target = None;
            } else {
                layer_shift += delta * (1.0 - (-anim_dt / SNAP_TAU).exp());
            }
            scroll_animating = true;
            needs_complete_redraw = true;
        } else if layer_shift != 0.0
            && !touches
                .values()
                .any(|t| matches!(t, TouchState::LayerSwipe { .. }))
        {
            // Safety net: a slide left dangling (e.g. its touch was cancelled)
            // resolves toward whichever layer is showing more.
            let (_, t, _) = slide_params(
                &layers,
                active_layer,
                layer_shift > 0.0,
                width as f64,
                &cfg.style,
            );
            layer_slide_target = Some(if layer_shift.abs() > t / 2.0 {
                t.copysign(layer_shift)
            } else {
                0.0
            });
            scroll_animating = true;
        }
        for (i, layer) in layers.iter_mut().enumerate() {
            // Self-heal any non-finite scroll state: NaN fails every settle
            // comparison, so it would otherwise animate (and force full
            // redraws) forever, with no button hittable -- a frozen bar.
            if !layer.scroll_offset.is_finite()
                || !layer.scroll_velocity.is_finite()
                || layer.scroll_snap.is_some_and(|t| !t.is_finite())
            {
                layer.scroll_offset = 0.0;
                layer.scroll_velocity = 0.0;
                layer.scroll_snap = None;
            }
            if layer.scroll_velocity == 0.0 && layer.scroll_snap.is_none() {
                // Safety net: a rubber-banding band must never REST stretched
                // past an end. Whatever path left it overscrolled with nothing
                // armed (a cancelled touch, a missed release), spring it back
                // -- unless a finger on this layer is holding the stretch.
                let finger_on_layer = touches.values().any(|t| match *t {
                    TouchState::Held { layer, .. }
                    | TouchState::Pending { layer, .. }
                    | TouchState::Scroll { layer, .. } => layer == i,
                    // A layer swipe holds the slide, not any band stretch.
                    TouchState::LayerSwipe { .. } => false,
                });
                if !layer.rubber_bands() || finger_on_layer {
                    continue;
                }
                let Some(geo) = layer.scroll_geometry(width as f64, &cfg.style) else {
                    continue;
                };
                if layer.scroll_offset < 0.0 || layer.scroll_offset > geo.max_offset {
                    layer.scroll_snap = Some(layer.scroll_offset.clamp(0.0, geo.max_offset));
                    // Step from the next frame: this tick's anim_dt spans the
                    // idle gap and would teleport the glide to its target.
                    scroll_animating = true;
                }
                continue;
            }
            let Some(geo) = layer.scroll_geometry(width as f64, &cfg.style) else {
                layer.scroll_velocity = 0.0;
                layer.scroll_snap = None;
                continue;
            };
            if layer.scroll_velocity != 0.0 {
                if layer.rubber_bands() {
                    let edge = layer.scroll_offset.clamp(0.0, geo.max_offset);
                    let over = layer.scroll_offset - edge;
                    if over != 0.0 {
                        // Past an end: a critically damped spring hauls the
                        // band back in one continuous out-and-back motion.
                        // scroll_offset moves by -velocity, so its rate is
                        // u = -velocity. Stepped with the exact closed-form
                        // solution, NOT Euler: Euler diverges when a stalled
                        // frame hands it a long timestep (this froze the bar
                        // -- the state exploded to NaN and never settled),
                        // while the closed form only ever decays.
                        let omega = 1.0 / RUBBER_SPRING_TAU;
                        let u0 = -layer.scroll_velocity;
                        let b = u0 + omega * over;
                        let decay = (-omega * anim_dt).exp();
                        let u = (u0 - omega * b * anim_dt) * decay;
                        layer.scroll_offset = edge + (over + b * anim_dt) * decay;
                        layer.scroll_velocity = -u;
                        if (layer.scroll_offset - edge).abs() <= SNAP_EPSILON
                            && u.abs() < FLING_STOP_VELOCITY
                        {
                            // Settled: the ends are slot-aligned, no glide.
                            layer.scroll_offset = edge;
                            layer.scroll_velocity = 0.0;
                        }
                    } else {
                        let next = layer.scroll_offset - layer.scroll_velocity * anim_dt;
                        if next < 0.0 || next > geo.max_offset {
                            // Crossing an end: cap the momentum handed to the
                            // spring so the bounce can't stretch further than
                            // a hard drag.
                            layer.scroll_velocity = layer
                                .scroll_velocity
                                .clamp(-RUBBER_MAX_BOUNCE_VELOCITY, RUBBER_MAX_BOUNCE_VELOCITY);
                            layer.scroll_offset -= layer.scroll_velocity * anim_dt;
                        } else {
                            layer.scroll_offset = next;
                            layer.scroll_velocity *= (-anim_dt / layer.fling_tau).exp();
                            if layer.scroll_velocity.abs() < FLING_STOP_VELOCITY {
                                layer.scroll_velocity = 0.0;
                                layer.scroll_snap =
                                    Some(layer.snap_target(&geo, layer.scroll_offset));
                            }
                        }
                    }
                } else {
                    layer.scroll_offset = layer.normalize_offset(
                        &geo,
                        layer.scroll_offset - layer.scroll_velocity * anim_dt,
                    );
                    // Without looping a fling stops dead at the ends (which are
                    // always slot-aligned, so no snap glide is needed).
                    if !layer.scroll_loop
                        && ((layer.scroll_offset <= 0.0 && layer.scroll_velocity > 0.0)
                            || (layer.scroll_offset >= geo.max_offset
                                && layer.scroll_velocity < 0.0))
                    {
                        layer.scroll_velocity = 0.0;
                        layer.scroll_snap = None;
                    }
                    layer.scroll_velocity *= (-anim_dt / layer.fling_tau).exp();
                    if layer.scroll_velocity.abs() < FLING_STOP_VELOCITY {
                        // Hand the residual distance over to the snap glide.
                        layer.scroll_velocity = 0.0;
                        layer.scroll_snap = Some(layer.snap_target(&geo, layer.scroll_offset));
                    }
                }
            } else if let Some(target) = layer.scroll_snap {
                let delta = target - layer.scroll_offset;
                if delta.abs() <= SNAP_EPSILON {
                    layer.scroll_offset = layer.normalize_offset(&geo, target);
                    layer.scroll_snap = None;
                } else {
                    layer.scroll_offset += delta * (1.0 - (-anim_dt / SNAP_TAU).exp());
                }
            }
            scroll_animating = true;
            if i == active_layer {
                needs_complete_redraw = true;
            }
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        // While still waiting for a login, keep the loop lively so we notice one
        // within ~a second rather than idling for the full timeout.
        if !privileges_dropped {
            next_timeout_ms = min(next_timeout_ms, USER_POLL_INTERVAL.as_millis() as i32);
        }

        // Wake in time to promote a pending touch into a key hold.
        if let Some(wait) = hold_wait_ms {
            next_timeout_ms = min(next_timeout_ms, wait.max(1) as i32);
        }

        let current_ts = if layers[active_layer].faster_refresh {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if layers[active_layer].displays_time && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }
        // Redraw battery buttons only when the poller's cached reading really
        // changed; marking them unconditionally used to force a redraw for
        // every input event on the seat -- dropped frames.
        if layers[active_layer].displays_battery {
            let reading = *BATTERY_STATE.lock().unwrap();
            if reading != last_battery_drawn {
                last_battery_drawn = reading;
                for button in &mut layers[active_layer].buttons {
                    if let ButtonImage::Battery(_, _) = button.1.image {
                        button.1.changed = true;
                    }
                }
            }
        }
        if layers[active_layer].displays_cpu_temp {
            let reading = *CPU_TEMP_STATE.lock().unwrap();
            if reading != last_cpu_temp_drawn {
                last_cpu_temp_drawn = reading;
                for button in &mut layers[active_layer].buttons {
                    if let ButtonImage::CpuTemp(_) = button.1.image {
                        button.1.changed = true;
                    }
                }
            }
        }

        // VRR-style pacing: render at most one frame per FRAME_PERIOD, on
        // absolute deadlines -- next_frame advances by exactly one period per
        // frame, so timer rounding and wake-up latency never accumulate into
        // a slower average rate. A frame due too early is deferred, not
        // dropped: the pending state stays marked and the frame timer below
        // fires at the deadline. An idle bar draws nothing at all.
        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.1.changed) {
            let now = Instant::now();
            if now + FRAME_SLACK >= next_frame {
                let period_us = (now - last_frame_start).as_micros() as u64;
                last_frame_start = now;
                // Deadlines are stamped at frame START (draw + flush count
                // against the budget); re-anchor only if we fell more than a
                // whole frame behind.
                next_frame = if now > next_frame + FRAME_PERIOD {
                    now + FRAME_PERIOD
                } else {
                    next_frame + FRAME_PERIOD
                };
                let was_complete = needs_complete_redraw;
                let shift = if cfg.enable_pixel_shift {
                    pixel_shift.get()
                } else {
                    (0.0, 0.0)
                };
                let clips = if layer_shift != 0.0 {
                    // Mid layer-swipe: composite both layers sliding along
                    // the bar, the incoming one exactly one slide-travel away
                    // so its content abuts the outgoing content seamlessly
                    // (a full bar-width would tow an Esc-sized hole). Which
                    // neighbor slides in depends on the direction: dragging
                    // right reveals the previous layer, left the next.
                    let (incoming, travel, stay) = slide_params(
                        &layers,
                        active_layer,
                        layer_shift > 0.0,
                        width as f64,
                        &cfg.style,
                    );
                    let incoming_off = layer_shift - travel.copysign(layer_shift);
                    let clips = layers[active_layer].draw(
                        &cfg,
                        width as i32,
                        height as i32,
                        &surface,
                        shift,
                        true,
                        layer_shift,
                        stay,
                        true,
                    );
                    layers[incoming].draw(
                        &cfg,
                        width as i32,
                        height as i32,
                        &surface,
                        shift,
                        true,
                        incoming_off,
                        stay,
                        false,
                    );
                    clips
                } else {
                    layers[active_layer].draw(
                        &cfg,
                        width as i32,
                        height as i32,
                        &surface,
                        shift,
                        needs_complete_redraw,
                        0.0,
                        true,
                        true,
                    )
                };
                let draw_done = Instant::now();
                // A changed button that is scrolled out of view produces no dirty
                // rects; flushing zero clips is EINVAL (this crashed the daemon),
                // so skip the frame entirely.
                if !clips.is_empty() {
                    let data = surface.data().unwrap();
                    {
                        let mut map = drm.map().unwrap();
                        let out = &mut map.as_mut()[..data.len()];
                        let dim = backlight.soft_dim_factor();
                        if dim < 1.0 {
                            // Software brightness: the appletb hardware backlight
                            // only has full/dim/off, so finer levels are done by
                            // scaling the pixels on their way to the framebuffer.
                            let mut lut = [0u8; 256];
                            for (i, v) in lut.iter_mut().enumerate() {
                                *v = (i as f64 * dim) as u8;
                            }
                            for (dst, src) in out.chunks_exact_mut(4).zip(data.chunks_exact(4)) {
                                dst[0] = lut[src[0] as usize];
                                dst[1] = lut[src[1] as usize];
                                dst[2] = lut[src[2] as usize];
                                dst[3] = src[3];
                            }
                        } else {
                            out.copy_from_slice(&data);
                        }
                    }
                    if let Err(err) = drm.dirty(&clips) {
                        // A struggling appletbdrm surfaces its errors here;
                        // panicking into emergency mode would only pile more
                        // traffic onto a panel that needs the opposite.
                        println!("dirty flush failed: {err}");
                    }
                }
                needs_complete_redraw = false;
                if frame_log {
                    println!(
                        "frame: period={:.1}ms draw={:.1}ms flush={:.1}ms complete={} clips={}",
                        period_us as f64 / 1000.0,
                        (draw_done - now).as_secs_f64() * 1000.0,
                        draw_done.elapsed().as_secs_f64() * 1000.0,
                        was_complete,
                        clips.len(),
                    );
                }
                // The flush is a synchronous request/response with the T2
                // over USB. A stalled flush means appletbdrm is waiting out
                // its response timeout -- the display stream is desyncing
                // ("Failed to read response (-110)" in the kernel log), and
                // the panel goes through a glitchy phase before continued
                // traffic wedges it completely (endless "Failed to send
                // message", dead until reboot). So at the first stall the
                // daemon goes quiet, backing off exponentially while stalls
                // persist; a healthy flush ends the episode. Mild overruns
                // just reschedule from completion instead of firing the next
                // frame back-to-back.
                let frame_end = Instant::now();
                let frame_cost = frame_end - now;
                if frame_cost >= FLUSH_STALL_MIN {
                    let cooldown =
                        FLUSH_COOLDOWN_BASE * (1 << flush_stalls.min(FLUSH_STALL_MAX_DOUBLINGS));
                    flush_stalls += 1;
                    next_frame = frame_end + cooldown;
                    println!(
                        "flush stalled ({} ms): cooling down {} s (stall #{})",
                        frame_cost.as_millis(),
                        cooldown.as_secs(),
                        flush_stalls,
                    );
                } else {
                    if flush_stalls > 0 {
                        println!("flush healthy again after {flush_stalls} stall(s)");
                        flush_stalls = 0;
                    }
                    if frame_cost > FRAME_PERIOD {
                        next_frame = frame_end + frame_cost;
                    }
                }
            }
        }

        // Arm the frame timer whenever another frame is coming: a deferred
        // draw (still-marked changes), or an animation that keeps producing
        // motion. The timerfd fires at the deadline with sub-ms precision.
        let frame_pending = scroll_animating
            || needs_complete_redraw
            || backlight.soft_dim_animating()
            || backlight::dim_held()
            || layers[active_layer].buttons.iter().any(|b| b.1.changed);
        if frame_pending {
            let remaining = next_frame
                .saturating_duration_since(Instant::now())
                .max(Duration::from_micros(100));
            let _ = frame_timer.set(
                Expiration::OneShot(TimeSpec::from_duration(remaining)),
                TimerSetTimeFlags::empty(),
            );
            frame_timer_armed = true;
        } else if frame_timer_armed {
            let _ = frame_timer.unset();
            frame_timer_armed = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        _ = udev_monitor.iter().last();

        // Clear the frame timer if it fired (nonblocking; harmless otherwise).
        let mut timer_buf = [0u8; 8];
        unsafe {
            libc::read(
                frame_timer.as_fd().as_raw_fd(),
                timer_buf.as_mut_ptr() as *mut libc::c_void,
                8,
            );
        }

        input_tb.dispatch().unwrap();
        input_main.dispatch().unwrap();
        for event in &mut input_tb.clone().chain(input_main.clone()) {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains(" Touch Bar") {
                        digitizer = Some(dev);
                    }
                }
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    // Fn peeks at the next layer; with a single layer there is
                    // nothing to peek at (or swap to).
                    if key.key() == Key::Fn as u32 && layers.len() > 1 {
                        if cfg.double_press_switch_layers > 0
                            && key.key_state() == KeyState::Pressed
                        {
                            if last.elapsed()
                                < Duration::from_millis(cfg.double_press_switch_layers.into())
                            {
                                layers.swap(0, 1);
                            }
                            last = Instant::now();
                        }
                        let new_layer = match key.key_state() {
                            KeyState::Pressed => 1,
                            KeyState::Released => 0,
                        };
                        if active_layer != new_layer {
                            active_layer = new_layer;
                            needs_complete_redraw = true;
                        }
                    }
                }
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer || backlight.current_bl() == 0 {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);
                            if touch_log {
                                println!("touch: down slot={} x={x:.1} y={y:.1}", dn.seat_slot());
                            }
                            // Touching a bar that is mid layer-slide catches
                            // the slide and takes it over as a new swipe; no
                            // button on a half-shown layer should press. The
                            // pinned Esc sits outside the slide and keeps
                            // pressing normally. But only while the slide is
                            // still visibly traveling: its exponential tail
                            // spends a long time within a few (invisible)
                            // pixels of done, and hijacking touches there
                            // turned every scroll started right after a layer
                            // switch into a phantom swipe.
                            if layer_shift != 0.0 || layer_slide_target.is_some() {
                                let target = layer_slide_target.unwrap_or(0.0);
                                let swiping = touches
                                    .values()
                                    .any(|t| matches!(t, TouchState::LayerSwipe { .. }));
                                if swiping || (target - layer_shift).abs() > SCROLL_SLOP_PX {
                                    // Only a held-still Esc is at its resting spot
                                    // and safe to press; when this transition
                                    // slides everything, nothing is static.
                                    let dir_positive = if layer_shift != 0.0 {
                                        layer_shift > 0.0
                                    } else {
                                        target > 0.0
                                    };
                                    let (_, _, stay) = slide_params(
                                        &layers,
                                        active_layer,
                                        dir_positive,
                                        width as f64,
                                        &cfg.style,
                                    );
                                    let esc_hit = layers[active_layer]
                                        .hit(&cfg.style, width, height, x, y, None)
                                        .filter(|&btn| {
                                            stay && btn < layers[active_layer].swipe_pinned_count()
                                        });
                                    if let Some(btn) = esc_hit {
                                        layers[active_layer].buttons[btn]
                                            .1
                                            .set_active(uinput, true);
                                        touches.insert(
                                            dn.seat_slot(),
                                            TouchState::Held {
                                                layer: active_layer,
                                                btn,
                                            },
                                        );
                                    } else {
                                        layer_slide_target = None;
                                        touches.insert(
                                            dn.seat_slot(),
                                            TouchState::LayerSwipe {
                                                last_x: x,
                                                last_t_us: dn.time_usec(),
                                                velocity: 0.0,
                                            },
                                        );
                                    }
                                    continue;
                                }
                                // Within a finger-slop of settling: the touch
                                // means the layer the user can already see.
                                // Finish the slide on the spot (a sub-slop
                                // jump) and let the touch land normally.
                                if target < 0.0 {
                                    layers.rotate_left(1);
                                    rotate_touch_layers(&mut touches, layers.len(), true);
                                } else if target > 0.0 {
                                    layers.rotate_right(1);
                                    rotate_touch_layers(&mut touches, layers.len(), false);
                                }
                                layer_shift = 0.0;
                                layer_slide_target = None;
                                needs_complete_redraw = true;
                            }
                            let layer = &mut layers[active_layer];
                            // Touching the band catches it: any fling stops, and
                            // a catch-tap should not also press a button. A
                            // pending snap glide is grabbed too.
                            let was_flinging = layer.scroll_velocity != 0.0;
                            layer.scroll_velocity = 0.0;
                            layer.scroll_snap = None;
                            let geo = layer.scroll_geometry(width as f64, &cfg.style);
                            match layer.hit(&cfg.style, width, height, x, y, None) {
                                // Band buttons (and, with layer swipe on, any
                                // unpinned button) wait out the tap/hold/scroll/
                                // swipe ambiguity before pressing anything, but
                                // light up right away.
                                Some(btn)
                                    if btn >= layer.pinned_count
                                        && (geo.is_some() || cfg.layer_swipe) =>
                                {
                                    if !was_flinging {
                                        layer.buttons[btn].1.set_visual_active(true);
                                    }
                                    touches.insert(
                                        dn.seat_slot(),
                                        TouchState::Pending {
                                            layer: active_layer,
                                            btn: (!was_flinging).then_some(btn),
                                            start_x: x,
                                            x,
                                            at: Instant::now(),
                                        },
                                    );
                                }
                                // Pinned buttons (Esc) keep the immediate
                                // press-on-touch behavior.
                                Some(btn) => {
                                    layer.buttons[btn].1.set_active(uinput, true);
                                    touches.insert(
                                        dn.seat_slot(),
                                        TouchState::Held {
                                            layer: active_layer,
                                            btn,
                                        },
                                    );
                                }
                                // A miss inside the band region can still start
                                // a scroll drag; with layer swipe on, a miss
                                // anywhere can start a swipe.
                                None => {
                                    if geo.is_some_and(|g| x >= g.region_left) || cfg.layer_swipe {
                                        touches.insert(
                                            dn.seat_slot(),
                                            TouchState::Pending {
                                                layer: active_layer,
                                                btn: None,
                                                start_x: x,
                                                x,
                                                at: Instant::now(),
                                            },
                                        );
                                    }
                                }
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            // Two-finger detection, computed before borrowing
                            // this touch's state: a horizontal drag with a
                            // second (non-held) finger down is a layer swipe,
                            // and only one finger drives the slide at a time.
                            let multi = touches
                                .values()
                                .filter(|t| !matches!(t, TouchState::Held { .. }))
                                .count()
                                >= 2;
                            let has_swipe = touches
                                .values()
                                .any(|t| matches!(t, TouchState::LayerSwipe { .. }));
                            // A band scroll already in progress owns the
                            // gesture: a finger added mid-scroll must not
                            // start a layer swipe on top of it.
                            let has_scroll = touches
                                .values()
                                .any(|t| matches!(t, TouchState::Scroll { .. }));
                            let Some(state) = touches.get_mut(&mtn.seat_slot()) else {
                                continue;
                            };
                            if touch_log {
                                println!(
                                    "touch: move slot={} x={x:.1} y={y:.1} state={}",
                                    mtn.seat_slot(),
                                    state.name()
                                );
                            }
                            match *state {
                                TouchState::Held { layer, btn } => {
                                    if btn < layers[layer].buttons.len() {
                                        let hit = layers[layer]
                                            .hit(&cfg.style, width, height, x, y, Some(btn))
                                            .is_some();
                                        layers[layer].buttons[btn].1.set_active(uinput, hit);
                                    }
                                }
                                TouchState::Pending {
                                    layer,
                                    btn,
                                    start_x,
                                    at,
                                    ..
                                } => {
                                    let crossed = (x - start_x).abs() > SCROLL_SLOP_PX;
                                    // With a second finger down, a horizontal
                                    // drag swipes layers; alone, it scrolls
                                    // the band.
                                    let became_swipe = crossed
                                        && cfg.layer_swipe
                                        && layers.len() > 1
                                        && multi
                                        && !has_swipe
                                        && !has_scroll;
                                    if crossed {
                                        // Became a gesture: the highlighted
                                        // candidate button is off the hook.
                                        if let Some(btn) = btn {
                                            if btn < layers[layer].buttons.len() {
                                                layers[layer].buttons[btn]
                                                    .1
                                                    .set_visual_active(false);
                                            }
                                        }
                                    }
                                    *state = if became_swipe {
                                        TouchState::LayerSwipe {
                                            last_x: x,
                                            last_t_us: mtn.time_usec(),
                                            velocity: 0.0,
                                        }
                                    } else if crossed
                                        && !multi
                                        && layers[layer]
                                            .scroll_geometry(width as f64, &cfg.style)
                                            .is_some()
                                    {
                                        TouchState::Scroll {
                                            layer,
                                            last_x: x,
                                            last_t_us: mtn.time_usec(),
                                            velocity: 0.0,
                                        }
                                    } else {
                                        TouchState::Pending {
                                            layer,
                                            // A drag that can't scroll or swipe
                                            // (single finger on a non-scrolling
                                            // layer, or a second finger next to
                                            // an active swipe) is a cancelled
                                            // tap.
                                            btn: if crossed { None } else { btn },
                                            start_x,
                                            x,
                                            at,
                                        }
                                    };
                                }
                                TouchState::Scroll {
                                    layer,
                                    last_x,
                                    last_t_us,
                                    velocity,
                                } => {
                                    if let Some(geo) =
                                        layers[layer].scroll_geometry(width as f64, &cfg.style)
                                    {
                                        let t_us = mtn.time_usec();
                                        let dx = x - last_x;
                                        // dt from the events' own timestamps:
                                        // batched events processed back-to-back
                                        // have near-zero wall-clock spacing,
                                        // which would explode dx/dt into a
                                        // phantom mega-fling.
                                        let dt = t_us.saturating_sub(last_t_us) as f64 / 1e6;
                                        let l = &mut layers[layer];
                                        l.scroll_offset = if l.rubber_bands() {
                                            // Track the finger in raw space so
                                            // pulling past an end meets growing
                                            // resistance, and dragging back
                                            // retraces the same stretch.
                                            geo.rubber_display(geo.rubber_raw(l.scroll_offset) - dx)
                                        } else {
                                            l.normalize_offset(&geo, l.scroll_offset - dx)
                                        };
                                        // Smooth the release velocity over the
                                        // last few motion events, capped so one
                                        // glitchy event can't run away with it.
                                        let velocity = if dt > 0.0 {
                                            (0.6 * (dx / dt) + 0.4 * velocity)
                                                .clamp(-FLING_MAX_VELOCITY, FLING_MAX_VELOCITY)
                                        } else {
                                            velocity
                                        };
                                        *state = TouchState::Scroll {
                                            layer,
                                            last_x: x,
                                            last_t_us: t_us,
                                            velocity,
                                        };
                                        if dx != 0.0 && layer == active_layer {
                                            needs_complete_redraw = true;
                                        }
                                    }
                                }
                                TouchState::LayerSwipe {
                                    last_x,
                                    last_t_us,
                                    velocity,
                                } => {
                                    let t_us = mtn.time_usec();
                                    let dx = x - last_x;
                                    let dt = t_us.saturating_sub(last_t_us) as f64 / 1e6;
                                    let (_, travel, _) = slide_params(
                                        &layers,
                                        active_layer,
                                        layer_shift + dx > 0.0,
                                        width as f64,
                                        &cfg.style,
                                    );
                                    layer_shift = (layer_shift + dx).clamp(-travel, travel);
                                    let velocity = if dt > 0.0 {
                                        (0.6 * (dx / dt) + 0.4 * velocity)
                                            .clamp(-FLING_MAX_VELOCITY, FLING_MAX_VELOCITY)
                                    } else {
                                        velocity
                                    };
                                    *state = TouchState::LayerSwipe {
                                        last_x: x,
                                        last_t_us: t_us,
                                        velocity,
                                    };
                                    if dx != 0.0 {
                                        needs_complete_redraw = true;
                                    }
                                }
                            }
                        }
                        TouchEvent::Up(up) => {
                            let Some(state) = touches.remove(&up.seat_slot()) else {
                                continue;
                            };
                            if touch_log {
                                println!(
                                    "touch: up slot={} state={}",
                                    up.seat_slot(),
                                    state.name()
                                );
                            }
                            match state {
                                TouchState::Held { layer, btn } => {
                                    if btn < layers[layer].buttons.len() {
                                        layers[layer].buttons[btn].1.set_active(uinput, false);
                                    }
                                }
                                // A quick tap: press and release (it was
                                // already lit up since touch-down).
                                TouchState::Pending {
                                    layer,
                                    btn: Some(btn),
                                    ..
                                } => {
                                    if btn < layers[layer].buttons.len() {
                                        let button = &mut layers[layer].buttons[btn].1;
                                        button.emit_keys(uinput, true);
                                        button.emit_keys(uinput, false);
                                        button.set_visual_active(false);
                                    }
                                }
                                TouchState::Pending { .. } => {}
                                TouchState::Scroll {
                                    layer,
                                    last_t_us,
                                    velocity,
                                    ..
                                } => {
                                    // A finger that rested before lifting was
                                    // placing the band, not flicking it: any
                                    // stale velocity from earlier motion must
                                    // not turn into a surprise fling.
                                    let velocity = if up.time_usec().saturating_sub(last_t_us)
                                        > FLING_STALE_US
                                    {
                                        0.0
                                    } else {
                                        velocity
                                    };
                                    if let Some(geo) =
                                        layers[layer].scroll_geometry(width as f64, &cfg.style)
                                    {
                                        let l = &mut layers[layer];
                                        if l.rubber_bands()
                                            && (l.scroll_offset < 0.0
                                                || l.scroll_offset > geo.max_offset)
                                        {
                                            // Let go while stretched past an
                                            // end: discard any fling and spring
                                            // back to the edge.
                                            l.scroll_snap =
                                                Some(l.scroll_offset.clamp(0.0, geo.max_offset));
                                        } else if velocity.abs() >= FLING_MIN_VELOCITY {
                                            // Align the natural landing point
                                            // with a slot boundary by adjusting
                                            // the friction, not the velocity:
                                            // the band must leave the finger at
                                            // exactly the speed it was dragged.
                                            let landing = l.snap_target(
                                                &geo,
                                                l.scroll_offset - velocity * FLING_FRICTION_TAU,
                                            );
                                            let tau = (l.scroll_offset - landing) / velocity;
                                            if tau > 0.0 {
                                                l.fling_tau = tau.clamp(
                                                    FLING_FRICTION_TAU * 0.5,
                                                    FLING_FRICTION_TAU * 2.0,
                                                );
                                                l.scroll_velocity = velocity;
                                            } else {
                                                // The aligned landing sits behind
                                                // the travel direction: too slow
                                                // to carry past it, glide there.
                                                l.scroll_snap = Some(landing);
                                            }
                                        } else {
                                            // Released without a fling: glide
                                            // to the nearest resting position.
                                            l.scroll_snap =
                                                Some(l.snap_target(&geo, l.scroll_offset));
                                        }
                                        fling_tick = Instant::now();
                                    }
                                }
                                TouchState::LayerSwipe {
                                    last_t_us,
                                    velocity,
                                    ..
                                } => {
                                    let velocity = if up.time_usec().saturating_sub(last_t_us)
                                        > FLING_STALE_US
                                    {
                                        0.0
                                    } else {
                                        velocity
                                    };
                                    // A flick commits the swap in its direction;
                                    // otherwise the slide settles to whichever
                                    // layer is showing more. The travel depends
                                    // on which transition the direction picks.
                                    let dir_positive = if velocity.abs() >= LAYER_SWIPE_MIN_VELOCITY
                                    {
                                        velocity > 0.0
                                    } else {
                                        layer_shift > 0.0
                                    };
                                    let (_, t, _) = slide_params(
                                        &layers,
                                        active_layer,
                                        dir_positive,
                                        width as f64,
                                        &cfg.style,
                                    );
                                    layer_slide_target =
                                        Some(if velocity.abs() >= LAYER_SWIPE_MIN_VELOCITY {
                                            t.copysign(velocity)
                                        } else if layer_shift.abs() > t / 2.0 {
                                            t.copysign(layer_shift)
                                        } else {
                                            0.0
                                        });
                                    fling_tick = Instant::now();
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        backlight.update_backlight(&cfg);
        // A soft-dim change re-scales every pixel, not just changed buttons.
        if backlight.soft_dim_factor() != last_soft_dim {
            last_soft_dim = backlight.soft_dim_factor();
            needs_complete_redraw = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Style;

    const W: u16 = 2170;
    const H: u16 = 60;

    /// A layer of `n` text buttons, the first `pinned` of them marked Pinned,
    /// showing `visible` slots at a time (0 = scrolling disabled).
    fn text_layer_mode(n: usize, pinned: usize, visible: usize, looping: bool) -> FunctionLayer {
        let keys = (0..n)
            .map(|i| ButtonConfig {
                text: Some(format!("B{i}")),
                pinned: (i < pinned).then_some(true),
                ..Default::default()
            })
            .collect();
        FunctionLayer::with_config(
            keys,
            &mut Vec::new(),
            &mut 0,
            48,
            visible,
            looping,
            true,
            true,
            true,
        )
    }

    fn text_layer(n: usize, pinned: usize, visible: usize) -> FunctionLayer {
        text_layer_mode(n, pinned, visible, true)
    }

    #[test]
    fn no_scroll_when_disabled_or_fitting() {
        let style = Style::default();
        // VisibleButtons unset (0): never scrolls.
        assert!(text_layer(20, 1, 0)
            .scroll_geometry(W as f64, &style)
            .is_none());
        // 6 band slots fit in 6 visible: no scrolling.
        assert!(text_layer(7, 1, 6)
            .scroll_geometry(W as f64, &style)
            .is_none());
        // 13 band slots > 6 visible: scrolls.
        assert!(text_layer(14, 1, 6)
            .scroll_geometry(W as f64, &style)
            .is_some());
    }

    #[test]
    fn scroll_geometry_dimensions() {
        let style = Style::default(); // spacing 16, edge padding 0
        let layer = text_layer(14, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        // 7 slots visible in total (6 band + pinned esc), 6 gaps between them.
        let expected_slot = (W as f64 - 16.0 * 6.0) / 7.0;
        let pitch = expected_slot + 16.0;
        assert!((geo.slot_width - expected_slot).abs() < 1e-9);
        assert!((geo.pitch - pitch).abs() < 1e-9);
        assert!((geo.region_left - pitch).abs() < 1e-9);
        assert!((geo.region_width - (W as f64 - pitch)).abs() < 1e-9);
        assert!((geo.period - 13.0 * pitch).abs() < 1e-9);
    }

    /// Like `text_layer`, but with an explicit stretch per button.
    fn stretched_layer(stretches: &[usize], pinned: usize, visible: usize) -> FunctionLayer {
        let keys = stretches
            .iter()
            .enumerate()
            .map(|(i, s)| ButtonConfig {
                text: Some(format!("B{i}")),
                pinned: (i < pinned).then_some(true),
                stretch: Some(*s),
                ..Default::default()
            })
            .collect();
        FunctionLayer::with_config(
            keys,
            &mut Vec::new(),
            &mut 0,
            48,
            visible,
            true,
            true,
            true,
            true,
        )
    }

    #[test]
    fn pinned_flags_declare_the_pinned_run() {
        let style = Style::default();
        // Two leading Pinned buttons -> both outside the band.
        let layer = text_layer_mode(14, 2, 6, true);
        assert_eq!(layer.pinned_count, 2);
        assert_eq!(layer.pinned_slots, 2);
        assert!(layer.scroll_geometry(W as f64, &style).is_some());
        // PinnedIgnoreScroll = false dissolves the pinned region entirely.
        let keys = (0..14)
            .map(|i| ButtonConfig {
                text: Some(format!("B{i}")),
                pinned: (i < 2).then_some(true),
                ..Default::default()
            })
            .collect();
        let layer = FunctionLayer::with_config(
            keys,
            &mut Vec::new(),
            &mut 0,
            48,
            6,
            true,
            true,
            false,
            true,
        );
        assert_eq!(layer.pinned_count, 0);
        assert_eq!(layer.swipe_pinned_count(), 0);
        // PinnedIgnoreLayerSwipe = false keeps the scroll pin but lets the
        // buttons slide with a layer swipe.
        let keys = (0..14)
            .map(|i| ButtonConfig {
                text: Some(format!("B{i}")),
                pinned: (i < 2).then_some(true),
                ..Default::default()
            })
            .collect();
        let layer = FunctionLayer::with_config(
            keys,
            &mut Vec::new(),
            &mut 0,
            48,
            6,
            true,
            true,
            true,
            false,
        );
        assert_eq!(layer.pinned_count, 2);
        assert_eq!(layer.swipe_pinned_count(), 0);
        assert_eq!(layer.slide_travel(W as f64, &style), W as f64);
    }

    #[test]
    fn mixed_pinning_slides_the_whole_bar() {
        let style = Style::default();
        // A pins its esc, B doesn't: nothing can hold still coherently, so
        // that transition slides the full bar and A carries its esc along.
        let layers = vec![text_layer(14, 1, 6), text_layer(14, 0, 6)];
        let (incoming, travel, stay) = slide_params(&layers, 0, false, W as f64, &style);
        assert_eq!(incoming, 1);
        assert!(!stay);
        assert_eq!(travel, W as f64);
        // Same from B's side going back to A.
        let (_, travel, stay) = slide_params(&layers, 1, true, W as f64, &style);
        assert!(!stay);
        assert_eq!(travel, W as f64);
        // Matching pins hold the esc still and travel only the band region.
        let layers = vec![text_layer(14, 1, 6), text_layer(14, 1, 6)];
        let (_, travel, stay) = slide_params(&layers, 0, false, W as f64, &style);
        assert!(stay);
        assert!(travel < W as f64);
    }

    #[test]
    fn slide_travel_spans_the_sliding_region() {
        let style = Style::default();
        // Pinned esc held still: travel = band region + one gap, so the
        // incoming layer abuts the outgoing content with no Esc-sized hole.
        let layer = text_layer(14, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let t = layer.slide_travel(W as f64, &style);
        assert!((t - (geo.region_width + style.button_spacing)).abs() < 1e-9);
        assert!(t < W as f64);
        // Nothing pinned: the whole bar slides.
        let layer = text_layer(14, 0, 6);
        assert_eq!(layer.slide_travel(W as f64, &style), W as f64);
    }

    #[test]
    fn non_looping_clamps_and_snaps_within_ends() {
        let style = Style::default();
        let mut layer = text_layer_mode(14, 1, 6, false);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let max = geo.max_offset;
        assert!((max - 7.0 * geo.pitch).abs() < 1e-9); // 13 band slots - 6 visible
                                                       // Offsets clamp at the ends instead of wrapping.
        assert!(layer.normalize_offset(&geo, -50.0).abs() < 1e-9);
        assert!((layer.normalize_offset(&geo, max + 50.0) - max).abs() < 1e-9);
        // Snap never rests past the last button (or before the first).
        assert!((layer.snap_target(&geo, max - 0.1 * geo.pitch) - max).abs() < 1e-9);
        assert!((layer.snap_target(&geo, max + 5.0 * geo.pitch) - max).abs() < 1e-9);
        assert!(layer.snap_target(&geo, 0.2 * geo.pitch).abs() < 1e-9);
        // The last button is reachable at max offset, and slot 0 holds
        // button 1 at offset 0 (no wrapped content from the far end).
        let y = (H / 2) as f64;
        layer.scroll_offset = max;
        let x = geo.region_left + geo.region_width - 5.0;
        assert_eq!(layer.hit(&style, W, H, x, y, None), Some(13));
        layer.scroll_offset = 0.0;
        assert_eq!(
            layer.hit(&style, W, H, geo.region_left + 5.0, y, None),
            Some(1)
        );
    }

    #[test]
    fn rubber_band_compresses_and_inverts() {
        let style = Style::default();
        let layer = text_layer_mode(14, 1, 6, false);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let max = geo.max_offset;
        // In range both maps are the identity.
        assert!((geo.rubber_display(0.5 * max) - 0.5 * max).abs() < 1e-9);
        assert!((geo.rubber_raw(0.5 * max) - 0.5 * max).abs() < 1e-9);
        // Overshoot compresses monotonically and stays under the cap.
        let d1 = geo.rubber_display(-100.0);
        let d2 = geo.rubber_display(-300.0);
        assert!(d1 < 0.0 && d2 < d1);
        assert!(-d2 < RUBBER_BAND_RANGE);
        let d3 = geo.rubber_display(max + 200.0);
        assert!(d3 > max && d3 - max < RUBBER_BAND_RANGE);
        // raw -> displayed -> raw round-trips, so drags retrace their stretch.
        for raw in [-400.0, -10.0, 3.0, max + 50.0] {
            assert!((geo.rubber_raw(geo.rubber_display(raw)) - raw).abs() < 1e-6);
        }
    }

    #[test]
    fn overscrolled_band_hits_nothing_left_of_first_button() {
        let style = Style::default();
        let mut layer = text_layer_mode(14, 1, 6, false);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let y = (H / 2) as f64;
        // Rubber-banded past the start the band sits shifted right; the
        // exposed gap at the region's left edge must not read as a button.
        layer.scroll_offset = -40.0;
        assert_eq!(
            layer.hit(&style, W, H, geo.region_left + 5.0, y, None),
            None
        );
        assert_eq!(
            layer.hit(&style, W, H, geo.region_left + 45.0, y, None),
            Some(1)
        );
    }

    #[test]
    fn looping_layer_wraps_offsets() {
        let style = Style::default();
        let layer = text_layer(14, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let wrapped = layer.normalize_offset(&geo, -geo.pitch);
        assert!((wrapped - (geo.period - geo.pitch)).abs() < 1e-9);
    }

    #[test]
    fn snap_targets_nearest_slot_boundary() {
        let style = Style::default();
        let layer = text_layer(14, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        assert!(layer.snap_target(&geo, 0.0).abs() < 1e-9);
        assert!(layer.snap_target(&geo, 0.4 * geo.pitch).abs() < 1e-9);
        assert!((layer.snap_target(&geo, 2.6 * geo.pitch) - 3.0 * geo.pitch).abs() < 1e-9);
        // Slightly negative offsets (mid-glide near the wrap) snap back to 0.
        assert!(layer.snap_target(&geo, -0.3 * geo.pitch).abs() < 1e-9);
    }

    #[test]
    fn snap_avoids_cutting_stretched_buttons() {
        let style = Style::default();
        // Esc + 12 band buttons, one spanning two slots -> 13 band slots.
        // Band start slots: 0,1,2,3,4,5,6,8,... (the wide button covers 6-7).
        let mut stretches = vec![1usize; 13];
        stretches[7] = 2; // overall button 7 = band button 6, slots 6-7
        let layer = stretched_layer(&stretches, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        // Resting at slot 1 would put the window's right edge at slot 7,
        // slicing the wide button; the nearest clean position is slot 0.
        assert!(layer.snap_target(&geo, 0.9 * geo.pitch).abs() < 1e-9);
        // Slot 2 is fine (right edge at slot 8, a real boundary).
        assert!((layer.snap_target(&geo, 2.1 * geo.pitch) - 2.0 * geo.pitch).abs() < 1e-9);
        // Mid-button offsets can't be resting positions either: 6.4 pitch sits
        // inside the wide button, so it settles at its start (slot 6).
        assert!((layer.snap_target(&geo, 6.4 * geo.pitch) - 6.0 * geo.pitch).abs() < 1e-9);
    }

    #[test]
    fn hit_pinned_band_gap_and_wrap() {
        let style = Style::default();
        let mut layer = text_layer(14, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let (region_left, slot_width, pitch) =
            (geo.region_left, geo.slot_width, geo.slot_width + 16.0);
        let y = (H / 2) as f64;

        // The pinned Esc is always hit at the left edge, at any scroll offset.
        assert_eq!(layer.hit(&style, W, H, 10.0, y, None), Some(0));
        layer.scroll_offset = 1234.5;
        assert_eq!(layer.hit(&style, W, H, 10.0, y, None), Some(0));
        layer.scroll_offset = 0.0;

        // At offset 0 the first band slot holds button 1.
        assert_eq!(layer.hit(&style, W, H, region_left + 5.0, y, None), Some(1));
        // The gap between band slots hits nothing.
        assert_eq!(
            layer.hit(&style, W, H, region_left + slot_width + 8.0, y, None),
            None
        );
        // Scrolling forward one slot brings button 2 under the same spot.
        layer.scroll_offset = pitch;
        assert_eq!(layer.hit(&style, W, H, region_left + 5.0, y, None), Some(2));
        // Scrolling backwards wraps around to the last button (the band loops).
        layer.scroll_offset = -pitch;
        assert_eq!(
            layer.hit(&style, W, H, region_left + 5.0, y, None),
            Some(13)
        );
        // Outside the vertical touch band nothing is hit.
        layer.scroll_offset = 0.0;
        assert_eq!(layer.hit(&style, W, H, region_left + 5.0, 1.0, None), None);
    }

    #[test]
    fn hit_motion_tracking_matches_target_button() {
        let style = Style::default();
        let layer = text_layer(14, 1, 6);
        let geo = layer.scroll_geometry(W as f64, &style).unwrap();
        let y = (H / 2) as f64;
        let x = geo.region_left + 5.0;
        // Tracking button 1: still on it -> hit; tracking button 2 -> not.
        assert_eq!(layer.hit(&style, W, H, x, y, Some(1)), Some(1));
        assert_eq!(layer.hit(&style, W, H, x, y, Some(2)), None);
    }

    #[test]
    fn non_scrollable_hit_unchanged() {
        let style = Style::default();
        let layer = text_layer(13, 1, 0);
        let y = (H / 2) as f64;
        let slot = W as f64 / 13.0;
        assert_eq!(layer.hit(&style, W, H, 10.0, y, None), Some(0));
        assert_eq!(layer.hit(&style, W, H, slot * 5.5, y, None), Some(5));
        assert_eq!(layer.hit(&style, W, H, slot * 5.5, 1.0, None), None);
    }
}
