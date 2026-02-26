pub mod theme;
pub mod views;
pub mod widgets;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

use crate::config::model::AppConfig;
use crate::tui::views::{help, list};
use crate::tui::widgets::{search_bar, status_bar};

/// Duration before status messages auto-clear.
const STATUS_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Tick rate for UI updates.
const TICK_RATE: Duration = Duration::from_millis(100);

/// Number of items to jump with Page Up/Down.
const PAGE_JUMP: usize = 10;

/// TUI screen states.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // AddForm, EditForm, DeleteConfirm used in Phase 3
pub enum Screen {
    List,
    AddForm,
    EditForm(usize),
    DeleteConfirm(usize),
    Help,
}

/// Map number keys to environment filter values.
const ENV_FILTER_MAP: &[&str] = &[
    "",            // 0 = clear
    "production",  // 1
    "staging",     // 2
    "development", // 3
    "local",       // 4
    "testing",     // 5
];

/// Main application state.
pub struct App {
    pub config: AppConfig,
    pub screen: Screen,
    pub search_query: String,
    pub search_active: bool,
    pub filtered_indices: Vec<usize>,
    pub selected_index: usize,
    pub env_filter: Option<String>,
    pub should_quit: bool,
    pub status_message: Option<(String, Instant)>,
    matcher: SkimMatcherV2,
}

impl App {
    /// Create a new App from loaded config.
    pub fn new(config: AppConfig) -> Self {
        let matcher = SkimMatcherV2::default();
        let filtered_indices = search_bar::filter_bookmarks(&matcher, &config.bookmarks, "", None);

        Self {
            config,
            screen: Screen::List,
            search_query: String::new(),
            search_active: false,
            filtered_indices,
            selected_index: 0,
            env_filter: None,
            should_quit: false,
            status_message: None,
            matcher,
        }
    }

    /// Set a temporary status message that auto-clears.
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    /// Recompute filtered_indices based on current search query and env filter.
    fn refilter(&mut self) {
        self.filtered_indices = search_bar::filter_bookmarks(
            &self.matcher,
            &self.config.bookmarks,
            &self.search_query,
            self.env_filter.as_deref(),
        );
        // Clamp selection to valid range
        if self.filtered_indices.is_empty() {
            self.selected_index = 0;
        } else if self.selected_index >= self.filtered_indices.len() {
            self.selected_index = self.filtered_indices.len() - 1;
        }
    }

    /// Clear expired status messages.
    fn tick(&mut self) {
        if let Some((_, when)) = &self.status_message
            && when.elapsed() > STATUS_MESSAGE_TIMEOUT
        {
            self.status_message = None;
        }
    }
}

/// Launch the TUI, blocking until the user quits.
pub fn run(config: AppConfig) -> Result<()> {
    // Set up terminal
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;

    // Install panic hook that restores terminal state
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut app = App::new(config);
    let result = event_loop(&mut terminal, &mut app);

    // Always restore terminal state
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("Failed to leave alternate screen")?;
    terminal.show_cursor().context("Failed to show cursor")?;

    result
}

/// Main event loop: draw, poll, handle.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .context("Failed to draw frame")?;

        if event::poll(TICK_RATE).context("Failed to poll events")?
            && let Event::Key(key) = event::read().context("Failed to read event")?
        {
            handle_key_event(app, key);
        }

        app.tick();

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Route drawing to the appropriate view based on current screen.
fn draw(frame: &mut ratatui::Frame, app: &App) {
    let outer_block = Block::default()
        .title(" sshore ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = outer_block.inner(frame.area());
    frame.render_widget(outer_block, frame.area());

    // Layout: optional search bar + optional env filter + main content + status bar
    let has_search = app.search_active || !app.search_query.is_empty();
    let has_env_filter = app.env_filter.is_some();

    let mut constraints = Vec::new();
    if has_search {
        constraints.push(Constraint::Length(1)); // Search bar
    }
    if has_env_filter {
        constraints.push(Constraint::Length(1)); // Env filter indicator
    }
    constraints.push(Constraint::Min(3)); // Main content
    if let Some((ref msg, _)) = app.status_message
        && !msg.is_empty()
    {
        constraints.push(Constraint::Length(1)); // Status message
    }
    constraints.push(Constraint::Length(1)); // Status bar (keybinding hints)

    let chunks = Layout::vertical(constraints).split(inner);

    let mut chunk_idx = 0;

    // Search bar
    if has_search {
        search_bar::render_search_bar(
            frame,
            chunks[chunk_idx],
            &app.search_query,
            app.search_active,
        );
        chunk_idx += 1;
    }

    // Env filter indicator
    if let Some(ref env) = app.env_filter {
        list::render_env_filter_indicator(frame, chunks[chunk_idx], env, &app.config.settings);
        chunk_idx += 1;
    }

    // Main content area
    let content_area = chunks[chunk_idx];
    chunk_idx += 1;

    match app.screen {
        Screen::List | Screen::AddForm | Screen::EditForm(_) | Screen::DeleteConfirm(_) => {
            list::render_list(frame, content_area, app);
        }
        Screen::Help => {
            list::render_list(frame, content_area, app);
        }
    }

    // Status message
    if let Some((ref msg, _)) = app.status_message
        && !msg.is_empty()
    {
        let status_line = ratatui::text::Line::from(ratatui::text::Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Yellow),
        ));
        frame.render_widget(
            ratatui::widgets::Paragraph::new(status_line),
            chunks[chunk_idx],
        );
        chunk_idx += 1;
    }

    // Status bar (keybinding hints) — always last
    status_bar::render_status_bar(frame, chunks[chunk_idx], &app.screen, app.search_active);

    // Help overlay on top of everything
    if app.screen == Screen::Help {
        help::render_help(frame, frame.area());
    }
}

