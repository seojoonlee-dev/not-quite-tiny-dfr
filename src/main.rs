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
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
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
        signal::{SigSet, Signal},
    },
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod config;
mod display;
mod fonts;
mod pixel_shift;
mod style;
mod user;
mod widget;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonConfig, Config};
use display::DrmBackend;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use style::Color;
use widget::{WidgetRuntime, WidgetSpec};

const DEFAULT_ICON_SIZE: i32 = 48;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
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
    Battery(String, BatteryIconMode, BatteryImages),
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
    action: Vec<Key>,
    icon_width: f64,
    icon_height: f64,
    // Per-button style overrides; fall back to the global Style when None.
    bg_color: Option<Color>,
    bg_color_active: Option<Color>,
    text_color: Option<Color>,
}

/// Copy the latest widget outputs into their buttons, marking changed ones for
/// redraw. Cheap enough to call every loop iteration (the results map is small).
fn apply_widget_results(layers: &mut [FunctionLayer; 2], rt: &WidgetRuntime) {
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
/// one is configured, otherwise the solid background color.
fn set_background_source(c: &Context, style: &style::Style) {
    if let Some(img) = &style.background_image {
        c.set_source_surface(img, 0.0, 0.0).unwrap();
    } else {
        style.background.set_source(c);
    }
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
        candidates.push(PathBuf::from(format!("/usr/share/not-quite-tiny-dfr/{name}.svg")));
        candidates.push(PathBuf::from(format!("/usr/share/not-quite-tiny-dfr/{name}.png")));
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
            if let Some(battery) = find_battery_device() {
                Button::new_battery(cfg.action, battery, battery_mode, cfg.theme)
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
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
    fn new_text(text: String, action: Vec<Key>) -> Button {
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
    fn new_command(id: usize, action: Vec<Key>) -> Button {
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
        action: Vec<Key>,
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
        action: Vec<Key>,
        battery: String,
        battery_mode: String,
        theme: Option<impl AsRef<str>>,
    ) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar", "battery_1_bar", "battery_2_bar", "battery_3_bar",
            "battery_4_bar", "battery_5_bar", "battery_6_bar", "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20", "battery_charging_30", "battery_charging_50",
            "battery_charging_60", "battery_charging_80",
            "battery_charging_90", "battery_charging_full",
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
                battery,
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

    fn new_time(action: Vec<Key>, format: &str, locale_str: Option<&str>) -> Button {
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
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
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
                let time_extents = c.text_extents(&formatted_time).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - time_extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + time_extents.height() / 2.0).round(),
                );
                c.show_text(&formatted_time).unwrap();
            }
            ButtonImage::Battery(battery, battery_mode, icons) => {
                let (capacity, state) = get_battery_state(battery);
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
                let extents = c.text_extents(&percent_str).unwrap();
                let mut width = extents.width();
                let mut text_offset = 0;
                if let Some(svg) = icon {
                    if !battery_mode.should_draw_text() {
                        width = DEFAULT_ICON_SIZE as f64;
                    } else {
                        width += DEFAULT_ICON_SIZE as f64;
                    }
                    text_offset = DEFAULT_ICON_SIZE;
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
                            + (button_width as f64 / 2.0 - width / 2.0 + text_offset as f64)
                                .round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&percent_str).unwrap();
                }
            }
            ButtonImage::Command { text, .. } => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::Spacer => (),
        }
    }
    /// The color to draw this button's text in, letting a command widget's own
    /// JSON `color` override the configured/default text color.
    fn effective_text_color(&self, style: &style::Style) -> Color {
        if let ButtonImage::Command {
            color: Some(color), ..
        } = &self.image
        {
            return *color;
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
    /// Resolve the fill color for this button's rounded rectangle, or `None`
    /// if it should not be drawn (outlines disabled and button is inactive).
    /// Battery buttons signal charge state via color and are always drawn.
    fn fill_color(&self, style: &style::Style, show_outlines: bool) -> Option<Color> {
        if let ButtonImage::Battery(battery, _, _) = &self.image {
            let (_, state) = get_battery_state(battery);
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

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    displays_battery: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
    faster_refresh: bool,
}

impl FunctionLayer {
    fn with_config(
        cfg: Vec<ButtonConfig>,
        widgets: &mut Vec<WidgetSpec>,
        next_id: &mut usize,
        default_icon_size: i32,
    ) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0;
        let displays_time = cfg.iter().any(|cfg| cfg.time.is_some());
        let displays_battery = cfg.iter().any(|cfg| cfg.battery.is_some());
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
        FunctionLayer {
            displays_time,
            displays_battery,
            buttons,
            virtual_button_count,
            faster_refresh,
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
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let style = &config.style;
        let spacing = style.button_spacing;
        let edge = style.edge_padding;
        let virtual_button_width = ((width - pixel_shift_width as i32) as f64
            - 2.0 * edge
            - spacing * (self.virtual_button_count - 1) as f64)
            / self.virtual_button_count as f64;
        let margin = (1.0 - style.height_percent / 100.0) / 2.0;
        let bot = (height as f64) * margin;
        let top = (height as f64) * (1.0 - margin);
        // Cap the radius at half the button height, otherwise the rounded-corner
        // arcs overlap into a degenerate shape that stops responding to changes.
        let radius = style.corner_radius.clamp(0.0, (top - bot) / 2.0);
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            set_background_source(&c, style);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(style.font_size);

        for i in 0..self.buttons.len() {
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

            let left_edge = (start as f64 * (virtual_button_width + spacing))
                .floor()
                + edge
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + spacing)).floor();
            // Also cap against the button width so narrow buttons stay valid.
            let radius = radius.min(button_width / 2.0);

            if !complete_redraw {
                set_background_source(&c, style);
                c.rectangle(
                    left_edge,
                    bot - radius,
                    button_width,
                    top - bot + radius * 2.0,
                );
                c.fill().unwrap();
            }
            let fill = if matches!(button.image, ButtonImage::Spacer) {
                None
            } else {
                button.fill_color(style, config.show_button_outlines)
            };
            if let Some(fill) = fill {
                fill.set_source(&c);
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
            button.effective_text_color(style).set_source(&c);
            button.render(
                &c,
                height,
                left_edge,
                button_width.ceil() as u64,
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

        modified_regions
    }

    fn hit(&self, spacing: f64, edge: f64, width: u16, height: u16, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let usable = width as f64 - 2.0 * edge;
        let virtual_button_width =
            (usable - spacing * (self.virtual_button_count - 1) as f64) / self.virtual_button_count as f64;

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

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
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

fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<Key>, value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for kc in codes {
        emit(uinput, EventKind::Key, *kc as u16, value);
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

fn main() {
    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm)));
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let mut map = drm.map().unwrap();
    let data = map.as_mut();
    let mut wptr = 0;
    for byte in crash_bitmap {
        for i in 0..8 {
            let bit = ((byte >> i) & 0x1) == 0;
            let color = if bit { 0xFF } else { 0x0 };
            data[wptr] = color;
            data[wptr + 1] = color;
            data[wptr + 2] = color;
            data[wptr + 3] = color;
            wptr += 4;
        }
    }
    drop(map);
    drm.dirty(&[ClipRect::new(0, 0, height, width)]).unwrap();
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().unwrap();
}

fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
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
        println!("not-quite-tiny-dfr: serving user {:?}, config dir {}", u.name, dir.display());
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
    let mut widget_rt = WidgetRuntime::new(
        if privileges_dropped { initial_widgets } else { Vec::new() },
        wake_write.clone(),
    );
    let mut last_user_poll = Instant::now();

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut active_layer = 0;
    let mut needs_complete_redraw = true;

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
                println!("not-quite-tiny-dfr: {:?} logged in, loading config dir {}", u.name, dir.display());
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

        let current_ts = if layers[active_layer].faster_refresh {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if layers[active_layer].displays_time && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }
        if layers[active_layer].displays_battery {
            for button in &mut layers[active_layer].buttons {
                if let ButtonImage::Battery(_, _, _) = button.1.image {
                    button.1.changed = true;
                }
            }
        }

        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let clips = layers[active_layer].draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                needs_complete_redraw,
            );
            let data = surface.data().unwrap();
            drm.map().unwrap().as_mut()[..data.len()].copy_from_slice(&data);
            drm.dirty(&clips).unwrap();
            needs_complete_redraw = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        _ = udev_monitor.iter().last();

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
                    if key.key() == Key::Fn as u32 {
                        if cfg.double_press_switch_layers > 0 && key.key_state() == KeyState::Pressed {
                            if last.elapsed() < Duration::from_millis(cfg.double_press_switch_layers.into()) {
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
                            if let Some(btn) = layers[active_layer].hit(cfg.style.button_spacing, cfg.style.edge_padding, width, height, x, y, None) {
                                touches.insert(dn.seat_slot(), (active_layer, btn));
                                layers[active_layer].buttons[btn]
                                    .1
                                    .set_active(&mut uinput, true);
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            if !touches.contains_key(&mtn.seat_slot()) {
                                continue;
                            }

                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            let (layer, btn) = *touches.get(&mtn.seat_slot()).unwrap();
                            let hit = layers[active_layer]
                                .hit(cfg.style.button_spacing, cfg.style.edge_padding, width, height, x, y, Some(btn))
                                .is_some();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, hit);
                        }
                        TouchEvent::Up(up) => {
                            if !touches.contains_key(&up.seat_slot()) {
                                continue;
                            }
                            let (layer, btn) = *touches.get(&up.seat_slot()).unwrap();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        backlight.update_backlight(&cfg);
    }
}
