use serde::de::Error as SerdeError;
use serde::{Deserialize, Deserializer, Serialize};

use alacritty_config_derive::ConfigDeserialize;

use crate::display::color::{CellRgb, Rgb};

#[derive(ConfigDeserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Colors {
    pub primary: PrimaryColors,
    pub cursor: InvertedCellColors,
    pub vi_mode_cursor: InvertedCellColors,
    pub selection: InvertedCellColors,
    pub normal: NormalColors,
    pub bright: BrightColors,
    pub dim: Option<DimColors>,
    pub indexed_colors: Vec<IndexedColor>,
    pub search: SearchColors,
    pub line_indicator: LineIndicatorColors,
    pub hints: HintColors,
    pub transparent_background_colors: bool,
    pub draw_bold_text_with_bright_colors: bool,
    footer_bar: BarColors,
    pub tab_bar: TabBarColors,
}

impl Colors {
    pub fn footer_bar_foreground(&self) -> Rgb {
        self.footer_bar.foreground.unwrap_or(self.primary.background)
    }

    pub fn footer_bar_background(&self) -> Rgb {
        self.footer_bar.background.unwrap_or(self.primary.foreground)
    }

    pub fn tab_bar_active_foreground(&self) -> Rgb {
        self.tab_bar.active.foreground.unwrap_or(self.primary.foreground)
    }

    pub fn tab_bar_active_background(&self) -> Rgb {
        self.tab_bar.active.background.unwrap_or(self.primary.background)
    }

    pub fn tab_bar_inactive_foreground(&self) -> Rgb {
        self.tab_bar.inactive.foreground.unwrap_or_else(|| self.footer_bar_foreground())
    }

    pub fn tab_bar_inactive_background(&self) -> Rgb {
        self.tab_bar.inactive.background.unwrap_or_else(|| self.footer_bar_background())
    }
}

#[derive(ConfigDeserialize, Serialize, Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct LineIndicatorColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
}

#[derive(ConfigDeserialize, Serialize, Default, Copy, Clone, Debug, PartialEq, Eq)]
pub struct HintColors {
    pub start: HintStartColors,
    pub end: HintEndColors,
}

#[derive(ConfigDeserialize, Serialize, Copy, Clone, Debug, PartialEq, Eq)]
pub struct HintStartColors {
    pub foreground: CellRgb,
    pub background: CellRgb,
}

impl Default for HintStartColors {
    fn default() -> Self {
        Self {
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
            background: CellRgb::Rgb(Rgb::new(0xf4, 0xbf, 0x75)),
        }
    }
}

#[derive(ConfigDeserialize, Serialize, Copy, Clone, Debug, PartialEq, Eq)]
pub struct HintEndColors {
    pub foreground: CellRgb,
    pub background: CellRgb,
}

impl Default for HintEndColors {
    fn default() -> Self {
        Self {
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
            background: CellRgb::Rgb(Rgb::new(0xac, 0x42, 0x42)),
        }
    }
}

#[derive(Deserialize, Serialize, Copy, Clone, Default, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IndexedColor {
    pub color: Rgb,

    index: ColorIndex,
}

impl IndexedColor {
    #[inline]
    pub fn index(&self) -> u8 {
        self.index.0
    }
}

#[derive(Serialize, Copy, Clone, Default, Debug, PartialEq, Eq)]
struct ColorIndex(u8);

impl<'de> Deserialize<'de> for ColorIndex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let index = u8::deserialize(deserializer)?;

        if index < 16 {
            Err(SerdeError::custom(
                "Config error: indexed_color's index is {}, but a value bigger than 15 was \
                 expected; ignoring setting",
            ))
        } else {
            Ok(Self(index))
        }
    }
}

#[derive(ConfigDeserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct InvertedCellColors {
    #[config(alias = "text")]
    pub foreground: CellRgb,
    #[config(alias = "cursor")]
    pub background: CellRgb,
}

impl Default for InvertedCellColors {
    fn default() -> Self {
        Self { foreground: CellRgb::CellBackground, background: CellRgb::CellForeground }
    }
}

#[derive(ConfigDeserialize, Serialize, Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct SearchColors {
    pub focused_match: FocusedMatchColors,
    pub matches: MatchColors,
}

#[derive(ConfigDeserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct FocusedMatchColors {
    pub foreground: CellRgb,
    pub background: CellRgb,
}

impl Default for FocusedMatchColors {
    fn default() -> Self {
        Self {
            background: CellRgb::Rgb(Rgb::new(0xf4, 0xbf, 0x75)),
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
        }
    }
}

#[derive(ConfigDeserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct MatchColors {
    pub foreground: CellRgb,
    pub background: CellRgb,
}

impl Default for MatchColors {
    fn default() -> Self {
        Self {
            background: CellRgb::Rgb(Rgb::new(0xac, 0x42, 0x42)),
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
        }
    }
}

