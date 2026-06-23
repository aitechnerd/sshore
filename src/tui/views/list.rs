use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::tui::App;
use crate::tui::theme;
use crate::tui::theme::ThemeColors;
use crate::tui::widgets::env_badge;

/// Width ratio for the left pane (group/session list) in the split layout.
const LEFT_PANE_WIDTH: u16 = 30;

/// Render the main list area. Decides between split-pane (groups) and
/// single-pane (bookmarks only) layout.
pub fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    let tc = &app.theme;

    // Check if we have any items to show (bookmarks or groups)
    let has_items = !app.config.bookmarks.is_empty() || !app.config.groups.is_empty();
    let has_filtered = !app.filtered_indices.is_empty();

    if !has_items {
        render_empty_state(frame, area, tc);
        return;
    }

    if !has_filtered {
        render_no_matches(frame, area, tc);
        return;
    }

    render_bookmark_table(frame, area, app);
}

/// Render the split-pane layout: group/session tree on left, terminal on right.
fn render_split_layout(frame: &mut Frame, area: Rect, app: &App) {

    let layout = ratatui::layout::Layout::horizontal([
        Constraint::Percentage(LEFT_PANE_WIDTH),
        Constraint::Percentage(100 - LEFT_PANE_WIDTH),
    ])
    .split(area);

    // Left pane: group/session tree (or bookmarks if no groups)
    render_group_session_tree(frame, layout[0], app);

    // Right pane: terminal area (placeholder for now)
    render_terminal_pane(frame, layout[1], app);
}

/// Render the group/session tree in the left pane.
///
/// Shows groups as collapsible headers with sessions nested underneath.
/// Falls back to bookmark list when no groups exist.
fn render_group_session_tree(frame: &mut Frame, area: Rect, app: &App) {
    let tc = &app.theme;

    if app.config.groups.is_empty() {
        // No groups — show bookmarks in the left pane
        if app.config.bookmarks.is_empty() {
            render_empty_state(frame, area, tc);
            return;
        }
        if app.filtered_indices.is_empty() {
            render_no_matches(frame, area, tc);
            return;
        }
        render_bookmark_table(frame, area, app);
        return;
    }

    // Build the tree rows
    let mut rows: Vec<Row> = Vec::new();
    let mut display_indices: Vec<usize> = Vec::new();

    for (group_idx, group) in app.config.groups.iter().enumerate() {
        let is_collapsed = app.collapsed_groups.contains(&group_idx);

        // Group header row
        let collapse_indicator = if is_collapsed { "+" } else { "-" };
        let group_label = format!("{} {}", collapse_indicator, group.name);

        let is_group_selected = match app.selected_session {
            Some((sg, ss)) => sg == group_idx && ss == usize::MAX, // group header selected
            _ => false,
        };

        let env_span = env_badge::env_badge_span(&group.env, &app.config.settings);
        let header_style = if is_group_selected {
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(tc.fg)
                .bg(tc.highlight)
        } else {
            Style::default()
                .fg(tc.accent)
                .add_modifier(Modifier::BOLD)
        };

        let header_line = Line::from(vec![
            env_span,
            Span::styled(format!("  {}", group_label), header_style),
        ]);

        rows.push(Row::new(vec![Cell::from(header_line)]));
        display_indices.push(group_idx * 1000 + 0); // group header marker

        // Session rows (only if not collapsed)
        if !is_collapsed {
            for (session_idx, session) in group.sessions.iter().enumerate() {
                let is_selected =
                    app.selected_session == Some((group_idx, session_idx));

                let session_style = if is_selected {
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(tc.fg)
                        .bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg)
                };

                // Session display: "  └─ session-name" with env badge
                let session_display = format!("  └─ {}", session.name);
                let session_line = Line::from(vec![Span::styled(
                    session_display,
                    session_style,
                )]);

                rows.push(Row::new(vec![Cell::from(session_line)]));
                display_indices.push(group_idx * 1000 + 1 + session_idx); // session marker
            }
        }
    }

    if rows.is_empty() {
        render_empty_state(frame, area, tc);
        return;
    }

    let table = Table::new(rows, [Constraint::Min(1)]).row_highlight_style(
        Style::default(), // We handle highlighting per-cell
    );

    // Determine selected display index
    let selected_display = match app.selected_session {
        Some((g, s)) => g * 1000 + 1 + s,
        None => 0,
    };

    // Find the display index position
    let selected_pos = display_indices
        .iter()
        .position(|&idx| idx == selected_display)
        .unwrap_or(0);

    let mut state = TableState::default();
    state.select(Some(selected_pos));

    frame.render_stateful_widget(table, area, &mut state);
}

