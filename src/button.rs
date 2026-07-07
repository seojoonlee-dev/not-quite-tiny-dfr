//! The `Button` widget: its `ButtonImage` variants (text, icon, time, battery,
//! CPU/GPU, slider, media, command), their per-frame content rendering, the
//! tap-to-expand state, and image loading. `FunctionLayer` (in main) lays these
//! out and drives their draw; the media panel itself lives in `render`.

use crate::config::{ButtonAction, ButtonConfig};
use crate::render::{capsule, show_layout_centered, text_layout, MediaState};
use crate::sensors::{
    find_battery_device, BatteryState, BATTERY_STATE, CPU_POWER_STATE, CPU_TEMP_STATE, GPU_LABEL,
    GPU_POWER_STATE, GPU_TEMP_STATE,
};
use crate::style::{self, Color};
use crate::uinput::toggle_keys;
use crate::{
    ease_expand, BATTERY_ICON_TEXT_GAP, DEFAULT_ICON_SIZE, SLIDER_ANIM, SLIDER_EDGE_PAD,
    SLIDER_KNOB_RADIUS, SLIDER_LOW_THRESHOLD, SLIDER_PAD, SLIDER_TRACK_HEIGHT, USER_ICON_DIR,
};
use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface};
use chrono::{
    format::{Item as ChronoItem, StrftimeItems},
    Local, Locale,
};
use freedesktop_icons::lookup;
use input_linux::uinput::UInputHandle;
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum CpuTempUnit {
    Celsius,
    Fahrenheit,
}

/// What a `Cpu` widget shows: an optional temperature (in some unit) and/or the
/// package power draw, with an optional "CPU" label prefix. Selected by the
/// `Cpu` config value (e.g. "celsius watts") and `CpuLabel`.
#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) struct CpuDisplay {
    temp: Option<CpuTempUnit>,
    watts: bool,
    label: bool,
}

/// What a `Gpu` widget shows: an optional temperature (in some unit) and/or the
/// power draw, with an optional vendor label prefix ("AMD"/"NVIDIA"/...).
/// Selected by the `Gpu` config value (e.g. "celsius watts") and `GpuLabel`.
/// Reuses [`CpuTempUnit`] for the unit -- celsius/fahrenheit are the same here.
#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) struct GpuDisplay {
    temp: Option<CpuTempUnit>,
    watts: bool,
    label: bool,
}

/// An SVG icon together with a cache of its rasterization. librsvg's
/// `render_document` re-parses and re-rasterizes the SVG on every call, which is
/// the dominant per-frame cost of a full-bar redraw while scrolling or rubber-
/// banding (each frame re-rasterizes every visible icon). Icons never change
/// during the daemon's lifetime, so each is rasterized once per size and the
/// bitmap is blitted thereafter.
pub(crate) struct CachedSvg {
    handle: Handle,
    raster: std::cell::RefCell<Option<(i32, i32, ImageSurface)>>,
}

impl CachedSvg {
    pub(crate) fn new(handle: Handle) -> CachedSvg {
        CachedSvg {
            handle,
            raster: std::cell::RefCell::new(None),
        }
    }

    /// Blit the icon at `(x, y)` sized `w`x`h`, rasterizing (and caching) it the
    /// first time each size is requested. Positions are already rounded by
    /// callers, so blits stay crisp.
    pub(crate) fn render(&self, c: &Context, x: f64, y: f64, w: f64, h: f64) {
        let (kw, kh) = (w.round() as i32, h.round() as i32);
        if kw <= 0 || kh <= 0 {
            return;
        }
        let mut raster = self.raster.borrow_mut();
        if !matches!(raster.as_ref(), Some((cw, ch, _)) if *cw == kw && *ch == kh) {
            let surf = ImageSurface::create(Format::ARgb32, kw, kh).unwrap();
            {
                let cc = Context::new(&surf).unwrap();
                self.handle
                    .render_document(&cc, &Rectangle::new(0.0, 0.0, kw as f64, kh as f64))
                    .unwrap();
            }
            *raster = Some((kw, kh, surf));
        }
        let surf = &raster.as_ref().unwrap().2;
        // save/restore so the blit does not leave the icon bitmap as the cairo
        // source: callers (e.g. the battery widget) draw text right after the
        // icon relying on the text color still being set.
        c.save().unwrap();
        c.set_source_surface(surf, x, y).unwrap();
        c.rectangle(x, y, kw as f64, kh as f64);
        c.fill().unwrap();
        c.restore().unwrap();
    }
}