/// Handle a key event based on current screen and search state.
fn handle_key_event(app: &mut App, key: KeyEvent) {
    // Ctrl+C always quits
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match app.screen {
        Screen::Help => handle_help_key(app, key),
        Screen::List if app.search_active => handle_search_key(app, key),
        Screen::List => handle_list_key(app, key),
        // Phase 3 screens — Esc goes back to list for now
        Screen::AddForm | Screen::EditForm(_) | Screen::DeleteConfirm(_) => {
            if key.code == KeyCode::Esc {
                app.screen = Screen::List;
            }
        }
    }
}

/// Handle key events in the list view (not searching).
fn handle_list_key(app: &mut App, key: KeyEvent) {
    match key.code {
        // Quit
        KeyCode::Char('q') => app.should_quit = true,

        // Navigation
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Home | KeyCode::Char('g') => app.selected_index = 0,
        KeyCode::End => jump_to_end(app),
        KeyCode::Char('G') => jump_to_end(app),
        KeyCode::PageUp => move_selection(app, -(PAGE_JUMP as isize)),
        KeyCode::PageDown => move_selection(app, PAGE_JUMP as isize),

        // Ctrl+P / Ctrl+N navigation
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            move_selection(app, -1);
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            move_selection(app, 1);
        }

        // Search
        KeyCode::Char('/') => {
            app.search_active = true;
        }

        // Environment filter (number keys)
        KeyCode::Char(c @ '0'..='5') => {
            let idx = (c as u8 - b'0') as usize;
            if idx == 0 {
                app.env_filter = None;
            } else {
                app.env_filter = Some(ENV_FILTER_MAP[idx].to_string());
            }
            app.refilter();
        }

        // Actions
        KeyCode::Enter => {
            if !app.filtered_indices.is_empty() {
                app.set_status("SSH connection not yet implemented (Phase 4)");
            }
        }
        KeyCode::Char('a') => {
            app.set_status("Add bookmark not yet implemented (Phase 3)");
        }
        KeyCode::Char('e') => {
            if !app.filtered_indices.is_empty() {
                app.set_status("Edit bookmark not yet implemented (Phase 3)");
            }
        }
        KeyCode::Char('d') => {
            if !app.filtered_indices.is_empty() {
                app.set_status("Delete bookmark not yet implemented (Phase 3)");
            }
        }

        // Help
        KeyCode::Char('?') => app.screen = Screen::Help,

        _ => {}
    }
}

/// Handle key events while search is active.
fn handle_search_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Clear search and exit search mode
            app.search_query.clear();
            app.search_active = false;
            app.refilter();
        }
        KeyCode::Enter => {
            // Exit search mode, keep filter active
            app.search_active = false;
        }
        KeyCode::Backspace => {
            app.search_query.pop();
            app.refilter();
        }
        // Allow arrow navigation while searching (before Char catch-all)
        KeyCode::Up => move_selection(app, -1),
        KeyCode::Down => move_selection(app, 1),
        KeyCode::Char(c) => {
            app.search_query.push(c);
            app.refilter();
        }
        _ => {}
    }
}

