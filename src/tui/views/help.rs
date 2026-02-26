use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// Render a centered help overlay with all keybindings.
pub fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(60, 70, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Keybindings ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let sections = build_help_sections();
    let paragraph = Paragraph::new(sections);
    frame.render_widget(paragraph, inner);
}

fn build_help_sections() -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    section_header(&mut lines, "Navigation");
    key_hint(&mut lines, "\u{2191} / k / Ctrl+P", "Move selection up");
    key_hint(&mut lines, "\u{2193} / j / Ctrl+N", "Move selection down");
    key_hint(&mut lines, "Home / g", "Jump to first item");
    key_hint(&mut lines, "End / G", "Jump to last item");
    key_hint(&mut lines, "Page Up", "Jump up 10 items");
    key_hint(&mut lines, "Page Down", "Jump down 10 items");
    lines.push(Line::from(""));

    section_header(&mut lines, "Actions");
    key_hint(&mut lines, "Enter", "Connect to selected bookmark");
    key_hint(&mut lines, "a", "Add new bookmark");
    key_hint(&mut lines, "e", "Edit selected bookmark");
    key_hint(&mut lines, "d", "Delete selected bookmark");
    lines.push(Line::from(""));

    section_header(&mut lines, "Search & Filter");
    key_hint(&mut lines, "/", "Toggle search mode");
    key_hint(&mut lines, "1-5", "Filter by environment");
    key_hint(&mut lines, "0", "Clear environment filter");
    lines.push(Line::from(""));

    section_header(&mut lines, "Environment Filters");
    key_hint(&mut lines, "1", "Production");
    key_hint(&mut lines, "2", "Staging");
    key_hint(&mut lines, "3", "Development");
    key_hint(&mut lines, "4", "Local");
    key_hint(&mut lines, "5", "Testing");
    lines.push(Line::from(""));

    section_header(&mut lines, "General");
    key_hint(&mut lines, "?", "Toggle this help");
    key_hint(&mut lines, "q / Ctrl+C", "Quit");

    lines
}

fn section_header(lines: &mut Vec<Line<'static>>, title: &str) {
    lines.push(Line::from(Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
}

fn key_hint(lines: &mut Vec<Line<'static>>, key: &str, desc: &str) {
    lines.push(Line::from(vec![
        Span::styled(
            format!("    {key:<20}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::default().fg(Color::White)),
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
