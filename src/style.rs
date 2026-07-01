use cairo::Context;
use serde::{
    de::{self, Visitor},
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

/// Fully-resolved visual style used by the renderer.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub background: Color,
    pub button_color: Color,
    pub button_color_active: Color,
    pub text_color: Color,
    pub button_spacing: f64,
    pub corner_radius: f64,
    pub font_size: f64,
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
            button_color: Color::rgb(0.2, 0.2, 0.2),
            button_color_active: Color::rgb(0.4, 0.4, 0.4),
            text_color: Color::rgb(1.0, 1.0, 1.0),
            button_spacing: 16.0,
            corner_radius: 8.0,
            font_size: 32.0,
            height_percent: 70.0,
            battery_charging_color: Color::rgb(0.0, 0.7, 0.0),
            battery_low_color: Color::rgb(0.7, 0.0, 0.0),
        }
    }
}

/// Deserialized `[Style]` table. Every field is optional so that the base and
/// user configs can be merged field-by-field before defaults are applied.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct StyleProxy {
    pub background: Option<Color>,
    pub button_color: Option<Color>,
    pub button_color_active: Option<Color>,
    pub text_color: Option<Color>,
    pub button_spacing: Option<f64>,
    pub corner_radius: Option<f64>,
    pub font_size: Option<f64>,
    pub height_percent: Option<f64>,
    pub battery_charging_color: Option<Color>,
    pub battery_low_color: Option<Color>,
}

impl StyleProxy {
    /// Overlay `user` fields onto `self`, preferring user-provided values.
    pub fn merge(&mut self, user: StyleProxy) {
        self.background = user.background.or(self.background);
        self.button_color = user.button_color.or(self.button_color);
        self.button_color_active = user.button_color_active.or(self.button_color_active);
        self.text_color = user.text_color.or(self.text_color);
        self.button_spacing = user.button_spacing.or(self.button_spacing);
        self.corner_radius = user.corner_radius.or(self.corner_radius);
        self.font_size = user.font_size.or(self.font_size);
        self.height_percent = user.height_percent.or(self.height_percent);
        self.battery_charging_color = user.battery_charging_color.or(self.battery_charging_color);
        self.battery_low_color = user.battery_low_color.or(self.battery_low_color);
    }

    /// Resolve into a concrete [`Style`], filling any unset field with its default.
    pub fn resolve(self) -> Style {
        let d = Style::default();
        Style {
            background: self.background.unwrap_or(d.background),
            button_color: self.button_color.unwrap_or(d.button_color),
            button_color_active: self.button_color_active.unwrap_or(d.button_color_active),
            text_color: self.text_color.unwrap_or(d.text_color),
            button_spacing: self.button_spacing.unwrap_or(d.button_spacing),
            corner_radius: self.corner_radius.unwrap_or(d.corner_radius),
            font_size: self.font_size.unwrap_or(d.font_size),
            height_percent: self.height_percent.unwrap_or(d.height_percent).clamp(0.0, 100.0),
            battery_charging_color: self
                .battery_charging_color
                .unwrap_or(d.battery_charging_color),
            battery_low_color: self.battery_low_color.unwrap_or(d.battery_low_color),
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
        assert!(approx(Color::from_hex("#00000080").unwrap().a, 128.0 / 255.0));
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
