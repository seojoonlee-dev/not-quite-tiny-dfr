use crate::fonts::{FontConfig, Pattern};
use crate::style::{Color, Style, StyleProxy};
use crate::widget::WidgetSpec;
use crate::FunctionLayer;
use cairo::{Context, FontFace, Format, ImageSurface};
use freetype::Library as FtLibrary;
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
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    /// Seconds of inactivity before dimming the Touch Bar; 0 disables dimming.
    pub dim_timeout: u32,
    /// Seconds of inactivity before turning the Touch Bar off; 0 disables it.
    pub off_timeout: u32,
    pub style: Style,
}

// Defaults for every setting, so the shipped template can be fully commented
// (a commented-out key falls back to these instead of leaving the daemon with
// nothing to use).
const DEFAULT_MEDIA_LAYER_DEFAULT: bool = false;
const DEFAULT_SHOW_BUTTON_OUTLINES: bool = true;
const DEFAULT_ENABLE_PIXEL_SHIFT: bool = false;
const DEFAULT_FONT_TEMPLATE: &str = ":bold";
const DEFAULT_ADAPTIVE_BRIGHTNESS: bool = true;
const DEFAULT_ACTIVE_BRIGHTNESS: u32 = 128;
const DEFAULT_DOUBLE_PRESS_SWITCH_LAYERS: u32 = 0;
const DEFAULT_DIM_TIMEOUT: u32 = 30;
const DEFAULT_OFF_TIMEOUT: u32 = 60;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    media_layer_default: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    font_family: Option<String>,
    font_bold: Option<bool>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    double_press_switch_layers: Option<u32>,
    dim_timeout: Option<u32>,
    off_timeout: Option<u32>,
    style: Option<StyleProxy>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
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
        if o.font_template.is_some() {
            self.font_template = o.font_template;
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
        if o.primary_layer_keys.is_some() {
            self.primary_layer_keys = o.primary_layer_keys;
        }
        if o.media_layer_keys.is_some() {
            self.media_layer_keys = o.media_layer_keys;
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

fn array_or_single<'de, D>(deserializer: D) -> Result<Vec<Key>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArrayOrSingle;

    impl<'de> Visitor<'de> for ArrayOrSingle {
        type Value = Vec<Key>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<Key>, E> {
            Ok(vec![Deserialize::deserialize(
                de::value::BorrowedStrDeserializer::new(value),
            )?])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Vec<Key>, A::Error> {
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
    pub locale: Option<String>,
    #[serde(deserialize_with = "array_or_single", default)]
    pub action: Vec<Key>,
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
}

fn load_font(name: &str) -> FontFace {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = match fontconfig.match_pattern(&pattern) {
        Ok(pat) => pat,
        Err(_) => panic!("Unable to find specified font. If you are using the default config, make sure you have at least one font installed")
    };
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().unwrap();
    let face = ft_library.new_face(file_name, file_idx).unwrap();
    FontFace::create_from_ft(&face).unwrap()
}

/// Build the fontconfig pattern for text labels. A plain `FontFamily` resolves
/// to the REGULAR weight (add FontBold = true for bold); if it is unset we fall
/// back to the legacy `FontTemplate` pattern, then the default.
fn resolve_font_pattern(
    family: Option<&str>,
    bold: Option<bool>,
    template: Option<&str>,
) -> String {
    match family.map(str::trim).filter(|f| !f.is_empty()) {
        Some(family) if bold.unwrap_or(false) => format!("{family}:bold"),
        Some(family) => family.to_string(),
        None => template.unwrap_or(DEFAULT_FONT_TEMPLATE).to_string(),
    }
}

/// The stock F1-F12 primary layer, used when the config sets no PrimaryLayerKeys.
fn default_primary_layer() -> Vec<ButtonConfig> {
    [
        Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10,
        Key::F11, Key::F12,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, key)| ButtonConfig {
        text: Some(format!("F{}", i + 1)),
        action: vec![key],
        ..Default::default()
    })
    .collect()
}

/// The stock media-key layer, used when the config sets no MediaLayerKeys.
fn default_media_layer() -> Vec<ButtonConfig> {
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
        action: vec![key],
        ..Default::default()
    })
    .collect()
}

/// The Esc key that is auto-added on Macs without a physical one.
fn esc_button() -> ButtonConfig {
    ButtonConfig {
        icon: None,
        text: Some("esc".into()),
        theme: None,
        action: vec![Key::Esc],
        stretch: None,
        time: None,
        locale: None,
        battery: None,
        icon_width: None,
        icon_height: None,
        color: None,
        color_active: None,
        text_color: None,
        command: None,
        interval: None,
    }
}

/// Prepend the auto Esc key on panels wide enough to need one. Used for both the
/// normal layers and the error banner, so an error never hides Esc.
fn prepend_esc_if_needed(keys: &mut Vec<ButtonConfig>, width: u16) {
    if width >= 2170 {
        keys.insert(0, esc_button());
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
fn error_layer(message: &str, width: u16) -> FunctionLayer {
    let mut keys = Vec::new();
    prepend_esc_if_needed(&mut keys, width);
    keys.push(ButtonConfig {
        icon: None,
        text: Some(message.to_string()),
        theme: None,
        action: vec![], // inert: shows the message, sends nothing
        stretch: Some(24),
        time: None,
        locale: None,
        battery: None,
        icon_width: None,
        icon_height: None,
        color: None,
        color_active: None,
        text_color: None,
        command: None,
        interval: None,
    });
    FunctionLayer::with_config(keys, &mut Vec::new(), &mut 0, 48)
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

/// Load a PNG and scale/center-crop it to exactly `width` x `height` (CSS
/// `background-size: cover`), so an image too tall for the bar shows its middle
/// band. Returns a bar-sized surface ready to paint at the origin.
fn load_background_image(path: &str, width: i32, height: i32) -> Result<ImageSurface, String> {
    let resolved = resolve_image_path(path).ok_or_else(|| format!("not found: {path}"))?;
    let mut file = File::open(&resolved).map_err(|e| e.to_string())?;
    let src =
        ImageSurface::create_from_png(&mut file).map_err(|e| format!("not a readable PNG: {e}"))?;
    let (iw, ih) = (src.width() as f64, src.height() as f64);
    if iw <= 0.0 || ih <= 0.0 {
        return Err("image has zero size".to_string());
    }
    let scale = (width as f64 / iw).max(height as f64 / ih); // cover
    let dst = ImageSurface::create(Format::ARgb32, width, height).map_err(|e| e.to_string())?;
    let c = Context::new(&dst).map_err(|e| e.to_string())?;
    // Center the scaled image, cropping the overflow to the bar.
    c.translate(
        (width as f64 - iw * scale) / 2.0,
        (height as f64 - ih * scale) / 2.0,
    );
    c.scale(scale, scale);
    c.set_source_surface(&src, 0.0, 0.0).map_err(|e| e.to_string())?;
    c.paint().map_err(|e| e.to_string())?;
    Ok(dst)
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
) -> (Config, [FunctionLayer; 2], Vec<WidgetSpec>) {
    let mut base =
        toml::from_str::<ConfigProxy>(&read_to_string(BASE_CFG_PATH).unwrap()).unwrap();
    let mut config_error: Option<String> = None;
    for path in override_paths {
        match read_to_string(path) {
            Ok(text) => match toml::from_str::<ConfigProxy>(&text) {
                Ok(over) => base.merge(over),
                Err(e) => {
                    eprintln!("not-quite-tiny-dfr: {}: {e}", path.display());
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config.toml");
                    config_error.get_or_insert_with(|| short_error(name, &e.to_string()));
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => eprintln!("not-quite-tiny-dfr: cannot read {}: {e}", path.display()),
        }
    }

    let style_proxy = base.style.take().unwrap_or_default();
    let mut style = style_proxy.resolve();
    // Load the background image once here (we know the bar size), if it wins.
    if let Some(path) = style_proxy.image_path() {
        match load_background_image(path, width as i32, height as i32) {
            Ok(surf) => style.background_image = Some(surf),
            Err(e) => eprintln!("not-quite-tiny-dfr: background image {path:?}: {e}"),
        }
    }
    // Default icon size for buttons that don't set their own IconWidth/Height.
    let default_icon_size = style.icon_size.round() as i32;
    // Command widgets found while building the layers, with unique ids.
    let mut widgets = Vec::new();
    let mut next_id = 0;
    let layers = match &config_error {
        Some(message) => {
            // Unmistakable red banner; blend button fill into it and keep white
            // text so the message reads regardless of the user's colors.
            let red = Color::rgb(0.45, 0.0, 0.0);
            style.background = red;
            style.background_image = None; // don't let an image hide the error
            style.button_color = red;
            style.text_color = Color::rgb(1.0, 1.0, 1.0);
            [error_layer(message, width), error_layer(message, width)]
        }
        None => {
            let mut media_layer_keys = base.media_layer_keys.unwrap_or_else(default_media_layer);
            let mut primary_layer_keys =
                base.primary_layer_keys.unwrap_or_else(default_primary_layer);
            prepend_esc_if_needed(&mut media_layer_keys, width);
            prepend_esc_if_needed(&mut primary_layer_keys, width);
            let media_layer = FunctionLayer::with_config(
                media_layer_keys,
                &mut widgets,
                &mut next_id,
                default_icon_size,
            );
            let fkey_layer = FunctionLayer::with_config(
                primary_layer_keys,
                &mut widgets,
                &mut next_id,
                default_icon_size,
            );
            if base.media_layer_default.unwrap_or(DEFAULT_MEDIA_LAYER_DEFAULT) {
                [media_layer, fkey_layer]
            } else {
                [fkey_layer, media_layer]
            }
        }
    };
    let cfg = Config {
        show_button_outlines: base
            .show_button_outlines
            .unwrap_or(DEFAULT_SHOW_BUTTON_OUTLINES),
        enable_pixel_shift: base.enable_pixel_shift.unwrap_or(DEFAULT_ENABLE_PIXEL_SHIFT),
        adaptive_brightness: base.adaptive_brightness.unwrap_or(DEFAULT_ADAPTIVE_BRIGHTNESS),
        font_face: load_font(&resolve_font_pattern(
            base.font_family.as_deref(),
            base.font_bold,
            base.font_template.as_deref(),
        )),
        active_brightness: base.active_brightness.unwrap_or(DEFAULT_ACTIVE_BRIGHTNESS),
        double_press_switch_layers: base
            .double_press_switch_layers
            .unwrap_or(DEFAULT_DOUBLE_PRESS_SWITCH_LAYERS),
        dim_timeout: base.dim_timeout.unwrap_or(DEFAULT_DIM_TIMEOUT),
        off_timeout: base.off_timeout.unwrap_or(DEFAULT_OFF_TIMEOUT),
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
    pub fn load_config(&self) -> (Config, [FunctionLayer; 2], Vec<WidgetSpec>) {
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
        layers: &mut [FunctionLayer; 2],
    ) -> Option<Vec<WidgetSpec>> {
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
    fn handle_events(&mut self, cfg: &mut Config, layers: &mut [FunctionLayer; 2], evts: Result<Vec<InotifyEvent>, Errno>) -> Option<Vec<WidgetSpec>> {
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
            self.watches.iter().any(|w| {
                w.wd == Some(evt.wd) && evt.name.as_deref() == Some(w.name.as_os_str())
            })
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
    fn error_banner_keeps_esc_on_wide_panels() {
        // Wide panels get the auto Esc key; the error banner must keep it.
        assert_eq!(error_layer("config error", 2170).buttons.len(), 2); // esc + banner
        // Narrow panels have no auto Esc, so just the banner.
        assert_eq!(error_layer("config error", 1000).buttons.len(), 1);
    }

    #[test]
    fn short_error_is_single_line_and_bounded() {
        let e = short_error("config.toml", "line one\nline two\nline three");
        assert!(!e.contains('\n'));
        assert!(e.contains("config.toml"));
        assert!(e.chars().count() <= 78);
    }
}
