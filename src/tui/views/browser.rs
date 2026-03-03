use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::sftp::shortcuts::format_bytes;
use crate::storage::{Backend, FileEntry};
use crate::tui::theme::ThemeColors;

/// Poll timeout when idle (no timed state changes pending).
/// User input is detected instantly regardless of this value.
const POLL_RATE: Duration = Duration::from_secs(1);

/// Faster poll rate when background transfers are active, for progress updates.
const PROGRESS_POLL_RATE: Duration = Duration::from_millis(100);

/// Buffer size for background file transfers (256 KB).
const TRANSFER_CHUNK_SIZE: usize = 256 * 1024;

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

/// Input mode for inline prompts and confirmations.
#[derive(Debug, Clone)]
pub enum InputMode {
    Normal,
    Filter(String),
    MkdirPrompt(String),
    RenamePrompt {
        input: String,
        source: FileEntry,
    },
    ConfirmDelete {
        entries: Vec<(String, String, bool, u64)>,
    }, // (path, name, is_dir, size)
    SelectPattern {
        input: String,
        selecting: bool,
    },
}

/// Transfer direction for background file copies.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TransferDirection {
    LocalToRemote,
    RemoteToLocal,
}

/// A single file to transfer in a background copy job.
struct TransferTarget {
    src_path: String,
    dst_path: String,
    name: String,
    size: u64,
}

/// Shared atomic progress counters for a background transfer, read by the main event loop.
struct TransferProgress {
    current_file: AtomicU64,
    total_files: AtomicU64,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    current_name: std::sync::Mutex<String>,
    files_done: AtomicU64,
}

impl TransferProgress {
    fn new(total_files: u64) -> Self {
        Self {
            current_file: AtomicU64::new(0),
            total_files: AtomicU64::new(total_files),
            bytes_done: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            current_name: std::sync::Mutex::new(String::new()),
            files_done: AtomicU64::new(0),
        }
    }
}

/// Result of a completed background transfer.
struct TransferResult {
    copied: usize,
    total: usize,
    last_error: Option<String>,
}

/// A background file transfer running in a separate tokio task.
struct BackgroundTransfer {
    handle: tokio::task::JoinHandle<TransferResult>,
    progress: Arc<TransferProgress>,
    cancel: Arc<AtomicBool>,
    dest_side: Side,
    direction: TransferDirection,
    description: String,
}

/// Overall browser state.
pub struct BrowserState {
    pub active_pane: Side,
    pub show_hidden: bool,
    pub sort_by: SortField,
    pub sort_asc: bool,
    pub filter: Option<String>,
    pub input_mode: InputMode,
    pub status_message: Option<String>,
    pub bookmark_name: String,
    pub env: String,
    pub left_label: PaneLabel,
    pub right_label: PaneLabel,
    /// Background file transfers (processed by polling in the main event loop).
    background_transfers: Vec<BackgroundTransfer>,
    pub theme: ThemeColors,
}

/// Whether a pane shows a local or remote filesystem.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaneLabel {
    Local,
    Remote,
}