pub(crate) struct BatteryImages {
    plain: Vec<CachedSvg>,
    charging: Vec<CachedSvg>,
    bolt: CachedSvg,
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub(crate) enum BatteryIconMode {
    Percentage,
    Icon,
    Both,
}

impl BatteryIconMode {
    pub(crate) fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    pub(crate) fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

/// The tap-to-expand mechanic shared by the Slider widget and any OnClick =
/// Expand button (e.g. a command widget). Collapsed the button spans
/// `base_stretch` slots; a tap expands it to `expanded_stretch` slots until it
/// idles (see SLIDER_COLLAPSE), animating the width with `ease_expand`.
pub(crate) struct ExpandState {
    pub(crate) expanded: bool,
    /// Slots occupied collapsed (the config's Stretch) and expanded.
    pub(crate) base_stretch: usize,
    pub(crate) expanded_stretch: usize,
    /// Last expand/drag/tap, for the auto-collapse timer.
    pub(crate) last_interaction: Instant,
    /// Start of the width animation toward the current `expanded` state;
    /// `None` once settled. The layout switches instantly (hit testing uses
    /// the target); this only drives the drawn width's transition.
    pub(crate) anim: Option<Instant>,
}

impl ExpandState {
    pub(crate) fn new(base_stretch: usize, expanded_stretch: usize) -> ExpandState {
        ExpandState {
            expanded: false,
            base_stretch,
            expanded_stretch: expanded_stretch.max(base_stretch),
            last_interaction: Instant::now(),
            anim: None,
        }
    }

    /// Eased expand progress: 0 fully collapsed, 1 fully expanded. Shares the
    /// clock and curve of `expand_anim` so content glides in step with the
    /// width, instead of teleporting when the animation ends.
    pub(crate) fn expand_progress(&self) -> f64 {
        let settled = if self.expanded { 1.0 } else { 0.0 };
        let Some(t0) = self.anim else { return settled };
        let t = t0.elapsed().as_secs_f64() / SLIDER_ANIM.as_secs_f64();
        if t >= 1.0 {
            return settled;
        }
        let eased = ease_expand(t);
        if self.expanded {
            eased
        } else {
            1.0 - eased
        }
    }

    /// Slots the drawn width currently lags behind the laid-out width, or
    /// `None` once the animation has settled. Positive while expanding (drawn
    /// narrower than the layout), negative while collapsing.
    pub(crate) fn anim_slots(&self) -> Option<f64> {
        let t0 = self.anim?;
        let t = t0.elapsed().as_secs_f64() / SLIDER_ANIM.as_secs_f64();
        if t >= 1.0 {
            return None; // settled; the main loop clears `anim`
        }
        let delta = (self.expanded_stretch - self.base_stretch) as f64;
        let remaining = delta * (1.0 - ease_expand(t));
        Some(if self.expanded { remaining } else { -remaining })
    }

    /// Slots the button occupies right now, per its target (not drawn) state.
    pub(crate) fn current_stretch(&self) -> usize {
        if self.expanded {
            self.expanded_stretch
        } else {
            self.base_stretch
        }
    }

