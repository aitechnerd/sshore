use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::Deserialize;

use crate::config::model::{EnvColor, Settings};

// ---------------------------------------------------------------------------
// ThemeColors — the 11 semantic colors that define a TUI theme
// ---------------------------------------------------------------------------

/// Semantic color palette used by all TUI rendering code.
///
/// These 11 fields map to every UI role: backgrounds, text tiers,
/// borders, accents, and keyboard hint badges. Individual render
/// functions pick the right field for each element — theme files
/// only need to specify the palette.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeColors {
    /// Popup/overlay/dialog background.
    pub surface: Color,
    /// Selected row / highlighted item background.
    pub highlight: Color,
    /// Primary text.
    pub fg: Color,
    /// Secondary text (hints, descriptions).
    pub fg_dim: Color,
    /// Tertiary text (unfocused labels, inactive elements).
    pub fg_muted: Color,
    /// Window borders, frames.
    pub border: Color,
    /// Focused labels, section headers, interactive highlights.
    pub accent: Color,
    /// Warnings, search highlights, status messages.
    pub warning: Color,
    /// Errors, destructive action warnings.
    pub error: Color,
    /// Keyboard hint badge foreground.
    pub hint_key_fg: Color,
    /// Keyboard hint badge background.
    pub hint_key_bg: Color,
}

// ---------------------------------------------------------------------------
// Built-in presets
// ---------------------------------------------------------------------------

/// Cool blue-toned dark theme. Default.
fn tokyo_night() -> ThemeColors {
    ThemeColors {
        surface: Color::Rgb(0x16, 0x16, 0x1e),
        highlight: Color::Rgb(0x29, 0x2e, 0x42),
        fg: Color::Rgb(0xc0, 0xca, 0xf5),
        fg_dim: Color::Rgb(0x73, 0x7a, 0xa2),
        fg_muted: Color::Rgb(0x56, 0x5f, 0x89),
        border: Color::Rgb(0x7a, 0xa2, 0xf7),
        accent: Color::Rgb(0x7d, 0xcf, 0xff),
        warning: Color::Rgb(0xe0, 0xaf, 0x68),
        error: Color::Rgb(0xf7, 0x76, 0x8e),
        hint_key_fg: Color::Rgb(0x16, 0x16, 0x1e),
        hint_key_bg: Color::Rgb(0x41, 0x48, 0x68),
    }
}

/// Warm pastel dark theme.
fn catppuccin_mocha() -> ThemeColors {
    ThemeColors {
        surface: Color::Rgb(0x18, 0x18, 0x25),
        highlight: Color::Rgb(0x31, 0x32, 0x44),
        fg: Color::Rgb(0xcd, 0xd6, 0xf4),
        fg_dim: Color::Rgb(0xa6, 0xad, 0xc8),
        fg_muted: Color::Rgb(0x6c, 0x70, 0x86),
        border: Color::Rgb(0x89, 0xb4, 0xfa),
        accent: Color::Rgb(0x94, 0xe2, 0xd5),
        warning: Color::Rgb(0xf9, 0xe2, 0xaf),
        error: Color::Rgb(0xf3, 0x8b, 0xa8),
        hint_key_fg: Color::Rgb(0x1e, 0x1e, 0x2e),
        hint_key_bg: Color::Rgb(0x58, 0x5b, 0x70),
    }
}

/// Purple-toned dark theme.
fn dracula() -> ThemeColors {
    ThemeColors {
        surface: Color::Rgb(0x21, 0x22, 0x2c),
        highlight: Color::Rgb(0x44, 0x47, 0x5a),
        fg: Color::Rgb(0xf8, 0xf8, 0xf2),
        fg_dim: Color::Rgb(0xbf, 0xbf, 0xbf),
        fg_muted: Color::Rgb(0x62, 0x72, 0xa4),
        border: Color::Rgb(0xbd, 0x93, 0xf9),
        accent: Color::Rgb(0x8b, 0xe9, 0xfd),
        warning: Color::Rgb(0xf1, 0xfa, 0x8c),
        error: Color::Rgb(0xff, 0x55, 0x55),
        hint_key_fg: Color::Rgb(0x28, 0x2a, 0x36),
        hint_key_bg: Color::Rgb(0x44, 0x47, 0x5a),
    }
}

