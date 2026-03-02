pub mod theme;
pub mod views;
pub mod widgets;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};

use crate::config;
use crate::config::model::AppConfig;
use crate::config::ssh_import::merge_imports;
use crate::ssh;
use crate::tui::theme::{ThemeColors, resolve_theme};
use crate::tui::views::confirm::ConfirmState;
use crate::tui::views::form::FormState;
use crate::tui::views::{confirm, form, help, import_wizard, list};
use crate::tui::widgets::{search_bar, status_bar};

/// Duration before status messages auto-clear.
const STATUS_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Tick rate for UI updates.
const TICK_RATE: Duration = Duration::from_millis(100);

/// Number of items to jump with Page Up/Down.
const PAGE_JUMP: usize = 10;

/// TUI screen states.
#[derive(Debug, Clone, PartialEq)]
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

/// Action returned by the event loop to signal leaving the TUI for SSH.
enum LoopAction {
    Quit,
    Connect(usize),
}

/// Main application state.
pub struct App {
    pub config: AppConfig,
    pub theme: ThemeColors,
    pub screen: Screen,
    pub search_query: String,
    pub search_active: bool,
    pub filtered_indices: Vec<usize>,
    pub selected_index: usize,
    pub env_filter: Option<String>,
    pub should_quit: bool,
    pub status_message: Option<(String, Instant)>,
    pub form_state: Option<FormState>,
    pub confirm_state: Option<ConfirmState>,
    /// Bookmark names that have active tunnels (for TUI indicator).
    pub tunnel_bookmarks: HashSet<String>,
    /// Set when the user presses Enter to connect; signals the event loop to exit.
    connect_request: Option<usize>,
    /// Config file path override (from --config flag or SSHORE_CONFIG env var).
    config_path_override: Option<String>,
    matcher: SkimMatcherV2,
}

impl App {
    /// Create a new App from loaded config.
    pub fn new(config: AppConfig) -> Self {
        let matcher = SkimMatcherV2::default();
        let filtered_indices = search_bar::filter_bookmarks(&matcher, &config.bookmarks, "", None);
        let theme = resolve_theme(&config.settings.theme);
        let tunnel_bookmarks = crate::ssh::tunnel::active_tunnel_bookmarks();

        Self {
            config,
            theme,
            screen: Screen::List,
            search_query: String::new(),
            search_active: false,
            filtered_indices,
            selected_index: 0,
            env_filter: None,
            should_quit: false,
            status_message: None,
            form_state: None,
            confirm_state: None,
            tunnel_bookmarks,
            connect_request: None,
            config_path_override: None,
            matcher,
        }
    }

    /// Set the config path override for saving.
    pub fn with_config_override(mut self, path: Option<&str>) -> Self {
        self.config_path_override = path.map(|s| s.to_string());
        self
    }

    /// Save the config, respecting any path override.
    fn save_config(&self) -> anyhow::Result<()> {
        config::save_with_override(&self.config, self.config_path_override.as_deref())
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

    /// Get the bookmark index in config.bookmarks for the currently selected filtered item.
    fn selected_bookmark_index(&self) -> Option<usize> {
        if self.filtered_indices.is_empty() {
            None
        } else {
            Some(self.filtered_indices[self.selected_index])
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

/// Enter the alternate screen and set up the terminal for TUI rendering.
fn enter_tui() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    drain_events();
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("Failed to create terminal")
}

/// Drain any stale events from crossterm's input queue.
/// Called after re-entering raw mode (e.g. returning from SSH) to prevent
/// leftover key-release or resize events from swallowing the first real keypress.
fn drain_events() {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
}

/// Leave the alternate screen and restore the terminal for normal I/O.
fn leave_tui(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("Failed to leave alternate screen")?;
    terminal.show_cursor().context("Failed to show cursor")?;
    Ok(())
}

/// Launch the TUI, blocking until the user quits.
/// Loops: TUI -> SSH connect -> TUI, allowing repeated connections.
///
/// Terminal cleanup on panic is handled by `setup_panic_hook()` in main.rs,
/// which covers raw mode, alternate screen, cursor, colors, and SSH theming.
pub async fn run(config: &mut AppConfig, cfg_override: Option<&str>) -> Result<()> {
    // First-run import wizard: show when no bookmarks and not previously dismissed
    if config.bookmarks.is_empty() && !config.settings.import_wizard_dismissed {
        match import_wizard::run_wizard(config, false, None, &[], true)? {
            Some(result) => {
                let imported = merge_imports(&mut config.bookmarks, result.bookmarks, false);
                config.settings.import_wizard_dismissed = true;
                config::save_with_override(config, cfg_override)
                    .context("Failed to save config after import")?;
                eprintln!(
                    "Imported {} bookmark(s) from {}",
                    imported.imported.len(),
                    result.source_label
                );
            }
            None => {
                // User cancelled/skipped — persist dismissal so wizard doesn't nag
                config.settings.import_wizard_dismissed = true;
                config::save_with_override(config, cfg_override)
                    .context("Failed to save config after dismissing wizard")?;
            }
        }
    }

    let mut app = App::new(config.clone()).with_config_override(cfg_override);

    loop {
        let mut terminal = enter_tui()?;
        let action = event_loop(&mut terminal, &mut app)?;
        leave_tui(&mut terminal)?;

        match action {
            LoopAction::Quit => break,
            LoopAction::Connect(bookmark_index) => {
                if let Err(e) = ssh::connect(
                    &mut app.config,
                    bookmark_index,
                    app.config_path_override.as_deref(),
                )
                .await
                {
                    eprintln!("SSH error: {e:#}");
                    eprintln!("Press Enter to return to sshore...");
                    let _ = wait_for_enter();
                }
            }
        }
    }

    // Write back updated config (connection stats may have changed)
    *config = app.config;

    Ok(())
}

/// Wait for the user to press Enter (used after SSH error messages).
fn wait_for_enter() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(())
}

/// Main event loop: draw, poll, handle. Returns action when the loop exits.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<LoopAction> {
    // Reset connect request from any previous iteration
    app.connect_request = None;

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
            return Ok(LoopAction::Quit);
        }

