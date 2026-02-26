use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::Screen;

/// Render a context-aware status bar with keybinding hints.
pub fn render_status_bar(frame: &mut Frame, area: Rect, screen: &Screen, search_active: bool) {
    let hints = build_hints(screen, search_active);
    let widget = Paragraph::new(hints);
    frame.render_widget(widget, area);
}

fn build_hints(screen: &Screen, search_active: bool) -> Line<'static> {
    if search_active {
        return search_hints();
    }

    match screen {
        Screen::List => list_hints(),
        Screen::Help => help_hints(),
        // Phase 3 screens — show basic hints for now
        Screen::AddForm | Screen::EditForm(_) => form_hints(),
        Screen::DeleteConfirm(_) => delete_hints(),
    }
}

fn hint_pair(key: &str, action: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {action}  "), Style::default().fg(Color::Gray)),
    ]
}

fn list_hints() -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("\u{2191}\u{2193}/jk", "Navigate"));
    spans.extend(hint_pair("Enter", "Connect"));
    spans.extend(hint_pair("/", "Search"));
    spans.extend(hint_pair("a", "Add"));
    spans.extend(hint_pair("d", "Delete"));
    spans.extend(hint_pair("1-5", "Filter Env"));
    spans.extend(hint_pair("?", "Help"));
    spans.extend(hint_pair("q", "Quit"));
    Line::from(spans)
}

fn search_hints() -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Type", "Search"));
    spans.extend(hint_pair("Enter", "Done"));
    spans.extend(hint_pair("Esc", "Clear"));
    spans.extend(hint_pair("\u{2191}\u{2193}", "Navigate"));
    Line::from(spans)
}

fn form_hints() -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Tab/\u{2193}", "Next field"));
    spans.extend(hint_pair("S-Tab/\u{2191}", "Prev field"));
    spans.extend(hint_pair("Enter", "Save"));
    spans.extend(hint_pair("Esc", "Cancel"));
    Line::from(spans)
}

fn delete_hints() -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Enter", "Confirm"));
    spans.extend(hint_pair("Esc", "Cancel"));
    Line::from(spans)
}

fn help_hints() -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(hint_pair("Esc/?", "Close"));
    spans.extend(hint_pair("q", "Quit"));
    Line::from(spans)
}
