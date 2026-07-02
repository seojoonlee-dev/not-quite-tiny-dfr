use cairo::{Context, ImageSurface};
use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use std::fmt;

/// An RGBA color with components in the 0.0..=1.0 range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Color {
    pub const fn rgb(r: f64, g: f64, b: f64) -> Color {
        Color { r, g, b, a: 1.0 }
    }

    /// Set this color as the cairo source for subsequent drawing.
    pub fn set_source(&self, c: &Context) {
        c.set_source_rgba(self.r, self.g, self.b, self.a);
    }

    /// Parse a hex color string, returning `None` on error (for lenient callers
    /// like widget JSON output).
    pub fn parse_hex(s: &str) -> Option<Color> {
        Color::from_hex(s).ok()
    }

    /// Parse a `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` hex string.
    fn from_hex(s: &str) -> Result<Color, String> {
        let hex = s
            .strip_prefix('#')
            .ok_or_else(|| format!("color must start with '#': {s:?}"))?;
        // parse a two-char hex byte into a 0.0..=1.0 component
        let byte = |slice: &str| -> Result<f64, String> {
            u8::from_str_radix(slice, 16)
                .map(|v| v as f64 / 255.0)
                .map_err(|_| format!("invalid hex color: {s:?}"))
        };
        // expand a single nibble char ("a") into a full byte ("aa")
        let nibble = |i: usize| -> Result<f64, String> {
            let c = &hex[i..i + 1];
            byte(&format!("{c}{c}"))
        };
        let (r, g, b, a) = match hex.len() {
            3 => (nibble(0)?, nibble(1)?, nibble(2)?, 1.0),
            4 => (nibble(0)?, nibble(1)?, nibble(2)?, nibble(3)?),
            6 => (byte(&hex[0..2])?, byte(&hex[2..4])?, byte(&hex[4..6])?, 1.0),
            8 => (
                byte(&hex[0..2])?,
                byte(&hex[2..4])?,
                byte(&hex[4..6])?,
                byte(&hex[6..8])?,
            ),
            _ => return Err(format!("invalid hex color length: {s:?}")),
        };
        Ok(Color { r, g, b, a })
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Color, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ColorVisitor;
        impl<'de> Visitor<'de> for ColorVisitor {
            type Value = Color;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a hex color string like \"#rrggbb\" or \"#rrggbbaa\"")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Color, E> {
                Color::from_hex(value).map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_str(ColorVisitor)
    }
}

/// Whether the resolved background should be a solid color or an image. Records
/// which of `Background` / `BackgroundImage` was declared later so that one wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundSource {
    Color,
    Image,
}

/// Fully-resolved visual style used by the renderer.
#[derive(Clone)]
pub struct Style {
    pub background: Color,
    /// Pre-cropped, bar-sized background image; painted instead of `background`
    /// when present. Loaded by the config loader, which knows the bar size.
    pub background_image: Option<ImageSurface>,
    pub button_color: Color,
    /// Whether `ButtonColor` was explicitly set. When true the idle fill is drawn
    /// even with `ShowButtonOutlines = false`, so buttons can be tinted over a
    /// background image.
    pub button_color_set: bool,
    pub button_color_active: Color,
    pub text_color: Color,
    pub button_spacing: f64,
    /// Padding in px between the left/right screen edges and the first/last button.
    pub edge_padding: f64,
    pub corner_radius: f64,
    pub font_size: f64,
    /// Default icon size in px for buttons that don't set IconWidth/IconHeight.
    pub icon_size: f64,
    /// Vertical extent of buttons as a percentage (0..=100) of the bar height.
    pub height_percent: f64,
    pub battery_charging_color: Color,
    pub battery_low_color: Color,
}

impl Default for Style {
    fn default() -> Style {
        // Defaults reproduce the original hardcoded look.
        Style {
            background: Color::rgb(0.0, 0.0, 0.0),
            background_image: None,
            button_color: Color::rgb(0.2, 0.2, 0.2),
            button_color_set: false,
            button_color_active: Color::rgb(0.4, 0.4, 0.4),
            text_color: Color::rgb(1.0, 1.0, 1.0),
            button_spacing: 16.0,
            edge_padding: 0.0,
            corner_radius: 8.0,
            font_size: 32.0,
            icon_size: 48.0,
            height_percent: 90.0,
            battery_charging_color: Color::rgb(0.0, 0.7, 0.0),
            battery_low_color: Color::rgb(0.7, 0.0, 0.0),
        }
    }
}

