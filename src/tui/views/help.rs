use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::theme::ThemeColors;

/// Render a centered help overlay with all keybindings.
pub fn render_help(frame: &mut Frame, area: Rect, theme: &ThemeColors) {
    let popup = centered_rect(60, 70, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Keybindings ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let sections = build_help_sections(theme);
    let paragraph = Paragraph::new(sections);
    frame.render_widget(paragraph, inner);
}

fn build_help_sections(theme: &ThemeColors) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    section_header(&mut lines, "Navigation", theme);
    key_hint(
        &mut lines,
        "\u{2191} / k / Ctrl+P",
        "Move selection up",
        theme,
    );
    key_hint(
        &mut lines,
        "\u{2193} / j / Ctrl+N",
        "Move selection down",
        theme,
    );
    key_hint(&mut lines, "Home / g", "Jump to first item", theme);
    key_hint(&mut lines, "End / G", "Jump to last item", theme);
    key_hint(&mut lines, "Page Up", "Jump up 10 items", theme);
    key_hint(&mut lines, "Page Down", "Jump down 10 items", theme);
    lines.push(Line::from(""));

    section_header(&mut lines, "Actions", theme);
    key_hint(&mut lines, "Enter", "Connect to selected bookmark", theme);
    key_hint(&mut lines, "a", "Add new bookmark", theme);
    key_hint(&mut lines, "e", "Edit selected bookmark", theme);
    key_hint(&mut lines, "d", "Delete selected bookmark", theme);
    lines.push(Line::from(""));

    section_header(&mut lines, "Search & Filter", theme);
    key_hint(&mut lines, "/", "Toggle search mode", theme);
    key_hint(&mut lines, "1-5", "Filter by environment", theme);
    key_hint(&mut lines, "0", "Clear environment filter", theme);
    lines.push(Line::from(""));

    section_header(&mut lines, "Environment Filters", theme);
    key_hint(&mut lines, "1", "Production", theme);
    key_hint(&mut lines, "2", "Staging", theme);
    key_hint(&mut lines, "3", "Development", theme);
    key_hint(&mut lines, "4", "Local", theme);
    key_hint(&mut lines, "5", "Testing", theme);
    lines.push(Line::from(""));

    section_header(&mut lines, "General", theme);
    key_hint(&mut lines, "?", "Toggle this help", theme);
    key_hint(&mut lines, "q / Ctrl+C", "Quit", theme);

    lines
}

fn section_header(lines: &mut Vec<Line<'static>>, title: &str, theme: &ThemeColors) {
    lines.push(Line::from(Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
}

fn key_hint(lines: &mut Vec<Line<'static>>, key: &str, desc: &str, theme: &ThemeColors) {
    lines.push(Line::from(vec![
        Span::styled(
            format!("    {key:<20}"),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::default().fg(theme.fg)),
    ]));
}

/// Create a centered rectangle with given percentage width and height.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1]);

    horizontal[1]
}
