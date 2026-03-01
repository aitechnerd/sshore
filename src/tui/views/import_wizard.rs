//! Interactive import wizard.
//!
//! Launched when `sshore import` is run without `--from` or `--file` flags.
//! Guides the user through source selection, file path input, and a preview
//! of bookmarks to import before writing config.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::config;
use crate::config::ImportSourceKind;
use crate::config::model::{AppConfig, Bookmark, Settings};
use crate::tui::theme::{ThemeColors, resolve_theme};
use crate::tui::widgets::env_badge;

// ─── Constants ───────────────────────────────────────────────────────────────

/// Tick rate for UI updates.
const TICK_RATE: std::time::Duration = std::time::Duration::from_millis(100);

/// Number of available import sources.
const SOURCE_COUNT: usize = 8;

/// Display names for each import source.
const SOURCE_LABELS: [&str; SOURCE_COUNT] = [
    "SSH Config",
    "PuTTY",
    "MobaXterm",
    "Tabby",
    "SecureCRT",
    "CSV",
    "JSON",
    "sshore TOML",
];

/// Format hints shown next to each source.
const SOURCE_HINTS: [&str; SOURCE_COUNT] = [
    "OpenSSH config (~/.ssh/config)",
    "Registry export (.reg file)",
    "Session export (.mxtsessions)",
    "Terminal config (config.yaml)",
    "XML session export",
    "CSV file (name,host,user,port,env)",
    "JSON bookmark array",
    "Exported sshore config",
];

// ─── Types ───────────────────────────────────────────────────────────────────

/// Result of a completed import wizard.
pub struct WizardResult {
    pub source_label: String,
    pub file_path: PathBuf,
    pub bookmarks: Vec<Bookmark>,
}

/// Current wizard step.
#[derive(Clone, PartialEq)]
enum Step {
    SourceSelect,
    PathInput,
    Preview,
}

/// A single entry in the preview list.
struct PreviewEntry {
    name: String,
    host: String,
    env: String,
    status: EntryStatus,
}

/// Import status for a preview entry.
#[derive(Clone, Debug, PartialEq)]
enum EntryStatus {
    New,
    Skip,
    Overwrite,
}

/// Internal wizard state.
struct WizardState {
    step: Step,
    selected_source: usize,
    path_input: String,
    error: Option<String>,
    preview_entries: Vec<PreviewEntry>,
    new_count: usize,
    skip_count: usize,
    overwrite_count: usize,
    scroll_offset: usize,
    /// Whether preview was reached via auto-detected path (affects back navigation).
    auto_detected: bool,
    parsed_bookmarks: Vec<Bookmark>,
    overwrite: bool,
    should_quit: bool,
    confirmed: bool,
    /// Whether this wizard was triggered by first-run (affects header and hint text).
    first_run: bool,
}

// ─── Terminal guard ──────────────────────────────────────────────────────────

/// Drop guard to restore terminal state on any exit path.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Map a source index to its `ImportSourceKind`.
fn source_kind(idx: usize) -> ImportSourceKind {
    match idx {
        0 => ImportSourceKind::SshConfig,
        1 => ImportSourceKind::Putty,
        2 => ImportSourceKind::Mobaxterm,
        3 => ImportSourceKind::Tabby,
        4 => ImportSourceKind::Securecrt,
        5 => ImportSourceKind::Csv,
        6 => ImportSourceKind::Json,
        7 => ImportSourceKind::Sshore,
        _ => unreachable!("invalid source index"),
    }
}

/// Return the known default file path for a source, if one is defined.
/// The path may or may not exist on disk — caller must check.
fn known_default_path(idx: usize) -> Option<PathBuf> {
    match idx {
        0 => Some(dirs::home_dir()?.join(".ssh").join("config")),
        3 => Some(dirs::config_dir()?.join("tabby").join("config.yaml")),
        _ => None,
    }
}