#[derive(ConfigDeserialize, Serialize, Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct BarColors {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
}

#[derive(ConfigDeserialize, Serialize, Default, Copy, Clone, Debug, PartialEq, Eq)]
pub struct TabBarColors {
    pub active: TabBarStateColors,
    pub inactive: TabBarStateColors,
}

#[derive(ConfigDeserialize, Serialize, Default, Copy, Clone, Debug, PartialEq, Eq)]
pub struct TabBarStateColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
}

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PrimaryColors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub bright_foreground: Option<Rgb>,
    pub dim_foreground: Option<Rgb>,
}

impl Default for PrimaryColors {
    fn default() -> Self {
        PrimaryColors {
            background: Rgb::new(0x18, 0x18, 0x18),
            foreground: Rgb::new(0xd8, 0xd8, 0xd8),
            bright_foreground: Default::default(),
            dim_foreground: Default::default(),
        }
    }
}

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct NormalColors {
    pub black: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub white: Rgb,
}

impl Default for NormalColors {
    fn default() -> Self {
        NormalColors {
            black: Rgb::new(0x18, 0x18, 0x18),
            red: Rgb::new(0xac, 0x42, 0x42),
            green: Rgb::new(0x90, 0xa9, 0x59),
            yellow: Rgb::new(0xf4, 0xbf, 0x75),
            blue: Rgb::new(0x6a, 0x9f, 0xb5),
            magenta: Rgb::new(0xaa, 0x75, 0x9f),
            cyan: Rgb::new(0x75, 0xb5, 0xaa),
            white: Rgb::new(0xd8, 0xd8, 0xd8),
        }
    }
}

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct BrightColors {
    pub black: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub white: Rgb,
}

impl Default for BrightColors {
    fn default() -> Self {
        // Generated with oklab by multiplying brightness by 1.12 and then adjusting numbers
        // to make them look "nicer". Yellow color was generated the same way, however the first
        // srgb representable color was picked.
        BrightColors {
            black: Rgb::new(0x6b, 0x6b, 0x6b),
            red: Rgb::new(0xc5, 0x55, 0x55),
            green: Rgb::new(0xaa, 0xc4, 0x74),
            yellow: Rgb::new(0xfe, 0xca, 0x88),
            blue: Rgb::new(0x82, 0xb8, 0xc8),
            magenta: Rgb::new(0xc2, 0x8c, 0xb8),
            cyan: Rgb::new(0x93, 0xd3, 0xc3),
            white: Rgb::new(0xf8, 0xf8, 0xf8),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::UiConfig;

    #[test]
    fn tab_bar_defaults() {
        let colors = Colors::default();

        assert_eq!(colors.tab_bar_active_foreground(), colors.primary.foreground);
        assert_eq!(colors.tab_bar_active_background(), colors.primary.background);
        assert_eq!(colors.tab_bar_inactive_foreground(), colors.footer_bar_foreground());
        assert_eq!(colors.tab_bar_inactive_background(), colors.footer_bar_background());
    }

    #[test]
    fn tab_bar_colors_parse() {
        let config = toml::from_str::<UiConfig>(concat!(
            "[colors.tab_bar.active]\n",
            "foreground = \"#010203\"\n",
            "background = \"#040506\"\n\n",
            "[colors.tab_bar.inactive]\n",
            "foreground = \"#070809\"\n",
            "background = \"#0a0b0c\"\n",
        ))
        .unwrap();

        assert_eq!(config.colors.tab_bar.active.foreground, Some(Rgb::new(0x01, 0x02, 0x03)));
        assert_eq!(config.colors.tab_bar.active.background, Some(Rgb::new(0x04, 0x05, 0x06)));
        assert_eq!(config.colors.tab_bar.inactive.foreground, Some(Rgb::new(0x07, 0x08, 0x09)));
        assert_eq!(config.colors.tab_bar.inactive.background, Some(Rgb::new(0x0a, 0x0b, 0x0c)));
    }

    #[test]
    fn invalid_tab_bar_color_parse() {
        assert!(toml::from_str::<Rgb>("\"not-a-color\"").is_err());
    }
}

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct DimColors {
    pub black: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub white: Rgb,
}

impl Default for DimColors {
    fn default() -> Self {
        // Generated with builtin alacritty's color dimming function.
        DimColors {
            black: Rgb::new(0x0f, 0x0f, 0x0f),
            red: Rgb::new(0x71, 0x2b, 0x2b),
            green: Rgb::new(0x5f, 0x6f, 0x3a),
            yellow: Rgb::new(0xa1, 0x7e, 0x4d),
            blue: Rgb::new(0x45, 0x68, 0x77),
            magenta: Rgb::new(0x70, 0x4d, 0x68),
            cyan: Rgb::new(0x4d, 0x77, 0x70),
            white: Rgb::new(0x8e, 0x8e, 0x8e),
        }
    }
}
