use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::sftp::shortcuts::format_bytes;
use crate::storage::{Backend, FileEntry};

/// Duration for TUI event poll.
const POLL_RATE: Duration = Duration::from_millis(100);

/// Drop guard that restores terminal state when the browser exits (normally or on error).
struct BrowserGuard;

impl Drop for BrowserGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show,
        );
    }
}

/// Which pane is active.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Left,
    Right,
}

/// Sort field for file listings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortField {
    Name,
    Size,
    Modified,
}

/// State for one pane of the browser.
pub struct PaneState {
    pub entries: Vec<FileEntry>,
    pub selected: usize,
    pub cwd: String,
    pub marked: HashSet<usize>,
    pub list_state: ListState,
}

impl PaneState {
    fn new(cwd: String) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            entries: Vec::new(),
            selected: 0,
            cwd,
            marked: HashSet::new(),
            list_state,
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.list_state.select(Some(self.selected));
        }
    }

    fn move_down(&mut self) {
        let max = if self.entries.is_empty() {
            0
        } else {
            self.entries.len() - 1
        };
        if self.selected < max {
            self.selected += 1;
            self.list_state.select(Some(self.selected));
        }
    }

    fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(10);
        self.list_state.select(Some(self.selected));
    }

    fn page_down(&mut self) {
        let max = if self.entries.is_empty() {
            0
        } else {
            self.entries.len() - 1
        };
        self.selected = (self.selected + 10).min(max);
        self.list_state.select(Some(self.selected));
    }

    fn toggle_mark(&mut self) {
        if !self.entries.is_empty() {
            if self.marked.contains(&self.selected) {
                self.marked.remove(&self.selected);
            } else {
                self.marked.insert(self.selected);
            }
        }
    }

    fn selected_entry(&self) -> Option<&FileEntry> {
        self.entries.get(self.selected)
    }
}

/// Overall browser state.
pub struct BrowserState {
    pub active_pane: Side,
    pub show_hidden: bool,
    pub sort_by: SortField,
    pub sort_asc: bool,
    pub filter: Option<String>,
    pub filter_input: Option<String>,
    pub status_message: Option<String>,
    pub bookmark_name: String,
    pub env: String,
}

/// Run the dual-pane file browser.
pub async fn run(
    left: &mut Backend,
    right: &mut Backend,
    bookmark_name: &str,
    env: &str,
    show_hidden: bool,
) -> Result<()> {
    // Enter TUI mode — BrowserGuard ensures cleanup on any exit path
    crossterm::terminal::enable_raw_mode()?;
    let _guard = BrowserGuard;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let left_cwd = left.cwd().unwrap_or_else(|_| ".".to_string());
    let right_cwd = right.cwd().unwrap_or_else(|_| "/".to_string());

    let mut left_pane = PaneState::new(left_cwd.clone());
    let mut right_pane = PaneState::new(right_cwd.clone());

    let mut state = BrowserState {
        active_pane: Side::Left,
        show_hidden,
        sort_by: SortField::Name,
        sort_asc: true,
        filter: None,
        filter_input: None,
        status_message: None,
        bookmark_name: bookmark_name.to_string(),
        env: env.to_string(),
    };

    // Initial load
    refresh_pane(&mut left_pane, left, &state).await?;
    refresh_pane(&mut right_pane, right, &state).await?;

    loop {
        terminal.draw(|frame| draw(frame, &mut left_pane, &mut right_pane, &state))?;

        if event::poll(POLL_RATE)?
            && let Event::Key(key) = event::read()?
        {
            // Handle filter input mode
            if state.filter_input.is_some() {
                match key.code {
                    KeyCode::Esc => {
                        state.filter_input = None;
                        state.filter = None;
                        let pane = active_pane_mut(&mut left_pane, &mut right_pane, &state);
                        let backend = active_backend_mut(left, right, &state);
                        refresh_pane(pane, backend, &state).await?;
                    }
                    KeyCode::Enter => {
                        state.filter = state.filter_input.take();
                        let pane = active_pane_mut(&mut left_pane, &mut right_pane, &state);
                        let backend = active_backend_mut(left, right, &state);
                        refresh_pane(pane, backend, &state).await?;
                    }
                    KeyCode::Char(c) => {
                        if let Some(ref mut input) = state.filter_input {
                            input.push(c);
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(ref mut input) = state.filter_input {
                            input.pop();
                        }
                    }
                    _ => {}
                }
                continue;
            }

            let action = handle_key(
                key,
                &mut left_pane,
                &mut right_pane,
                &mut state,
                left,
                right,
            )
            .await?;

            if action == BrowserAction::Quit {
                break;
            }
        }
    }

    // BrowserGuard handles terminal cleanup on drop
    Ok(())
}

#[derive(PartialEq)]
enum BrowserAction {
    Continue,
    Quit,
}

fn active_pane_mut<'a>(
    left: &'a mut PaneState,
    right: &'a mut PaneState,
    state: &BrowserState,
) -> &'a mut PaneState {
    match state.active_pane {
        Side::Left => left,
        Side::Right => right,
    }
}