/// Handle key events on the help overlay.
fn handle_help_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
            app.screen = Screen::List;
        }
        _ => {}
    }
}

/// Move the selection cursor by delta, clamping to valid range.
fn move_selection(app: &mut App, delta: isize) {
    if app.filtered_indices.is_empty() {
        return;
    }

    let max = app.filtered_indices.len() - 1;
    let new_index = app.selected_index as isize + delta;
    app.selected_index = new_index.clamp(0, max as isize) as usize;
}

/// Jump selection to the last item.
fn jump_to_end(app: &mut App) {
    if !app.filtered_indices.is_empty() {
        app.selected_index = app.filtered_indices.len() - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{Bookmark, Settings};

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
        }
    }

    fn sample_app() -> App {
        let config = AppConfig {
            settings: Settings::default(),
            bookmarks: vec![
                sample_bookmark("prod-web-01", "production"),
                sample_bookmark("staging-api", "staging"),
                sample_bookmark("dev-worker", "development"),
                sample_bookmark("local-docker", "local"),
                sample_bookmark("test-runner", "testing"),
            ],
        };
        App::new(config)
    }

    #[test]
    fn test_new_app_shows_all_bookmarks() {
        let app = sample_app();
        assert_eq!(app.filtered_indices.len(), 5);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.screen, Screen::List);
        assert!(!app.search_active);
    }

    #[test]
    fn test_move_selection_down() {
        let mut app = sample_app();
        move_selection(&mut app, 1);
        assert_eq!(app.selected_index, 1);
        move_selection(&mut app, 1);
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn test_move_selection_up() {
        let mut app = sample_app();
        app.selected_index = 3;
        move_selection(&mut app, -1);
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn test_move_selection_clamps_at_top() {
        let mut app = sample_app();
        move_selection(&mut app, -1);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_move_selection_clamps_at_bottom() {
        let mut app = sample_app();
        app.selected_index = 4;
        move_selection(&mut app, 1);
        assert_eq!(app.selected_index, 4);
    }

    #[test]
    fn test_page_jump() {
        let mut app = sample_app();
        move_selection(&mut app, PAGE_JUMP as isize);
        assert_eq!(app.selected_index, 4); // Clamped to last
    }

    #[test]
    fn test_jump_to_end() {
        let mut app = sample_app();
        jump_to_end(&mut app);
        assert_eq!(app.selected_index, 4);
    }

    #[test]
    fn test_env_filter() {
        let mut app = sample_app();
        app.env_filter = Some("production".to_string());
        app.refilter();
        assert_eq!(app.filtered_indices.len(), 1);
        assert_eq!(
            app.config.bookmarks[app.filtered_indices[0]].name,
            "prod-web-01"
        );
    }

    #[test]
    fn test_search_filter() {
        let mut app = sample_app();
        app.search_query = "web".to_string();
        app.refilter();
        assert!(!app.filtered_indices.is_empty());
        // "prod-web-01" should be in results
        let names: Vec<&str> = app
            .filtered_indices
            .iter()
            .map(|&i| app.config.bookmarks[i].name.as_str())
            .collect();
        assert!(names.contains(&"prod-web-01"));
    }

    #[test]
    fn test_combined_search_and_env_filter() {
        let mut app = sample_app();
        app.env_filter = Some("production".to_string());
        app.search_query = "web".to_string();
        app.refilter();
        assert_eq!(app.filtered_indices.len(), 1);
        assert_eq!(
            app.config.bookmarks[app.filtered_indices[0]].name,
            "prod-web-01"
        );
    }

    #[test]
    fn test_refilter_clamps_selection() {
        let mut app = sample_app();
        app.selected_index = 4;
        app.env_filter = Some("production".to_string());
        app.refilter();
        assert_eq!(app.selected_index, 0); // Clamped to 0 (only 1 result)
    }

    #[test]
    fn test_status_message() {
        let mut app = sample_app();
        app.set_status("Test message");
        assert!(app.status_message.is_some());
        let (msg, _) = app.status_message.as_ref().unwrap();
        assert_eq!(msg, "Test message");
    }

    #[test]
    fn test_empty_bookmarks() {
        let config = AppConfig::default();
        let app = App::new(config);
        assert!(app.filtered_indices.is_empty());
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_move_selection_with_empty_list() {
        let config = AppConfig::default();
        let mut app = App::new(config);
        move_selection(&mut app, 1); // Should not panic
        assert_eq!(app.selected_index, 0);
    }
}