    /// Toggle the target state, starting the width animation. Returns whether
    /// anything changed (the caller then relayouts and forces a redraw).
    pub(crate) fn set_expanded(&mut self, expanded: bool) -> bool {
        if self.expanded == expanded {
            return false;
        }
        self.expanded = expanded;
        self.last_interaction = Instant::now();
        self.anim = Some(Instant::now());
        true
    }
}

/// State of an interactive slider button. Collapsed it is an icon; tapping it
/// expands it into a draggable track until it idles (see SLIDER_COLLAPSE).
pub(crate) struct SliderState {
    /// Id shared with the widget runtime: the get command's poll results
    /// arrive under it, and set commands are queued against it.
    pub(crate) id: usize,
    pub(crate) icon: Option<CachedSvg>,
    /// Icon shown in place of `icon` while `muted`; falls back to `icon`.
    pub(crate) muted_icon: Option<CachedSvg>,
    /// Icon shown in place of `icon` while the value is below
    /// `SLIDER_LOW_THRESHOLD`; falls back to `icon`.
    pub(crate) low_icon: Option<CachedSvg>,
    /// Current value, 0-100.
    pub(crate) value: i32,
    /// Whether the backing control reports itself muted (swaps in the mute
    /// icon).
    pub(crate) muted: bool,
    /// Whether a SliderMute command is configured (enables the drag-unmute and
    /// auto-mute-at-0 behaviors).
    pub(crate) has_mute: bool,
    /// The tap-to-expand track state.
    pub(crate) expand: ExpandState,
}

/// The expanded view of an OnClick = Expand command widget: a second script
/// (its own widget `id`) whose latest output is shown -- with the same
/// icon+text layout as the collapsed reading -- while the button is expanded.
/// `state` drives the shared tap-to-expand width animation.
pub(crate) struct CommandExpand {
    pub(crate) id: usize,
    pub(crate) text: String,
    pub(crate) color: Option<Color>,
    pub(crate) icon_name: Option<String>,
    pub(crate) icon: Option<CachedSvg>,
    pub(crate) state: ExpandState,
}

pub(crate) enum ButtonImage {
    Text(String),
    Svg(CachedSvg),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(BatteryIconMode, BatteryImages),
    Cpu(CpuDisplay),
    Gpu(GpuDisplay),
    Slider(SliderState),
    Media(MediaState),
    /// A command widget: `text`/`color`/`icon` are updated from its script's
    /// output. `icon_name` is the last icon the script asked for; `icon` is the
    /// SVG resolved from it (via `theme`), reloaded only when the name changes.
    /// `expand` is set for an OnClick = Expand widget: tapping expands the
    /// button and shows its expand script's output (see [`CommandExpand`]).
    Command {
        id: usize,
        text: String,
        color: Option<Color>,
        theme: Option<String>,
        icon_name: Option<String>,
        icon: Option<CachedSvg>,
        expand: Option<CommandExpand>,
    },
    Spacer,
}

pub(crate) struct Button {
    pub(crate) image: ButtonImage,
    pub(crate) changed: bool,
    pub(crate) active: bool,
    pub(crate) action: Vec<ButtonAction>,
    pub(crate) icon_width: f64,
    pub(crate) icon_height: f64,
    // Per-button style overrides; fall back to the global Style when None.
    pub(crate) bg_color: Option<Color>,
    pub(crate) bg_color_active: Option<Color>,
    pub(crate) text_color: Option<Color>,
    /// Whether tapping shows the pressed (active) fill. Only buttons that
    /// declare an `OnClick` light up on tap; others stay flat.
    pub(crate) highlight_on_tap: bool,
}

/// The track region of a slider button, given the button's on-screen rect:
/// everything right of the icon cap, inset by the slider padding.
pub(crate) fn slider_track_rect(button: &Button, left: f64, width: f64) -> (f64, f64) {
    let cap = match &button.image {
        ButtonImage::Slider(s) if s.icon.is_some() => {
            SLIDER_EDGE_PAD + SLIDER_PAD + button.icon_width + SLIDER_PAD
        }
        _ => SLIDER_EDGE_PAD + SLIDER_PAD,
    };
    (left + cap, width - cap - SLIDER_PAD - SLIDER_EDGE_PAD)
}

pub(crate) fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(CachedSvg::new(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    )))
}

