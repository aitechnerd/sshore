use std::io::Write;

use crate::config::model::{Bookmark, Settings};

/// Apply terminal theming: set tab title and tab color based on bookmark environment.
pub fn apply_theme(bookmark: &Bookmark, settings: &Settings) {
    let title = render_tab_title(&settings.tab_title_template, bookmark, settings);
    set_tab_title(&title);

    if let Some(env_color) = settings.env_colors.get(&bookmark.env) {
        set_tab_color(&env_color.bg);
    }
}

/// Reset terminal tab title and color to defaults.
pub fn reset_theme() {
    reset_tab_title();
    reset_tab_color();
}

/// Set terminal tab title via OSC 0 (universally supported).
fn set_tab_title(title: &str) {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]0;{title}\x07");
    let _ = stdout.flush();
}

/// Reset terminal tab title to empty.
fn reset_tab_title() {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]0;\x07");
    let _ = stdout.flush();
}

/// Set terminal tab color using both Terminal.app (OSC 6) and iTerm2 (OSC 1337) codes.
/// Unknown OSC codes are silently ignored by terminals, so it's safe to emit both.
fn set_tab_color(hex_color: &str) {
    let Some((r, g, b)) = parse_hex_rgb(hex_color) else {
        return;
    };

    let mut stdout = std::io::stdout();
    // Terminal.app (macOS proprietary OSC 6)
    let _ = write!(stdout, "\x1b]6;1;bg;red;brightness;{r}\x07");
    let _ = write!(stdout, "\x1b]6;1;bg;green;brightness;{g}\x07");
    let _ = write!(stdout, "\x1b]6;1;bg;blue;brightness;{b}\x07");

    // iTerm2 (macOS proprietary OSC 1337)
    if let Some(hex_stripped) = hex_color.strip_prefix('#') {
        let _ = write!(stdout, "\x1b]1337;SetColors=tab={hex_stripped}\x07");
    }

    let _ = stdout.flush();
}

/// Reset terminal tab color for both Terminal.app and iTerm2.
fn reset_tab_color() {
    let mut stdout = std::io::stdout();
    // Terminal.app reset
    let _ = write!(stdout, "\x1b]6;1;bg;*;default\x07");
    // iTerm2 reset
    let _ = write!(stdout, "\x1b]1337;SetColors=tab=default\x07");
    let _ = stdout.flush();
}

/// Parse a hex color string "#RRGGBB" into (r, g, b) components.
fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Render tab title template with bookmark values.
/// Supported placeholders: {name}, {host}, {user}, {env}, {badge}, {label}
pub fn render_tab_title(template: &str, bookmark: &Bookmark, settings: &Settings) -> String {
    let env_color = settings.env_colors.get(&bookmark.env);
    template
        .replace("{name}", &bookmark.name)
        .replace("{host}", &bookmark.host)
        .replace("{user}", bookmark.user.as_deref().unwrap_or(""))
        .replace("{env}", &bookmark.env)
        .replace("{badge}", env_color.map_or("", |c| &c.badge))
        .replace("{label}", env_color.map_or("", |c| &c.label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bookmark() -> Bookmark {
        Bookmark {
            name: "prod-web-01".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "production".into(),
            tags: vec![],
            identity_file: None,
            proxy_jump: None,
            notes: None,
            last_connected: None,
            connect_count: 0,
        }
    }

    #[test]
    fn test_render_tab_title_default_template() {
        let settings = Settings::default();
        let bookmark = sample_bookmark();
        let result = render_tab_title(&settings.tab_title_template, &bookmark, &settings);
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("PROD"));
    }

    #[test]
    fn test_render_tab_title_all_placeholders() {
        let settings = Settings::default();
        let bookmark = sample_bookmark();
        let template = "{badge} {label} — {name} ({user}@{host}) [{env}]";
        let result = render_tab_title(template, &bookmark, &settings);
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("deploy"));
        assert!(result.contains("10.0.1.5"));
        assert!(result.contains("production"));
        assert!(result.contains("PROD"));
    }

    #[test]
    fn test_render_tab_title_no_user() {
        let settings = Settings::default();
        let mut bookmark = sample_bookmark();
        bookmark.user = None;
        let template = "{user}@{host}";
        let result = render_tab_title(template, &bookmark, &settings);
        assert_eq!(result, "@10.0.1.5");
    }

    #[test]
    fn test_render_tab_title_unknown_env() {
        let settings = Settings::default();
        let mut bookmark = sample_bookmark();
        bookmark.env = "custom".into();
        let template = "{badge} {label} — {name}";
        let result = render_tab_title(template, &bookmark, &settings);
        // No badge or label for unknown env
        assert_eq!(result, "  — prod-web-01");
    }

    #[test]
    fn test_parse_hex_rgb_valid() {
        assert_eq!(parse_hex_rgb("#CC0000"), Some((204, 0, 0)));
        assert_eq!(parse_hex_rgb("#FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex_rgb("#000000"), Some((0, 0, 0)));
    }

    #[test]
    fn test_parse_hex_rgb_invalid() {
        assert_eq!(parse_hex_rgb(""), None);
        assert_eq!(parse_hex_rgb("CC0000"), None);
        assert_eq!(parse_hex_rgb("#FFF"), None); // Too short
        assert_eq!(parse_hex_rgb("#GGGGGG"), None);
    }
}
