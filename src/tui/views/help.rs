use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::theme::ThemeColors;

/// Render a centered help overlay with all keybindings.
pub fn render_help(frame: &mut Frame, area: Rect, theme: &ThemeColors, scroll: u16) {
    let sections = build_help_sections(theme);
    let content_height = sections.len() as u16 + 2; // +2 for borders

    // Size to content, capped at 90% of terminal height
    let max_height = (area.height as u32 * 90 / 100) as u16;
    let popup_height = content_height.min(max_height);
    let popup = centered_rect_with_height(60, popup_height, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup);

    let is_scrollable = content_height > popup_height;
    let title_bottom = if is_scrollable {
        " \u{2191}\u{2193} scroll "
    } else {
        ""
    };

    let block = Block::default()
        .title(" Keybindings ")
        .title_alignment(Alignment::Center)
        .title_bottom(Line::from(title_bottom).alignment(Alignment::Center))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let paragraph = Paragraph::new(sections).scroll((scroll, 0));
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
    key_hint(&mut lines, "Enter", "SSH connect", theme);
    key_hint(&mut lines, "f", "SFTP file browser", theme);
    key_hint(&mut lines, "a", "Add new bookmark", theme);
    key_hint(&mut lines, "e", "Edit selected bookmark", theme);
    key_hint(&mut lines, "d", "Delete selected bookmark", theme);
    lines.push(Line::from(""));

    section_header(&mut lines, "In SSH Session", theme);
    key_hint(&mut lines, "~~ ", "Open snippet picker", theme);
    key_hint(&mut lines, "~b", "Save as bookmark", theme);
    key_hint(&mut lines, "~f", "Open file browser (SFTP)", theme);
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

/// Create a centered rectangle with given percentage width and fixed height.
fn centered_rect_with_height(percent_x: u16, height: u16, area: Rect) -> Rect {
    let v_pad = area.height.saturating_sub(height) / 2;
    let vertical = Layout::vertical([
        Constraint::Length(v_pad),
        Constraint::Length(height),
        Constraint::Min(0),
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