/// Build preview entries by comparing parsed bookmarks against existing ones.
fn build_preview(
    bookmarks: &[Bookmark],
    existing: &[Bookmark],
    overwrite: bool,
) -> (Vec<PreviewEntry>, usize, usize, usize) {
    let existing_names: HashSet<&str> = existing.iter().map(|b| b.name.as_str()).collect();
    let mut new_count = 0;
    let mut skip_count = 0;
    let mut overwrite_count = 0;

    let entries = bookmarks
        .iter()
        .map(|b| {
            let is_dup = existing_names.contains(b.name.as_str());
            let status = if !is_dup {
                new_count += 1;
                EntryStatus::New
            } else if overwrite {
                overwrite_count += 1;
                EntryStatus::Overwrite
            } else {
                skip_count += 1;
                EntryStatus::Skip
            };
            PreviewEntry {
                name: b.name.clone(),
                host: b.host.clone(),
                env: b.env.clone(),
                status,
            }
        })
        .collect();

    (entries, new_count, skip_count, overwrite_count)
}

/// Create a hint key + action span pair for status bars.
fn hint_pair(key: &str, action: &str, theme: &ThemeColors) -> Vec<Span<'static>> {
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

// ─── Public API ──────────────────────────────────────────────────────────────

/// Run the interactive import wizard.
///
/// Returns `Ok(Some(result))` if the user confirmed an import,
/// `Ok(None)` if cancelled.
///
/// When `first_run` is true, the wizard shows a welcome message and
/// "Skip" instead of "Quit" in the hint bar.
pub fn run_wizard(
    config: &AppConfig,
    overwrite: bool,
    env_override: Option<&str>,
    extra_tags: &[String],
    first_run: bool,
) -> Result<Option<WizardResult>> {
    let _guard = TermGuard;

    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen).context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;

    let theme = resolve_theme(&config.settings.theme);
    let mut state = WizardState {
        step: Step::SourceSelect,
        selected_source: 0,
        path_input: String::new(),
        error: None,
        preview_entries: Vec::new(),
        new_count: 0,
        skip_count: 0,
        overwrite_count: 0,
        scroll_offset: 0,
        auto_detected: false,
        parsed_bookmarks: Vec::new(),
        overwrite,
        should_quit: false,
        confirmed: false,
        first_run,
    };

    loop {
        terminal
            .draw(|frame| draw(frame, &state, &config.settings, &theme))
            .context("Failed to draw frame")?;

        if event::poll(TICK_RATE).context("Failed to poll events")?
            && let Event::Key(key) = event::read().context("Failed to read event")?
        {
            handle_key(&mut state, key, config, env_override, extra_tags);
        }

        if state.should_quit {
            return Ok(None);
        }
        if state.confirmed {
            let idx = state.selected_source;
            return Ok(Some(WizardResult {
                source_label: SOURCE_LABELS[idx].to_string(),
                file_path: PathBuf::from(&state.path_input),
                bookmarks: state.parsed_bookmarks,
            }));
        }
    }
}

// ─── Key handling ────────────────────────────────────────────────────────────

fn handle_key(
    state: &mut WizardState,
    key: KeyEvent,
    config: &AppConfig,
    env_override: Option<&str>,
    extra_tags: &[String],
) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.should_quit = true;
        return;
    }

    match state.step {
        Step::SourceSelect => handle_source_key(state, key, config, env_override, extra_tags),
        Step::PathInput => handle_path_key(state, key, config, env_override, extra_tags),
        Step::Preview => handle_preview_key(state, key),
    }
}

fn handle_source_key(
    state: &mut WizardState,
    key: KeyEvent,
    config: &AppConfig,
    env_override: Option<&str>,
    extra_tags: &[String],
) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.selected_source = state.selected_source.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.selected_source < SOURCE_COUNT - 1 {
                state.selected_source += 1;
            }
        }
        KeyCode::Enter => {
            let idx = state.selected_source;
            if let Some(path) = known_default_path(idx) {
                if path.exists() {
                    // AC-2: auto-detected path exists -> skip to preview
                    state.path_input = path.to_string_lossy().to_string();
                    state.auto_detected = true;
                    try_parse_and_preview(state, config, env_override, extra_tags);
                    return;
                }
                // AC-12: known path doesn't exist -> pre-fill path input
                state.path_input = path.to_string_lossy().to_string();
            } else {
                state.path_input.clear();
            }
            state.auto_detected = false;
            state.error = None;
            state.step = Step::PathInput;
        }
        // AC-11: Esc/q exits cleanly
        KeyCode::Esc | KeyCode::Char('q') => {
            state.should_quit = true;
        }
        _ => {}
    }
}

