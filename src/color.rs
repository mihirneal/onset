//! Colors as written in experiment files: `"#808080"`, `"white"`, or `[128, 128, 128]`.
//!
//! Values go to the display unmodified (no sRGB/gamma conversion), the same as
//! E-Prime and Psychtoolbox, so `[128, 128, 128]` lights pixels at 128/255.

use serde::{Deserialize, Deserializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const BLACK: Color = Color::rgb8(0, 0, 0);
    pub const WHITE: Color = Color::rgb8(255, 255, 255);
    pub const GRAY: Color = Color::rgb8(128, 128, 128);

    pub const fn rgb8(r: u8, g: u8, b: u8) -> Color {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
    }

    pub fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    pub fn to_u8(self) -> [u8; 4] {
        let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [c(self.r), c(self.g), c(self.b), c(self.a)]
    }
}

impl FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let named = match s.trim().to_ascii_lowercase().as_str() {
            "black" => Some(Color::rgb8(0, 0, 0)),
            "white" => Some(Color::rgb8(255, 255, 255)),
            "gray" | "grey" => Some(Color::GRAY),
            "red" => Some(Color::rgb8(220, 40, 40)),
            "green" => Some(Color::rgb8(40, 180, 70)),
            "blue" => Some(Color::rgb8(50, 100, 230)),
            "yellow" => Some(Color::rgb8(240, 210, 40)),
            _ => None,
        };
        if let Some(c) = named {
            return Ok(c);
        }
        let hex = s.trim().strip_prefix('#').ok_or_else(|| {
            format!("unknown color {s:?} (use a name like \"white\", \"#rrggbb\", or [r, g, b])")
        })?;
        let digit = |i: usize, len: usize| {
            u8::from_str_radix(&hex[i..i + len], 16).map_err(|_| format!("invalid hex color {s:?}"))
        };
        let (r, g, b, a) = match hex.len() {
            3 => (digit(0, 1)? * 17, digit(1, 1)? * 17, digit(2, 1)? * 17, 255),
            6 => (digit(0, 2)?, digit(2, 2)?, digit(4, 2)?, 255),
            8 => (digit(0, 2)?, digit(2, 2)?, digit(4, 2)?, digit(6, 2)?),
            _ => return Err(format!("invalid hex color {s:?} (expected #rgb, #rrggbb or #rrggbbaa)")),
        };
        let mut c = Color::rgb8(r, g, b);
        c.a = a as f32 / 255.0;
        Ok(c)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Rgb(Vec<u8>),
        }
        match Raw::deserialize(d)? {
            Raw::Text(s) => s.parse().map_err(serde::de::Error::custom),
            Raw::Rgb(v) if v.len() == 3 => Ok(Color::rgb8(v[0], v[1], v[2])),
            Raw::Rgb(v) if v.len() == 4 => {
                let mut c = Color::rgb8(v[0], v[1], v[2]);
                c.a = v[3] as f32 / 255.0;
                Ok(c)
            }
            Raw::Rgb(_) => Err(serde::de::Error::custom("color arrays need 3 or 4 values (0-255)")),
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b, _] = self.to_u8();
        write!(f, "#{r:02x}{g:02x}{b:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_hex_and_short_hex() {
        assert_eq!("white".parse::<Color>().unwrap(), Color::WHITE);
        assert_eq!("#808080".parse::<Color>().unwrap(), Color::GRAY);
        assert_eq!("#fff".parse::<Color>().unwrap(), Color::WHITE);
        assert!("#12345".parse::<Color>().is_err());
        assert!("mauve".parse::<Color>().is_err());
    }
}
