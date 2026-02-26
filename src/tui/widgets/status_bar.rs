use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::Screen;
use crate::tui::theme::ThemeColors;

/// Render a context-aware status bar with keybinding hints.
pub fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    screen: &Screen,
    search_active: bool,
    theme: &ThemeColors,
) {
    let hints = build_hints(screen, search_active, theme);
    let widget = Paragraph::new(hints);
    frame.render_widget(widget, area);
}

fn build_hints<'a>(screen: &Screen, search_active: bool, theme: &ThemeColors) -> Line<'a> {
    if search_active {
        return search_hints(theme);
    }

    match screen {
        Screen::List => list_hints(theme),
        Screen::Help => help_hints(theme),
        Screen::AddForm | Screen::EditForm(_) => form_hints(theme),
        Screen::DeleteConfirm(_) => delete_hints(theme),
    }
}

fn hint_pair<'a>(key: &str, action: &str, theme: &ThemeColors) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(theme.hint_key_fg)
                .bg(theme.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {action}  "), Style::default().fg(theme.fg_dim)),
    ]
}

fn list_hints(theme: &ThemeColors) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("\u{2191}\u{2193}/jk", "Navigate", theme));
    spans.extend(hint_pair("Enter", "Connect", theme));
    spans.extend(hint_pair("/", "Search", theme));
    spans.extend(hint_pair("a", "Add", theme));
    spans.extend(hint_pair("d", "Delete", theme));
    spans.extend(hint_pair("1-5", "Filter Env", theme));
    spans.extend(hint_pair("?", "Help", theme));
    spans.extend(hint_pair("q", "Quit", theme));
    Line::from(spans)
}

fn search_hints(theme: &ThemeColors) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Type", "Search", theme));
    spans.extend(hint_pair("Enter", "Done", theme));
    spans.extend(hint_pair("Esc", "Clear", theme));
    spans.extend(hint_pair("\u{2191}\u{2193}", "Navigate", theme));
    Line::from(spans)
}

fn form_hints(theme: &ThemeColors) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Tab/\u{2193}", "Next field", theme));
    spans.extend(hint_pair("S-Tab/\u{2191}", "Prev field", theme));
    spans.extend(hint_pair("Enter", "Save", theme));
    spans.extend(hint_pair("Esc", "Cancel", theme));
    Line::from(spans)
}

fn delete_hints(theme: &ThemeColors) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Enter", "Confirm", theme));
    spans.extend(hint_pair("Esc", "Cancel", theme));
    Line::from(spans)
}

fn help_hints(theme: &ThemeColors) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Esc/?", "Close", theme));
    spans.extend(hint_pair("q", "Quit", theme));
    Line::from(spans)
}