fn active_backend_mut<'a>(
    left: &'a mut Backend,
    right: &'a mut Backend,
    state: &BrowserState,
) -> &'a mut Backend {
    match state.active_pane {
        Side::Left => left,
        Side::Right => right,
    }
}

/// Handle a key event in the browser.
async fn handle_key(
    key: KeyEvent,
    left_pane: &mut PaneState,
    right_pane: &mut PaneState,
    state: &mut BrowserState,
    left: &mut Backend,
    right: &mut Backend,
) -> Result<BrowserAction> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(BrowserAction::Quit),

        KeyCode::Tab => {
            state.active_pane = match state.active_pane {
                Side::Left => Side::Right,
                Side::Right => Side::Left,
            };
        }

        KeyCode::Up | KeyCode::Char('k') => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.move_up();
        }

        KeyCode::Down | KeyCode::Char('j') => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.move_down();
        }

        KeyCode::PageUp => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.page_up();
        }

        KeyCode::PageDown => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.page_down();
        }

        KeyCode::Home | KeyCode::Char('g') if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            if !pane.entries.is_empty() {
                pane.selected = 0;
                pane.list_state.select(Some(0));
            }
        }

        KeyCode::End | KeyCode::Char('G') => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            if !pane.entries.is_empty() {
                pane.selected = pane.entries.len() - 1;
                pane.list_state.select(Some(pane.selected));
            }
        }

        KeyCode::Enter => {
            // Open directory or navigate
            let pane = active_pane_mut(left_pane, right_pane, state);
            if let Some(entry) = pane.selected_entry().cloned()
                && entry.is_dir
            {
                let backend = active_backend_mut(left, right, state);
                if entry.name == ".." {
                    backend.cd("..").await?;
                } else {
                    backend.cd(&entry.name).await?;
                }
                let new_cwd = backend.cwd().unwrap_or_default();
                let pane = active_pane_mut(left_pane, right_pane, state);
                pane.cwd = new_cwd;
                pane.selected = 0;
                pane.list_state.select(Some(0));
                pane.marked.clear();
                state.filter = None;
                let pane = active_pane_mut(left_pane, right_pane, state);
                let backend = active_backend_mut(left, right, state);
                refresh_pane(pane, backend, state).await?;
            }
        }

        KeyCode::Char(' ') => {
            // Copy selected to other pane
            let pane = active_pane_mut(left_pane, right_pane, state);
            if let Some(entry) = pane.selected_entry().cloned()
                && !entry.is_dir
            {
                let (src_backend, dst_backend, dst_pane) = match state.active_pane {
                    Side::Left => (left as &Backend, right as &mut Backend, &mut *right_pane),
                    Side::Right => (right as &Backend, left as &mut Backend, &mut *left_pane),
                };

                let dst_path = format!("{}/{}", dst_pane.cwd.trim_end_matches('/'), entry.name);

                // Use a temp file as intermediary for cross-backend transfers
                let temp_dir = tempfile::tempdir()?;
                let temp_file = temp_dir.path().join(&entry.name);

                state.status_message = Some(format!("Copying {}...", entry.name));

                match src_backend.download(&entry.path, &temp_file).await {
                    Ok(()) => match dst_backend.upload(&temp_file, &dst_path).await {
                        Ok(()) => {
                            state.status_message = Some(format!("Copied: {}", entry.name));
                            refresh_pane(dst_pane, dst_backend, state).await?;
                        }
                        Err(e) => {
                            state.status_message = Some(format!("Upload error: {e}"));
                        }
                    },
                    Err(e) => {
                        state.status_message = Some(format!("Download error: {e}"));
                    }
                }
            }
        }

        KeyCode::Char('v') | KeyCode::Insert => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.toggle_mark();
            pane.move_down();
        }

        KeyCode::Char('.') => {
            state.show_hidden = !state.show_hidden;
            refresh_pane(left_pane, left, state).await?;
            refresh_pane(right_pane, right, state).await?;
        }

        KeyCode::Char('/') => {
            state.filter_input = Some(String::new());
        }

        KeyCode::Char('s') => {
            state.sort_by = match state.sort_by {
                SortField::Name => SortField::Size,
                SortField::Size => SortField::Modified,
                SortField::Modified => SortField::Name,
            };
            let pane = active_pane_mut(left_pane, right_pane, state);
            sort_entries(&mut pane.entries, state.sort_by, state.sort_asc);
        }

        KeyCode::Char('S') => {
            state.sort_asc = !state.sort_asc;
            let pane = active_pane_mut(left_pane, right_pane, state);
            sort_entries(&mut pane.entries, state.sort_by, state.sort_asc);
        }

        KeyCode::Char('d') | KeyCode::F(8) => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            if let Some(entry) = pane.selected_entry().cloned()
                && entry.name != ".."
            {
                let backend = active_backend_mut(left, right, state);
                if entry.is_dir {
                    backend.rmdir(&entry.path).await?;
                } else {
                    backend.delete(&entry.path).await?;
                }
                state.status_message = Some(format!("Deleted: {}", entry.name));
                let pane = active_pane_mut(left_pane, right_pane, state);
                let backend = active_backend_mut(left, right, state);
                refresh_pane(pane, backend, state).await?;
            }
        }

        KeyCode::Char('+') | KeyCode::F(7) => {
            // Quick mkdir — prompt is handled inline (simplified: use first char input)
            // For now, skip the inline prompt and use a simple approach
            state.status_message = Some("mkdir: not yet interactive".to_string());
        }

        KeyCode::Char('r') => {
            state.status_message = Some("rename: not yet interactive".to_string());
        }

        KeyCode::Backspace => {
            // Navigate up (parent directory)
            let backend = active_backend_mut(left, right, state);
            backend.cd("..").await?;
            let new_cwd = backend.cwd().unwrap_or_default();
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.cwd = new_cwd;
            pane.selected = 0;
            pane.list_state.select(Some(0));
            pane.marked.clear();
            state.filter = None;
            let pane = active_pane_mut(left_pane, right_pane, state);
            let backend = active_backend_mut(left, right, state);
            refresh_pane(pane, backend, state).await?;
        }

        _ => {}
    }

    Ok(BrowserAction::Continue)
}