fn handle_path_key(
    state: &mut WizardState,
    key: KeyEvent,
    config: &AppConfig,
    env_override: Option<&str>,
    extra_tags: &[String],
) {
    match key.code {
        // AC-6: Esc returns to source selection
        KeyCode::Esc => {
            state.error = None;
            state.step = Step::SourceSelect;
        }
        KeyCode::Enter => {
            if state.path_input.trim().is_empty() {
                state.error = Some("File path cannot be empty".into());
                return;
            }
            try_parse_and_preview(state, config, env_override, extra_tags);
        }
        KeyCode::Backspace => {
            state.path_input.pop();
            state.error = None;
        }
        KeyCode::Char(c) => {
            state.path_input.push(c);
            state.error = None;
        }
        _ => {}
    }
}

fn handle_preview_key(state: &mut WizardState, key: KeyEvent) {
    match key.code {
        // AC-7: Esc returns to path input or source selection
        KeyCode::Esc => {
            if state.auto_detected {
                state.step = Step::SourceSelect;
            } else {
                state.step = Step::PathInput;
            }
        }
        // AC-3: Enter confirms import
        KeyCode::Enter => {
            if state.new_count > 0 || state.overwrite_count > 0 {
                state.confirmed = true;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !state.preview_entries.is_empty() {
                state.scroll_offset = state
                    .scroll_offset
                    .saturating_add(1)
                    .min(state.preview_entries.len().saturating_sub(1));
            }
        }
        _ => {}
    }
}

// ─── Parse & preview ─────────────────────────────────────────────────────────

/// Try to parse the file at `state.path_input` and transition to preview.
/// On failure, sets `state.error` and falls back to path input.
fn try_parse_and_preview(
    state: &mut WizardState,
    config: &AppConfig,
    env_override: Option<&str>,
    extra_tags: &[String],
) {
    let expanded = shellexpand::tilde(state.path_input.trim()).to_string();
    let path = PathBuf::from(&expanded);

    // AC-5: non-existent path shows inline error
    if !path.exists() {
        state.error = Some(format!("File not found: {}", path.display()));
        if state.auto_detected {
            // AC-12: auto-detected path doesn't exist -> fall through to path input
            state.auto_detected = false;
        }
        state.step = Step::PathInput;
        return;
    }

    state.path_input = expanded;

    let kind = source_kind(state.selected_source);
    let is_passthrough = matches!(kind, ImportSourceKind::SshConfig | ImportSourceKind::Sshore);

    match config::import_from_source(&path, kind, env_override, extra_tags) {
        Ok(mut bookmarks) => {
            // Apply env/tag overrides for passthrough sources (same logic as cmd_import)
            if let Some(env) = env_override
                && is_passthrough
            {
                for b in &mut bookmarks {
                    b.env = env.to_string();
                }
            }
            if !extra_tags.is_empty() && is_passthrough {
                for b in &mut bookmarks {
                    for tag in extra_tags {
                        if !b.tags.contains(tag) {
                            b.tags.push(tag.clone());
                        }
                    }
                }
            }

            let (entries, new_count, skip_count, overwrite_count) =
                build_preview(&bookmarks, &config.bookmarks, state.overwrite);

            state.preview_entries = entries;
            state.new_count = new_count;
            state.skip_count = skip_count;
            state.overwrite_count = overwrite_count;
            state.parsed_bookmarks = bookmarks;
            state.scroll_offset = 0;
            state.error = None;
            state.step = Step::Preview;
        }
        Err(e) => {
            state.error = Some(format!("Parse error: {e}"));
            if state.auto_detected {
                state.auto_detected = false;
            }
            state.step = Step::PathInput;
        }
    }
}

// ─── Drawing ─────────────────────────────────────────────────────────────────

fn draw(frame: &mut ratatui::Frame, state: &WizardState, settings: &Settings, theme: &ThemeColors) {
    let title = if state.first_run {
        " sshore "
    } else {
        " sshore import "
    };
    let outer = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    match state.step {
        Step::SourceSelect => draw_source_select(frame, inner, state, theme),
        Step::PathInput => draw_path_input(frame, inner, state, theme),
        Step::Preview => draw_preview(frame, inner, state, settings, theme),
    }
}

fn draw_source_select(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &WizardState,
    theme: &ThemeColors,
) {
    let mut constraints = vec![
        Constraint::Length(1), // top pad
    ];
    if state.first_run {
        constraints.push(Constraint::Length(1)); // welcome line
        constraints.push(Constraint::Length(1)); // gap
    }
    constraints.extend([
        Constraint::Length(1),                   // header
        Constraint::Length(1),                   // gap
        Constraint::Length(SOURCE_COUNT as u16), // source list
        Constraint::Min(0),                      // spacer
        Constraint::Length(1),                   // hints
    ]);

    let chunks = Layout::vertical(constraints).split(area);
    let mut idx = 1; // skip top pad

    // Welcome message (first-run only)
    if state.first_run {
        let welcome = Line::from(Span::styled(
            "  No bookmarks yet. Import from an existing source?",
            Style::default().fg(theme.fg_dim),
        ));
        frame.render_widget(Paragraph::new(welcome), chunks[idx]);
        idx += 2; // skip gap
    }

    // Header
    let header = Line::from(Span::styled(
        "  Select import source:",
        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(header), chunks[idx]);
    idx += 2; // skip gap

    // Source list
    let mut lines = Vec::new();
    for i in 0..SOURCE_COUNT {
        let selected = i == state.selected_source;
        let prefix = if selected { "  \u{25b8} " } else { "    " };
        let label_style = if selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let hint_style = if selected {
            Style::default().fg(theme.fg_dim)
        } else {
            Style::default().fg(theme.fg_muted)
        };

        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), label_style),
            Span::styled(format!("{:<16}", SOURCE_LABELS[i]), label_style),
            Span::styled(SOURCE_HINTS[i].to_string(), hint_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), chunks[idx]);

    // Hints
    let mut hints = Vec::new();
    hints.extend(hint_pair("\u{2191}\u{2193}/jk", "Navigate", theme));
    hints.extend(hint_pair("Enter", "Select", theme));
    if state.first_run {
        hints.extend(hint_pair("Esc", "Skip", theme));
    } else {
        hints.extend(hint_pair("Esc/q", "Quit", theme));
    }
    frame.render_widget(Paragraph::new(Line::from(hints)), chunks[chunks.len() - 1]);
}