        if let Some(idx) = app.connect_request.take() {
            return Ok(LoopAction::Connect(idx));
        }
    }
}

/// Route drawing to the appropriate view based on current screen.
fn draw(frame: &mut ratatui::Frame, app: &App) {
    let theme = &app.theme;

    let outer_block = Block::default()
        .title(" sshore ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

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
            theme,
        );
        chunk_idx += 1;
    }

    // Env filter indicator
    if let Some(ref env) = app.env_filter {
        list::render_env_filter_indicator(
            frame,
            chunks[chunk_idx],
            env,
            &app.config.settings,
            theme,
        );
        chunk_idx += 1;
    }

    // Main content area — always render the list as background
    let content_area = chunks[chunk_idx];
    chunk_idx += 1;
    list::render_list(frame, content_area, app);

    // Status message
    if let Some((ref msg, _)) = app.status_message
        && !msg.is_empty()
    {
        let status_line = ratatui::text::Line::from(ratatui::text::Span::styled(
            format!(" {msg}"),
            Style::default().fg(theme.warning),
        ));
        frame.render_widget(
            ratatui::widgets::Paragraph::new(status_line),
            chunks[chunk_idx],
        );
        chunk_idx += 1;
    }

    // Status bar (keybinding hints) — always last
    status_bar::render_status_bar(
        frame,
        chunks[chunk_idx],
        &app.screen,
        app.search_active,
        theme,
    );

    // Overlays on top of everything
    match app.screen {
        Screen::Help => {
            help::render_help(frame, frame.area(), theme);
        }
        Screen::AddForm | Screen::EditForm(_) => {
            if let Some(ref state) = app.form_state {
                form::render_form(frame, frame.area(), state, &app.config.settings, theme);
            }
        }
        Screen::DeleteConfirm(_) => {
            if let Some(ref state) = app.confirm_state {
                confirm::render_confirm(frame, frame.area(), state, &app.config.settings, theme);
            }
        }
        Screen::List => {}
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
        Screen::AddForm | Screen::EditForm(_) => handle_form_key(app, key),
        Screen::DeleteConfirm(_) => handle_confirm_key(app, key),
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
            // Clear search state so the search prompt doesn't linger
            app.search_active = false;
            app.search_query.clear();
            app.refilter();
        }

        // Actions
        KeyCode::Enter => {
            if let Some(idx) = app.selected_bookmark_index() {
                app.connect_request = Some(idx);
            }
        }
        KeyCode::Char('a') => {
            app.form_state = Some(FormState::new_add(&app.config.settings));
            app.screen = Screen::AddForm;
        }
        KeyCode::Char('e') => {
            if let Some(idx) = app.selected_bookmark_index() {
                let bookmark = &app.config.bookmarks[idx];
                app.form_state = Some(FormState::new_edit(bookmark));
                app.screen = Screen::EditForm(idx);
            }
        }
        KeyCode::Char('d') => {
            if let Some(idx) = app.selected_bookmark_index() {
                let bookmark = &app.config.bookmarks[idx];
                app.confirm_state = Some(ConfirmState::new(bookmark));
                app.screen = Screen::DeleteConfirm(idx);
            }
        }

        // Help
        KeyCode::Char('?') => app.screen = Screen::Help,

        _ => {}
    }
}

/// Handle key events in the add/edit form.
fn handle_form_key(app: &mut App, key: KeyEvent) {
    let Some(ref mut form) = app.form_state else {
        app.screen = Screen::List;
        return;
    };

    match key.code {
        KeyCode::Esc => {
            app.form_state = None;
            app.screen = Screen::List;
        }
        KeyCode::Tab | KeyCode::Down => form.next_field(),
        KeyCode::BackTab | KeyCode::Up => form.prev_field(),
        KeyCode::Left if form.focused == 4 => form.cycle_env_left(),
        KeyCode::Right if form.focused == 4 => form.cycle_env_right(),
        KeyCode::Backspace => form.delete_char(),
        KeyCode::Enter => {
            // Attempt to save
            try_save_form(app);
        }
        KeyCode::Char(c) => form.insert_char(c),
        _ => {}
    }
}

