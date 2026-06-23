use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::config::model::{Bookmark, BookmarkGroup, Settings};
use crate::tui::theme;
use crate::tui::theme::ThemeColors;
use crate::tui::widgets::env_badge;

/// Unified target for the confirmation dialog.
#[derive(Debug, Clone)]
pub struct ConfirmTarget {
    pub name: String,
    pub host: String,
    pub env: String,
    pub session_count: usize, // 0 for bookmarks
    pub is_group: bool,
}

/// State for the delete confirmation dialog.
pub struct ConfirmState {
    pub target: ConfirmTarget,
    /// Whether this is a production item (requires typing "yes").
    pub is_production: bool,
    /// User's typed confirmation input (only used for production).
    pub input: String,
}

impl ConfirmState {
    /// Create a new confirmation state for the given bookmark.
    pub fn new(bookmark: &Bookmark) -> Self {
        let is_production = bookmark.env == "production";
        Self {
            target: ConfirmTarget {
                name: bookmark.name.clone(),
                host: bookmark.host.clone(),
                env: bookmark.env.clone(),
                session_count: 0,
                is_group: false,
            },
            is_production,
            input: String::new(),
        }
    }

    /// Create a new confirmation state for the given group.
    pub fn new_group(group: &BookmarkGroup) -> Self {
        let is_production = group.env == "production";
        Self {
            target: ConfirmTarget {
                name: group.name.clone(),
                host: group.host.clone(),
                env: group.env.clone(),
                session_count: group.sessions.len(),
                is_group: true,
            },
            is_production,
            input: String::new(),
        }
    }

    /// Check if the user has confirmed deletion.
    pub fn is_confirmed(&self) -> bool {
        if self.is_production {
            self.input.trim().eq_ignore_ascii_case("yes")
        } else {
            true // Non-production just needs Enter
        }
    }

    /// Insert a character into the confirmation input.
    pub fn insert_char(&mut self, c: char) {
        self.input.push(c);
    }

    /// Delete the last character from the confirmation input.
    pub fn delete_char(&mut self) {
        self.input.pop();
    }
}

/// Render the delete confirmation dialog as a centered overlay.
pub fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    state: &ConfirmState,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let popup = centered_rect(55, 40, area);
    frame.render_widget(Clear, popup);

    let (border_color, title, item_type) = if state.target.is_group {
        if state.is_production {
            let (_, bg) = theme::env_style("production", settings);
            (bg, " \u{26a0}\u{fe0f}  Delete PRODUCTION Group ", "group")
        } else {
            (tc.warning, " Delete Group ", "group")
        }
    } else {
        if state.is_production {
            let (_, bg) = theme::env_style("production", settings);
            (bg, " \u{26a0}\u{fe0f}  Delete PRODUCTION Bookmark ", "bookmark")
        } else {
            (tc.warning, " Delete Bookmark ", "bookmark")
        }
    };

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(tc.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    // Extract common fields from target
    let name = &state.target.name;
    let host = &state.target.host;
    let env = &state.target.env;

    if state.is_production {
        lines.push(Line::from(Span::styled(
            format!("  You are about to delete a PRODUCTION {}:", item_type),
            Style::default().fg(tc.error).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));

        let badge_span = env_badge::env_badge_span(env, settings);
        lines.push(Line::from(vec![
            Span::raw("  "),
            badge_span,
            Span::raw("  "),
            Span::styled(
                name.clone(),
                Style::default().fg(tc.fg).add_modifier(Modifier::BOLD),
            ),
        ]));
    } else {
        let _badge_span = env_badge::env_badge_span(env, settings);
        let type_label = item_type.to_uppercase();
        lines.push(Line::from(vec![
            Span::styled(format!("  Delete \""), Style::default().fg(tc.fg)),
            Span::styled(
                name.clone(),
                Style::default().fg(tc.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("\" ({})?", type_label), Style::default().fg(tc.fg)),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("  Host: ", Style::default().fg(tc.fg)),
        Span::styled(
            host.clone(),
            Style::default().fg(tc.fg_muted),
        ),
    ]));

    // Show session count for groups
    if state.target.session_count > 0 {
        lines.push(Line::from(vec![
            Span::styled(format!("  Sessions: {}", state.target.session_count), Style::default().fg(tc.fg_muted)),
        ]));
    }

    lines.push(Line::from(""));

    if state.is_production {
        lines.push(Line::from(vec![
            Span::styled(
                "  Type \"yes\" to confirm: ",
                Style::default().fg(tc.warning),
            ),
            Span::styled(
                format!("{}_", state.input),
                Style::default().fg(tc.fg).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [Esc] Cancel",
            Style::default().fg(tc.fg_muted),
        )));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                " Enter ",
                Style::default()
                    .fg(tc.hint_key_fg)
                    .bg(tc.hint_key_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Confirm    ", Style::default().fg(tc.fg_dim)),
            Span::styled(
                " Esc ",
                Style::default()
                    .fg(tc.hint_key_fg)
                    .bg(tc.hint_key_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Cancel", Style::default().fg(tc.fg_dim)),
        ]));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prod_bookmark() -> Bookmark {
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
            on_connect: None,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            profile: None,
        }
    }

    fn staging_bookmark() -> Bookmark {
        Bookmark {
            name: "staging-api".into(),
            host: "10.0.2.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "staging".into(),
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

    #[test]
    fn test_production_requires_yes() {
        let mut state = ConfirmState::new(&prod_bookmark());
        assert!(state.is_production);
        assert!(!state.is_confirmed());

        state.input = "no".into();
        assert!(!state.is_confirmed());

        state.input = "yes".into();
        assert!(state.is_confirmed());

        state.input = "YES".into();
        assert!(state.is_confirmed());

        state.input = " yes ".into();
        assert!(state.is_confirmed());
    }

    #[test]
    fn test_non_production_confirms_immediately() {
        let state = ConfirmState::new(&staging_bookmark());
        assert!(!state.is_production);
        assert!(state.is_confirmed());
    }

    #[test]
    fn test_confirm_char_input() {
        let mut state = ConfirmState::new(&prod_bookmark());
        state.insert_char('y');
        state.insert_char('e');
        state.insert_char('s');
        assert_eq!(state.input, "yes");
        assert!(state.is_confirmed());

        state.delete_char();
        assert_eq!(state.input, "ye");
        assert!(!state.is_confirmed());
    }

    #[test]
    fn test_confirm_target_bookmark_has_zero_sessions() {
        let state = ConfirmState::new(&prod_bookmark());
        assert!(!state.target.is_group);
        assert_eq!(state.target.session_count, 0);
    }

    #[test]
    fn test_confirm_target_group_shows_session_count() {
        use crate::config::model::{BookmarkGroup, Session};
        let group = BookmarkGroup {
            name: "test-group".into(),
            host: "10.0.1.5".into(),
            user: None,
            port: 22,
            env: "staging".into(),
            tags: vec![],
            identity_file: None,
            proxy_jump: None,
            notes: None,
            profile: None,
            on_connect: None,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            sessions: vec![
                Session { name: "s1".into(), ..Session::default() },
                Session { name: "s2".into(), ..Session::default() },
            ],
        };
        let state = ConfirmState::new_group(&group);
        assert!(state.target.is_group);
        assert_eq!(state.target.session_count, 2);
    }
}