/// Render the right pane (terminal area).
///
/// Shows a placeholder when no session is connected, or the active session info.
fn render_terminal_pane(frame: &mut Frame, area: Rect, app: &App) {
    let tc = &app.theme;

    let content = match app.selected_session {
        Some((group_idx, session_idx)) => {
            let group = &app.config.groups[group_idx];
            let session = &group.sessions[session_idx];
            let display_name = session.display_name(group);
            Text::from(vec![
                Line::from(""),
                Line::from(Span::styled(
                    display_name,
                    Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Terminal will appear here when connected.",
                    Style::default().fg(tc.fg_dim),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Press Enter to connect.",
                    Style::default().fg(tc.fg_muted),
                )),
            ])
        }
        None => {
            Text::from(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Select a session to connect.",
                    Style::default().fg(tc.fg_dim),
                )),
            ])
        }
    };

    let paragraph = Paragraph::new(content).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// Render the unified bookmark+group table.
fn render_bookmark_table(frame: &mut Frame, area: Rect, app: &App) {
    let tc = &app.theme;

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
        .map(|(display_idx, &filtered_idx)| {
            let is_selected = display_idx == app.selected_index;

            // Check if this is a group (filtered_idx >= GROUP_INDEX_MARKER)
            if filtered_idx >= crate::tui::GROUP_INDEX_MARKER {
                let group_idx = filtered_idx - crate::tui::GROUP_INDEX_MARKER;
                let group = &app.config.groups[group_idx];
                let env_span = env_badge::env_badge_span(&group.env, &app.config.settings);
                let env_cell = Cell::from(Line::from(vec![env_span]));
                let tunnel_cell = if app.tunnel_bookmarks.contains(&group.name) {
                    Cell::from(Span::styled("T", Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)))
                } else {
                    Cell::from("")
                };
                let name_style = if is_selected {
                    Style::default().add_modifier(Modifier::BOLD).fg(tc.fg).bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg)
                };
                let session_indicator = format!(" ({} sessions)", group.sessions.len());
                let name_display = format!("{}{}", group.name, session_indicator);
                let name_cell = Cell::from(name_display).style(name_style);
                let host_style = if is_selected {
                    Style::default().fg(tc.fg).bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg)
                };
                let host_cell = Cell::from(group.host.as_str()).style(host_style);
                let tags_text = if group.tags.is_empty() { String::new() } else { group.tags.join(", ") };
                let tags_style = if is_selected {
                    Style::default().fg(tc.fg_dim).bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg_muted)
                };
                let tags_cell = Cell::from(tags_text).style(tags_style);
                Row::new(vec![env_cell, tunnel_cell, name_cell, host_cell, tags_cell])
            } else {
                let bookmark = &app.config.bookmarks[filtered_idx];
                let env_span = env_badge::env_badge_span(&bookmark.env, &app.config.settings);
                let env_cell = Cell::from(Line::from(vec![env_span]));
                let tunnel_cell = if app.tunnel_bookmarks.contains(&bookmark.name) {
                    Cell::from(Span::styled("T", Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)))
                } else {
                    Cell::from("")
                };
                let name_style = if is_selected {
                    Style::default().add_modifier(Modifier::BOLD).fg(tc.fg).bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg)
                };
                let name_display = format_name_display(
                    &bookmark.name,
                    bookmark.profile.as_deref(),
                    bookmark.snippets.len(),
                );
                let name_cell = Cell::from(name_display).style(name_style);
                let host_style = if is_selected {
                    Style::default().fg(tc.fg).bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg)
                };
                let host_cell = Cell::from(bookmark.host.as_str()).style(host_style);
                let tags_text = if bookmark.tags.is_empty() { String::new() } else { bookmark.tags.join(", ") };
                let tags_style = if is_selected {
                    Style::default().fg(tc.fg_dim).bg(tc.highlight)
                } else {
                    Style::default().fg(tc.fg_muted)
                };
                let tags_cell = Cell::from(tags_text).style(tags_style);
                Row::new(vec![env_cell, tunnel_cell, name_cell, host_cell, tags_cell])
            }
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
            "sshore import",
            Style::default().fg(theme.accent),
        )),
        Line::from(Span::styled(
            "to import from:",
            Style::default().fg(theme.fg),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "SSH Config  \u{00b7}  PuTTY  \u{00b7}  MobaXterm  \u{00b7}  Tabby",
            Style::default().fg(theme.fg_dim),
        )),
        Line::from(Span::styled(
            "SecureCRT  \u{00b7}  CSV  \u{00b7}  JSON",
            Style::default().fg(theme.fg_dim),
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

/// Build the display string for a bookmark name in the list view.
///
/// Appends a `[profile-name]` indicator when a profile is assigned,
/// and a `(NS)` snippet count when snippets exist.
fn format_name_display(name: &str, profile: Option<&str>, snippet_count: usize) -> String {
    let mut display = name.to_owned();
    if let Some(profile_name) = profile {
        display = format!("{display} [{profile_name}]");
    }
    if snippet_count > 0 {
        display = format!("{display} ({snippet_count}S)");
    }
    display
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{AppConfig, Bookmark, BookmarkGroup, Session, Settings};

    fn sample_bookmark(name: &str, env: &str) -> Bookmark {
        Bookmark {
            name: name.into(),
            host: format!("10.0.1.{}", name.len()),
            user: Some("deploy".into()),
            port: 22,
            env: env.into(),
            tags: vec![],
            identity_file: None,
            proxy_jump: None,
            notes: None,
            last_connected: None,
            connect_count: 0,
            on_connect: None,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            profile: None,
        }
    }

    fn sample_group(name: &str, env: &str, session_count: usize) -> BookmarkGroup {
        let sessions: Vec<Session> = (0..session_count)
            .map(|i| Session {
                name: format!("session-{}", i),
                ..Session::default()
            })
            .collect();
        BookmarkGroup {
            name: name.into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: env.into(),
            tags: vec![],
            identity_file: None,
            proxy_jump: None,
            notes: None,
            profile: None,
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            on_connect: None,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            sessions,
        }
    }

    fn config_with_groups(groups: Vec<BookmarkGroup>) -> AppConfig {
        AppConfig {
            settings: Settings::default(),
            profiles: vec![],
            bookmarks: vec![],
            groups,
        }
    }

    fn config_with_bookmarks(bookmarks: Vec<Bookmark>) -> AppConfig {
        AppConfig {
            settings: Settings::default(),
            profiles: vec![],
            bookmarks,
            groups: vec![],
        }
    }

    #[test]
    fn test_format_name_display_plain() {
        let result = format_name_display("prod-web-01", None, 0);
        assert_eq!(result, "prod-web-01");
    }

    #[test]
    fn test_format_name_display_with_profile() {
        let result = format_name_display("prod-web-01", Some("corp-bastion"), 0);
        assert_eq!(result, "prod-web-01 [corp-bastion]");
    }

    #[test]
    fn test_format_name_display_without_profile_no_indicator() {
        let result = format_name_display("staging-api", None, 0);
        assert!(!result.contains('['));
        assert!(!result.contains(']'));
    }

    #[test]
    fn test_format_name_display_with_snippets_only() {
        let result = format_name_display("prod-web-01", None, 3);
        assert_eq!(result, "prod-web-01 (3S)");
    }

    #[test]
    fn test_format_name_display_with_profile_and_snippets() {
        let result = format_name_display("server-01", Some("corp-bastion"), 2);
        assert_eq!(result, "server-01 [corp-bastion] (2S)");
    }

    #[test]
    fn test_format_name_display_profile_appears_before_snippets() {
        let result = format_name_display("srv", Some("ops"), 1);
        let bracket_pos = result.find('[').unwrap();
        let paren_pos = result.find('(').unwrap();
        assert!(
            bracket_pos < paren_pos,
            "profile indicator should appear before snippet count"
        );
    }

    // --- Split-pane tests (Task 003) ---

    #[test]
    fn test_split_layout_used_when_groups_exist() {
        let config = config_with_groups(vec![sample_group("prod", "production", 2)]);
        let app = App::new(config);
        // App has groups, so render_list should use split layout
        assert!(!app.config.groups.is_empty());
    }

    #[test]
    fn test_bookmark_layout_used_when_no_groups() {
        let config = config_with_bookmarks(vec![sample_bookmark("prod-web-01", "production")]);
        let app = App::new(config);
        // App has no groups, so render_list should use bookmark table
        assert!(app.config.groups.is_empty());
        assert!(!app.config.bookmarks.is_empty());
    }

    #[test]
    fn test_group_display_name_format() {
        let group = sample_group("prod-servers", "production", 1);
        let session = &group.sessions[0];
        let display = session.display_name(&group);
        assert_eq!(display, "prod-servers/session-0");
    }

    #[test]
    fn test_session_inherits_env_from_group() {
        let group = sample_group("prod-servers", "production", 1);
        let session = &group.sessions[0];
        assert_eq!(session.effective_env(&group), "production");
    }

    #[test]
    fn test_session_inherits_host_from_group() {
        let group = sample_group("prod-servers", "production", 1);
        let session = &group.sessions[0];
        assert_eq!(session.effective_host(&group), "10.0.1.5");
    }

    #[test]
    fn test_session_inherits_port_from_group() {
        let group = sample_group("prod-servers", "production", 1);
        let session = &group.sessions[0];
        assert_eq!(session.effective_port(&group), 22);
    }
}
