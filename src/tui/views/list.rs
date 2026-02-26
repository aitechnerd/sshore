use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::tui::App;
use crate::tui::theme;
use crate::tui::theme::ThemeColors;
use crate::tui::widgets::env_badge;

/// Render the main bookmark list table.
pub fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    let tc = &app.theme;

    if app.config.bookmarks.is_empty() {
        render_empty_state(frame, area, tc);
        return;
    }

    if app.filtered_indices.is_empty() {
        render_no_matches(frame, area, tc);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Env").style(Style::default().fg(tc.fg).add_modifier(Modifier::BOLD)),
        Cell::from("").style(Style::default()), // Tunnel indicator column (no header)
        Cell::from("Name").style(Style::default().fg(tc.fg).add_modifier(Modifier::BOLD)),
        Cell::from("Host").style(Style::default().fg(tc.fg).add_modifier(Modifier::BOLD)),
        Cell::from("Tags").style(Style::default().fg(tc.fg).add_modifier(Modifier::BOLD)),
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

            // Tunnel indicator: "T" if this bookmark has an active tunnel
            let tunnel_cell = if app.tunnel_bookmarks.contains(&bookmark.name) {
                Cell::from(Span::styled(
                    "T",
                    Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
                ))
            } else {
                Cell::from("")
            };

            let name_style = if is_selected {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(tc.fg)
                    .bg(tc.highlight)
            } else {
                Style::default().fg(tc.fg)
            };
            let name_cell = Cell::from(bookmark.name.as_str()).style(name_style);

            let host_style = if is_selected {
                Style::default().fg(tc.fg).bg(tc.highlight)
            } else {
                Style::default().fg(tc.fg)
            };
            let host_cell = Cell::from(bookmark.host.as_str()).style(host_style);

            let tags_text = if bookmark.tags.is_empty() {
                String::new()
            } else {
                bookmark.tags.join(", ")
            };
            let tags_style = if is_selected {
                Style::default().fg(tc.fg_dim).bg(tc.highlight)
            } else {
                Style::default().fg(tc.fg_muted)
            };
            let tags_cell = Cell::from(tags_text).style(tags_style);

            Row::new(vec![env_cell, tunnel_cell, name_cell, host_cell, tags_cell])
        })
        .collect();

    /// Width of the tunnel indicator column.
    const TUNNEL_COL_WIDTH: u16 = 2;

    let widths = [
        Constraint::Length(env_badge::ENV_BADGE_WIDTH),
        Constraint::Length(TUNNEL_COL_WIDTH),
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
fn render_empty_state(frame: &mut Frame, area: Rect, theme: &ThemeColors) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "No bookmarks yet.",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press 'a' to add one, or run:",
            Style::default().fg(theme.fg),
        )),
        Line::from(Span::styled(
            "  sshore import",
            Style::default().fg(theme.accent),
        )),
        Line::from(Span::styled(
            "to import from ~/.ssh/config",
            Style::default().fg(theme.fg),
        )),
    ]);

    let paragraph = Paragraph::new(text).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// Render message when search/filter yields no results.
fn render_no_matches(frame: &mut Frame, area: Rect, theme: &ThemeColors) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "No matching bookmarks.",
            Style::default().fg(theme.warning),
        )),
        Line::from(Span::styled(
            "Press Esc to clear search, or 0 to clear filter.",
            Style::default().fg(theme.fg),
        )),
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
    tc: &ThemeColors,
) {
    let (badge, label) = theme::env_badge_label(env, settings);
    let (fg, bg) = theme::env_style(env, settings);

    let line = Line::from(vec![
        Span::styled(" Filter: ", Style::default().fg(tc.fg_dim)),
        Span::styled(format!("{badge} {label}"), Style::new().fg(fg).bg(bg)),
        Span::styled(" (press 0 to clear) ", Style::default().fg(tc.fg_muted)),
    ]);

    let widget = Paragraph::new(line);
    frame.render_widget(widget, area);
}