pub(crate) fn try_load_png(path: impl AsRef<Path>, icon_width: i32, icon_height: i32) -> Result<ButtonImage> {
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

pub(crate) fn try_load_image(
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

/// The CPU widget's label for the current cached readings.
pub(crate) fn cpu_text(d: &CpuDisplay) -> String {
    let mut parts: Vec<String> = Vec::new();
    if d.label {
        parts.push("CPU".to_string());
    }
    if let Some(unit) = d.temp {
        parts.push(match *CPU_TEMP_STATE.lock().unwrap() {
            Some(c) => match unit {
                CpuTempUnit::Celsius => format!("{c}\u{00b0}C"),
                CpuTempUnit::Fahrenheit => format!("{}\u{00b0}F", c * 9 / 5 + 32),
            },
            None => "n/a".to_string(),
        });
    }
    if d.watts {
        parts.push(match *CPU_POWER_STATE.lock().unwrap() {
            Some(w) => format!("{w}W"),
            None => "n/a".to_string(),
        });
    }
    // A spec that selects nothing and hides the label still shows a bare "CPU".
    if parts.is_empty() {
        return "CPU".to_string();
    }
    parts.join(" ")
}

/// The GPU widget's label for the current cached readings. Mirrors `cpu_text`,
/// but the label is the detected vendor name rather than a fixed "CPU".
pub(crate) fn gpu_text(d: &GpuDisplay) -> String {
    let label = *GPU_LABEL.lock().unwrap();
    let mut parts: Vec<String> = Vec::new();
    if d.label {
        parts.push(label.to_string());
    }
    if let Some(unit) = d.temp {
        parts.push(match *GPU_TEMP_STATE.lock().unwrap() {
            Some(c) => match unit {
                CpuTempUnit::Celsius => format!("{c}\u{00b0}C"),
                CpuTempUnit::Fahrenheit => format!("{}\u{00b0}F", c * 9 / 5 + 32),
            },
            None => "n/a".to_string(),
        });
    }
    if d.watts {
        parts.push(match *GPU_POWER_STATE.lock().unwrap() {
            Some(w) => format!("{w}W"),
            None => "n/a".to_string(),
        });
    }
    // A spec that selects nothing and hides the label still shows a bare label.
    if parts.is_empty() {
        return label.to_string();
    }
    parts.join(" ")
}

impl Button {
    pub(crate) fn with_config(cfg: ButtonConfig, default_icon_size: i32) -> Button {
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
        } else if let Some(spec) = cfg.cpu {
            Button::new_cpu(cfg.action, &spec, cfg.cpu_label.unwrap_or(true))
        } else if let Some(spec) = cfg.gpu {
            Button::new_gpu(cfg.action, &spec, cfg.gpu_label.unwrap_or(true))
        } else {
            Button::new_spacer()
        };
        button.bg_color = bg_color;
        button.bg_color_active = bg_color_active;
        button.text_color = text_color;
        button
    }
    pub(crate) fn new_spacer() -> Button {
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
            highlight_on_tap: false,
        }
    }
    pub(crate) fn new_text(text: String, action: Vec<ButtonAction>) -> Button {
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
            highlight_on_tap: false,
        }
    }
    pub(crate) fn new_command(
        id: usize,
        action: Vec<ButtonAction>,
        theme: Option<String>,
        expand: Option<CommandExpand>,
    ) -> Button {
        Button {
            action,
            active: false,
            changed: true, // draw the placeholder until the first result arrives
            image: ButtonImage::Command {
                id,
                text: "\u{2026}".to_string(),
                color: None,
                theme,
                icon_name: None,
                icon: None,
                expand,
            },
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
            highlight_on_tap: false,
        }
    }
    pub(crate) fn new_icon(
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
            highlight_on_tap: false,
        }
    }
    pub(crate) fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> CachedSvg {
        if let ButtonImage::Svg(svg) =
            try_load_image(icon, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap()
        {
            return svg;
        }
        panic!("failed to load icon");
    }
    pub(crate) fn new_battery(
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
            highlight_on_tap: false,
        }
    }

    pub(crate) fn new_slider(
        id: usize,
        icon: Option<&str>,
        muted_icon: Option<&str>,
        low_icon: Option<&str>,
        theme: Option<impl AsRef<str>>,
        base_stretch: usize,
        expanded_stretch: usize,
        icon_size: i32,
        has_mute: bool,
    ) -> Button {
        let icon = icon.map(|i| Self::load_battery_image(i, theme.as_ref()));
        let muted_icon = muted_icon.map(|i| Self::load_battery_image(i, theme.as_ref()));
        let low_icon = low_icon.map(|i| Self::load_battery_image(i, theme.as_ref()));
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Slider(SliderState {
                id,
                icon,
                muted_icon,
                low_icon,
                value: 0,
                muted: false,
                has_mute,
                expand: ExpandState::new(base_stretch, expanded_stretch),
            }),
            icon_width: icon_size as f64,
            icon_height: icon_size as f64,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
            highlight_on_tap: false,
        }
    }

    pub(crate) fn new_media(theme: Option<impl AsRef<str>>, icon_size: i32) -> Button {
        let load = |name| Some(Self::load_battery_image(name, theme.as_ref()));
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Media(MediaState {
                prev_icon: load("fast_rewind"),
                play_icon: load("play_arrow"),
                pause_icon: load("pause"),
                next_icon: load("fast_forward"),
                pressed: None,
                show_lyrics: true,
                lyric_gap: false,
                lyrics_track: String::new(),
                view_anim: None,
                last_interaction: Instant::now(),
                lyric_idx: usize::MAX,
                prev_lyric: String::new(),
                lyric_anim: None,
            }),
            icon_width: icon_size as f64,
            icon_height: icon_size as f64,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
            highlight_on_tap: false,
        }
    }

    pub(crate) fn new_cpu(action: Vec<ButtonAction>, spec: &str, label: bool) -> Button {
        // A space-separated list of what to show. Unknown tokens are only worth
        // a journal line, not a daemon abort (this also runs on live reloads).
        let mut temp = None;
        let mut watts = false;
        let mut any = false;
        for tok in spec.split_whitespace() {
            match tok {
                "celsius" => (temp, any) = (Some(CpuTempUnit::Celsius), true),
                "fahrenheit" => (temp, any) = (Some(CpuTempUnit::Fahrenheit), true),
                "watts" | "power" => (watts, any) = (true, true),
                other => {
                    eprintln!("not-quite-tiny-dfr: unknown Cpu component {other:?}, ignoring");
                }
            }
        }
        // An empty or all-unknown spec falls back to temperature in celsius.
        if !any {
            temp = Some(CpuTempUnit::Celsius);
        }
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Cpu(CpuDisplay { temp, watts, label }),
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
            highlight_on_tap: false,
        }
    }

    pub(crate) fn new_gpu(action: Vec<ButtonAction>, spec: &str, label: bool) -> Button {
        // Same space-separated component list as the Cpu widget; unknown tokens
        // are a journal line, not a daemon abort (this runs on live reloads too).
        let mut temp = None;
        let mut watts = false;
        let mut any = false;
        for tok in spec.split_whitespace() {
            match tok {
                "celsius" => (temp, any) = (Some(CpuTempUnit::Celsius), true),
                "fahrenheit" => (temp, any) = (Some(CpuTempUnit::Fahrenheit), true),
                "watts" | "power" => (watts, any) = (true, true),
                other => {
                    eprintln!("not-quite-tiny-dfr: unknown Gpu component {other:?}, ignoring");
                }
            }
        }
        // An empty or all-unknown spec falls back to temperature in celsius.
        if !any {
            temp = Some(CpuTempUnit::Celsius);
        }
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Gpu(GpuDisplay { temp, watts, label }),
            icon_width: 0.0,
            icon_height: 0.0,
            bg_color: None,
            bg_color_active: None,
            text_color: None,
            highlight_on_tap: false,
        }
    }

    pub(crate) fn new_time(action: Vec<ButtonAction>, format: &str, locale_str: Option<&str>) -> Button {
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
            highlight_on_tap: false,
        }
    }
    pub(crate) fn needs_faster_refresh(&self) -> bool {
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
    pub(crate) fn render(
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

                svg.render(c, x, y, self.icon_width, self.icon_height);
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

                    svg.render(c, x, y, DEFAULT_ICON_SIZE as f64, DEFAULT_ICON_SIZE as f64);
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
            ButtonImage::Cpu(d) => {
                let layout = text_layout(c, style, &cpu_text(d));
                show_layout_centered(c, &layout, height, button_left_edge, button_width, y_shift);
            }
            ButtonImage::Gpu(d) => {
                let layout = text_layout(c, style, &gpu_text(d));
                show_layout_centered(c, &layout, height, button_left_edge, button_width, y_shift);
            }
            ButtonImage::Slider(s) => {
                let color = self.text_color.unwrap_or(style.text_color);
                // The icon reflects the control's state: the mute symbol while
                // muted, the low symbol below the threshold, else the default.
                // Each falls back to the default icon when not configured.
                let icon = if s.muted {
                    s.muted_icon.as_ref().or(s.icon.as_ref())
                } else if s.value < SLIDER_LOW_THRESHOLD {
                    s.low_icon.as_ref().or(s.icon.as_ref())
                } else {
                    s.icon.as_ref()
                };
                // Icon x glides between centered (collapsed) and the left cap
                // (expanded) with the expand progress, so it tracks the width
                // instead of snapping when the animation ends.
                let p = s.expand.expand_progress();
                let centered = button_left_edge + (button_width as f64 - self.icon_width) / 2.0;
                let capped = button_left_edge + SLIDER_EDGE_PAD + SLIDER_PAD;
                let icon_x = (centered + (capped - centered) * p).round();
                let icon_y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();
                let draw_icon = |c: &Context| {
                    if let Some(svg) = icon {
                        svg.render(c, icon_x, icon_y, self.icon_width, self.icon_height);
                    }
                };
                // The expanded look also plays through the collapse animation,
                // so the track shrinks shut instead of vanishing.
                if !s.expand.expanded && s.expand.anim.is_none() {
                    // Collapsed: just the icon (a plain button look).
                    draw_icon(c);
                    return;
                }
                // Expanded: icon cap on the left, then the track with its
                // fill and handle at the current value.
                draw_icon(c);
                let (track_left, track_width) =
                    slider_track_rect(self, button_left_edge, button_width as f64);
                if track_width <= 0.0 {
                    return;
                }
                let track_y = y_shift + ((height as f64 - SLIDER_TRACK_HEIGHT) / 2.0).round();
                // The track (but not the icon) fades out as the slider closes,
                // so the bar dissolves instead of just shrinking; opening stays
                // fully opaque.
                let track_alpha = if s.expand.expanded { 1.0 } else { p };
                Color {
                    a: color.a * 0.25 * track_alpha,
                    ..color
                }
                .set_source(c);
                capsule(c, track_left, track_y, track_width, SLIDER_TRACK_HEIGHT);
                Color {
                    a: color.a * track_alpha,
                    ..color
                }
                .set_source(c);
                if track_width <= 2.0 * SLIDER_KNOB_RADIUS {
                    return; // too narrow (mid-animation) for a handle yet
                }
                // Handle centered on the value, kept inside the track; the
                // fill runs up to it.
                let cx = (track_left + track_width * s.value as f64 / 100.0).clamp(
                    track_left + SLIDER_KNOB_RADIUS,
                    track_left + track_width - SLIDER_KNOB_RADIUS,
                );
                let fill = cx - track_left;
                if fill > 0.0 {
                    capsule(c, track_left, track_y, fill, SLIDER_TRACK_HEIGHT);
                }
                c.arc(
                    cx,
                    y_shift + height as f64 / 2.0,
                    SLIDER_KNOB_RADIUS,
                    0.0,
                    (360.0f64).to_radians(),
                );
                c.fill().unwrap();
            }
            ButtonImage::Command {
                text, icon, expand, ..
            } => {
                // Draw one content group -- icon + text centered together,
                // mirroring the built-in battery "both" layout.
                let draw_content = |text: &str, icon: &Option<CachedSvg>| {
                    let layout = text_layout(c, style, text);
                    match icon {
                        Some(svg) => {
                            let (text_width, text_height) = layout.pixel_size();
                            let icon_sz = DEFAULT_ICON_SIZE as f64;
                            let gap = if text.is_empty() { 0.0 } else { BATTERY_ICON_TEXT_GAP };
                            let group = icon_sz + gap + text_width as f64;
                            let x = button_left_edge
                                + (button_width as f64 / 2.0 - group / 2.0).round();
                            let y = y_shift + ((height as f64 - icon_sz) / 2.0).round();
                            svg.render(c, x, y, icon_sz, icon_sz);
                            if !text.is_empty() {
                                c.move_to(
                                    x + icon_sz + gap,
                                    y_shift + ((height as f64 - text_height as f64) / 2.0).round(),
                                );
                                pangocairo::functions::show_layout(c, &layout);
                            }
                        }
                        None => show_layout_centered(
                            c,
                            &layout,
                            height,
                            button_left_edge,
                            button_width,
                            y_shift,
                        ),
                    }
                };
                match expand {
                    // Mid expand/collapse: crossfade the collapsed reading and
                    // the expand view into each other. Each is drawn to its own
                    // group and painted with complementary alpha, so the text
                    // and icon dissolve from one to the other as the width
                    // animates. `p` runs 0 (collapsed) .. 1 (expanded); re-set
                    // the text color each time since pop_group clobbers it.
                    Some(e) if e.state.anim.is_some() => {
                        let p = e.state.expand_progress();
                        let color = self.effective_text_color(style);
                        color.set_source(c);
                        c.push_group();
                        draw_content(text, icon);
                        c.pop_group_to_source().unwrap();
                        c.paint_with_alpha(1.0 - p).unwrap();
                        color.set_source(c);
                        c.push_group();
                        draw_content(&e.text, &e.icon);
                        c.pop_group_to_source().unwrap();
                        c.paint_with_alpha(p).unwrap();
                    }
                    // Settled open: just the expand view.
                    Some(e) if e.state.expanded => draw_content(&e.text, &e.icon),
                    // Collapsed, or a plain command widget: the normal reading.
                    _ => draw_content(text, icon),
                }
            }
            // The media widget is painted in full by paint_media (it needs the
            // rounded-rect geometry), so it never reaches this content pass.
            ButtonImage::Media(_) => (),
            ButtonImage::Spacer => (),
        }
    }
    /// The tap-to-expand state shared by Slider and OnClick = Expand command
    /// widgets, or `None` for buttons that don't expand.
    pub(crate) fn expand_state(&self) -> Option<&ExpandState> {
        match &self.image {
            ButtonImage::Slider(s) => Some(&s.expand),
            ButtonImage::Command {
                expand: Some(e), ..
            } => Some(&e.state),
            _ => None,
        }
    }
    pub(crate) fn expand_state_mut(&mut self) -> Option<&mut ExpandState> {
        match &mut self.image {
            ButtonImage::Slider(s) => Some(&mut s.expand),
            ButtonImage::Command {
                expand: Some(e), ..
            } => Some(&mut e.state),
            _ => None,
        }
    }
    /// The color to draw this button's text in, letting a command widget's own
    /// JSON `color` override the configured/default text color. An expanded
    /// command widget uses its expand script's color instead.
    pub(crate) fn effective_text_color(&self, style: &style::Style) -> Color {
        if let ButtonImage::Command { color, expand, .. } = &self.image {
            let color = match expand {
                Some(e) if e.state.expanded || e.state.anim.is_some() => &e.color,
                _ => color,
            };
            if let Some(color) = color {
                return *color;
            }
        }
        self.text_color.unwrap_or(style.text_color)
    }
    pub(crate) fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
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
    pub(crate) fn set_visual_active(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.changed = true;
        }
    }
    /// Emit this button's key events without touching the visual state.
    pub(crate) fn emit_keys<F>(&self, uinput: &mut UInputHandle<F>, pressed: bool)
    where
        F: AsRawFd,
    {
        toggle_keys(uinput, &self.action, pressed as i32);
    }
    /// Resolve the fill color for this button's rounded rectangle, or `None`
    /// if it should not be drawn (outlines disabled and button is inactive).
    /// Battery buttons signal charge state via color and are always drawn.
    pub(crate) fn fill_color(&self, style: &style::Style, show_outlines: bool) -> Option<Color> {
        if let ButtonImage::Battery(_, _) = &self.image {
            let (_, state) = *BATTERY_STATE.lock().unwrap();
            match state {
                BatteryState::Charging => return Some(style.battery_charging_color),
                BatteryState::Low => return Some(style.battery_low_color),
                BatteryState::NotCharging => {}
            }
        }
        if self.active && self.highlight_on_tap {
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
