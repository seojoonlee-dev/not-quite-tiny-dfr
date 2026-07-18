use crate::style::{build_font, Color, Style, StyleProxy};
use crate::widget::Widgets;
use crate::FunctionLayer;
use cairo::{Context, Format, ImageSurface};
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::{
    ffi::OsString,
    fmt,
    fs::{read_to_string, File},
    io::ErrorKind,
    os::fd::AsFd,
    path::{Path, PathBuf},
};

const BASE_CFG_PATH: &str = "/usr/share/not-quite-tiny-dfr/config.toml";

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    /// Seconds of inactivity before dimming the Touch Bar; 0 disables dimming.
    pub dim_timeout: u32,
    /// Seconds of inactivity before turning the Touch Bar off; 0 disables it.
    pub off_timeout: u32,
    /// Whether a vertical swipe on the bar slides between the two layers.
    pub layer_swipe: bool,
    /// Seconds to shift synced lyrics against the audio: positive shows each
    /// line earlier (compensating for audio output latency), negative later.
    pub lyric_offset: f64,
    /// Whether the media widget blurs the album cover behind the panel.
    pub media_cover_blur: bool,
    /// Whether fetched album covers are cached on disk.
    pub media_art_cache: bool,
    /// Whether fetched lyrics are cached on disk.
    pub media_lyrics_cache: bool,
    pub style: Style,
}

// Defaults for every setting, so the shipped template can be fully commented
// (a commented-out key falls back to these instead of leaving the daemon with
// nothing to use).
const DEFAULT_MEDIA_LAYER_DEFAULT: bool = false;
const DEFAULT_SHOW_BUTTON_OUTLINES: bool = true;
const DEFAULT_ENABLE_PIXEL_SHIFT: bool = false;
// Bold by default, matching the original hardcoded ":bold" pattern.
const DEFAULT_FONT_BOLD: bool = true;
const DEFAULT_ADAPTIVE_BRIGHTNESS: bool = true;
const DEFAULT_ACTIVE_BRIGHTNESS: u32 = 128;
const DEFAULT_DOUBLE_PRESS_SWITCH_LAYERS: u32 = 0;
const DEFAULT_DIM_TIMEOUT: u32 = 30;
const DEFAULT_OFF_TIMEOUT: u32 = 60;
const DEFAULT_VISIBLE_BUTTONS: usize = 0;
const DEFAULT_SCROLL_LOOP: bool = true;
const DEFAULT_SCROLL_RUBBER_BAND: bool = true;
const DEFAULT_LAYER_SWIPE: bool = true;
const DEFAULT_PINNED_IGNORE_SCROLL: bool = true;
const DEFAULT_PINNED_IGNORE_LAYER_SWIPE: bool = true;
const DEFAULT_LYRIC_OFFSET: f64 = 0.0;
const DEFAULT_MEDIA_COVER_BLUR: bool = false;
const DEFAULT_MEDIA_ART_CACHE: bool = true;
const DEFAULT_MEDIA_LYRICS_CACHE: bool = true;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    media_layer_default: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_family: Option<String>,
    font_bold: Option<bool>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    double_press_switch_layers: Option<u32>,
    dim_timeout: Option<u32>,
    off_timeout: Option<u32>,
    visible_buttons: Option<usize>,
    scroll_loop: Option<bool>,
    scroll_rubber_band: Option<bool>,
    layer_swipe: Option<bool>,
    pinned_ignore_scroll: Option<bool>,
    pinned_ignore_layer_swipe: Option<bool>,
    lyric_offset: Option<f64>,
    media_cover_blur: Option<bool>,
    media_art_cache: Option<bool>,
    media_lyrics_cache: Option<bool>,
    style: Option<StyleProxy>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    // Any number of layers, each an array of buttons; swiping cycles through
    // them in order. When set (non-empty), this wins over PrimaryLayerKeys /
    // MediaLayerKeys / MediaLayerDefault.
    layers: Option<Vec<Vec<ButtonConfig>>>,
}