fn draw_path_input(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &WizardState,
    theme: &ThemeColors,
) {
    let has_error = state.error.is_some();
    let mut constraints = vec![
        Constraint::Length(1), // top pad
        Constraint::Length(1), // source label
        Constraint::Length(1), // format hint
        Constraint::Length(1), // gap
        Constraint::Length(1), // "File path:" label
        Constraint::Length(1), // input
    ];
    if has_error {
        constraints.push(Constraint::Length(1)); // gap
        constraints.push(Constraint::Length(1)); // error
    }
    constraints.push(Constraint::Min(0)); // spacer
    constraints.push(Constraint::Length(1)); // hints

    let chunks = Layout::vertical(constraints).split(area);
    let mut idx = 1; // skip top pad

    // Source label
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  Import from: ", Style::default().fg(theme.fg)),
            Span::styled(
                SOURCE_LABELS[state.selected_source].to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        chunks[idx],
    );
    idx += 1;

    // Format hint (AC-4)
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  Format: {}", SOURCE_HINTS[state.selected_source]),
            Style::default().fg(theme.fg_muted),
        ))),
        chunks[idx],
    );
    idx += 2; // skip gap

    // "File path:" label
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  File path:",
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ))),
        chunks[idx],
    );
    idx += 1;

    // Input field with cursor
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  > {}_", state.path_input),
            Style::default().fg(theme.fg),
        ))),
        chunks[idx],
    );
    idx += 1;

    // Error message (AC-5)
    if let Some(ref err) = state.error {
        idx += 1; // gap
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(theme.error),
            ))),
            chunks[idx],
        );
        idx += 1;
    }

    idx += 1; // spacer

    // Hints
    let mut hints = Vec::new();
    hints.extend(hint_pair("Enter", "Confirm", theme));
    hints.extend(hint_pair("Esc", "Back", theme));
    frame.render_widget(Paragraph::new(Line::from(hints)), chunks[idx]);
}