/// Refresh a pane's file listing from its backend.
async fn refresh_pane(
    pane: &mut PaneState,
    backend: &mut Backend,
    state: &BrowserState,
) -> Result<()> {
    pane.cwd = backend.cwd().unwrap_or_default();
    let mut entries = backend.list(&pane.cwd).await?;

    // Filter hidden files
    if !state.show_hidden {
        entries.retain(|e| !e.name.starts_with('.'));
    }

    // Apply glob filter
    if let Some(ref filter) = state.filter {
        entries.retain(|e| e.is_dir || glob_match(filter, &e.name));
    }

    // Sort
    sort_entries(&mut entries, state.sort_by, state.sort_asc);

    // Add ".." entry at the top
    entries.insert(
        0,
        FileEntry {
            name: "..".to_string(),
            path: "..".to_string(),
            is_dir: true,
            size: 0,
            modified: None,
            permissions: None,
        },
    );

    pane.entries = entries;
    pane.marked.clear();

    // Clamp selection
    if pane.selected >= pane.entries.len() {
        pane.selected = pane.entries.len().saturating_sub(1);
    }
    pane.list_state.select(Some(pane.selected));

    Ok(())
}

/// Sort file entries. Directories always come first.
fn sort_entries(entries: &mut [FileEntry], sort_by: SortField, ascending: bool) {
    entries.sort_by(|a, b| {
        // Directories first
        match (a.is_dir, b.is_dir) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }

        let ord = match sort_by {
            SortField::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortField::Size => a.size.cmp(&b.size),
            SortField::Modified => a.modified.cmp(&b.modified),
        };

        if ascending { ord } else { ord.reverse() }
    });
}