/// ANSI named colors for maximum terminal compatibility.
fn default_ansi() -> ThemeColors {
    ThemeColors {
        surface: Color::Black,
        highlight: Color::DarkGray,
        fg: Color::White,
        fg_dim: Color::Gray,
        fg_muted: Color::DarkGray,
        border: Color::Cyan,
        accent: Color::Cyan,
        warning: Color::Yellow,
        error: Color::Red,
        hint_key_fg: Color::Black,
        hint_key_bg: Color::DarkGray,
    }
}

/// Look up a built-in preset by name.
fn builtin_theme(name: &str) -> Option<ThemeColors> {
    match name {
        "tokyo-night" => Some(tokyo_night()),
        "catppuccin-mocha" => Some(catppuccin_mocha()),
        "dracula" => Some(dracula()),
        "default" => Some(default_ansi()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Custom theme loading from TOML
// ---------------------------------------------------------------------------

/// TOML file structure: `[colors]` section with 11 hex color keys.
#[derive(Deserialize)]
struct ThemeFile {
    colors: ThemeColorsRaw,
}

/// Raw hex color strings from a theme TOML file.
#[derive(Deserialize)]
struct ThemeColorsRaw {
    surface: String,
    highlight: String,
    fg: String,
    fg_dim: String,
    fg_muted: String,
    border: String,
    accent: String,
    warning: String,
    error: String,
    hint_key_fg: String,
    hint_key_bg: String,
}

impl ThemeColorsRaw {
    /// Parse all hex strings into ratatui Colors.
    fn into_theme_colors(self) -> Result<ThemeColors> {
        Ok(ThemeColors {
            surface: parse_hex_required(&self.surface, "surface")?,
            highlight: parse_hex_required(&self.highlight, "highlight")?,
            fg: parse_hex_required(&self.fg, "fg")?,
            fg_dim: parse_hex_required(&self.fg_dim, "fg_dim")?,
            fg_muted: parse_hex_required(&self.fg_muted, "fg_muted")?,
            border: parse_hex_required(&self.border, "border")?,
            accent: parse_hex_required(&self.accent, "accent")?,
            warning: parse_hex_required(&self.warning, "warning")?,
            error: parse_hex_required(&self.error, "error")?,
            hint_key_fg: parse_hex_required(&self.hint_key_fg, "hint_key_fg")?,
            hint_key_bg: parse_hex_required(&self.hint_key_bg, "hint_key_bg")?,
        })
    }
}

/// Parse a hex color or return a descriptive error.
fn parse_hex_required(hex: &str, field: &str) -> Result<Color> {
    parse_hex_color(hex).with_context(|| format!("Invalid hex color for '{field}': {hex}"))
}

/// Directory where custom theme files are stored.
fn themes_dir() -> PathBuf {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("sshore").join("themes")
}

/// Load a custom theme TOML from `~/.config/sshore/themes/<name>.toml`.
fn load_custom_theme(name: &str) -> Result<ThemeColors> {
    let path = themes_dir().join(format!("{name}.toml"));
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read theme file: {}", path.display()))?;
    let file: ThemeFile = toml::from_str(&content)
        .with_context(|| format!("Failed to parse theme file: {}", path.display()))?;
    file.colors.into_theme_colors()
}

// ---------------------------------------------------------------------------
// Public API — theme resolution
// ---------------------------------------------------------------------------

/// Resolve a theme name to a `ThemeColors` palette.
///
/// Resolution order:
/// 1. Built-in presets (tokyo-night, catppuccin-mocha, dracula, default)
/// 2. Custom TOML file from `~/.config/sshore/themes/<name>.toml`
/// 3. Fall back to `default` (ANSI named colors) with a warning
pub fn resolve_theme(name: &str) -> ThemeColors {
    // 1. Built-in?
    if let Some(theme) = builtin_theme(name) {
        return theme;
    }

    // 2. Custom file?
    match load_custom_theme(name) {
        Ok(theme) => return theme,
        Err(e) => {
            eprintln!("Warning: could not load theme '{name}': {e:#}");
            eprintln!("Falling back to 'default' theme.");
        }
    }

    // 3. Fallback
    default_ansi()
}

// ---------------------------------------------------------------------------
// Environment colors — independent of TUI theme
// ---------------------------------------------------------------------------

/// Parse a hex color string (e.g. "#CC0000" or "#FFF") into a ratatui Color.
/// Returns None for invalid formats.
pub fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.strip_prefix('#')?;
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        3 => {
            // Short form: #FFF -> #FFFFFF
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

/// Fallback colors when hex parsing fails or env is unknown.
const DEFAULT_FG: Color = Color::White;
const DEFAULT_BG: Color = Color::DarkGray;

/// Resolve foreground and background ratatui Colors for an environment tier.
pub fn env_style(env: &str, settings: &Settings) -> (Color, Color) {
    if env.is_empty() {
        return (DEFAULT_FG, DEFAULT_BG);
    }

    settings
        .env_colors
        .get(env)
        .map(resolve_env_color)
        .unwrap_or((DEFAULT_FG, DEFAULT_BG))
}

/// Convert an EnvColor's hex strings to ratatui Colors.
fn resolve_env_color(ec: &EnvColor) -> (Color, Color) {
    let fg = parse_hex_color(&ec.fg).unwrap_or(DEFAULT_FG);
    let bg = parse_hex_color(&ec.bg).unwrap_or(DEFAULT_BG);
    (fg, bg)
}

/// Get the badge and label for an environment tier. Returns ("-", "-") for empty,
/// ("-", "ENV_NAME") for unknown envs.
pub fn env_badge_label(env: &str, settings: &Settings) -> (String, String) {
    if env.is_empty() {
        return ("-".to_string(), "-".to_string());
    }

    settings
        .env_colors
        .get(env)
        .map(|ec| (ec.badge.clone(), ec.label.clone()))
        .unwrap_or_else(|| ("-".to_string(), env.to_uppercase()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- hex parsing --

    #[test]
    fn test_parse_hex_color_6_digit() {
        assert_eq!(parse_hex_color("#CC0000"), Some(Color::Rgb(204, 0, 0)));
        assert_eq!(parse_hex_color("#FFFFFF"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_hex_color("#000000"), Some(Color::Rgb(0, 0, 0)));
        assert_eq!(parse_hex_color("#0066CC"), Some(Color::Rgb(0, 102, 204)));
    }

    #[test]
    fn test_parse_hex_color_3_digit() {
        assert_eq!(parse_hex_color("#FFF"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_hex_color("#000"), Some(Color::Rgb(0, 0, 0)));
        assert_eq!(parse_hex_color("#F00"), Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("CC0000"), None); // missing #
        assert_eq!(parse_hex_color("#GG0000"), None); // invalid hex
        assert_eq!(parse_hex_color("#12345"), None); // wrong length
    }

    // -- env colors (unchanged behavior) --

    #[test]
    fn test_env_style_known_env() {
        let settings = Settings::default();
        let (fg, bg) = env_style("production", &settings);
        assert_eq!(fg, Color::Rgb(255, 255, 255));
        assert_eq!(bg, Color::Rgb(204, 0, 0));
    }

    #[test]
    fn test_env_style_empty_env() {
        let settings = Settings::default();
        let (fg, bg) = env_style("", &settings);
        assert_eq!(fg, DEFAULT_FG);
        assert_eq!(bg, DEFAULT_BG);
    }

    #[test]
    fn test_env_style_unknown_env() {
        let settings = Settings::default();
        let (fg, bg) = env_style("custom-env", &settings);
        assert_eq!(fg, DEFAULT_FG);
        assert_eq!(bg, DEFAULT_BG);
    }

    #[test]
    fn test_env_badge_label_known() {
        let settings = Settings::default();
        let (badge, label) = env_badge_label("production", &settings);
        assert_eq!(badge, "\u{1f534}");
        assert_eq!(label, "PROD");
    }

    #[test]
    fn test_env_badge_label_empty() {
        let settings = Settings::default();
        let (badge, label) = env_badge_label("", &settings);
        assert_eq!(badge, "-");
        assert_eq!(label, "-");
    }

    #[test]
    fn test_env_badge_label_unknown() {
        let settings = Settings::default();
        let (badge, label) = env_badge_label("custom", &settings);
        assert_eq!(badge, "-");
        assert_eq!(label, "CUSTOM");
    }

    // -- theme resolution --

    #[test]
    fn test_builtin_tokyo_night() {
        let theme = builtin_theme("tokyo-night").unwrap();
        assert_eq!(theme.border, Color::Rgb(0x7a, 0xa2, 0xf7));
    }

    #[test]
    fn test_builtin_catppuccin_mocha() {
        let theme = builtin_theme("catppuccin-mocha").unwrap();
        assert_eq!(theme.border, Color::Rgb(0x89, 0xb4, 0xfa));
    }

    #[test]
    fn test_builtin_dracula() {
        let theme = builtin_theme("dracula").unwrap();
        assert_eq!(theme.border, Color::Rgb(0xbd, 0x93, 0xf9));
    }

    #[test]
    fn test_builtin_default() {
        let theme = builtin_theme("default").unwrap();
        assert_eq!(theme.border, Color::Cyan);
        assert_eq!(theme.fg, Color::White);
    }

    #[test]
    fn test_builtin_unknown_returns_none() {
        assert!(builtin_theme("nonexistent").is_none());
    }

    #[test]
    fn test_resolve_builtin() {
        let theme = resolve_theme("tokyo-night");
        assert_eq!(theme, tokyo_night());
    }

    #[test]
    fn test_resolve_unknown_falls_back_to_default() {
        let theme = resolve_theme("nonexistent-theme-xyz");
        assert_eq!(theme, default_ansi());
    }

    #[test]
    fn test_load_custom_theme_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let themes_path = dir.path().join("test-theme.toml");

        let toml_content = r##"
[colors]
surface = "#111111"
highlight = "#222222"
fg = "#333333"
fg_dim = "#444444"
fg_muted = "#555555"
border = "#666666"
accent = "#777777"
warning = "#888888"
error = "#999999"
hint_key_fg = "#aaaaaa"
hint_key_bg = "#bbbbbb"
"##;
        fs::write(&themes_path, toml_content).unwrap();

        let content = fs::read_to_string(&themes_path).unwrap();
        let file: ThemeFile = toml::from_str(&content).unwrap();
        let theme = file.colors.into_theme_colors().unwrap();

        assert_eq!(theme.surface, Color::Rgb(0x11, 0x11, 0x11));
        assert_eq!(theme.fg, Color::Rgb(0x33, 0x33, 0x33));
        assert_eq!(theme.error, Color::Rgb(0x99, 0x99, 0x99));
    }

    #[test]
    fn test_custom_theme_invalid_hex_fails() {
        let toml_content = r##"
[colors]
surface = "not-a-color"
highlight = "#222222"
fg = "#333333"
fg_dim = "#444444"
fg_muted = "#555555"
border = "#666666"
accent = "#777777"
warning = "#888888"
error = "#999999"
hint_key_fg = "#aaaaaa"
hint_key_bg = "#bbbbbb"
"##;
        let file: ThemeFile = toml::from_str(toml_content).unwrap();
        assert!(file.colors.into_theme_colors().is_err());
    }

    #[test]
    fn test_custom_theme_missing_field_fails() {
        let toml_content = r##"
[colors]
surface = "#111111"
fg = "#333333"
"##;
        let result: std::result::Result<ThemeFile, _> = toml::from_str(toml_content);
        assert!(result.is_err());
    }

    #[test]
    fn test_all_builtins_have_distinct_borders() {
        let themes: Vec<ThemeColors> =
            vec![tokyo_night(), catppuccin_mocha(), dracula(), default_ansi()];
        // All border colors should be unique across presets
        let borders: Vec<Color> = themes.iter().map(|t| t.border).collect();
        for (i, a) in borders.iter().enumerate() {
            for (j, b) in borders.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "Presets {i} and {j} have the same border color");
                }
            }
        }
    }
}
