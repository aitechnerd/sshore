use ratatui::style::Style;
use ratatui::text::Span;

use crate::config::model::Settings;
use crate::tui::theme;

/// Maximum display width for the env badge column (badge + space + label).
pub const ENV_BADGE_WIDTH: u16 = 9;

/// Create a styled Span for an environment badge cell in the bookmark table.
/// Renders as "{badge} {label}" (e.g., "🔴 PROD") with the env's fg/bg colors.
pub fn env_badge_span<'a>(env: &str, settings: &Settings) -> Span<'a> {
    let (fg, bg) = theme::env_style(env, settings);
    let (badge, label) = theme::env_badge_label(env, settings);
    let text = format!("{badge} {label}");

    Span::styled(text, Style::new().fg(fg).bg(bg))
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn test_env_badge_span_production() {
        let settings = Settings::default();
        let span = env_badge_span("production", &settings);
        assert!(span.content.contains("PROD"));
        assert_eq!(span.style.fg, Some(Color::Rgb(255, 255, 255)));
        assert_eq!(span.style.bg, Some(Color::Rgb(204, 0, 0)));
    }

    #[test]
    fn test_env_badge_span_empty() {
        let settings = Settings::default();
        let span = env_badge_span("", &settings);
        assert!(span.content.contains("-"));
    }

    #[test]
    fn test_env_badge_span_unknown() {
        let settings = Settings::default();
        let span = env_badge_span("custom", &settings);
        assert!(span.content.contains("CUSTOM"));
    }
}