impl ConfigProxy {
    /// Overlay the values set in `o` on top of `self`, so later (higher
    /// precedence) config layers win over earlier ones.
    fn merge(&mut self, o: ConfigProxy) {
        if o.media_layer_default.is_some() {
            self.media_layer_default = o.media_layer_default;
        }
        if o.show_button_outlines.is_some() {
            self.show_button_outlines = o.show_button_outlines;
        }
        if o.enable_pixel_shift.is_some() {
            self.enable_pixel_shift = o.enable_pixel_shift;
        }
        if o.font_family.is_some() {
            self.font_family = o.font_family;
        }
        if o.font_bold.is_some() {
            self.font_bold = o.font_bold;
        }
        if o.adaptive_brightness.is_some() {
            self.adaptive_brightness = o.adaptive_brightness;
        }
        if o.active_brightness.is_some() {
            self.active_brightness = o.active_brightness;
        }
        if o.double_press_switch_layers.is_some() {
            self.double_press_switch_layers = o.double_press_switch_layers;
        }
        if o.dim_timeout.is_some() {
            self.dim_timeout = o.dim_timeout;
        }
        if o.off_timeout.is_some() {
            self.off_timeout = o.off_timeout;
        }
        if o.visible_buttons.is_some() {
            self.visible_buttons = o.visible_buttons;
        }
        if o.scroll_loop.is_some() {
            self.scroll_loop = o.scroll_loop;
        }
        if o.scroll_rubber_band.is_some() {
            self.scroll_rubber_band = o.scroll_rubber_band;
        }
        if o.layer_swipe.is_some() {
            self.layer_swipe = o.layer_swipe;
        }
        if o.pinned_ignore_scroll.is_some() {
            self.pinned_ignore_scroll = o.pinned_ignore_scroll;
        }
        if o.pinned_ignore_layer_swipe.is_some() {
            self.pinned_ignore_layer_swipe = o.pinned_ignore_layer_swipe;
        }
        if o.lyric_offset.is_some() {
            self.lyric_offset = o.lyric_offset;
        }
        if o.media_cover_blur.is_some() {
            self.media_cover_blur = o.media_cover_blur;
        }
        if o.media_art_cache.is_some() {
            self.media_art_cache = o.media_art_cache;
        }
        if o.media_lyrics_cache.is_some() {
            self.media_lyrics_cache = o.media_lyrics_cache;
        }
        if o.primary_layer_keys.is_some() {
            self.primary_layer_keys = o.primary_layer_keys;
        }
        if o.media_layer_keys.is_some() {
            self.media_layer_keys = o.media_layer_keys;
        }
        if o.layers.is_some() {
            self.layers = o.layers;
        }
        // The [Style] table merges field-by-field rather than replacing wholesale.
        if let Some(user_style) = o.style {
            match self.style.as_mut() {
                Some(base_style) => base_style.merge(user_style),
                None => self.style = Some(user_style),
            }
        }
    }
}

/// What pressing a button does: emit an input key event, or one of the
/// daemon-internal actions. In the config an action is the key name
/// ("VolumeUp", "IllumUp" = keyboard backlight, ...) or an internal action
/// name ("TouchBarBrightnessUp"/"TouchBarBrightnessDown" = the Touch Bar's
/// own brightness).
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum ButtonAction {
    Key(Key),
    TouchBarBrightnessUp,
    TouchBarBrightnessDown,
}

/// What a tap on a button does, chosen with the `OnClick` config key.
/// `Action` (the default) runs the button's configured Action/keys; `Expand`
/// instead expands the button in place -- reusing the slider's expand
/// animation -- and shows an `ExpandCommand` script's output until it idles.
/// Any other string is a shell command: on a media widget it runs when the
/// active panel is tapped outside the transport controls (e.g. to raise the
/// player app).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OnClick {
    Action,
    Expand,
    Command(String),
}

impl<'de> Deserialize<'de> for OnClick {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "Action" => OnClick::Action,
            "Expand" => OnClick::Expand,
            _ => OnClick::Command(s),
        })
    }
}

impl<'de> Deserialize<'de> for ButtonAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "TouchBarBrightnessUp" => Ok(ButtonAction::TouchBarBrightnessUp),
            "TouchBarBrightnessDown" => Ok(ButtonAction::TouchBarBrightnessDown),
            _ => Key::deserialize(de::value::StrDeserializer::new(&name)).map(ButtonAction::Key),
        }
    }
}

fn array_or_single<'de, D>(deserializer: D) -> Result<Vec<ButtonAction>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArrayOrSingle;

    impl<'de> Visitor<'de> for ArrayOrSingle {
        type Value = Vec<ButtonAction>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<ButtonAction>, E> {
            Ok(vec![Deserialize::deserialize(
                de::value::BorrowedStrDeserializer::new(value),
            )?])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Vec<ButtonAction>, A::Error> {
            Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }
    }

    deserializer.deserialize_any(ArrayOrSingle)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub time: Option<String>,
    pub battery: Option<String>,
    // Built-in CPU widget. The value is a space-separated list of what to show,
    // any of "celsius", "fahrenheit", "watts" (e.g. "celsius watts"). `CpuTemp`
    // is kept as an alias for the old name.
    #[serde(alias = "CpuTemp")]
    pub cpu: Option<String>,
    // Whether the CPU widget shows the "CPU" label prefix (default true).
    pub cpu_label: Option<bool>,
    // Built-in GPU widget. Same space-separated component list as the CPU widget
    // ("celsius", "fahrenheit", "watts"). The vendor (AMD/NVIDIA/Intel) is
    // detected automatically and used as the label prefix.
    pub gpu: Option<String>,
    // Whether the GPU widget shows the vendor label prefix (default true).
    pub gpu_label: Option<bool>,
    pub locale: Option<String>,
    #[serde(deserialize_with = "array_or_single", default)]
    pub action: Vec<ButtonAction>,
    // What a tap does: "Action" (default) runs the button's Action/keys;
    // "Expand" instead expands the button in place (the slider's animation)
    // and shows a separate ExpandCommand script's output until it idles.
    pub on_click: Option<OnClick>,
    // For OnClick = "Expand": the shell command whose stdout fills the expanded
    // view (same JSON `{"text","color","icon"}` or plain-text protocol as a
    // command widget). It polls in the background so a value is always ready the
    // instant the button opens; the shown value is then frozen until it
    // collapses (it only refreshes while out of sight), and the collapsed and
    // expanded contents crossfade during the animation. `expand_stretch` is the
    // expanded width in slots (default 2).
    pub expand_command: Option<String>,
    pub expand_stretch: Option<usize>,
    // Leading buttons marked Pinned sit outside the scrolling band and hold
    // still during layer swipes (each behavior has its own global toggle).
    pub pinned: Option<bool>,
    pub stretch: Option<usize>,
    pub icon_width: Option<i32>,
    pub icon_height: Option<i32>,
    // Per-button style overrides. When unset, the corresponding value from the
    // global [Style] table is used.
    pub color: Option<Color>,
    pub color_active: Option<Color>,
    pub text_color: Option<Color>,
    // A command widget: run this shell command every `interval` seconds and show
    // its stdout (JSON `{"text","color"}` or plain text). Takes precedence over
    // Text/Icon/etc. when set.
    pub command: Option<String>,
    pub interval: Option<f64>,
    // A slider widget (get+set required): `slider_get` prints the current
    // value (0-100, optionally followed by "muted") and is polled every
    // `interval` seconds; `slider_set` runs with `{}` replaced by the new
    // value when the slider is moved. `slider_mute` (optional) runs with `{}`
    // replaced by "toggle" when the expanded slider's icon is tapped, or "0"
    // when a drag unmutes. Collapsed it shows `icon` at `stretch` slots;
    // tapping expands it to `slider_stretch` slots until it idles.
    // `slider_mute_icon` (optional) replaces `icon` while the control is muted;
    // `slider_low_icon` (optional) replaces `icon` while the value is below 50.
    pub slider_get: Option<String>,
    pub slider_set: Option<String>,
    pub slider_mute: Option<String>,
    pub slider_mute_icon: Option<String>,
    pub slider_low_icon: Option<String>,
    pub slider_stretch: Option<usize>,
    // A media widget: shows transport controls (previous / play-pause / next)
    // that, while a player is running, join into a panel backed by the album
    // cover with the track title and artist. Driven by playerctl (MPRIS); taps
    // control the active player. Spans `stretch` slots.
    pub media: Option<bool>,
}