/// Simple glob matching (reused from config module).
fn glob_match(pattern: &str, text: &str) -> bool {
    let regex_pattern = format!("^{}$", regex::escape(pattern).replace(r"\*", ".*"));
    regex::Regex::new(&regex_pattern)
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

/// Draw the browser TUI.
fn draw(
    frame: &mut Frame,
    left_pane: &mut PaneState,
    right_pane: &mut PaneState,
    state: &BrowserState,
) {
    let size = frame.area();

    // Layout: header, main (left | right), filter bar, status bar
    let has_filter = state.filter_input.is_some() || state.filter.is_some();
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                              // header
            Constraint::Min(5),                                 // panes
            Constraint::Length(if has_filter { 1 } else { 0 }), // filter
            Constraint::Length(1),                              // status bar
        ])
        .split(size);

    // Header
    let env_color = match state.env.to_lowercase().as_str() {
        "production" => Color::Red,
        "staging" => Color::Yellow,
        "development" => Color::Green,
        "local" => Color::Blue,
        "testing" => Color::Cyan,
        _ => Color::White,
    };
    let header = Line::from(vec![
        Span::styled(
            format!(" sshore browse: {} ", state.bookmark_name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", state.env.to_uppercase()),
            Style::default().fg(Color::White).bg(env_color),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), main_chunks[0]);

    // Split main area into two panes
    let pane_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_chunks[1]);

    draw_pane(
        frame,
        left_pane,
        pane_chunks[0],
        state.active_pane == Side::Left,
        "Local",
    );
    draw_pane(
        frame,
        right_pane,
        pane_chunks[1],
        state.active_pane == Side::Right,
        "Remote",
    );

    // Filter bar
    if has_filter {
        let filter_text = if let Some(ref input) = state.filter_input {
            format!(" Filter: {}_ ", input)
        } else if let Some(ref filter) = state.filter {
            format!(" Filter: {} ", filter)
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(filter_text).style(Style::default().fg(Color::Yellow)),
            main_chunks[2],
        );
    }

    // Status bar
    let status_text = if let Some(ref msg) = state.status_message {
        msg.clone()
    } else {
        " Tab=Switch  Enter=Open  Space=Copy  d=Delete  s=Sort  .=Hidden  /=Filter  q=Quit"
            .to_string()
    };
    frame.render_widget(
        Paragraph::new(status_text).style(Style::default().fg(Color::DarkGray)),
        main_chunks[3],
    );
}

/// Draw a single pane.
fn draw_pane(frame: &mut Frame, pane: &mut PaneState, area: Rect, is_active: bool, label: &str) {
    let border_style = if is_active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Truncate cwd to fit in the title
    let max_title_len = area.width as usize - 4;
    let cwd_display = if pane.cwd.len() > max_title_len {
        format!("...{}", &pane.cwd[pane.cwd.len() - max_title_len + 3..])
    } else {
        pane.cwd.clone()
    };

    let block = Block::default()
        .title(format!(" {}: {} ", label, cwd_display))
        .borders(Borders::ALL)
        .border_style(border_style);

    let items: Vec<ListItem> = pane
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_marked = pane.marked.contains(&i);
            let prefix = if is_marked { ">> " } else { "   " };
            let icon = if entry.is_dir { "d " } else { "  " };
            let size_str = if entry.is_dir {
                "<DIR>".to_string()
            } else {
                format_bytes(entry.size)
            };

            let text = format!(
                "{}{}{:<30} {:>8}",
                prefix,
                icon,
                truncate_name(&entry.name, 30),
                size_str,
            );

            let style = if is_marked {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if entry.is_dir {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };

            ListItem::new(text).style(style)
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(if is_active {
                Color::DarkGray
            } else {
                Color::Black
            })
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(list, area, &mut pane.list_state);
}

/// Truncate a filename to fit within a given width.
fn truncate_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        format!("{}...", &name[..max_len - 3])
    }
}
