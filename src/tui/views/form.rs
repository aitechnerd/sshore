use std::path::Path;

use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::config::env::detect_env;
use crate::config::model::{
    AppConfig, Bookmark, Settings, validate_bookmark_name, validate_hostname,
};
use crate::tui::theme::ThemeColors;
use crate::tui::widgets::env_badge;

/// Number of editable fields in the form.
const FIELD_COUNT: usize = 10;

/// Environment options for the cycle selector.
const ENV_OPTIONS: &[&str] = &[
    "",
    "production",
    "staging",
    "development",
    "local",
    "testing",
];

/// Index of each form field.
const FIELD_NAME: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_USER: usize = 2;
const FIELD_PORT: usize = 3;
const FIELD_ENV: usize = 4;
const FIELD_TAGS: usize = 5;
const FIELD_IDENTITY: usize = 6;
const FIELD_PROXY: usize = 7;
const FIELD_NOTES: usize = 8;
const FIELD_ON_CONNECT: usize = 9;

/// Form state for add/edit bookmark.
pub struct FormState {
    pub fields: [String; FIELD_COUNT],
    pub focused: usize,
    pub env_index: usize,
    pub is_edit: bool,
    /// Original bookmark name (for edit mode uniqueness check).
    pub original_name: Option<String>,
    /// Validation error to display.
    pub error: Option<String>,
}