/// Try to validate and save the form. On success, return to list. On failure, show error.
fn try_save_form(app: &mut App) {
    let Some(ref mut form) = app.form_state else {
        return;
    };

    match form.validate_and_build(&app.config) {
        Ok(bookmark) => {
            let name = bookmark.name.clone();

            match app.screen {
                Screen::AddForm => {
                    app.config.bookmarks.push(bookmark);
                }
                Screen::EditForm(idx) => {
                    // Preserve last_connected and connect_count from the original
                    let original = &app.config.bookmarks[idx];
                    let mut updated = bookmark;
                    updated.last_connected = original.last_connected;
                    updated.connect_count = original.connect_count;
                    app.config.bookmarks[idx] = updated;
                }
                _ => {}
            }

            // Save to disk
            if let Err(e) = app.save_config() {
                app.set_status(format!("Error saving config: {e}"));
            } else {
                app.set_status(format!("Bookmark '{name}' saved"));
            }

            app.form_state = None;
            app.screen = Screen::List;
            app.refilter();
        }
        Err(e) => {
            // Show validation error in the form
            form.error = Some(e.to_string());
        }
    }
}

/// Handle key events in the delete confirmation dialog.
fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    let Some(ref mut state) = app.confirm_state else {
        app.screen = Screen::List;
        return;
    };

    match key.code {
        KeyCode::Esc => {
            app.confirm_state = None;
            app.screen = Screen::List;
        }
        KeyCode::Enter => {
            if state.is_confirmed()
                && let Screen::DeleteConfirm(idx) = app.screen
            {
                let name = app.config.bookmarks[idx].name.clone();
                app.config.bookmarks.remove(idx);

                if let Err(e) = app.save_config() {
                    app.set_status(format!("Error saving config: {e}"));
                } else {
                    app.set_status(format!("Bookmark '{name}' deleted"));
                }

                app.confirm_state = None;
                app.screen = Screen::List;
                app.refilter();
            }
        }
        KeyCode::Backspace if state.is_production => {
            state.delete_char();
        }
        KeyCode::Char(c) if state.is_production => {
            state.insert_char(c);
        }
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
        KeyCode::Char(c) if !c.is_control() => {
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
            on_connect: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::HashMap::new(),
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
        assert!(app.form_state.is_none());
        assert!(app.confirm_state.is_none());
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
        assert_eq!(app.selected_index, 0);
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
        move_selection(&mut app, 1);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_selected_bookmark_index() {
        let app = sample_app();
        assert!(app.selected_bookmark_index().is_some());

        let empty_app = App::new(AppConfig::default());
        assert!(empty_app.selected_bookmark_index().is_none());
    }

    #[test]
    fn test_open_add_form() {
        let mut app = sample_app();
        app.form_state = Some(FormState::new_add(&app.config.settings));
        app.screen = Screen::AddForm;
        assert!(app.form_state.is_some());
        assert_eq!(app.screen, Screen::AddForm);
    }

    #[test]
    fn test_open_edit_form() {
        let mut app = sample_app();
        if let Some(idx) = app.selected_bookmark_index() {
            let bookmark = app.config.bookmarks[idx].clone();
            app.form_state = Some(FormState::new_edit(&bookmark));
            app.screen = Screen::EditForm(idx);
            assert!(app.form_state.is_some());
        }
    }

    #[test]
    fn test_open_delete_confirm() {
        let mut app = sample_app();
        if let Some(idx) = app.selected_bookmark_index() {
            let bookmark = &app.config.bookmarks[idx];
            app.confirm_state = Some(ConfirmState::new(bookmark));
            app.screen = Screen::DeleteConfirm(idx);
            assert!(app.confirm_state.is_some());
        }
    }

    #[test]
    fn test_delete_bookmark_from_app() {
        let mut app = sample_app();
        let initial_count = app.config.bookmarks.len();

        // Delete the first bookmark
        app.config.bookmarks.remove(0);
        app.refilter();

        assert_eq!(app.config.bookmarks.len(), initial_count - 1);
    }

    #[test]
    fn test_add_bookmark_to_app() {
        let mut app = sample_app();
        let initial_count = app.config.bookmarks.len();

        let new_bookmark = Bookmark {
            name: "new-server".into(),
            host: "10.0.5.1".into(),
            user: None,
            port: 22,
            env: "development".into(),
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
        };
        app.config.bookmarks.push(new_bookmark);
        app.refilter();

        assert_eq!(app.config.bookmarks.len(), initial_count + 1);
        assert!(app.config.bookmarks.iter().any(|b| b.name == "new-server"));
    }
}
