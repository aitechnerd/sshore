use ratatui::style::Color;

use crate::config::model::{EnvColor, Settings};

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

/// Get the badge and label for an environment tier. Returns ("", "") for unknown envs.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