/// The stock Esc + F1-F12 primary layer, used when the config sets no
/// PrimaryLayerKeys. The Esc is a regular (pinned) button here, not an
/// injected special case: configs that define their own layers declare
/// their own Esc.
fn default_primary_layer() -> Vec<ButtonConfig> {
    std::iter::once(esc_button())
        .chain(
            [
                Key::F1,
                Key::F2,
                Key::F3,
                Key::F4,
                Key::F5,
                Key::F6,
                Key::F7,
                Key::F8,
                Key::F9,
                Key::F10,
                Key::F11,
                Key::F12,
            ]
            .into_iter()
            .enumerate()
            .map(|(i, key)| ButtonConfig {
                text: Some(format!("F{}", i + 1)),
                action: vec![ButtonAction::Key(key)],
                on_click: Some(OnClick::Action),
                ..Default::default()
            }),
        )
        .collect()
}

/// The stock media-key layer, used when the config sets no MediaLayerKeys.
fn default_media_layer() -> Vec<ButtonConfig> {
    std::iter::once(esc_button())
        .chain(
            [
                ("brightness_low", Key::BrightnessDown),
                ("brightness_high", Key::BrightnessUp),
                ("mic_off", Key::MicMute),
                ("search", Key::Search),
                ("backlight_low", Key::IllumDown),
                ("backlight_high", Key::IllumUp),
                ("fast_rewind", Key::PreviousSong),
                ("play_pause", Key::PlayPause),
                ("fast_forward", Key::NextSong),
                ("volume_off", Key::Mute),
                ("volume_down", Key::VolumeDown),
                ("volume_up", Key::VolumeUp),
            ]
            .into_iter()
            .map(|(icon, key)| ButtonConfig {
                icon: Some(icon.to_string()),
                action: vec![ButtonAction::Key(key)],
                on_click: Some(OnClick::Action),
                ..Default::default()
            }),
        )
        .collect()
}

/// The pinned Esc button used by the default layers and the error banner.
/// Esc is never injected automatically: user-defined layers declare their
/// own (`Pinned = true` marks it pinned).
fn esc_button() -> ButtonConfig {
    ButtonConfig {
        icon: None,
        text: Some("esc".into()),
        theme: None,
        action: vec![ButtonAction::Key(Key::Esc)],
        pinned: Some(true),
        stretch: None,
        time: None,
        locale: None,
        battery: None,
        cpu: None,
        cpu_label: None,
        gpu: None,
        gpu_label: None,
        icon_width: None,
        icon_height: None,
        color: None,
        color_active: None,
        text_color: None,
        command: None,
        interval: None,
        slider_get: None,
        slider_set: None,
        slider_mute: None,
        slider_mute_icon: None,
        slider_low_icon: None,
        slider_stretch: None,
        on_click: Some(OnClick::Action),
        expand_command: None,
        expand_stretch: None,
        media: None,
    }
}

/// A one-line, bar-width-friendly summary of a config parse error.
fn short_error(name: &str, full: &str) -> String {
    let first = full.lines().next().unwrap_or("parse error");
    let msg = format!("config error \u{2014} {name}: {first}");
    const MAX: usize = 78;
    if msg.chars().count() > MAX {
        msg.chars().take(MAX - 1).collect::<String>() + "\u{2026}"
    } else {
        msg
    }
}