/// Run the dual-pane file browser.
pub async fn run(
    left: &mut Backend,
    right: &mut Backend,
    bookmark_name: &str,
    env: &str,
    show_hidden: bool,
    theme: &ThemeColors,
) -> Result<()> {
    tracing::debug!("browser starting for bookmark={bookmark_name} env={env}");

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

    let left_label = match left {
        Backend::Local(_) => PaneLabel::Local,
        Backend::Sftp(_) => PaneLabel::Remote,
    };
    let right_label = match right {
        Backend::Local(_) => PaneLabel::Local,
        Backend::Sftp(_) => PaneLabel::Remote,
    };

    let mut state = BrowserState {
        active_pane: Side::Left,
        show_hidden,
        sort_by: SortField::Name,
        sort_asc: true,
        filter: None,
        input_mode: InputMode::Normal,
        status_message: None,
        bookmark_name: bookmark_name.to_string(),
        env: env.to_string(),
        left_label,
        right_label,
        background_transfers: Vec::new(),
        theme: theme.clone(),
    };

    // Initial load
    refresh_pane(&mut left_pane, left, &state).await?;
    refresh_pane(&mut right_pane, right, &state).await?;

    let mut needs_redraw = true;

    loop {
        // === Poll background transfers for completion ===
        {
            let mut i = 0;
            while i < state.background_transfers.len() {
                if state.background_transfers[i].handle.is_finished() {
                    let transfer = state.background_transfers.remove(i);
                    let result = match transfer.handle.await {
                        Ok(r) => r,
                        Err(_) => TransferResult {
                            copied: 0,
                            total: 0,
                            last_error: Some("Transfer task panicked".into()),
                        },
                    };
                    state.status_message =
                        Some(format_transfer_result(&result, &transfer.description));

                    // Refresh destination pane
                    match transfer.dest_side {
                        Side::Left => refresh_pane(&mut left_pane, left, &state).await?,
                        Side::Right => refresh_pane(&mut right_pane, right, &state).await?,
                    }
                    needs_redraw = true;
                } else {
                    i += 1;
                }
            }

            // Update progress display for active transfers
            if !state.background_transfers.is_empty() {
                state.status_message =
                    Some(format_background_progress(&state.background_transfers));
                needs_redraw = true;
            }
        }

        // === Normal rendering and event processing ===
        if needs_redraw {
            terminal.draw(|frame| draw(frame, &mut left_pane, &mut right_pane, &state))?;
            needs_redraw = false;
        }

        let poll_timeout = if state.background_transfers.is_empty() {
            POLL_RATE
        } else {
            PROGRESS_POLL_RATE
        };
        if event::poll(poll_timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    // Handle input modes (filter, mkdir, rename, confirm, pattern select)
                    if !matches!(state.input_mode, InputMode::Normal) {
                        handle_input_mode(
                            key,
                            &mut left_pane,
                            &mut right_pane,
                            &mut state,
                            left,
                            right,
                        )
                        .await?;
                        needs_redraw = true;
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

                    needs_redraw = true;

                    if action == BrowserAction::Quit {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    needs_redraw = true;
                }
                _ => {}
            }
        }
    }

    // BrowserGuard handles terminal cleanup on drop
    tracing::debug!("browser exiting normally");
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
        KeyCode::Char('q') | KeyCode::F(10) => return Ok(BrowserAction::Quit),

        KeyCode::Esc => {
            if !state.background_transfers.is_empty() {
                for t in &state.background_transfers {
                    t.cancel.store(true, Ordering::Relaxed);
                }
                state.status_message = Some("Cancelling transfers...".to_string());
            } else {
                return Ok(BrowserAction::Quit);
            }
        }

        KeyCode::F(1) => {
            state.status_message = Some(
                "F3=View F5=Copy F6=Move F7=Mkdir F8=Del F10=Quit | v=Mark *=Invert +=Select -=Deselect Tab=Switch"
                    .to_string(),
            );
        }

        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !state.background_transfers.is_empty() {
                for t in &state.background_transfers {
                    t.cancel.store(true, Ordering::Relaxed);
                }
                state.status_message = Some("Cancelling transfers...".to_string());
            }
        }

        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            refresh_pane(left_pane, left, state).await?;
            refresh_pane(right_pane, right, state).await?;
            state.status_message = Some("Refreshed".to_string());
        }

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

        KeyCode::Char(' ') | KeyCode::F(5) => {
            // Spawn a background transfer (user keeps browsing while it runs).
            let pane = active_pane_mut(left_pane, right_pane, state);
            let targets = collect_batch_targets(pane);
            if targets.is_empty() {
                // Nothing to copy
            } else if !state.background_transfers.is_empty() {
                state.status_message = Some("Transfer already in progress".to_string());
            } else {
                let source_side = state.active_pane;
                let (src_label, dst_label) = match source_side {
                    Side::Left => (state.left_label, state.right_label),
                    Side::Right => (state.right_label, state.left_label),
                };

                if src_label == dst_label {
                    state.status_message =
                        Some("Copy requires one local and one remote pane".to_string());
                } else {
                    let direction = if src_label == PaneLabel::Local {
                        TransferDirection::LocalToRemote
                    } else {
                        TransferDirection::RemoteToLocal
                    };

                    // Open a new SFTP channel for the background task
                    let remote_backend = if src_label == PaneLabel::Remote {
                        match source_side {
                            Side::Left => &*left,
                            Side::Right => &*right,
                        }
                    } else {
                        match source_side {
                            Side::Left => &*right,
                            Side::Right => &*left,
                        }
                    };

                    match remote_backend.open_sftp_session().await {
                        Ok(sftp) => {
                            let dest_side = match source_side {
                                Side::Left => Side::Right,
                                Side::Right => Side::Left,
                            };
                            let dst_cwd = match dest_side {
                                Side::Left => &left_pane.cwd,
                                Side::Right => &right_pane.cwd,
                            };

                            let transfer_targets: Vec<TransferTarget> = targets
                                .iter()
                                .map(|(src, name, _, size)| TransferTarget {
                                    src_path: src.clone(),
                                    dst_path: format!("{}/{}", dst_cwd.trim_end_matches('/'), name),
                                    name: name.clone(),
                                    size: *size,
                                })
                                .collect();

                            let description = if transfer_targets.len() == 1 {
                                transfer_targets[0].name.clone()
                            } else {
                                format!("{} files", transfer_targets.len())
                            };

                            let progress =
                                Arc::new(TransferProgress::new(transfer_targets.len() as u64));
                            let cancel = Arc::new(AtomicBool::new(false));

                            let bg_progress = Arc::clone(&progress);
                            let bg_cancel = Arc::clone(&cancel);
                            let handle = tokio::spawn(async move {
                                run_background_transfer(
                                    sftp,
                                    transfer_targets,
                                    bg_progress,
                                    bg_cancel,
                                    direction,
                                )
                                .await
                            });

                            state.background_transfers.push(BackgroundTransfer {
                                handle,
                                progress,
                                cancel,
                                dest_side,
                                direction,
                                description: description.clone(),
                            });

                            // Clear source marks immediately
                            let src_pane = match source_side {
                                Side::Left => &mut *left_pane,
                                Side::Right => &mut *right_pane,
                            };
                            src_pane.marked.clear();

                            state.status_message =
                                Some(format!("Copying {description} in background..."));
                        }
                        Err(e) => {
                            state.status_message =
                                Some(format!("Failed to open SFTP channel: {e}"));
                        }
                    }
                }
            }
        }

        KeyCode::F(3) => {
            // View: directory = enter, file = open in $PAGER
            let pane = active_pane_mut(left_pane, right_pane, state);
            if let Some(entry) = pane.selected_entry().cloned() {
                if entry.is_dir {
                    // Same as Enter — navigate into directory
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
                } else {
                    // Download to temp file, open in pager
                    state.status_message = Some(format!("Downloading {}...", entry.name));
                    let temp_dir = tempfile::tempdir()?;
                    let temp_file = temp_dir.path().join(&entry.name);
                    let backend = active_backend_mut(left, right, state);
                    match backend.download(&entry.path, &temp_file).await {
                        Ok(()) => {
                            let pager =
                                std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
                            // Leave TUI mode for pager
                            crossterm::terminal::disable_raw_mode()?;
                            crossterm::execute!(
                                std::io::stdout(),
                                crossterm::terminal::LeaveAlternateScreen,
                                crossterm::cursor::Show,
                            )?;
                            let _ = std::process::Command::new(&pager)
                                .arg(temp_file.as_os_str())
                                .status();
                            // Re-enter TUI mode
                            crossterm::terminal::enable_raw_mode()?;
                            crossterm::execute!(
                                std::io::stdout(),
                                crossterm::terminal::EnterAlternateScreen,
                                crossterm::cursor::Hide,
                            )?;
                            state.status_message = None;
                        }
                        Err(e) => {
                            tracing::error!("view download failed: {}: {e:#}", entry.path);
                            state.status_message = Some(format!("Download error: {e}"));
                        }
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
            state.input_mode = InputMode::Filter(String::new());
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
            let targets = collect_batch_targets(pane);
            if !targets.is_empty() {
                let requires_confirm = is_production_remote(state);
                if requires_confirm {
                    let names: Vec<_> = targets.iter().map(|(_, n, _, _)| n.as_str()).collect();
                    state.status_message = Some(format!(
                        "PROD delete {}: press y to confirm, n/Esc to cancel",
                        if names.len() == 1 {
                            format!("'{}'", names[0])
                        } else {
                            format!("{} items", names.len())
                        }
                    ));
                    state.input_mode = InputMode::ConfirmDelete { entries: targets };
                } else {
                    let backend = active_backend_mut(left, right, state);
                    for (path, _, is_dir, _) in &targets {
                        if *is_dir {
                            backend.rmdir(path).await?;
                        } else {
                            backend.delete(path).await?;
                        }
                    }
                    state.status_message = Some(if targets.len() == 1 {
                        format!("Deleted: {}", targets[0].1)
                    } else {
                        format!("Deleted {} items", targets.len())
                    });
                    let pane = active_pane_mut(left_pane, right_pane, state);
                    let backend = active_backend_mut(left, right, state);
                    refresh_pane(pane, backend, state).await?;
                }
            }
        }

        KeyCode::F(7) => {
            state.input_mode = InputMode::MkdirPrompt(String::new());
        }

        KeyCode::Char('r') | KeyCode::F(6) => {
            let pane = active_pane_mut(left_pane, right_pane, state);
            if let Some(entry) = pane.selected_entry().cloned()
                && entry.name != ".."
            {
                state.input_mode = InputMode::RenamePrompt {
                    input: entry.name.clone(),
                    source: entry,
                };
            }
        }

        KeyCode::Char('*') => {
            // Invert all marks (skip "..")
            let pane = active_pane_mut(left_pane, right_pane, state);
            for (i, entry) in pane.entries.iter().enumerate() {
                if entry.name != ".." {
                    if pane.marked.contains(&i) {
                        pane.marked.remove(&i);
                    } else {
                        pane.marked.insert(i);
                    }
                }
            }
        }

        KeyCode::Char('+') => {
            state.input_mode = InputMode::SelectPattern {
                input: String::new(),
                selecting: true,
            };
        }

        KeyCode::Char('-') => {
            state.input_mode = InputMode::SelectPattern {
                input: String::new(),
                selecting: false,
            };
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
    let mut entries = backend.list(&pane.cwd).await.map_err(|e| {
        tracing::error!("failed to list directory '{}': {e:#}", pane.cwd);
        e
    })?;

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

    // Layout: header, panes, filter bar (0-1), prompt bar (0-1), status message (0-1), F-key bar
    let has_filter = matches!(state.input_mode, InputMode::Filter(_)) || state.filter.is_some();
    let has_prompt = matches!(
        state.input_mode,
        InputMode::MkdirPrompt(_)
            | InputMode::RenamePrompt { .. }
            | InputMode::SelectPattern { .. }
    );
    let has_status = state.status_message.is_some()
        || matches!(state.input_mode, InputMode::ConfirmDelete { .. });
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                              // header
            Constraint::Min(5),                                 // panes
            Constraint::Length(if has_filter { 1 } else { 0 }), // filter
            Constraint::Length(if has_prompt { 1 } else { 0 }), // prompt (mkdir/rename/pattern)
            Constraint::Length(if has_status { 1 } else { 0 }), // status message
            Constraint::Length(1),                              // F-key bar (always)
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
        state.left_label,
    );
    draw_pane(
        frame,
        right_pane,
        pane_chunks[1],
        state.active_pane == Side::Right,
        state.right_label,
    );

    // Filter bar
    if has_filter {
        let filter_text = if let InputMode::Filter(ref input) = state.input_mode {
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

    // Prompt bar (mkdir / rename / pattern select)
    if has_prompt {
        let prompt_text = match &state.input_mode {
            InputMode::MkdirPrompt(input) => format!(" Mkdir: {}_ ", input),
            InputMode::RenamePrompt { input, .. } => format!(" Rename: {}_ ", input),
            InputMode::SelectPattern { input, selecting } => {
                let label = if *selecting { "Select" } else { "Deselect" };
                format!(" {} pattern: {}_ ", label, input)
            }
            _ => String::new(),
        };
        frame.render_widget(
            Paragraph::new(prompt_text).style(Style::default().fg(Color::Yellow)),
            main_chunks[3],
        );
    }

    // Status message
    if has_status {
        let msg = if let InputMode::ConfirmDelete { ref entries } = state.input_mode {
            if entries.len() == 1 {
                format!(
                    " PROD delete '{}': press y to confirm, n/Esc to cancel",
                    entries[0].1
                )
            } else {
                format!(
                    " PROD delete {} items: press y to confirm, n/Esc to cancel",
                    entries.len()
                )
            }
        } else {
            state.status_message.as_deref().unwrap_or("").to_string()
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            main_chunks[4],
        );
    }

    // F-key bar (always visible)
    draw_fkey_bar(frame, main_chunks[5], &state.theme);
}

/// Draw a single pane.
fn draw_pane(
    frame: &mut Frame,
    pane: &mut PaneState,
    area: Rect,
    is_active: bool,
    label: PaneLabel,
) {
    let border_style = if is_active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let (badge_text, badge_style) = match label {
        PaneLabel::Local => (" LOCAL ", Style::default().fg(Color::White).bg(Color::Blue)),
        PaneLabel::Remote => (
            " REMOTE ",
            Style::default().fg(Color::White).bg(Color::Magenta),
        ),
    };

    // Reserve space for badge + padding in the title
    let badge_len = badge_text.len() + 2; // " LOCAL " + " "
    let max_chars = (area.width as usize).saturating_sub(badge_len + 4);
    let char_count = pane.cwd.chars().count();
    let cwd_display = if char_count > max_chars {
        let skip = char_count - (max_chars.saturating_sub(3));
        let tail: String = pane.cwd.chars().skip(skip).collect();
        format!("...{tail}")
    } else {
        pane.cwd.clone()
    };

    let title = Line::from(vec![
        Span::styled(badge_text, badge_style),
        Span::raw(format!(" {} ", cwd_display)),
    ]);

    let block = Block::default()
        .title(title)
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

/// Check if the active pane is a production remote pane.
fn is_production_remote(state: &BrowserState) -> bool {
    let label = match state.active_pane {
        Side::Left => state.left_label,
        Side::Right => state.right_label,
    };
    state.env.eq_ignore_ascii_case("production") && label == PaneLabel::Remote
}

/// Collect batch operation targets: marked entries if any, otherwise the selected entry (skip `..`).
fn collect_batch_targets(pane: &PaneState) -> Vec<(String, String, bool, u64)> {
    if !pane.marked.is_empty() {
        pane.marked
            .iter()
            .filter_map(|&idx| pane.entries.get(idx))
            .filter(|e| e.name != "..")
            .map(|e| (e.path.clone(), e.name.clone(), e.is_dir, e.size))
            .collect()
    } else if let Some(entry) = pane.selected_entry()
        && entry.name != ".."
    {
        vec![(
            entry.path.clone(),
            entry.name.clone(),
            entry.is_dir,
            entry.size,
        )]
    } else {
        vec![]
    }
}

/// Format a progress indicator for active background transfers.
fn format_background_progress(transfers: &[BackgroundTransfer]) -> String {
    let t = &transfers[0]; // v1: only one active transfer at a time
    let p = &t.progress;
    let done = p.bytes_done.load(Ordering::Relaxed);
    let total = p.bytes_total.load(Ordering::Relaxed);
    let current_file = p.current_file.load(Ordering::Relaxed);
    let total_files = p.total_files.load(Ordering::Relaxed);
    let name = p.current_name.lock().unwrap().clone();

    let arrow = match t.direction {
        TransferDirection::LocalToRemote => "\u{2191}",
        TransferDirection::RemoteToLocal => "\u{2193}",
    };

    let pct = if total > 0 {
        (done as f64 / total as f64 * 100.0).min(100.0)
    } else {
        0.0
    };

    if total_files <= 1 {
        format!(
            "{arrow} {name} [{}/{} {pct:.0}%]",
            format_bytes(done),
            format_bytes(total)
        )
    } else {
        format!(
            "{arrow} {current_file}/{total_files}: {name} [{}/{} {pct:.0}%]",
            format_bytes(done),
            format_bytes(total)
        )
    }
}

/// Format a completion message for a finished background transfer.
fn format_transfer_result(result: &TransferResult, description: &str) -> String {
    if result.copied == 0 && result.last_error.is_none() {
        "Copy cancelled".to_string()
    } else if let Some(ref err) = result.last_error {
        format!("Copied {}/{}, error: {err}", result.copied, result.total)
    } else if result.total == 1 {
        format!("Copied: {description}")
    } else {
        format!("Copied {} items", result.total)
    }
}

/// Run a file transfer in a background tokio task.
/// Owns the SftpSession so it operates on a dedicated SFTP channel.
async fn run_background_transfer(
    sftp: SftpSession,
    targets: Vec<TransferTarget>,
    progress: Arc<TransferProgress>,
    cancel: Arc<AtomicBool>,
    direction: TransferDirection,
) -> TransferResult {
    let total = targets.len();
    let mut copied = 0;
    let mut last_error = None;

    for (i, target) in targets.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        progress
            .current_file
            .store((i + 1) as u64, Ordering::Relaxed);
        {
            let mut name = progress.current_name.lock().unwrap();
            *name = target.name.clone();
        }
        progress.bytes_done.store(0, Ordering::Relaxed);
        progress.bytes_total.store(target.size, Ordering::Relaxed);

        let result = match direction {
            TransferDirection::LocalToRemote => {
                transfer_local_to_remote(
                    &sftp,
                    &target.src_path,
                    &target.dst_path,
                    &progress,
                    &cancel,
                )
                .await
            }
            TransferDirection::RemoteToLocal => {
                transfer_remote_to_local(
                    &sftp,
                    &target.src_path,
                    &target.dst_path,
                    &progress,
                    &cancel,
                )
                .await
            }
        };

        match result {
            Ok(()) => {
                copied += 1;
                progress.files_done.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) if cancel.load(Ordering::Relaxed) => break,
            Err(e) => {
                tracing::error!("transfer failed: {}: {e:#}", target.name);
                last_error = Some(format!("{}: {e}", target.name));
            }
        }
    }

    TransferResult {
        copied,
        total,
        last_error,
    }
}

/// Upload a local file to a remote SFTP destination.
async fn transfer_local_to_remote(
    sftp: &SftpSession,
    local_path: &str,
    remote_path: &str,
    progress: &TransferProgress,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut local_file = tokio::fs::File::open(local_path)
        .await
        .with_context(|| format!("Failed to open: {local_path}"))?;

    let mut remote_file = sftp
        .create(remote_path)
        .await
        .with_context(|| format!("Failed to create remote file: {remote_path}"))?;

    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
    loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("Transfer cancelled");
        }
        let n = local_file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        remote_file.write_all(&buf[..n]).await?;
        progress.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }

    remote_file.shutdown().await?;
    Ok(())
}

/// Download a remote SFTP file to a local destination.
async fn transfer_remote_to_local(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &str,
    progress: &TransferProgress,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut remote_file = sftp
        .open(remote_path)
        .await
        .with_context(|| format!("Failed to open remote file: {remote_path}"))?;

    let mut local_file = tokio::fs::File::create(local_path)
        .await
        .with_context(|| format!("Failed to create: {local_path}"))?;

    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
    loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("Transfer cancelled");
        }
        let n = remote_file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        local_file.write_all(&buf[..n]).await?;
        progress.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }

    Ok(())
}

