use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::tui::App;
use crate::tui::theme;
use crate::tui::widgets::env_badge;

/// Render the main bookmark list table.
pub fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    if app.config.bookmarks.is_empty() {
        render_empty_state(frame, area);
        return;
    }

    if app.filtered_indices.is_empty() {
        render_no_matches(frame, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Env").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Name").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Host").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Tags").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .filtered_indices
        .iter()
        .enumerate()
        .map(|(display_idx, &bookmark_idx)| {
            let bookmark = &app.config.bookmarks[bookmark_idx];
            let is_selected = display_idx == app.selected_index;

            let env_span = env_badge::env_badge_span(&bookmark.env, &app.config.settings);
            let env_cell = Cell::from(Line::from(vec![env_span]));

            let name_style = if is_selected {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::White)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let name_cell = Cell::from(bookmark.name.as_str()).style(name_style);

            let host_style = if is_selected {
                Style::default().fg(Color::White).bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let host_cell = Cell::from(bookmark.host.as_str()).style(host_style);

            let tags_text = if bookmark.tags.is_empty() {
                String::new()
            } else {
                bookmark.tags.join(", ")
            };
            let tags_style = if is_selected {
                Style::default().fg(Color::Gray).bg(Color::DarkGray)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let tags_cell = Cell::from(tags_text).style(tags_style);

            Row::new(vec![env_cell, name_cell, host_cell, tags_cell])
        })
        .collect();

    let widths = [
        Constraint::Length(env_badge::ENV_BADGE_WIDTH),
        Constraint::Percentage(30),
        Constraint::Percentage(35),
        Constraint::Percentage(35),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default()); // We handle highlighting per-cell

    // Use a table state for scrolling
    let mut state = TableState::default();
    state.select(Some(app.selected_index));

    frame.render_stateful_widget(table, area, &mut state);
}

/// Render empty state when no bookmarks exist.
fn render_empty_state(frame: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "No bookmarks yet.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Press 'a' to add one, or run:"),
        Line::from(Span::styled(
            "  sshore import",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("to import from ~/.ssh/config"),
    ]);

    let paragraph = Paragraph::new(text).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// Render message when search/filter yields no results.
fn render_no_matches(frame: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "No matching bookmarks.",
            Style::default().fg(Color::Yellow),
        )),
        Line::from("Press Esc to clear search, or 0 to clear filter."),
    ]);

    let paragraph = Paragraph::new(text).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// Render the environment filter indicator when active.
pub fn render_env_filter_indicator(
    frame: &mut Frame,
    area: Rect,
    env: &str,
    settings: &crate::config::model::Settings,
) {
    let (badge, label) = theme::env_badge_label(env, settings);
    let (fg, bg) = theme::env_style(env, settings);

    let line = Line::from(vec![
        Span::styled(" Filter: ", Style::default().fg(Color::Gray)),
        Span::styled(format!("{badge} {label}"), Style::new().fg(fg).bg(bg)),
        Span::styled(" (press 0 to clear) ", Style::default().fg(Color::DarkGray)),
    ]);

    let widget = Paragraph::new(line);
    frame.render_widget(widget, area);
}