fn draw_preview(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &WizardState,
    settings: &Settings,
    theme: &ThemeColors,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // top pad
        Constraint::Length(1), // source
        Constraint::Length(1), // file
        Constraint::Length(1), // gap
        Constraint::Length(1), // summary
        Constraint::Length(1), // gap
        Constraint::Min(3),    // entries
        Constraint::Length(1), // hints
    ])
    .split(area);

    // Source
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  Source: ", Style::default().fg(theme.fg)),
            Span::styled(
                SOURCE_LABELS[state.selected_source].to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        chunks[1],
    );

    // File
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  File: ", Style::default().fg(theme.fg)),
            Span::styled(
                state.path_input.clone(),
                Style::default().fg(theme.fg_muted),
            ),
        ])),
        chunks[2],
    );

    // Summary (AC-10: counts)
    let total = state.preview_entries.len();
    let summary_text = if total == 0 {
        "  No bookmarks found in file".to_string()
    } else {
        let mut parts = vec![format!(
            "  {} bookmark{}:",
            total,
            if total == 1 { "" } else { "s" }
        )];
        if state.new_count > 0 {
            parts.push(format!("{} new", state.new_count));
        }
        if state.skip_count > 0 {
            parts.push(format!("{} skip", state.skip_count));
        }
        if state.overwrite_count > 0 {
            parts.push(format!("{} overwrite", state.overwrite_count));
        }
        parts.join(", ")
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            summary_text,
            Style::default().fg(theme.fg),
        ))),
        chunks[4],
    );

    // Entries list with scrolling
    let entries_area = chunks[6];
    let visible = entries_area.height as usize;

    if !state.preview_entries.is_empty() {
        let max_offset = state.preview_entries.len().saturating_sub(visible);
        let offset = state.scroll_offset.min(max_offset);

        let mut lines = Vec::new();
        for entry in state.preview_entries.iter().skip(offset).take(visible) {
            // AC-10: duplicates clearly labeled as SKIP or OVERWRITE
            let (label, style) = match entry.status {
                EntryStatus::New => ("NEW", Style::default().fg(theme.accent)),
                EntryStatus::Skip => ("SKIP", Style::default().fg(theme.fg_muted)),
                EntryStatus::Overwrite => ("OVERWRITE", Style::default().fg(theme.warning)),
            };
            let badge = env_badge::env_badge_span(&entry.env, settings);

            lines.push(Line::from(vec![
                Span::styled(format!("  {:<10}", label), style),
                badge,
                Span::raw(" "),
                Span::styled(format!("{:<24}", entry.name), Style::default().fg(theme.fg)),
                Span::styled(entry.host.clone(), Style::default().fg(theme.fg_muted)),
            ]));
        }

        frame.render_widget(Paragraph::new(lines), entries_area);
    }

    // Hints
    let can_import = state.new_count > 0 || state.overwrite_count > 0;
    let scrollable = state.preview_entries.len() > visible;
    let mut hints = Vec::new();
    if can_import {
        hints.extend(hint_pair("Enter", "Import", theme));
    }
    hints.extend(hint_pair("Esc", "Back", theme));
    if scrollable {
        hints.extend(hint_pair("\u{2191}\u{2193}", "Scroll", theme));
    }
    frame.render_widget(Paragraph::new(Line::from(hints)), chunks[7]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::Bookmark;

    fn test_bookmark(name: &str, env: &str) -> Bookmark {
        Bookmark {
            name: name.into(),
            host: format!("{name}.example.com"),
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
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_source_kind_all_indices() {
        assert_eq!(source_kind(0), ImportSourceKind::SshConfig);
        assert_eq!(source_kind(1), ImportSourceKind::Putty);
        assert_eq!(source_kind(2), ImportSourceKind::Mobaxterm);
        assert_eq!(source_kind(3), ImportSourceKind::Tabby);
        assert_eq!(source_kind(4), ImportSourceKind::Securecrt);
        assert_eq!(source_kind(5), ImportSourceKind::Csv);
        assert_eq!(source_kind(6), ImportSourceKind::Json);
        assert_eq!(source_kind(7), ImportSourceKind::Sshore);
    }

    #[test]
    fn test_known_default_path_ssh() {
        let path = known_default_path(0);
        assert!(path.is_some());
        let p = path.unwrap();
        assert!(p.to_string_lossy().contains(".ssh"));
        assert!(p.to_string_lossy().ends_with("config"));
    }

    #[test]
    fn test_known_default_path_tabby() {
        let path = known_default_path(3);
        assert!(path.is_some());
        let p = path.unwrap();
        assert!(p.to_string_lossy().contains("tabby"));
    }

    #[test]
    fn test_known_default_path_none_for_others() {
        for idx in [1, 2, 4, 5, 6, 7] {
            assert!(
                known_default_path(idx).is_none(),
                "idx {idx} should be None"
            );
        }
    }

    #[test]
    fn test_build_preview_all_new() {
        let bookmarks = vec![
            test_bookmark("web-01", "production"),
            test_bookmark("db-01", "staging"),
        ];
        let existing: Vec<Bookmark> = vec![];

        let (entries, new, skip, overwrite) = build_preview(&bookmarks, &existing, false);

        assert_eq!(entries.len(), 2);
        assert_eq!(new, 2);
        assert_eq!(skip, 0);
        assert_eq!(overwrite, 0);
        assert_eq!(entries[0].status, EntryStatus::New);
        assert_eq!(entries[1].status, EntryStatus::New);
    }

    #[test]
    fn test_build_preview_with_duplicates_skip() {
        let bookmarks = vec![
            test_bookmark("web-01", "production"),
            test_bookmark("db-01", "staging"),
            test_bookmark("new-box", "development"),
        ];
        let existing = vec![
            test_bookmark("web-01", "production"),
            test_bookmark("db-01", "staging"),
        ];

        let (entries, new, skip, overwrite) = build_preview(&bookmarks, &existing, false);

        assert_eq!(entries.len(), 3);
        assert_eq!(new, 1);
        assert_eq!(skip, 2);
        assert_eq!(overwrite, 0);
        assert_eq!(entries[0].status, EntryStatus::Skip);
        assert_eq!(entries[1].status, EntryStatus::Skip);
        assert_eq!(entries[2].status, EntryStatus::New);
    }

    #[test]
    fn test_build_preview_with_duplicates_overwrite() {
        let bookmarks = vec![
            test_bookmark("web-01", "production"),
            test_bookmark("new-box", "development"),
        ];
        let existing = vec![test_bookmark("web-01", "production")];

        let (entries, new, skip, overwrite) = build_preview(&bookmarks, &existing, true);

        assert_eq!(entries.len(), 2);
        assert_eq!(new, 1);
        assert_eq!(skip, 0);
        assert_eq!(overwrite, 1);
        assert_eq!(entries[0].status, EntryStatus::Overwrite);
        assert_eq!(entries[1].status, EntryStatus::New);
    }

    #[test]
    fn test_build_preview_empty_input() {
        let bookmarks: Vec<Bookmark> = vec![];
        let existing = vec![test_bookmark("web-01", "production")];

        let (entries, new, skip, overwrite) = build_preview(&bookmarks, &existing, false);

        assert!(entries.is_empty());
        assert_eq!(new, 0);
        assert_eq!(skip, 0);
        assert_eq!(overwrite, 0);
    }

    #[test]
    fn test_build_preview_all_duplicates_no_overwrite() {
        let bookmarks = vec![
            test_bookmark("web-01", "production"),
            test_bookmark("db-01", "staging"),
        ];
        let existing = vec![
            test_bookmark("web-01", "production"),
            test_bookmark("db-01", "staging"),
        ];

        let (entries, new, skip, overwrite) = build_preview(&bookmarks, &existing, false);

        assert_eq!(entries.len(), 2);
        assert_eq!(new, 0);
        assert_eq!(skip, 2);
        assert_eq!(overwrite, 0);
    }
}