impl FormState {
    /// Create a blank form for adding a new bookmark.
    pub fn new_add(settings: &Settings) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_PORT] = "22".to_string();
        if let Some(ref user) = settings.default_user {
            fields[FIELD_USER] = user.clone();
        }

        Self {
            fields,
            focused: FIELD_NAME,
            env_index: 0, // (none)
            is_edit: false,
            original_name: None,
            error: None,
        }
    }

    /// Create a pre-populated form for editing an existing bookmark.
    pub fn new_edit(bookmark: &Bookmark) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_NAME] = bookmark.name.clone();
        fields[FIELD_HOST] = bookmark.host.clone();
        fields[FIELD_USER] = bookmark.user.clone().unwrap_or_default();
        fields[FIELD_PORT] = bookmark.port.to_string();
        fields[FIELD_TAGS] = bookmark.tags.join(", ");
        fields[FIELD_IDENTITY] = bookmark.identity_file.clone().unwrap_or_default();
        fields[FIELD_PROXY] = bookmark.proxy_jump.clone().unwrap_or_default();
        fields[FIELD_NOTES] = bookmark.notes.clone().unwrap_or_default();
        fields[FIELD_ON_CONNECT] = bookmark.on_connect.clone().unwrap_or_default();

        let env_index = ENV_OPTIONS
            .iter()
            .position(|&e| e == bookmark.env)
            .unwrap_or(0);

        Self {
            fields,
            focused: FIELD_NAME,
            env_index,
            is_edit: true,
            original_name: Some(bookmark.name.clone()),
            error: None,
        }
    }

    /// Move focus to the next field.
    pub fn next_field(&mut self) {
        if self.focused < FIELD_COUNT - 1 {
            self.focused += 1;
        }
    }

    /// Move focus to the previous field.
    pub fn prev_field(&mut self) {
        if self.focused > 0 {
            self.focused -= 1;
        }
    }

    /// Cycle environment selection forward.
    pub fn cycle_env_right(&mut self) {
        self.env_index = (self.env_index + 1) % ENV_OPTIONS.len();
    }

    /// Cycle environment selection backward.
    pub fn cycle_env_left(&mut self) {
        if self.env_index == 0 {
            self.env_index = ENV_OPTIONS.len() - 1;
        } else {
            self.env_index -= 1;
        }
    }

    /// Insert a character at the current field (except env, which uses cycling).
    pub fn insert_char(&mut self, c: char) {
        if self.focused == FIELD_ENV {
            return;
        }
        self.fields[self.focused].push(c);
        self.error = None;

        // Auto-detect env when name or host changes
        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Delete last character from the current field.
    pub fn delete_char(&mut self) {
        if self.focused == FIELD_ENV {
            return;
        }
        self.fields[self.focused].pop();
        self.error = None;

        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Auto-detect environment from name and host, updating env_index.
    fn auto_detect_env(&mut self) {
        let detected = detect_env(&self.fields[FIELD_NAME], &self.fields[FIELD_HOST]);
        if let Some(idx) = ENV_OPTIONS.iter().position(|&e| e == detected) {
            self.env_index = idx;
        }
    }

    /// Get the selected environment string.
    pub fn selected_env(&self) -> &str {
        ENV_OPTIONS[self.env_index]
    }

    /// Validate the form and build a Bookmark. Returns Err with a user-facing message on failure.
    pub fn validate_and_build(&mut self, config: &AppConfig) -> Result<Bookmark> {
        let name = self.fields[FIELD_NAME].trim().to_string();
        let host = self.fields[FIELD_HOST].trim().to_string();

        // Validate name
        validate_bookmark_name(&name)?;

        // Uniqueness check (skip for edit if name unchanged)
        let is_rename = self
            .original_name
            .as_ref()
            .is_some_and(|orig| orig != &name);
        if (!self.is_edit || is_rename) && config.bookmarks.iter().any(|b| b.name == name) {
            anyhow::bail!("A bookmark named '{}' already exists", name);
        }

        // Validate host
        validate_hostname(&host)?;

        // Validate port
        let port: u16 = self.fields[FIELD_PORT]
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Port must be a number between 1 and 65535"))?;
        if port == 0 {
            anyhow::bail!("Port must be between 1 and 65535");
        }

        // Parse tags
        let tags: Vec<String> = self.fields[FIELD_TAGS]
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        // Identity file: warn if provided but doesn't exist
        let identity_file = non_empty_option(&self.fields[FIELD_IDENTITY]);
        if let Some(ref path_str) = identity_file {
            let expanded = shellexpand::tilde(path_str).to_string();
            if !Path::new(&expanded).exists() {
                // Warn but allow — file might be on a different machine or not yet created
                self.error = Some(format!("Warning: identity file not found: {expanded}"));
            }
        }

        let user = non_empty_option(&self.fields[FIELD_USER]);
        let proxy_jump = non_empty_option(&self.fields[FIELD_PROXY]);
        let notes = non_empty_option(&self.fields[FIELD_NOTES]);
        let on_connect = non_empty_option(&self.fields[FIELD_ON_CONNECT]);
        let env = self.selected_env().to_string();

        Ok(Bookmark {
            name,
            host,
            user,
            port,
            env,
            tags,
            identity_file,
            proxy_jump,
            notes,
            last_connected: None,
            connect_count: 0,
            on_connect,
            snippets: vec![],
        })
    }
}

/// Convert a trimmed string to Option (None if empty).
fn non_empty_option(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Render the add/edit form as a centered overlay.
pub fn render_form(
    frame: &mut Frame,
    area: Rect,
    state: &FormState,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let popup = centered_rect(65, 80, area);
    frame.render_widget(Clear, popup);

    let title = if state.is_edit {
        " Edit Bookmark "
    } else {
        " Add Bookmark "
    };

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.border))
        .style(Style::default().bg(tc.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Layout: fields + optional error + hint line
    let field_count = FIELD_COUNT as u16;
    let mut constraints: Vec<Constraint> = (0..field_count)
        .map(|_| Constraint::Length(2)) // Each field: label + input on one line, gap
        .collect();
    if state.error.is_some() {
        constraints.push(Constraint::Length(1)); // Error line
    }
    constraints.push(Constraint::Min(0)); // Spacer
    constraints.push(Constraint::Length(1)); // Hints

    let chunks = Layout::vertical(constraints).split(inner);

    let field_labels = [
        "Name",
        "Host",
        "User",
        "Port",
        "Env",
        "Tags",
        "Identity File",
        "Proxy Jump",
        "Notes",
        "On-Connect",
    ];

    for (i, &label) in field_labels.iter().enumerate() {
        render_field(frame, chunks[i], label, i, state, settings, tc);
    }

    // Error message
    let mut hint_idx = FIELD_COUNT;
    if let Some(ref err) = state.error {
        let color = if err.starts_with("Warning:") {
            tc.warning
        } else {
            tc.error
        };
        let line = Line::from(Span::styled(format!(" {err}"), Style::default().fg(color)));
        frame.render_widget(Paragraph::new(line), chunks[hint_idx]);
        hint_idx += 1;
    }

    // Skip spacer
    hint_idx += 1;

    // Hints line
    let hints = Line::from(vec![
        Span::styled(
            " Tab/\u{2193} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Next  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " S-Tab/\u{2191} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Prev  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Enter ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Save  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Esc ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Cancel", Style::default().fg(tc.fg_dim)),
    ]);
    if hint_idx < chunks.len() {
        frame.render_widget(Paragraph::new(hints), chunks[hint_idx]);
    }
}

/// Render a single form field (label + value).
fn render_field(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    field_idx: usize,
    state: &FormState,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let is_focused = field_idx == state.focused;
    let [label_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Label
    let label_style = if is_focused {
        Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(tc.fg_dim)
    };
    let required = matches!(field_idx, FIELD_NAME | FIELD_HOST);
    let marker = if required { " *" } else { "" };
    let label_line = Line::from(Span::styled(format!("  {label}{marker}"), label_style));
    frame.render_widget(Paragraph::new(label_line), label_area);

    // Input value — special case for env field
    if field_idx == FIELD_ENV {
        render_env_selector(frame, input_area, state, settings, is_focused, tc);
    } else {
        let cursor = if is_focused { "_" } else { "" };
        let value = &state.fields[field_idx];
        let input_style = if is_focused {
            Style::default().fg(tc.fg)
        } else {
            Style::default().fg(tc.fg_muted)
        };
        let prefix = if is_focused { "  > " } else { "    " };
        let line = Line::from(Span::styled(
            format!("{prefix}{value}{cursor}"),
            input_style,
        ));
        frame.render_widget(Paragraph::new(line), input_area);
    }
}

/// Render the environment cycle selector as colored badges.
fn render_env_selector(
    frame: &mut Frame,
    area: Rect,
    state: &FormState,
    settings: &Settings,
    is_focused: bool,
    tc: &ThemeColors,
) {
    let mut spans: Vec<Span> = vec![Span::raw(if is_focused { "  > " } else { "    " })];

    for (i, &env) in ENV_OPTIONS.iter().enumerate() {
        let is_selected = i == state.env_index;

        if env.is_empty() {
            // "(none)" option
            let style = if is_selected {
                Style::default()
                    .fg(tc.fg)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(tc.fg_muted)
            };
            spans.push(Span::styled("(none)", style));
        } else {
            let span = env_badge::env_badge_span(env, settings);
            if is_selected {
                // Add underline to indicate selection
                let mut style = span.style;
                style = style.add_modifier(Modifier::UNDERLINED);
                spans.push(Span::styled(span.content.to_string(), style));
            } else if !is_focused {
                // Dim unselected options when not focused
                spans.push(Span::styled(
                    span.content.to_string(),
                    Style::default().fg(tc.fg_muted),
                ));
            } else {
                spans.push(span);
            }
        }
        spans.push(Span::raw(" "));
    }

    if is_focused {
        spans.push(Span::styled(
            " \u{2190}/\u{2192} to cycle",
            Style::default().fg(tc.fg_muted),
        ));
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
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

    fn sample_bookmark() -> Bookmark {
        Bookmark {
            name: "prod-web-01".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "production".into(),
            tags: vec!["web".into(), "frontend".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
            proxy_jump: Some("bastion".into()),
            notes: Some("Primary web server".into()),
            last_connected: None,
            connect_count: 0,
            on_connect: None,
            snippets: vec![],
        }
    }

    fn sample_config() -> AppConfig {
        AppConfig {
            settings: Settings::default(),
            bookmarks: vec![sample_bookmark()],
        }
    }

    #[test]
    fn test_new_add_form_defaults() {
        let settings = Settings {
            default_user: Some("admin".into()),
            ..Settings::default()
        };
        let form = FormState::new_add(&settings);
        assert!(!form.is_edit);
        assert_eq!(form.focused, FIELD_NAME);
        assert_eq!(form.fields[FIELD_PORT], "22");
        assert_eq!(form.fields[FIELD_USER], "admin");
        assert_eq!(form.env_index, 0); // (none)
    }

    #[test]
    fn test_new_edit_form_populates() {
        let bookmark = sample_bookmark();
        let form = FormState::new_edit(&bookmark);
        assert!(form.is_edit);
        assert_eq!(form.fields[FIELD_NAME], "prod-web-01");
        assert_eq!(form.fields[FIELD_HOST], "10.0.1.5");
        assert_eq!(form.fields[FIELD_USER], "deploy");
        assert_eq!(form.fields[FIELD_PORT], "22");
        assert_eq!(form.fields[FIELD_TAGS], "web, frontend");
        assert_eq!(form.fields[FIELD_IDENTITY], "~/.ssh/id_ed25519");
        assert_eq!(form.fields[FIELD_PROXY], "bastion");
        assert_eq!(form.fields[FIELD_NOTES], "Primary web server");
        assert_eq!(form.selected_env(), "production");
    }

    #[test]
    fn test_field_navigation() {
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        assert_eq!(form.focused, 0);

        form.next_field();
        assert_eq!(form.focused, 1);

        form.next_field();
        form.next_field();
        assert_eq!(form.focused, 3);

        form.prev_field();
        assert_eq!(form.focused, 2);

        // Clamp at top
        form.focused = 0;
        form.prev_field();
        assert_eq!(form.focused, 0);

        // Clamp at bottom
        form.focused = FIELD_COUNT - 1;
        form.next_field();
        assert_eq!(form.focused, FIELD_COUNT - 1);
    }

    #[test]
    fn test_env_cycling() {
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        assert_eq!(form.env_index, 0);

        form.cycle_env_right();
        assert_eq!(form.selected_env(), "production");

        form.cycle_env_right();
        assert_eq!(form.selected_env(), "staging");

        // Cycle wraps
        form.env_index = ENV_OPTIONS.len() - 1;
        form.cycle_env_right();
        assert_eq!(form.env_index, 0);

        // Left from 0 wraps to end
        form.cycle_env_left();
        assert_eq!(form.env_index, ENV_OPTIONS.len() - 1);
    }

    #[test]
    fn test_char_insert_and_delete() {
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.focused = FIELD_NAME;

        form.insert_char('a');
        form.insert_char('b');
        assert_eq!(form.fields[FIELD_NAME], "ab");

        form.delete_char();
        assert_eq!(form.fields[FIELD_NAME], "a");
    }

    #[test]
    fn test_env_field_ignores_typing() {
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.focused = FIELD_ENV;

        form.insert_char('x');
        assert_eq!(form.fields[FIELD_ENV], "");

        form.delete_char();
        assert_eq!(form.fields[FIELD_ENV], "");
    }

    #[test]
    fn test_validate_and_build_success() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PORT] = "2222".into();
        form.fields[FIELD_TAGS] = "web, api".into();
        form.env_index = 3; // development

        let bookmark = form.validate_and_build(&config).unwrap();
        assert_eq!(bookmark.name, "test-server");
        assert_eq!(bookmark.host, "10.0.1.5");
        assert_eq!(bookmark.port, 2222);
        assert_eq!(bookmark.tags, vec!["web", "api"]);
        assert_eq!(bookmark.env, "development");
        assert!(bookmark.user.is_none());
    }

    #[test]
    fn test_validate_empty_name_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_empty_host_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_NAME] = "test".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_invalid_port_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_NAME] = "test".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PORT] = "abc".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_duplicate_name_fails() {
        let config = sample_config();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_NAME] = "prod-web-01".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_validate_edit_same_name_succeeds() {
        let config = sample_config();
        let bookmark = sample_bookmark();
        let mut form = FormState::new_edit(&bookmark);

        let result = form.validate_and_build(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_edit_rename_to_existing_fails() {
        let mut config = sample_config();
        config.bookmarks.push(Bookmark {
            name: "other-server".into(),
            host: "10.0.2.1".into(),
            ..sample_bookmark()
        });

        let bookmark = sample_bookmark();
        let mut form = FormState::new_edit(&bookmark);
        form.fields[FIELD_NAME] = "other-server".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_shell_metachar_in_host_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_NAME] = "test".into();
        form.fields[FIELD_HOST] = "host;evil".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_auto_detect_env_on_name_input() {
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.focused = FIELD_NAME;
        for c in "prod-web".chars() {
            form.insert_char(c);
        }
        assert_eq!(form.selected_env(), "production");
    }

    #[test]
    fn test_non_empty_option() {
        assert_eq!(non_empty_option(""), None);
        assert_eq!(non_empty_option("  "), None);
        assert_eq!(non_empty_option("hello"), Some("hello".into()));
        assert_eq!(non_empty_option("  hello  "), Some("hello".into()));
    }

    #[test]
    fn test_validate_port_zero_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = FormState::new_add(&settings);
        form.fields[FIELD_NAME] = "test".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PORT] = "0".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }
}