/// A full-width banner layer showing `message`, with the Esc key kept usable.
fn error_layer(message: &str) -> FunctionLayer {
    let keys = vec![
        esc_button(),
        ButtonConfig {
            icon: None,
            text: Some(message.to_string()),
            theme: None,
            action: vec![], // inert: shows the message, sends nothing
            pinned: None,
            stretch: Some(24),
            time: None,
            locale: None,
            battery: None,
            cpu: None,
            cpu_label: None,
            gpu: None,
            gpu_label: None,
            icon_width: None,
            icon_height: None,
            color: None,
            color_active: None,
            text_color: None,
            command: None,
            interval: None,
            slider_get: None,
            slider_set: None,
            slider_mute: None,
            slider_mute_icon: None,
            slider_low_icon: None,
            slider_stretch: None,
            on_click: None,
            expand_command: None,
            expand_stretch: None,
            media: None,
        },
    ];
    FunctionLayer::with_config(
        keys,
        &mut Widgets::default(),
        &mut 0,
        48,
        0,
        true,
        true,
        true,
        true,
    )
}

/// Resolve a background image path: absolute paths are used as-is; relative ones
/// are looked up in the config dirs (~/.config, /etc, /usr/share), like icons.
fn resolve_image_path(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    if let Some(Some(dir)) = crate::USER_ICON_DIR.get() {
        let candidate = dir.join(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    for base in ["/etc/not-quite-tiny-dfr", "/usr/share/not-quite-tiny-dfr"] {
        let candidate = Path::new(base).join(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Load a PNG and scale/center-crop it to the bar size (CSS
/// `background-size: cover`), so an image too tall for the bar shows its middle
/// band. The surface is made one pixel-shift range larger than the bar in each
/// axis, so pixel shift can slide the image around without exposing its edges
/// (the renderer paints it centered, offset by half that margin).
fn load_background_image(
    path: &str,
    width: i32,
    height: i32,
    blur: bool,
) -> Result<ImageSurface, String> {
    let width = width + crate::pixel_shift::PIXEL_SHIFT_WIDTH_PX as i32;
    let height = height + crate::pixel_shift::PIXEL_SHIFT_HEIGHT_PX as i32;
    let resolved = resolve_image_path(path).ok_or_else(|| format!("not found: {path}"))?;
    let mut file = File::open(&resolved).map_err(|e| e.to_string())?;
    let src =
        ImageSurface::create_from_png(&mut file).map_err(|e| format!("not a readable PNG: {e}"))?;
    let (iw, ih) = (src.width() as f64, src.height() as f64);
    if iw <= 0.0 || ih <= 0.0 {
        return Err("image has zero size".to_string());
    }
    let scale = (width as f64 / iw).max(height as f64 / ih); // cover
    let mut dst = ImageSurface::create(Format::ARgb32, width, height).map_err(|e| e.to_string())?;
    let c = Context::new(&dst).map_err(|e| e.to_string())?;
    // Center the scaled image, cropping the overflow to the bar.
    c.translate(
        (width as f64 - iw * scale) / 2.0,
        (height as f64 - ih * scale) / 2.0,
    );
    c.scale(scale, scale);
    c.set_source_surface(&src, 0.0, 0.0)
        .map_err(|e| e.to_string())?;
    c.paint().map_err(|e| e.to_string())?;
    drop(c); // release the surface so its pixels can be taken for the blur
    if blur {
        let (w, h, stride) = (dst.width() as usize, dst.height() as usize, dst.stride() as usize);
        // Radius scaled to the bar's short side (~60px) so the whole-bar
        // wallpaper reads as clearly blurred, not just softened.
        let radius = (w.min(h) / 8).clamp(4, 20);
        dst.flush();
        let mut data = dst.data().map_err(|e| e.to_string())?;
        crate::box_blur_argb32(&mut data, w, h, stride, radius);
    }
    Ok(dst)
}

/// Slots covered by a layer's declared pinned prefix (the leading run of
/// `Pinned = true` buttons), counted the same way FunctionLayer::with_config
/// counts them.
fn declared_pinned_slots(keys: &[ButtonConfig]) -> usize {
    keys.iter()
        .take_while(|c| c.pinned.unwrap_or(false))
        .map(|c| c.stretch.unwrap_or(1).max(1))
        .sum()
}

/// Layer swipes only keep the pinned prefix still when every layer pins the
/// same slots; a mismatch silently forces the whole-bar slide path for every
/// transition, where nothing is static and mid-slide touches can't press
/// anything. A disagreeing config is rejected like a parse error -- red
/// banner on the bar, not a journal-only warning -- instead of silently
/// picking one behavior.
fn pin_mismatch_error(key_sets: &[Vec<ButtonConfig>]) -> Option<String> {
    let pin_slots: Vec<usize> = key_sets.iter().map(|k| declared_pinned_slots(k)).collect();
    if pin_slots.iter().any(|&s| s != pin_slots[0]) {
        Some(short_error(
            "Pinned",
            &format!("must match on every layer (leading slots {pin_slots:?})"),
        ))
    } else {
        None
    }
}

/// Load and merge the config. `override_paths` are override layers applied in
/// increasing order of precedence (e.g. `[/etc/.../config.toml,
/// ~/.config/.../config.toml]`) on top of the shipped base template. Missing
/// files are skipped. If a file fails to parse, its (already-merged) predecessors
/// still apply, but the bar shows a red error banner so the bad edit is obvious.
fn load_config(
    override_paths: &[PathBuf],
    width: u16,
    height: u16,
) -> (Config, Vec<FunctionLayer>, Widgets) {
    let mut base = toml::from_str::<ConfigProxy>(&read_to_string(BASE_CFG_PATH).unwrap()).unwrap();
    let mut config_error: Option<String> = None;
    for path in override_paths {
        match read_to_string(path) {
            Ok(text) => match toml::from_str::<ConfigProxy>(&text) {
                Ok(over) => base.merge(over),
                Err(e) => {
                    eprintln!("not-quite-tiny-dfr: {}: {e}", path.display());
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("config.toml");
                    config_error.get_or_insert_with(|| short_error(name, &e.to_string()));
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => eprintln!("not-quite-tiny-dfr: cannot read {}: {e}", path.display()),
        }
    }

    let style_proxy = base.style.take().unwrap_or_default();
    let mut style = style_proxy.resolve();
    style.font = build_font(
        base.font_family.as_deref(),
        base.font_bold.unwrap_or(DEFAULT_FONT_BOLD),
        style.font_size,
    );
    // Load the background image once here (we know the bar size), if it wins.
    if let Some(path) = style_proxy.image_path() {
        let blur = style_proxy.background_image_blur.unwrap_or(false);
        match load_background_image(path, width as i32, height as i32, blur) {
            Ok(surf) => style.background_image = Some(surf),
            Err(e) => eprintln!("not-quite-tiny-dfr: background image {path:?}: {e}"),
        }
    }
    // Default icon size for buttons that don't set their own IconWidth/Height.
    let default_icon_size = style.icon_size.round() as i32;
    // Command widgets found while building the layers, with unique ids.
    let mut widgets = Widgets::default();
    let mut next_id = 0;
    // The freeform Layers list wins; PrimaryLayerKeys/MediaLayerKeys
    // (with MediaLayerDefault ordering) are the two-layer fallback.
    let key_sets: Vec<Vec<ButtonConfig>> = match base.layers.take() {
        Some(sets) if sets.iter().any(|s| !s.is_empty()) => {
            let (kept, empty): (Vec<_>, Vec<_>) = sets.into_iter().partition(|s| !s.is_empty());
            if !empty.is_empty() {
                eprintln!("not-quite-tiny-dfr: ignoring empty layer(s) in Layers");
            }
            kept
        }
        _ => {
            let media = base.media_layer_keys.unwrap_or_else(default_media_layer);
            let primary = base
                .primary_layer_keys
                .unwrap_or_else(default_primary_layer);
            if base
                .media_layer_default
                .unwrap_or(DEFAULT_MEDIA_LAYER_DEFAULT)
            {
                vec![media, primary]
            } else {
                vec![primary, media]
            }
        }
    };
    // Cross-layer validation is a config error like a parse failure: the bar
    // shows the banner rather than running with a quietly altered layout. A
    // parse error keeps precedence -- these key sets may be a stale mix.
    if config_error.is_none() {
        if let Some(err) = pin_mismatch_error(&key_sets) {
            eprintln!("not-quite-tiny-dfr: {err}");
            config_error = Some(err);
        }
    }
    let layers = match &config_error {
        Some(message) => {
            // Unmistakable red banner; blend button fill into it and keep white
            // text so the message reads regardless of the user's colors.
            let red = Color::rgb(0.45, 0.0, 0.0);
            style.background = red;
            style.background_image = None; // don't let an image hide the error
            style.button_color = red;
            style.text_color = Color::rgb(1.0, 1.0, 1.0);
            vec![error_layer(message), error_layer(message)]
        }
        None => {
            // How many button-slots a layer shows at once; layers with more
            // become scrollable (pinned buttons don't count or scroll).
            let visible_buttons = base.visible_buttons.unwrap_or(DEFAULT_VISIBLE_BUTTONS);
            let scroll_loop = base.scroll_loop.unwrap_or(DEFAULT_SCROLL_LOOP);
            let scroll_rubber_band = base
                .scroll_rubber_band
                .unwrap_or(DEFAULT_SCROLL_RUBBER_BAND);
            let pin_scroll = base
                .pinned_ignore_scroll
                .unwrap_or(DEFAULT_PINNED_IGNORE_SCROLL);
            let pin_swipe = base
                .pinned_ignore_layer_swipe
                .unwrap_or(DEFAULT_PINNED_IGNORE_LAYER_SWIPE);
            key_sets
                .into_iter()
                .map(|keys| {
                    FunctionLayer::with_config(
                        keys,
                        &mut widgets,
                        &mut next_id,
                        default_icon_size,
                        visible_buttons,
                        scroll_loop,
                        scroll_rubber_band,
                        pin_scroll,
                        pin_swipe,
                    )
                })
                .collect()
        }
    };
    let cfg = Config {
        show_button_outlines: base
            .show_button_outlines
            .unwrap_or(DEFAULT_SHOW_BUTTON_OUTLINES),
        enable_pixel_shift: base
            .enable_pixel_shift
            .unwrap_or(DEFAULT_ENABLE_PIXEL_SHIFT),
        adaptive_brightness: base
            .adaptive_brightness
            .unwrap_or(DEFAULT_ADAPTIVE_BRIGHTNESS),
        active_brightness: base.active_brightness.unwrap_or(DEFAULT_ACTIVE_BRIGHTNESS),
        double_press_switch_layers: base
            .double_press_switch_layers
            .unwrap_or(DEFAULT_DOUBLE_PRESS_SWITCH_LAYERS),
        dim_timeout: base.dim_timeout.unwrap_or(DEFAULT_DIM_TIMEOUT),
        off_timeout: base.off_timeout.unwrap_or(DEFAULT_OFF_TIMEOUT),
        layer_swipe: base.layer_swipe.unwrap_or(DEFAULT_LAYER_SWIPE),
        lyric_offset: base.lyric_offset.unwrap_or(DEFAULT_LYRIC_OFFSET),
        media_cover_blur: base
            .media_cover_blur
            .unwrap_or(DEFAULT_MEDIA_COVER_BLUR),
        media_art_cache: base.media_art_cache.unwrap_or(DEFAULT_MEDIA_ART_CACHE),
        media_lyrics_cache: base
            .media_lyrics_cache
            .unwrap_or(DEFAULT_MEDIA_LYRICS_CACHE),
        style,
    };
    (cfg, layers, widgets)
}

/// A config file we live-reload on. We watch the file's parent *directory*
/// rather than the file itself: editors (and our own tooling) save atomically
/// by writing a temp file and renaming it into place, which replaces the
/// file's inode. A watch on the file would follow the old, now-unlinked inode
/// and go stale after the first save; a directory watch instead sees the new
/// file arrive (IN_MOVED_TO/IN_CREATE) and stays valid across saves.
///
/// `wd` is `None` when the directory does not exist yet (e.g. before the user
/// creates ~/.config/not-quite-tiny-dfr/); it is re-armed on later polls so a
/// config dropped in later starts being watched.
struct Watch {
    /// The directory we arm the inotify watch on (a config file's parent).
    dir: PathBuf,
    /// The config file's base name, matched against inotify event names so
    /// unrelated files changing in the same directory don't trigger a reload.
    name: OsString,
    wd: Option<WatchDescriptor>,
}

impl Watch {
    fn new(inotify_fd: &Inotify, path: &Path) -> Watch {
        // Real config paths always have both a parent and a file name; the
        // fallbacks just keep this total rather than panicking.
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let name = path
            .file_name()
            .unwrap_or_else(|| path.as_os_str())
            .to_os_string();
        Watch {
            wd: arm_inotify(inotify_fd, &dir),
            dir,
            name,
        }
    }
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watches: Vec<Watch>,
    cfg_paths: Vec<PathBuf>,
    width: u16,
    height: u16,
}

fn arm_inotify(inotify_fd: &Inotify, dir: &Path) -> Option<WatchDescriptor> {
    // IN_CLOSE_WRITE catches in-place saves; IN_MOVED_TO/IN_CREATE catch the
    // atomic-rename saves (temp file + rename) that a file-level watch misses.
    // The watch is persistent (no IN_ONESHOT) so it survives an unrelated file
    // changing in the same directory.
    let flags =
        AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CREATE | AddWatchFlags::IN_CLOSE_WRITE;
    match inotify_fd.add_watch(dir, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    /// `cfg_paths` are the override layers (lowest precedence first) that are
    /// both merged when loading and watched for live-reload. `width`/`height`
    /// are the bar dimensions, needed to size the background image.
    pub fn new(cfg_paths: Vec<PathBuf>, width: u16, height: u16) -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watches = cfg_paths
            .iter()
            .map(|path| Watch::new(&inotify_fd, path))
            .collect();
        ConfigManager {
            inotify_fd,
            watches,
            cfg_paths,
            width,
            height,
        }
    }
    pub fn load_config(&self) -> (Config, Vec<FunctionLayer>, Widgets) {
        load_config(&self.cfg_paths, self.width, self.height)
    }
    /// Add a higher-precedence override layer (and start watching it) after
    /// construction. Used to attach the per-user ~/.config layer once a user
    /// logs in, when the daemon started before anyone was logged in. The caller
    /// reloads afterwards to actually apply it.
    pub fn add_path(&mut self, path: PathBuf) {
        self.watches.push(Watch::new(&self.inotify_fd, &path));
        self.cfg_paths.push(path);
    }
    /// Returns `Some(new widget specs)` when the config was reloaded (the caller
    /// then rebuilds the widget runtime), or `None` when nothing changed.
    pub fn update_config(
        &mut self,
        cfg: &mut Config,
        layers: &mut Vec<FunctionLayer>,
    ) -> Option<Widgets> {
        // Pick up directories that did not exist when we last tried to watch
        // them (e.g. the user just created ~/.config/not-quite-tiny-dfr/).
        let mut newly_armed = false;
        for watch in &mut self.watches {
            if watch.wd.is_none() {
                watch.wd = arm_inotify(&self.inotify_fd, &watch.dir);
                newly_armed |= watch.wd.is_some();
            }
        }
        let event_reload = match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => None,
            r => self.handle_events(cfg, layers, r),
        };
        if event_reload.is_some() {
            return event_reload;
        }
        // A watched file appearing is itself a change: load it now rather than
        // waiting for the next write to it.
        if newly_armed {
            let parts = self.load_config();
            *cfg = parts.0;
            *layers = parts.1;
            return Some(parts.2);
        }
        None
    }
    #[cold]
    fn handle_events(
        &mut self,
        cfg: &mut Config,
        layers: &mut Vec<FunctionLayer>,
        evts: Result<Vec<InotifyEvent>, Errno>,
    ) -> Option<Widgets> {
        let evts = match evts {
            Ok(evts) => evts,
            Err(_) => return None,
        };
        // If a watched directory is removed, the kernel drops the watch and
        // sends IN_IGNORED; forget the wd so update_config re-arms it once the
        // directory reappears.
        for evt in &evts {
            if evt.mask.contains(AddWatchFlags::IN_IGNORED) {
                for watch in &mut self.watches {
                    if watch.wd == Some(evt.wd) {
                        watch.wd = None;
                    }
                }
            }
        }
        // Reload only when an event names one of our config files in the
        // directory it lives in.
        let reload = evts.iter().any(|evt| {
            self.watches
                .iter()
                .any(|w| w.wd == Some(evt.wd) && evt.name.as_deref() == Some(w.name.as_os_str()))
        });
        if !reload {
            return None;
        }
        let parts = load_config(&self.cfg_paths, self.width, self.height);
        *cfg = parts.0;
        *layers = parts.1;
        Some(parts.2)
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_template_parses() {
        // The template ships fully commented (a reference); it must still parse
        // to an all-None proxy, and the code defaults must supply valid layers.
        let text = read_to_string("share/not-quite-tiny-dfr/config.toml")
            .expect("template config should exist at share/not-quite-tiny-dfr/config.toml");
        let _cfg: ConfigProxy = toml::from_str(&text).expect("template config should parse");
        assert!(!default_primary_layer().is_empty());
        assert!(!default_media_layer().is_empty());
    }

    #[test]
    fn later_layer_overrides_earlier() {
        // Simulates the /etc -> ~/.config cascade: the higher-precedence layer
        // wins per-field, and unset fields (including within [Style]) are kept.
        let mut base: ConfigProxy = toml::from_str(
            "ShowButtonOutlines = true\nActiveBrightness = 128\n\
             [Style]\nButtonSpacing = 16\nCornerRadius = 8\n",
        )
        .unwrap();
        let over: ConfigProxy =
            toml::from_str("ActiveBrightness = 255\n[Style]\nButtonSpacing = 0\n").unwrap();
        base.merge(over);
        assert_eq!(base.show_button_outlines, Some(true)); // untouched by override
        assert_eq!(base.active_brightness, Some(255)); // overridden
        let style = base.style.unwrap().resolve();
        assert_eq!(style.button_spacing, 0.0); // overridden
        assert_eq!(style.corner_radius, 8.0); // retained through style field-merge
    }

    #[test]
    fn visible_buttons_parses_and_merges() {
        let mut base: ConfigProxy = toml::from_str("VisibleButtons = 8\n").unwrap();
        assert_eq!(base.visible_buttons, Some(8));
        let over: ConfigProxy = toml::from_str("VisibleButtons = 12\n").unwrap();
        base.merge(over);
        assert_eq!(base.visible_buttons, Some(12));
    }

    #[test]
    fn scroll_rubber_band_parses_and_merges() {
        let mut base: ConfigProxy = toml::from_str("ScrollRubberBand = true\n").unwrap();
        assert_eq!(base.scroll_rubber_band, Some(true));
        let over: ConfigProxy = toml::from_str("ScrollRubberBand = false\n").unwrap();
        base.merge(over);
        assert_eq!(base.scroll_rubber_band, Some(false));
    }

    #[test]
    fn layers_parse_and_merge() {
        // JSON-style nested arrays: any number of layers, each a button list.
        let mut base: ConfigProxy = toml::from_str(
            "Layers = [\n\
             \x20   [ { Text = \"A\", Action = \"F1\" }, { Text = \"B\", Action = \"F2\" } ],\n\
             \x20   [ { Text = \"C\", Action = \"F3\" } ],\n\
             \x20   [ { Text = \"D\", Action = \"F4\" } ],\n\
             ]\n",
        )
        .unwrap();
        let sets = base.layers.as_ref().unwrap();
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0].len(), 2);
        assert_eq!(sets[0][0].text.as_deref(), Some("A"));
        assert_eq!(sets[2][0].text.as_deref(), Some("D"));
        // A later config layer replaces the whole list.
        let over: ConfigProxy =
            toml::from_str("Layers = [ [ { Text = \"Z\", Action = \"F5\" } ] ]\n").unwrap();
        base.merge(over);
        assert_eq!(base.layers.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn layer_swipe_parses_and_merges() {
        let mut base: ConfigProxy = toml::from_str("LayerSwipe = true\n").unwrap();
        assert_eq!(base.layer_swipe, Some(true));
        let over: ConfigProxy = toml::from_str("LayerSwipe = false\n").unwrap();
        base.merge(over);
        assert_eq!(base.layer_swipe, Some(false));
    }

    #[test]
    fn lyric_offset_parses_and_merges() {
        let mut base: ConfigProxy = toml::from_str("LyricOffset = 0.4\n").unwrap();
        assert_eq!(base.lyric_offset, Some(0.4));
        let over: ConfigProxy = toml::from_str("LyricOffset = -0.25\n").unwrap();
        base.merge(over);
        assert_eq!(base.lyric_offset, Some(-0.25));
    }

    #[test]
    fn media_cover_blur_parses_and_merges() {
        let mut base: ConfigProxy = toml::from_str("MediaCoverBlur = true\n").unwrap();
        assert_eq!(base.media_cover_blur, Some(true));
        let over: ConfigProxy = toml::from_str("MediaCoverBlur = false\n").unwrap();
        base.merge(over);
        assert_eq!(base.media_cover_blur, Some(false));
    }

    #[test]
    fn cpu_button_parses() {
        let cfg: ButtonConfig =
            toml::from_str("Cpu = \"celsius watts\"\nCpuLabel = false\nStretch = 2\n").unwrap();
        assert_eq!(cfg.cpu.as_deref(), Some("celsius watts"));
        assert_eq!(cfg.cpu_label, Some(false));
        assert_eq!(cfg.stretch, Some(2));
        // The old `CpuTemp` key still works via the serde alias.
        let old: ButtonConfig = toml::from_str("CpuTemp = \"celsius\"\n").unwrap();
        assert_eq!(old.cpu.as_deref(), Some("celsius"));
    }

    #[test]
    fn gpu_button_parses() {
        let cfg: ButtonConfig =
            toml::from_str("Gpu = \"celsius watts\"\nGpuLabel = false\nStretch = 2\n").unwrap();
        assert_eq!(cfg.gpu.as_deref(), Some("celsius watts"));
        assert_eq!(cfg.gpu_label, Some(false));
        assert_eq!(cfg.stretch, Some(2));
    }

    #[test]
    fn on_click_expand_parses() {
        let cfg: ButtonConfig = toml::from_str(
            "Command = \"battery.sh\"\nOnClick = \"Expand\"\n\
             ExpandCommand = \"battery_eta.sh\"\nExpandStretch = 4\n",
        )
        .unwrap();
        assert_eq!(cfg.on_click, Some(OnClick::Expand));
        assert_eq!(cfg.expand_command.as_deref(), Some("battery_eta.sh"));
        assert_eq!(cfg.expand_stretch, Some(4));
        // Default (unset) leaves OnClick as the plain Action behavior.
        let plain: ButtonConfig = toml::from_str("Command = \"x\"\n").unwrap();
        assert_eq!(plain.on_click, None);
    }

    #[test]
    fn on_click_command_parses() {
        // Anything other than "Action"/"Expand" is a shell command.
        let cfg: ButtonConfig =
            toml::from_str("Media = true\nOnClick = \"gtk-launch tidal\"\n").unwrap();
        assert_eq!(
            cfg.on_click,
            Some(OnClick::Command("gtk-launch tidal".to_string()))
        );
    }

    #[test]
    fn error_banner_keeps_esc() {
        // The banner always carries its own Esc so an error never hides it.
        assert_eq!(error_layer("config error").buttons.len(), 2); // esc + banner
    }

    #[test]
    fn pin_mismatch_is_a_config_error() {
        let button = |pinned: &str| -> ButtonConfig {
            toml::from_str(&format!("Text = \"esc\"\nAction = \"Esc\"\n{pinned}")).unwrap()
        };
        // Disagreeing layers (1 pinned slot vs 0) are rejected with a banner
        // message naming the fix.
        let sets = vec![
            vec![button("Pinned = true"), button("")],
            vec![button("Pinned = false"), button("")],
        ];
        let err = pin_mismatch_error(&sets).expect("mismatch should be an error");
        assert!(err.contains("Pinned"));
        assert!(err.contains("every layer"));
        // Agreeing layers pass.
        let sets = vec![
            vec![button("Pinned = true"), button("")],
            vec![button("Pinned = true"), button("")],
        ];
        assert!(pin_mismatch_error(&sets).is_none());
        // Stretch counts in slots: a stretch-2 pin vs a plain pin disagrees.
        let sets = vec![
            vec![button("Pinned = true\nStretch = 2"), button("")],
            vec![button("Pinned = true"), button("")],
        ];
        assert!(pin_mismatch_error(&sets).is_some());
    }

    #[test]
    fn pinned_and_pin_toggles_parse() {
        let cfg: ButtonConfig =
            toml::from_str("Text = \"esc\"\nAction = \"Esc\"\nPinned = true\n").unwrap();
        assert_eq!(cfg.pinned, Some(true));
        let mut base: ConfigProxy =
            toml::from_str("PinnedIgnoreScroll = true\nPinnedIgnoreLayerSwipe = true\n").unwrap();
        let over: ConfigProxy = toml::from_str("PinnedIgnoreLayerSwipe = false\n").unwrap();
        base.merge(over);
        assert_eq!(base.pinned_ignore_scroll, Some(true));
        assert_eq!(base.pinned_ignore_layer_swipe, Some(false));
        // The stock layers declare their Esc as a pinned button.
        assert_eq!(default_primary_layer()[0].pinned, Some(true));
        assert_eq!(default_media_layer()[0].pinned, Some(true));
    }

    #[test]
    fn short_error_is_single_line_and_bounded() {
        let e = short_error("config.toml", "line one\nline two\nline three");
        assert!(!e.contains('\n'));
        assert!(e.contains("config.toml"));
        assert!(e.chars().count() <= 78);
    }
}