/// Deserialized `[Style]` table. Every field is optional so the base and user
/// configs can be merged field-by-field before defaults are applied.
///
/// Deserialized by hand (rather than derived) so we can record which of
/// `Background` / `BackgroundImage` appears last in the table -- that one wins.
#[derive(Default)]
pub struct StyleProxy {
    pub background: Option<Color>,
    pub background_image: Option<String>,
    /// Whichever of `Background` / `BackgroundImage` was declared last.
    pub background_last: Option<BackgroundSource>,
    pub button_color: Option<Color>,
    pub button_color_active: Option<Color>,
    pub text_color: Option<Color>,
    pub button_spacing: Option<f64>,
    pub edge_padding: Option<f64>,
    pub corner_radius: Option<f64>,
    pub font_size: Option<f64>,
    pub icon_size: Option<f64>,
    pub height_percent: Option<f64>,
    pub battery_charging_color: Option<Color>,
    pub battery_low_color: Option<Color>,
}

impl<'de> Deserialize<'de> for StyleProxy {
    fn deserialize<D>(deserializer: D) -> Result<StyleProxy, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StyleVisitor;
        impl<'de> Visitor<'de> for StyleVisitor {
            type Value = StyleProxy;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("the [Style] table")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<StyleProxy, M::Error> {
                let mut s = StyleProxy::default();
                // Visiting entries in file order lets us honor "declared later wins".
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "Background" => {
                            s.background = Some(map.next_value()?);
                            s.background_last = Some(BackgroundSource::Color);
                        }
                        "BackgroundImage" => {
                            s.background_image = Some(map.next_value()?);
                            s.background_last = Some(BackgroundSource::Image);
                        }
                        "ButtonColor" => s.button_color = Some(map.next_value()?),
                        "ButtonColorActive" => s.button_color_active = Some(map.next_value()?),
                        "TextColor" => s.text_color = Some(map.next_value()?),
                        "ButtonSpacing" => s.button_spacing = Some(map.next_value()?),
                        "EdgePadding" => s.edge_padding = Some(map.next_value()?),
                        "CornerRadius" => s.corner_radius = Some(map.next_value()?),
                        "FontSize" => s.font_size = Some(map.next_value()?),
                        "IconSize" => s.icon_size = Some(map.next_value()?),
                        "HeightPercent" => s.height_percent = Some(map.next_value()?),
                        "BatteryChargingColor" => {
                            s.battery_charging_color = Some(map.next_value()?)
                        }
                        "BatteryLowColor" => s.battery_low_color = Some(map.next_value()?),
                        // Unknown keys are ignored (kept lenient on purpose).
                        _ => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(s)
            }
        }
        deserializer.deserialize_map(StyleVisitor)
    }
}

impl StyleProxy {
    /// Overlay `user` fields onto `self`, preferring user-provided values.
    pub fn merge(&mut self, user: StyleProxy) {
        self.background = user.background.or(self.background);
        if user.background_image.is_some() {
            self.background_image = user.background_image;
        }
        // A higher-precedence file that sets any background key also decides,
        // via its own declaration order, which of color/image wins.
        if user.background_last.is_some() {
            self.background_last = user.background_last;
        }
        self.button_color = user.button_color.or(self.button_color);
        self.button_color_active = user.button_color_active.or(self.button_color_active);
        self.text_color = user.text_color.or(self.text_color);
        self.button_spacing = user.button_spacing.or(self.button_spacing);
        self.edge_padding = user.edge_padding.or(self.edge_padding);
        self.corner_radius = user.corner_radius.or(self.corner_radius);
        self.font_size = user.font_size.or(self.font_size);
        self.icon_size = user.icon_size.or(self.icon_size);
        self.height_percent = user.height_percent.or(self.height_percent);
        self.battery_charging_color = user.battery_charging_color.or(self.battery_charging_color);
        self.battery_low_color = user.battery_low_color.or(self.battery_low_color);
    }