/// Handle input modes: filter, mkdir prompt, rename prompt, confirm delete, pattern select.
async fn handle_input_mode(
    key: KeyEvent,
    left_pane: &mut PaneState,
    right_pane: &mut PaneState,
    state: &mut BrowserState,
    left: &mut Backend,
    right: &mut Backend,
) -> Result<()> {
    // Handle char/backspace editing for text input modes
    if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
        let input_ref = match &mut state.input_mode {
            InputMode::Filter(input)
            | InputMode::MkdirPrompt(input)
            | InputMode::SelectPattern { input, .. } => Some(input),
            InputMode::RenamePrompt { input, .. } => Some(input),
            _ => None,
        };
        if let Some(input) = input_ref {
            match key.code {
                KeyCode::Char(c) => {
                    input.push(c);
                    return Ok(());
                }
                KeyCode::Backspace => {
                    input.pop();
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    // Handle ConfirmDelete chars (y/n) separately
    if let InputMode::ConfirmDelete { .. } = &state.input_mode {
        let entries = if let InputMode::ConfirmDelete { entries } =
            std::mem::replace(&mut state.input_mode, InputMode::Normal)
        {
            entries
        } else {
            unreachable!()
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let backend = active_backend_mut(left, right, state);
                for (path, _, is_dir, _) in &entries {
                    if *is_dir {
                        backend.rmdir(path).await?;
                    } else {
                        backend.delete(path).await?;
                    }
                }
                state.status_message = Some(if entries.len() == 1 {
                    format!("Deleted: {}", entries[0].1)
                } else {
                    format!("Deleted {} items", entries.len())
                });
                let pane = active_pane_mut(left_pane, right_pane, state);
                let backend = active_backend_mut(left, right, state);
                refresh_pane(pane, backend, state).await?;
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                state.status_message = Some("Delete cancelled.".to_string());
            }
            _ => {
                // Put entries back — unrecognized key
                state.input_mode = InputMode::ConfirmDelete { entries };
            }
        }
        return Ok(());
    }

    // Handle Esc — cancel all input modes
    if key.code == KeyCode::Esc {
        if matches!(state.input_mode, InputMode::Filter(_)) {
            state.filter = None;
            state.input_mode = InputMode::Normal;
            let pane = active_pane_mut(left_pane, right_pane, state);
            let backend = active_backend_mut(left, right, state);
            refresh_pane(pane, backend, state).await?;
        } else {
            state.input_mode = InputMode::Normal;
        }
        return Ok(());
    }

    // Handle Enter — commit the current input mode
    if key.code == KeyCode::Enter {
        // Take ownership of input_mode so we can freely use state
        let mode = std::mem::replace(&mut state.input_mode, InputMode::Normal);
        match mode {
            InputMode::Filter(input) => {
                state.filter = if input.is_empty() { None } else { Some(input) };
                let pane = active_pane_mut(left_pane, right_pane, state);
                let backend = active_backend_mut(left, right, state);
                refresh_pane(pane, backend, state).await?;
            }
            InputMode::MkdirPrompt(input) => {
                let name = input.trim().to_string();
                if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                    state.status_message = Some("Invalid directory name".to_string());
                } else {
                    let pane = active_pane_mut(left_pane, right_pane, state);
                    let path = format!("{}/{}", pane.cwd.trim_end_matches('/'), name);
                    let backend = active_backend_mut(left, right, state);
                    match backend.mkdir(&path).await {
                        Ok(()) => {
                            state.status_message = Some(format!("Created: {name}"));
                            let pane = active_pane_mut(left_pane, right_pane, state);
                            let backend = active_backend_mut(left, right, state);
                            refresh_pane(pane, backend, state).await?;
                        }
                        Err(e) => {
                            tracing::error!("mkdir failed: {path}: {e:#}");
                            state.status_message = Some(format!("Mkdir error: {e}"));
                        }
                    }
                }
            }
            InputMode::RenamePrompt { input, source } => {
                let new_name = input.trim().to_string();
                if new_name.is_empty()
                    || new_name == "."
                    || new_name == ".."
                    || new_name.contains('/')
                {
                    state.status_message = Some("Invalid name".to_string());
                } else {
                    let pane = active_pane_mut(left_pane, right_pane, state);
                    let new_path = format!("{}/{}", pane.cwd.trim_end_matches('/'), new_name);
                    let backend = active_backend_mut(left, right, state);
                    match backend.rename(&source.path, &new_path).await {
                        Ok(()) => {
                            state.status_message = Some(format!("Renamed → {new_name}"));
                            let pane = active_pane_mut(left_pane, right_pane, state);
                            let backend = active_backend_mut(left, right, state);
                            refresh_pane(pane, backend, state).await?;
                        }
                        Err(e) => {
                            tracing::error!("rename failed: {} → {new_path}: {e:#}", source.path);
                            state.status_message = Some(format!("Rename error: {e}"));
                        }
                    }
                }
            }
            InputMode::SelectPattern { input, selecting } => {
                let pattern = input.trim().to_string();
                if !pattern.is_empty() {
                    let pane = active_pane_mut(left_pane, right_pane, state);
                    for (i, entry) in pane.entries.iter().enumerate() {
                        if entry.name != ".." && glob_match(&pattern, &entry.name) {
                            if selecting {
                                pane.marked.insert(i);
                            } else {
                                pane.marked.remove(&i);
                            }
                        }
                    }
                    let count = pane.marked.len();
                    state.status_message = Some(format!("{count} files marked"));
                }
            }
            InputMode::Normal | InputMode::ConfirmDelete { .. } => {}
        }
        return Ok(());
    }

    Ok(())
}

/// Draw the MC-style F-key bar at the bottom.
/// Build a key badge + label span pair, matching the bookmark list status bar style.
fn hint_pair<'a>(key: &str, action: &str, theme: &ThemeColors) -> Vec<Span<'a>> {
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

fn draw_fkey_bar(frame: &mut Frame, area: Rect, theme: &ThemeColors) {
    let mut spans = Vec::new();
    spans.extend(hint_pair("F3", "View", theme));
    spans.extend(hint_pair("F5", "Copy", theme));
    spans.extend(hint_pair("F6", "RenMov", theme));
    spans.extend(hint_pair("F7", "Mkdir", theme));
    spans.extend(hint_pair("F8", "Del", theme));
    spans.extend(hint_pair("v", "Mark", theme));
    spans.extend(hint_pair("/", "Filter", theme));
    spans.extend(hint_pair("Tab", "Switch", theme));
    spans.extend(hint_pair("Esc", "Quit", theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Truncate a filename to fit within a given character width.
/// Uses `char` boundaries so multi-byte UTF-8 filenames don't panic.
fn truncate_name(name: &str, max_len: usize) -> String {
    if name.chars().count() <= max_len {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn file_entry(name: &str, is_dir: bool, size: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!("/test/{name}"),
            is_dir,
            size,
            modified: None,
            permissions: None,
        }
    }

    fn file_entry_with_modified(name: &str, secs_ago: i64) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!("/test/{name}"),
            is_dir: false,
            size: 100,
            modified: Some(Utc::now() - chrono::Duration::seconds(secs_ago)),
            permissions: None,
        }
    }

    // --- PaneState ---

    #[test]
    fn test_pane_state_new() {
        let pane = PaneState::new("/home".into());
        assert_eq!(pane.cwd, "/home");
        assert_eq!(pane.selected, 0);
        assert!(pane.entries.is_empty());
        assert!(pane.marked.is_empty());
    }

    #[test]
    fn test_pane_move_down() {
        let mut pane = PaneState::new("/".into());
        pane.entries = vec![
            file_entry("a", false, 10),
            file_entry("b", false, 20),
            file_entry("c", false, 30),
        ];
        assert_eq!(pane.selected, 0);
        pane.move_down();
        assert_eq!(pane.selected, 1);
        pane.move_down();
        assert_eq!(pane.selected, 2);
        // Clamp at bottom
        pane.move_down();
        assert_eq!(pane.selected, 2);
    }

    #[test]
    fn test_pane_move_up() {
        let mut pane = PaneState::new("/".into());
        pane.entries = vec![file_entry("a", false, 10), file_entry("b", false, 20)];
        pane.selected = 1;
        pane.move_up();
        assert_eq!(pane.selected, 0);
        // Clamp at top
        pane.move_up();
        assert_eq!(pane.selected, 0);
    }

    #[test]
    fn test_pane_move_on_empty() {
        let mut pane = PaneState::new("/".into());
        pane.move_down();
        assert_eq!(pane.selected, 0);
        pane.move_up();
        assert_eq!(pane.selected, 0);
    }

    #[test]
    fn test_pane_page_up_down() {
        let mut pane = PaneState::new("/".into());
        pane.entries = (0..25)
            .map(|i| file_entry(&format!("f{i}"), false, 10))
            .collect();
        pane.selected = 0;
        pane.page_down();
        assert_eq!(pane.selected, 10);
        pane.page_down();
        assert_eq!(pane.selected, 20);
        // Clamp at max (24)
        pane.page_down();
        assert_eq!(pane.selected, 24);
        pane.page_up();
        assert_eq!(pane.selected, 14);
        pane.page_up();
        assert_eq!(pane.selected, 4);
        pane.page_up();
        assert_eq!(pane.selected, 0);
    }

    #[test]
    fn test_pane_toggle_mark() {
        let mut pane = PaneState::new("/".into());
        pane.entries = vec![file_entry("a", false, 10), file_entry("b", false, 20)];
        pane.selected = 0;
        assert!(!pane.marked.contains(&0));
        pane.toggle_mark();
        assert!(pane.marked.contains(&0));
        // Toggle off
        pane.toggle_mark();
        assert!(!pane.marked.contains(&0));
    }

    #[test]
    fn test_pane_toggle_mark_empty() {
        let mut pane = PaneState::new("/".into());
        // Should not panic on empty entries
        pane.toggle_mark();
        assert!(pane.marked.is_empty());
    }

    #[test]
    fn test_pane_selected_entry() {
        let mut pane = PaneState::new("/".into());
        assert!(pane.selected_entry().is_none());

        pane.entries = vec![file_entry("a", false, 10), file_entry("b", true, 0)];
        pane.selected = 1;
        let entry = pane.selected_entry().unwrap();
        assert_eq!(entry.name, "b");
        assert!(entry.is_dir);
    }

    // --- sort_entries ---

    #[test]
    fn test_sort_by_name_ascending() {
        let mut entries = vec![
            file_entry("c.txt", false, 30),
            file_entry("a.txt", false, 10),
            file_entry("b.txt", false, 20),
        ];
        sort_entries(&mut entries, SortField::Name, true);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[1].name, "b.txt");
        assert_eq!(entries[2].name, "c.txt");
    }

    #[test]
    fn test_sort_by_name_descending() {
        let mut entries = vec![
            file_entry("a.txt", false, 10),
            file_entry("c.txt", false, 30),
            file_entry("b.txt", false, 20),
        ];
        sort_entries(&mut entries, SortField::Name, false);
        assert_eq!(entries[0].name, "c.txt");
        assert_eq!(entries[1].name, "b.txt");
        assert_eq!(entries[2].name, "a.txt");
    }

    #[test]
    fn test_sort_by_size() {
        let mut entries = vec![
            file_entry("big", false, 1000),
            file_entry("small", false, 10),
            file_entry("medium", false, 500),
        ];
        sort_entries(&mut entries, SortField::Size, true);
        assert_eq!(entries[0].name, "small");
        assert_eq!(entries[1].name, "medium");
        assert_eq!(entries[2].name, "big");
    }

    #[test]
    fn test_sort_by_modified() {
        let mut entries = vec![
            file_entry_with_modified("old", 3600),
            file_entry_with_modified("new", 60),
            file_entry_with_modified("mid", 1800),
        ];
        sort_entries(&mut entries, SortField::Modified, true);
        assert_eq!(entries[0].name, "old");
        assert_eq!(entries[1].name, "mid");
        assert_eq!(entries[2].name, "new");
    }

    #[test]
    fn test_sort_dirs_first() {
        let mut entries = vec![
            file_entry("file.txt", false, 100),
            file_entry("subdir", true, 0),
            file_entry("another.txt", false, 200),
        ];
        sort_entries(&mut entries, SortField::Name, true);
        // Directory should be first regardless of sort field
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "subdir");
    }

    #[test]
    fn test_sort_case_insensitive() {
        let mut entries = vec![
            file_entry("Zebra", false, 10),
            file_entry("apple", false, 20),
            file_entry("Banana", false, 30),
        ];
        sort_entries(&mut entries, SortField::Name, true);
        assert_eq!(entries[0].name, "apple");
        assert_eq!(entries[1].name, "Banana");
        assert_eq!(entries[2].name, "Zebra");
    }

    // --- glob_match ---

    #[test]
    fn test_glob_match_star() {
        assert!(glob_match("*.txt", "readme.txt"));
        assert!(glob_match("*.txt", ".txt"));
        assert!(!glob_match("*.txt", "readme.md"));
    }

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("readme.txt", "readme.txt"));
        assert!(!glob_match("readme.txt", "README.txt"));
    }

    #[test]
    fn test_glob_match_prefix_star() {
        assert!(glob_match("log*", "logfile.txt"));
        assert!(glob_match("log*", "log"));
        assert!(!glob_match("log*", "mylog"));
    }

    #[test]
    fn test_glob_match_middle_star() {
        assert!(glob_match("test_*_spec.rs", "test_foo_spec.rs"));
        assert!(!glob_match("test_*_spec.rs", "test_foo_spec.py"));
    }

    #[test]
    fn test_glob_match_all() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    // --- truncate_name ---

    #[test]
    fn test_truncate_name_short() {
        assert_eq!(truncate_name("short", 10), "short");
    }

    #[test]
    fn test_truncate_name_exact_fit() {
        assert_eq!(truncate_name("exactfit", 8), "exactfit");
    }

    #[test]
    fn test_truncate_name_long() {
        assert_eq!(
            truncate_name("very_long_filename.txt", 15),
            "very_long_fi..."
        );
    }

    #[test]
    fn test_truncate_name_minimum() {
        // With max_len=3, we get "..."
        assert_eq!(truncate_name("abcd", 3), "...");
    }

    #[test]
    fn test_truncate_name_multibyte_utf8() {
        // Must not panic on multi-byte characters (Cyrillic, CJK, emoji)
        let cyrillic = "Библиотека_файлов.txt";
        let result = truncate_name(cyrillic, 10);
        assert_eq!(result, "Библиот...");

        let emoji = "📁documents_folder";
        let result = truncate_name(emoji, 8);
        assert_eq!(result, "📁docu...");
    }
}