    /// Resolve into a concrete [`Style`], filling any unset field with its
    /// default. The background image (if any) is loaded separately by the config
    /// loader, which knows the bar size; see [`StyleProxy::image_path`].
    pub fn resolve(&self) -> Style {
        let d = Style::default();
        Style {
            background: self.background.unwrap_or(d.background),
            background_image: None,
            button_color: self.button_color.unwrap_or(d.button_color),
            button_color_set: self.button_color.is_some(),
            button_color_active: self.button_color_active.unwrap_or(d.button_color_active),
            text_color: self.text_color.unwrap_or(d.text_color),
            button_spacing: self.button_spacing.unwrap_or(d.button_spacing),
            edge_padding: self.edge_padding.unwrap_or(d.edge_padding).max(0.0),
            corner_radius: self.corner_radius.unwrap_or(d.corner_radius),
            font_size: self.font_size.unwrap_or(d.font_size),
            icon_size: self.icon_size.unwrap_or(d.icon_size).max(0.0),
            height_percent: self
                .height_percent
                .unwrap_or(d.height_percent)
                .clamp(0.0, 100.0),
            battery_charging_color: self
                .battery_charging_color
                .unwrap_or(d.battery_charging_color),
            battery_low_color: self.battery_low_color.unwrap_or(d.battery_low_color),
        }
    }

    /// The background image path, but only when the image should win over the
    /// color (declared later). `None` means "use the solid color".
    pub fn image_path(&self) -> Option<&str> {
        if self.background_last == Some(BackgroundSource::Image) {
            self.background_image.as_deref()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn parses_six_digit_hex() {
        let c = Color::from_hex("#ff8000").unwrap();
        assert!(approx(c.r, 1.0) && approx(c.g, 128.0 / 255.0) && approx(c.b, 0.0));
        assert!(approx(c.a, 1.0));
    }

    #[test]
    fn parses_alpha_and_short_forms() {
        assert!(approx(
            Color::from_hex("#00000080").unwrap().a,
            128.0 / 255.0
        ));
        assert_eq!(Color::from_hex("#fff").unwrap(), Color::rgb(1.0, 1.0, 1.0));
        assert_eq!(Color::from_hex("#f00f").unwrap(), Color::rgb(1.0, 0.0, 0.0));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Color::from_hex("ffffff").is_err()); // missing '#'
        assert!(Color::from_hex("#gg0000").is_err()); // non-hex
        assert!(Color::from_hex("#12345").is_err()); // bad length
    }

    #[test]
    fn background_declared_later_wins() {
        let img_last: StyleProxy =
            toml::from_str("Background = \"#000000\"\nBackgroundImage = \"bg.png\"").unwrap();
        assert_eq!(img_last.background_last, Some(BackgroundSource::Image));
        assert_eq!(img_last.image_path(), Some("bg.png"));

        let color_last: StyleProxy =
            toml::from_str("BackgroundImage = \"bg.png\"\nBackground = \"#000000\"").unwrap();
        assert_eq!(color_last.background_last, Some(BackgroundSource::Color));
        assert_eq!(color_last.image_path(), None); // color wins -> no image

        // Priority carries through a merge (higher-precedence layer decides).
        let mut base: StyleProxy = toml::from_str("BackgroundImage = \"bg.png\"").unwrap();
        let user: StyleProxy = toml::from_str("Background = \"#123456\"").unwrap();
        base.merge(user);
        assert_eq!(base.image_path(), None); // user declared color last -> color wins
    }

    #[test]
    fn merge_prefers_user_then_defaults_fill_rest() {
        let mut base = StyleProxy {
            button_spacing: Some(16.0),
            corner_radius: Some(8.0),
            ..Default::default()
        };
        let user = StyleProxy {
            button_spacing: Some(0.0),
            ..Default::default()
        };
        base.merge(user);
        let style = base.resolve();
        assert!(approx(style.button_spacing, 0.0)); // user override wins
        assert!(approx(style.corner_radius, 8.0)); // base retained
        assert!(approx(style.font_size, Style::default().font_size)); // default filled
    }

    #[test]
    fn height_percent_is_clamped() {
        let style = StyleProxy {
            height_percent: Some(999.0),
            ..Default::default()
        }
        .resolve();
        assert!(approx(style.height_percent, 100.0));
    }
}
