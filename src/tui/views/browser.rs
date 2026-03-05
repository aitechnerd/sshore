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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use futures::future::join_all;

use crate::sftp::shortcuts::{format_bytes, format_bytes_per_sec, format_duration};
use crate::storage::{Backend, FileEntry};
use crate::tui::theme::ThemeColors;

/// Poll timeout when idle (no timed state changes pending).
/// User input is detected instantly regardless of this value.
const POLL_RATE: Duration = Duration::from_secs(1);

/// Faster poll rate when background transfers are active, for progress updates.
const PROGRESS_POLL_RATE: Duration = Duration::from_millis(100);

/// Buffer size for background file transfers (256 KB).
const TRANSFER_CHUNK_SIZE: usize = 256 * 1024;

/// Max concurrent SFTP `read_dir` calls during directory scanning.
/// Higher values hide more network latency but use more SFTP channel capacity.
const SFTP_SCAN_CONCURRENCY: usize = 16;

/// Number of parallel SFTP worker sessions for file transfers.
const TRANSFER_WORKERS: usize = 8;

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
        // Reset terminal tab title and color (must happen after leaving alternate screen)
        crate::ssh::terminal_theme::reset_theme();
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
    /// Pre-copy/move confirmation popup (MC-style).
    CopyConfirm {
        targets: Vec<(String, String, bool, u64)>, // (src_path, name, is_dir, size)
        direction: TransferDirection,
        source_side: Side,
        dst_cwd: String,
        is_move: bool,
    },
    /// Transfer progress popup overlay.
    TransferPopup,
    /// File already exists — ask user what to do.
    OverwriteConfirm {
        name: String,
        dst_path: String,
        size: u64,
    },
    /// Transfer complete popup with summary message (any key to close).
    TransferComplete(String),
}

/// Transfer direction for background file copies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransferDirection {
    LocalToRemote,
    RemoteToLocal,
}

/// User's answer to an overwrite prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OverwriteAnswer {
    Overwrite,
    OverwriteAll,
    Skip,
    SkipAll,
    Cancel,
}

/// A query from a worker asking what to do about an existing file.
struct OverwriteQuery {
    name: String,
    dst_path: String,
    size: u64,
    response: tokio::sync::oneshot::Sender<OverwriteAnswer>,
}

/// A single file or directory to transfer in a background copy job.
struct TransferTarget {
    src_path: String,
    dst_path: String,
    name: String,
    size: u64,
    is_dir: bool,
}

/// Info about a file currently being transferred by a worker.
#[derive(Clone)]
struct ActiveFile {
    name: String,
    #[allow(dead_code)]
    src_path: String,
    #[allow(dead_code)]
    dst_path: String,
    bytes_done: u64,
    bytes_total: u64,
}

/// Shared atomic progress counters for a background transfer, read by the main event loop.
struct TransferProgress {
    total_files: AtomicU64,
    files_done: AtomicU64,
    /// Sum of all target file sizes, set once after scanning.
    total_bytes_all: AtomicU64,
    /// Cumulative bytes transferred across all files.
    bytes_done_all: AtomicU64,
    /// True while directory scanning is in progress (before transfers start).
    scanning: AtomicBool,
    /// Number of entries discovered so far during scanning phase.
    scan_entries_found: AtomicU64,
    /// Active file slots for each worker thread.
    active_files: std::sync::Mutex<Vec<Option<ActiveFile>>>,
}

impl TransferProgress {
    fn new(total_files: u64, total_bytes_all: u64) -> Self {
        Self {
            total_files: AtomicU64::new(total_files),
            files_done: AtomicU64::new(0),
            total_bytes_all: AtomicU64::new(total_bytes_all),
            bytes_done_all: AtomicU64::new(0),
            scanning: AtomicBool::new(true),
            scan_entries_found: AtomicU64::new(0),
            active_files: std::sync::Mutex::new(Vec::new()),
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
    skip: Arc<AtomicBool>,
    dest_side: Side,
    direction: TransferDirection,
    description: String,
    started_at: std::time::Instant,
    /// Channel for workers to ask about existing files.
    overwrite_rx: tokio::sync::mpsc::Receiver<OverwriteQuery>,
    /// Shared overwrite policy: 0=ask, 1=overwrite all, 2=skip all.
    /// Set by UI when user picks "all"; workers check before asking.
    overwrite_policy: Arc<AtomicU64>,
    /// For move operations: source paths to delete after successful transfer.
    /// Each entry is (path, is_dir). None for plain copy.
    delete_sources: Option<Vec<(String, bool)>>,
    /// Side where the sources live (needed to pick the right backend for deletion).
    source_side: Side,
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
    /// Which button is focused in popup dialogs (0 = first/OK).
    pub popup_focus: usize,
    /// Which background transfer the progress popup is showing.
    popup_transfer_index: usize,
    /// Pending overwrite response sender (one at a time).
    overwrite_response_tx: Option<tokio::sync::oneshot::Sender<OverwriteAnswer>>,
    /// Force a full terminal repaint (e.g. after returning from an external pager).
    pub needs_full_redraw: bool,
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
        popup_focus: 0,
        popup_transfer_index: 0,
        overwrite_response_tx: None,
        needs_full_redraw: false,
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
                    let elapsed = transfer.started_at.elapsed();
                    let result = match transfer.handle.await {
                        Ok(r) => r,
                        Err(_) => TransferResult {
                            copied: 0,
                            total: 0,
                            last_error: Some("Transfer task panicked".into()),
                        },
                    };

                    let is_move = transfer.delete_sources.is_some();
                    let summary = format_transfer_result(&result, &transfer.description, is_move);

                    // If popup is showing and this was the last transfer, show completion
                    if matches!(state.input_mode, InputMode::TransferPopup)
                        && state.background_transfers.is_empty()
                    {
                        let total_bytes = transfer.progress.bytes_done_all.load(Ordering::Relaxed);
                        let completion_msg =
                            format_transfer_complete(&result, total_bytes, elapsed, is_move);
                        state.input_mode = InputMode::TransferComplete(completion_msg);
                    }

                    // Clamp popup index after removal
                    if state.popup_transfer_index >= state.background_transfers.len()
                        && !state.background_transfers.is_empty()
                    {
                        state.popup_transfer_index = state.background_transfers.len() - 1;
                    }

                    // For move operations: delete source files after successful transfer
                    if let Some(sources) = &transfer.delete_sources
                        && result.copied > 0
                        && result.last_error.is_none()
                    {
                        let src_backend = match transfer.source_side {
                            Side::Left => &mut *left,
                            Side::Right => &mut *right,
                        };
                        // Delete in reverse order so files are removed before their
                        // parent directories.
                        for (path, is_dir) in sources.iter().rev() {
                            let del_result = if *is_dir {
                                src_backend.rmdir(path).await
                            } else {
                                src_backend.delete(path).await
                            };
                            if let Err(e) = del_result {
                                tracing::warn!("move: failed to delete source {path}: {e:#}");
                            }
                        }
                    }

                    state.status_message = Some(summary);

                    // Refresh destination pane
                    match transfer.dest_side {
                        Side::Left => refresh_pane(&mut left_pane, left, &state).await?,
                        Side::Right => refresh_pane(&mut right_pane, right, &state).await?,
                    }

                    // For move operations, also refresh source pane (files were deleted)
                    if transfer.delete_sources.is_some() {
                        match transfer.source_side {
                            Side::Left => refresh_pane(&mut left_pane, left, &state).await?,
                            Side::Right => refresh_pane(&mut right_pane, right, &state).await?,
                        }
                    }

                    needs_redraw = true;
                } else {
                    i += 1;
                }
            }

            // Check for overwrite queries from workers (only when not already showing one)
            if !matches!(state.input_mode, InputMode::OverwriteConfirm { .. }) {
                for transfer in state.background_transfers.iter_mut() {
                    // If transfer is cancelled, drain and auto-answer Cancel
                    // so workers unblock and can exit.
                    if transfer.cancel.load(Ordering::Relaxed) {
                        while let Ok(query) = transfer.overwrite_rx.try_recv() {
                            let _ = query.response.send(OverwriteAnswer::Cancel);
                        }
                        continue;
                    }

                    // If policy is already decided, auto-answer without showing popup
                    let policy = transfer.overwrite_policy.load(Ordering::Relaxed);
                    if policy != 0 {
                        let auto_answer = if policy == 1 {
                            OverwriteAnswer::OverwriteAll
                        } else {
                            OverwriteAnswer::SkipAll
                        };
                        while let Ok(query) = transfer.overwrite_rx.try_recv() {
                            let _ = query.response.send(auto_answer);
                        }
                        continue;
                    }

                    if let Ok(query) = transfer.overwrite_rx.try_recv() {
                        state.overwrite_response_tx = Some(query.response);
                        state.popup_focus = 0;
                        state.input_mode = InputMode::OverwriteConfirm {
                            name: query.name,
                            dst_path: query.dst_path,
                            size: query.size,
                        };
                        needs_redraw = true;
                        break;
                    }
                }
            }

            // Update progress display for active transfers (background mode)
            if !state.background_transfers.is_empty()
                && !matches!(state.input_mode, InputMode::TransferPopup)
            {
                state.status_message =
                    Some(format_background_progress(&state.background_transfers));
                needs_redraw = true;
            }

            // Force redraw while popup is active for progress updates
            if matches!(state.input_mode, InputMode::TransferPopup) {
                needs_redraw = true;
            }
        }

        // === Normal rendering and event processing ===
        if state.needs_full_redraw {
            terminal.clear()?;
            state.needs_full_redraw = false;
        }
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
                "F3=View F5=Copy F6=Move F7=Mkdir F8=Del F10=Quit | r=Rename v=Mark *=Invert +=Select -=Deselect Tab=Switch"
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

        KeyCode::Char('p') if !state.background_transfers.is_empty() => {
            // Re-open the transfer progress popup (show latest transfer)
            state.popup_transfer_index = state.background_transfers.len().saturating_sub(1);
            state.input_mode = InputMode::TransferPopup;
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
            // Show copy confirmation popup before starting transfer.
            let pane = active_pane_mut(left_pane, right_pane, state);
            let targets = collect_batch_targets(pane);
            if targets.is_empty() {
                // Nothing to copy
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

                    let dest_side = match source_side {
                        Side::Left => Side::Right,
                        Side::Right => Side::Left,
                    };
                    let dst_cwd = match dest_side {
                        Side::Left => left_pane.cwd.clone(),
                        Side::Right => right_pane.cwd.clone(),
                    };

                    state.popup_focus = 0;
                    state.input_mode = InputMode::CopyConfirm {
                        targets,
                        direction,
                        source_side,
                        dst_cwd,
                        is_move: false,
                    };
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
                            state.needs_full_redraw = true;
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
                state.popup_focus = 0;
                state.input_mode = InputMode::ConfirmDelete { entries: targets };
            }
        }

        KeyCode::F(7) => {
            state.popup_focus = 0;
            state.input_mode = InputMode::MkdirPrompt(String::new());
        }

        KeyCode::Char('r') => {
            // Same-directory rename (r key only)
            let pane = active_pane_mut(left_pane, right_pane, state);
            if let Some(entry) = pane.selected_entry().cloned()
                && entry.name != ".."
            {
                state.popup_focus = 0;
                state.input_mode = InputMode::RenamePrompt {
                    input: entry.name.clone(),
                    source: entry,
                };
            }
        }

        KeyCode::F(6) => {
            // MC-style Move: copy to other pane then delete sources
            let pane = active_pane_mut(left_pane, right_pane, state);
            let targets = collect_batch_targets(pane);
            if targets.is_empty() {
                // Nothing to move
            } else {
                let source_side = state.active_pane;
                let (src_label, dst_label) = match source_side {
                    Side::Left => (state.left_label, state.right_label),
                    Side::Right => (state.right_label, state.left_label),
                };

                if src_label == dst_label {
                    state.status_message =
                        Some("Move requires one local and one remote pane".to_string());
                } else {
                    let direction = if src_label == PaneLabel::Local {
                        TransferDirection::LocalToRemote
                    } else {
                        TransferDirection::RemoteToLocal
                    };

                    let dest_side = match source_side {
                        Side::Left => Side::Right,
                        Side::Right => Side::Left,
                    };
                    let dst_cwd = match dest_side {
                        Side::Left => left_pane.cwd.clone(),
                        Side::Right => right_pane.cwd.clone(),
                    };

                    state.popup_focus = 0;
                    state.input_mode = InputMode::CopyConfirm {
                        targets,
                        direction,
                        source_side,
                        dst_cwd,
                        is_move: true,
                    };
                }
            }
        }

        KeyCode::Char('*') | KeyCode::Char('+') => {
            state.popup_focus = 0;
            state.input_mode = InputMode::SelectPattern {
                input: "*".to_string(),
                selecting: true,
            };
        }

        KeyCode::Char('-') => {
            state.popup_focus = 0;
            state.input_mode = InputMode::SelectPattern {
                input: "*".to_string(),
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
    let has_status = state.status_message.is_some();
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                              // header
            Constraint::Min(5),                                 // panes
            Constraint::Length(if has_filter { 1 } else { 0 }), // filter
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

    // Status message
    if has_status {
        let msg = state.status_message.as_deref().unwrap_or("");
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            main_chunks[3],
        );
    }

    // F-key bar (always visible)
    draw_fkey_bar(frame, main_chunks[4], &state.theme);

    // Popup overlays (rendered on top of everything)
    match &state.input_mode {
        InputMode::MkdirPrompt(input) => {
            draw_mkdir_popup(frame, size, input, state.popup_focus);
        }
        InputMode::RenamePrompt { input, source } => {
            draw_rename_popup(frame, size, &source.name, input, state.popup_focus);
        }
        InputMode::CopyConfirm {
            targets,
            direction,
            dst_cwd,
            is_move,
            ..
        } => {
            draw_copy_confirm_popup(
                frame,
                size,
                targets,
                *direction,
                dst_cwd,
                state.popup_focus,
                *is_move,
            );
        }
        InputMode::TransferPopup => {
            draw_transfer_popup(frame, size, state);
        }
        InputMode::OverwriteConfirm {
            name,
            dst_path,
            size: file_size,
            ..
        } => {
            draw_overwrite_confirm_popup(
                frame,
                size,
                name,
                dst_path,
                *file_size,
                state.popup_focus,
            );
        }
        InputMode::TransferComplete(msg) => {
            draw_transfer_complete_popup(frame, size, msg);
        }
        InputMode::ConfirmDelete { entries } => {
            let is_production = is_production_remote(state);
            draw_delete_confirm_popup(frame, size, entries, is_production, state.popup_focus);
        }
        InputMode::SelectPattern { input, selecting } => {
            draw_select_popup(frame, size, input, *selecting, state.popup_focus);
        }
        _ => {}
    }
}

/// Create a centered rectangle with fixed dimensions, clamped to the available area.
fn centered_fixed_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Render a text-based progress bar.
fn render_progress_bar(ratio: f64, width: usize) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * width as f64) as usize;
    let empty = width.saturating_sub(filled);
    format!(
        "[{}{}]",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty)
    )
}

/// Render a centered row of buttons with one focused (inverted style).
fn render_button_row(labels: &[&str], focus: usize, inner: Rect, y: u16, frame: &mut Frame) {
    let mut spans: Vec<Span> = Vec::new();
    // Calculate total width for centering
    let total_w: usize = labels.iter().map(|l| l.len() + 4).sum::<usize>() // "[ label ]" per button
        + (labels.len().saturating_sub(1)) * 2; // "  " gaps
    let pad = (inner.width as usize).saturating_sub(total_w) / 2;
    spans.push(Span::raw(" ".repeat(pad)));
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if i == focus {
            // Focused: inverted (black on white)
            let inv = Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled(format!("[ {label} ]"), inv));
        } else {
            // Unfocused: normal
            spans.push(Span::styled("[ ", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(
                label.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" ]", Style::default().fg(Color::DarkGray)));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Draw the mkdir popup overlay for entering a new directory name.
fn draw_mkdir_popup(frame: &mut Frame, area: Rect, input: &str, popup_focus: usize) {
    let popup_h: u16 = 8;
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_h, area);
    if popup_area.width < 20 || popup_area.height < popup_h {
        return;
    }

    let block = Block::default()
        .title(" Create a New Directory ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = Rect::new(
        popup_area.x + 2,
        popup_area.y + 1,
        popup_area.width.saturating_sub(4),
        popup_area.height.saturating_sub(2),
    );

    // Label
    frame.render_widget(
        Paragraph::new("Enter directory name:").style(Style::default().fg(Color::White)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Input field with background
    let field_y = inner.y + 1;
    let field_w = inner.width;
    let field_area = Rect::new(inner.x, field_y, field_w, 1);

    // Show the tail of input if it overflows
    let max_visible = field_w.saturating_sub(1) as usize; // leave room for cursor block
    let display_input = if input.len() > max_visible {
        &input[input.len() - max_visible..]
    } else {
        input
    };

    // Render input text on a highlighted background, with a cursor block
    let cursor_char = if display_input.len() < max_visible {
        "\u{2588}"
    } else {
        ""
    };
    let text_len = display_input.len() + cursor_char.len();
    let pad = (field_w as usize).saturating_sub(text_len);
    let field_line = Line::from(vec![
        Span::styled(
            display_input.to_string(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(
            cursor_char.to_string(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(field_line), field_area);

    // Separator line
    let sep_y = inner.y + 3;
    let sep_line = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(Color::Cyan)),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // Centered button row
    let btn_y = inner.y + 4;
    render_button_row(&["OK", "Cancel"], popup_focus, inner, btn_y, frame);
}

/// Draw the rename/move popup overlay.
fn draw_rename_popup(
    frame: &mut Frame,
    area: Rect,
    original_name: &str,
    input: &str,
    popup_focus: usize,
) {
    let popup_h: u16 = 9;
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_h, area);
    if popup_area.width < 20 || popup_area.height < popup_h {
        return;
    }

    let block = Block::default()
        .title(" Rename / Move ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = Rect::new(
        popup_area.x + 2,
        popup_area.y + 1,
        popup_area.width.saturating_sub(4),
        popup_area.height.saturating_sub(2),
    );

    // Original name
    let max_name = inner.width.saturating_sub(2) as usize;
    frame.render_widget(
        Paragraph::new(format!(" {}", truncate_name(original_name, max_name))).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Label
    frame.render_widget(
        Paragraph::new(" Rename to:").style(Style::default().fg(Color::White)),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    // Input field with background
    let field_y = inner.y + 2;
    let field_w = inner.width;
    let field_area = Rect::new(inner.x, field_y, field_w, 1);

    let max_visible = field_w.saturating_sub(1) as usize;
    let display_input = if input.len() > max_visible {
        &input[input.len() - max_visible..]
    } else {
        input
    };

    let cursor_char = if display_input.len() < max_visible {
        "\u{2588}"
    } else {
        ""
    };
    let text_len = display_input.len() + cursor_char.len();
    let pad = (field_w as usize).saturating_sub(text_len);
    let field_line = Line::from(vec![
        Span::styled(
            display_input.to_string(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(
            cursor_char.to_string(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(field_line), field_area);

    // Separator line
    let sep_y = inner.y + 4;
    let sep_line = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(Color::Cyan)),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // Buttons
    let btn_y = inner.y + 5;
    render_button_row(&["OK", "Cancel"], popup_focus, inner, btn_y, frame);
}

/// Draw the MC-style Select/Deselect popup with pattern input.
fn draw_select_popup(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    selecting: bool,
    popup_focus: usize,
) {
    let title = if selecting { " Select " } else { " Deselect " };
    let popup_h: u16 = 8;
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_h, area);
    if popup_area.width < 20 || popup_area.height < popup_h {
        return;
    }

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = Rect::new(
        popup_area.x + 2,
        popup_area.y + 1,
        popup_area.width.saturating_sub(4),
        popup_area.height.saturating_sub(2),
    );

    // Label
    let label = if selecting {
        "Select files matching (shell pattern):"
    } else {
        "Deselect files matching (shell pattern):"
    };
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(Color::White)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Input field with background
    let field_y = inner.y + 1;
    let field_w = inner.width;
    let field_area = Rect::new(inner.x, field_y, field_w, 1);

    let max_visible = field_w.saturating_sub(1) as usize;
    let display_input = if input.len() > max_visible {
        &input[input.len() - max_visible..]
    } else {
        input
    };

    let cursor_char = if display_input.len() < max_visible {
        "\u{2588}"
    } else {
        ""
    };
    let text_len = display_input.len() + cursor_char.len();
    let pad = (field_w as usize).saturating_sub(text_len);
    let field_line = Line::from(vec![
        Span::styled(
            display_input.to_string(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(
            cursor_char.to_string(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(field_line), field_area);

    // Separator line
    let sep_y = inner.y + 3;
    let sep_line = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(Color::Cyan)),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // Buttons
    let btn_y = inner.y + 4;
    render_button_row(&["OK", "Cancel"], popup_focus, inner, btn_y, frame);
}

/// Draw the MC-style delete confirmation popup.
/// Shows a red border and warning line for production remote targets.
fn draw_delete_confirm_popup(
    frame: &mut Frame,
    area: Rect,
    entries: &[(String, String, bool, u64)],
    is_production: bool,
    popup_focus: usize,
) {
    // Determine how many file name lines to show (max 5, then "...and N more")
    const MAX_NAMES: usize = 5;
    let name_lines: usize = if entries.len() == 1 {
        1
    } else {
        entries.len().min(MAX_NAMES) + if entries.len() > MAX_NAMES { 1 } else { 0 }
    };
    let prod_line: u16 = if is_production { 1 } else { 0 };
    // Layout: border(1) + blank(1) + prod_warning(0-1) + name_lines + blank(1) + separator(1) + buttons(1) + border(1)
    let popup_h: u16 = 6 + prod_line + name_lines as u16;
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_h, area);
    if popup_area.width < 20 || popup_area.height < popup_h {
        return;
    }

    let border_color = if is_production {
        Color::Red
    } else {
        Color::Cyan
    };
    let block = Block::default()
        .title(" Delete ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = Rect::new(
        popup_area.x + 2,
        popup_area.y + 1,
        popup_area.width.saturating_sub(4),
        popup_area.height.saturating_sub(2),
    );

    let mut y = inner.y;

    // Production warning line
    if is_production {
        frame.render_widget(
            Paragraph::new("\u{26a0} PRODUCTION")
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }

    // File name lines
    if entries.len() == 1 {
        let label = format!("Delete \"{}\"?", entries[0].1);
        frame.render_widget(
            Paragraph::new(label).style(Style::default().fg(Color::White)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    } else {
        let header = format!("Delete {} files?", entries.len());
        frame.render_widget(
            Paragraph::new(header).style(Style::default().fg(Color::White)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
        let show_count = entries.len().min(MAX_NAMES);
        let max_name_w = inner.width.saturating_sub(2) as usize;
        for entry in entries.iter().take(show_count) {
            let name = if entry.1.len() > max_name_w {
                format!("{}...", &entry.1[..max_name_w.saturating_sub(3)])
            } else {
                entry.1.clone()
            };
            frame.render_widget(
                Paragraph::new(format!("  {}", name)).style(Style::default().fg(Color::DarkGray)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
        if entries.len() > MAX_NAMES {
            let more = format!("  ...and {} more", entries.len() - MAX_NAMES);
            frame.render_widget(
                Paragraph::new(more).style(Style::default().fg(Color::DarkGray)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }

    // Blank line
    y += 1;

    // Separator
    let sep_line = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(border_color)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y += 1;

    // Centered button row
    render_button_row(&["OK", "Cancel"], popup_focus, inner, y, frame);
}

/// Fixed popup width for transfer dialogs.
const POPUP_WIDTH: u16 = 60;

/// Wider popup width for overwrite confirmation (5 buttons on one row).
const OVERWRITE_POPUP_WIDTH: u16 = 72;

/// Draw the MC-style copy/move confirmation popup.
fn draw_copy_confirm_popup(
    frame: &mut Frame,
    area: Rect,
    targets: &[(String, String, bool, u64)],
    _direction: TransferDirection,
    dst_cwd: &str,
    popup_focus: usize,
    is_move: bool,
) {
    let popup_h: u16 = 9;
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_h, area);
    if popup_area.width < 20 || popup_area.height < popup_h {
        return;
    }

    let title = if is_move { " Move " } else { " Copy " };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = Rect::new(
        popup_area.x + 2,
        popup_area.y + 1,
        popup_area.width.saturating_sub(4),
        popup_area.height.saturating_sub(2),
    );

    // "Copy/Move <name> to:" or "Copy/Move N files to:"
    let verb = if is_move { "Move" } else { "Copy" };
    let copy_label = if targets.len() == 1 {
        let name = &targets[0].1;
        let max_name = (inner.width as usize).saturating_sub(12);
        format!("{verb} \"{}\" to:", truncate_name(name, max_name))
    } else {
        format!("{verb} {} file(s) to:", targets.len())
    };
    frame.render_widget(
        Paragraph::new(copy_label).style(Style::default().fg(Color::White)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Destination path on highlighted background
    let field_y = inner.y + 1;
    let field_w = inner.width;
    let max_visible = field_w as usize;
    let display_dst = if dst_cwd.len() > max_visible {
        let skip = dst_cwd.len() - max_visible.saturating_sub(3);
        format!("...{}", &dst_cwd[skip..])
    } else {
        dst_cwd.to_string()
    };
    let pad = max_visible.saturating_sub(display_dst.len());
    let field_line = Line::from(vec![
        Span::styled(
            display_dst,
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(Color::DarkGray)),
    ]);
    frame.render_widget(
        Paragraph::new(field_line),
        Rect::new(inner.x, field_y, field_w, 1),
    );

    // Total size info
    let total_size: u64 = targets.iter().map(|(_, _, _, s)| s).sum();
    let dirs_count = targets.iter().filter(|(_, _, is_dir, _)| *is_dir).count();
    let files_count = targets.len() - dirs_count;
    let info = if dirs_count > 0 {
        format!(
            "{} file(s), {} dir(s) — {}",
            files_count,
            dirs_count,
            format_bytes(total_size)
        )
    } else {
        format_bytes(total_size).to_string()
    };
    frame.render_widget(
        Paragraph::new(format!(" {info}")).style(Style::default().fg(Color::DarkGray)),
        Rect::new(inner.x, inner.y + 3, inner.width, 1),
    );

    // Separator line
    let sep_y = inner.y + 4;
    let sep_line = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(Color::Cyan)),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // Centered button row
    let btn_y = inner.y + 5;
    render_button_row(&["OK", "Bkgnd", "Cancel"], popup_focus, inner, btn_y, frame);
}

/// Draw the transfer progress popup overlay with MC-style layout.
fn draw_transfer_popup(frame: &mut Frame, area: Rect, state: &BrowserState) {
    let idx = state
        .popup_transfer_index
        .min(state.background_transfers.len().saturating_sub(1));
    let Some(transfer) = state.background_transfers.get(idx) else {
        return;
    };
    let p = &transfer.progress;

    let total_files = p.total_files.load(Ordering::Relaxed);
    let bytes_done_all = p.bytes_done_all.load(Ordering::Relaxed);
    let total_bytes_all = p.total_bytes_all.load(Ordering::Relaxed);
    let scanning = p.scanning.load(Ordering::Relaxed);

    let elapsed = transfer.started_at.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();
    let speed = if elapsed_secs > 0.1 {
        bytes_done_all as f64 / elapsed_secs
    } else {
        0.0
    };
    let eta_secs = if speed > 0.0 && total_bytes_all > bytes_done_all {
        ((total_bytes_all - bytes_done_all) as f64 / speed) as u64
    } else {
        0
    };

    let is_move = transfer.delete_sources.is_some();
    let dir_label = match (transfer.direction, is_move) {
        (TransferDirection::LocalToRemote, false) => "Copying to Remote",
        (TransferDirection::RemoteToLocal, false) => "Copying to Local",
        (TransferDirection::LocalToRemote, true) => "Moving to Remote",
        (TransferDirection::RemoteToLocal, true) => "Moving to Local",
    };
    let title = if state.background_transfers.len() > 1 {
        format!(
            " {} [{}/{}] ",
            dir_label,
            idx + 1,
            state.background_transfers.len()
        )
    } else {
        format!(" {dir_label} ")
    };

    // Count active workers to size popup dynamically
    let active_count = {
        let active = p.active_files.lock().unwrap();
        active.iter().filter(|s| s.is_some()).count()
    };
    // Base height: border(2) + "Active:"(1) + blank(1) + progress_bar(1) + stats(1)
    //   + separator(1) + buttons(1) + padding(1) = 9, plus active file lines
    let active_lines = active_count.max(1) as u16;
    let popup_h: u16 = 9 + active_lines;
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_h, area);
    if popup_area.width < 30 || popup_area.height < 8 {
        return;
    }
    let bar_width = (popup_area.width as usize).saturating_sub(10);
    let max_path_len = (popup_area.width as usize).saturating_sub(12);

    if scanning {
        let scan_count = p.scan_entries_found.load(Ordering::Relaxed);
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(" Source: ", Style::default().fg(Color::DarkGray)),
            Span::raw(truncate_name(&transfer.description, max_path_len)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" Scanning... {scan_count} entries found"),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(""));

        // Separator + cancel button
        let sep = "\u{2500}".repeat((popup_area.width as usize).saturating_sub(4));
        lines.push(Line::from(Span::styled(
            format!(" {sep}"),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("[ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(Color::White)),
            Span::styled(" ]", Style::default().fg(Color::DarkGray)),
        ]));

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let popup_area = centered_fixed_rect(POPUP_WIDTH, 10, area);
        frame.render_widget(Clear, popup_area);
        frame.render_widget(Paragraph::new(lines).block(block), popup_area);
        return;
    }

    // Read active files from workers
    let active_files: Vec<ActiveFile> = {
        let active = p.active_files.lock().unwrap();
        active.iter().filter_map(|slot| slot.clone()).collect()
    };

    let overall_pct = if total_bytes_all > 0 {
        bytes_done_all as f64 / total_bytes_all as f64
    } else {
        0.0
    };

    let mut lines: Vec<Line> = Vec::new();

    // Active files list (show all workers)
    lines.push(Line::from(Span::styled(
        " Active:",
        Style::default().fg(Color::DarkGray),
    )));
    if active_files.is_empty() {
        lines.push(Line::from("   ..."));
    } else {
        for af in &active_files {
            let file_pct = if af.bytes_total > 0 {
                format!(
                    " {:.0}%",
                    af.bytes_done as f64 / af.bytes_total as f64 * 100.0
                )
            } else {
                String::new()
            };
            let name_max = max_path_len.saturating_sub(20);
            lines.push(Line::from(format!(
                "   {} [{}/{}{}]",
                truncate_name(&af.name, name_max),
                format_bytes(af.bytes_done),
                format_bytes(af.bytes_total),
                file_pct,
            )));
        }
    }
    lines.push(Line::from(""));

    // Overall progress bar
    let overall_bar = render_progress_bar(overall_pct, bar_width);
    lines.push(Line::from(format!(
        " {overall_bar} {:.0}%",
        overall_pct * 100.0
    )));

    // Files done/total + size + speed + ETA
    let files_done = p.files_done.load(Ordering::Relaxed);
    let stats_line = if speed > 0.0 {
        format!(
            " Files: {files_done}/{total_files}  {} / {}  {}  ETA {}",
            format_bytes(bytes_done_all),
            format_bytes(total_bytes_all),
            format_bytes_per_sec(speed),
            format_duration(eta_secs),
        )
    } else {
        format!(
            " Files: {files_done}/{total_files}  {} / {}",
            format_bytes(bytes_done_all),
            format_bytes(total_bytes_all),
        )
    };
    lines.push(Line::from(stats_line));

    // Separator
    let sep = "\u{2500}".repeat((popup_area.width as usize).saturating_sub(4));
    lines.push(Line::from(Span::styled(
        format!(" {sep}"),
        Style::default().fg(Color::DarkGray),
    )));

    // MC-style buttons: [Skip] [Background] [Esc cancel] [Tab next]
    let mut btn_spans = vec![
        Span::raw(" "),
        Span::styled("[ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "s",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("kip", Style::default().fg(Color::White)),
        Span::styled(" ]", Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled("[ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "b",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("kgnd", Style::default().fg(Color::White)),
        Span::styled(" ]", Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled("[ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(Color::White)),
        Span::styled(" ]", Style::default().fg(Color::DarkGray)),
    ];
    if state.background_transfers.len() > 1 {
        btn_spans.push(Span::raw("  "));
        btn_spans.push(Span::styled("[ ", Style::default().fg(Color::DarkGray)));
        btn_spans.push(Span::styled(
            "Tab",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        btn_spans.push(Span::styled(" next", Style::default().fg(Color::White)));
        btn_spans.push(Span::styled(" ]", Style::default().fg(Color::DarkGray)));
    }
    lines.push(Line::from(btn_spans));

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(Paragraph::new(lines).block(block), popup_area);
}

/// Draw the MC-style "File exists" overwrite confirmation popup.
fn draw_overwrite_confirm_popup(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    dst_path: &str,
    size: u64,
    popup_focus: usize,
) {
    let popup_h: u16 = 9;
    let popup_area = centered_fixed_rect(OVERWRITE_POPUP_WIDTH, popup_h, area);
    if popup_area.width < 30 || popup_area.height < popup_h {
        return;
    }

    let block = Block::default()
        .title(" File exists ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = Rect::new(
        popup_area.x + 2,
        popup_area.y + 1,
        popup_area.width.saturating_sub(4),
        popup_area.height.saturating_sub(2),
    );

    let max_path = inner.width as usize;
    frame.render_widget(
        Paragraph::new(" Target file already exists:").style(Style::default().fg(Color::White)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(
            " {} ({})",
            truncate_name(name, max_path.saturating_sub(15)),
            format_bytes(size)
        ))
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", truncate_name(dst_path, max_path)))
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );

    // Separator
    let sep = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep).style(Style::default().fg(Color::Yellow)),
        Rect::new(inner.x, inner.y + 4, inner.width, 1),
    );

    // All 5 buttons on one row
    render_button_row(
        &["Overwrite", "Overwrite all", "Skip", "Skip all", "Cancel"],
        popup_focus,
        inner,
        inner.y + 5,
        frame,
    );
}

/// Draw the transfer complete popup overlay.
fn draw_transfer_complete_popup(frame: &mut Frame, area: Rect, msg: &str) {
    let popup_area = centered_fixed_rect(POPUP_WIDTH, 5, area);
    if popup_area.width < 20 || popup_area.height < 5 {
        return;
    }

    let lines = vec![
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(" Press any key to close"),
    ];

    let block = Block::default()
        .title(" Copy Complete ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(Paragraph::new(lines).block(block), popup_area);
}

/// Format a completion message for the transfer complete popup.
fn format_transfer_complete(
    result: &TransferResult,
    total_bytes: u64,
    elapsed: Duration,
    is_move: bool,
) -> String {
    let verb_past = if is_move { "Moved" } else { "Copied" };
    let verb_noun = if is_move { "Move" } else { "Copy" };
    let elapsed_secs = elapsed.as_secs();
    if result.copied == 0 && result.last_error.is_none() {
        format!("{verb_noun} cancelled")
    } else if let Some(ref err) = result.last_error {
        format!(
            "{verb_past} {}/{} files, error: {err}",
            result.copied, result.total
        )
    } else if result.total == 1 {
        format!(
            "\u{2713} {verb_past} 1 file ({}) in {}",
            format_bytes(total_bytes),
            format_duration(elapsed_secs),
        )
    } else {
        format!(
            "\u{2713} {verb_past} {} files ({}) in {}",
            result.total,
            format_bytes(total_bytes),
            format_duration(elapsed_secs),
        )
    }
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
    if transfers.len() == 1 {
        let t = &transfers[0];
        let p = &t.progress;
        let bytes_done_all = p.bytes_done_all.load(Ordering::Relaxed);
        let total_bytes_all = p.total_bytes_all.load(Ordering::Relaxed);
        let files_done = p.files_done.load(Ordering::Relaxed);
        let total_files = p.total_files.load(Ordering::Relaxed);

        let arrow = match t.direction {
            TransferDirection::LocalToRemote => "\u{2191}",
            TransferDirection::RemoteToLocal => "\u{2193}",
        };

        let pct = if total_bytes_all > 0 {
            (bytes_done_all as f64 / total_bytes_all as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        let name = {
            let active = p.active_files.lock().unwrap();
            active
                .iter()
                .find_map(|slot| slot.as_ref().map(|f| f.name.clone()))
                .unwrap_or_default()
        };

        if total_files <= 1 {
            format!(
                "{arrow} {name} [{}/{} {pct:.0}%] (p=progress)",
                format_bytes(bytes_done_all),
                format_bytes(total_bytes_all)
            )
        } else {
            format!(
                "{arrow} {files_done}/{total_files}: {name} [{}/{} {pct:.0}%] (p=progress)",
                format_bytes(bytes_done_all),
                format_bytes(total_bytes_all)
            )
        }
    } else {
        // Aggregate across all transfers
        let mut total_bytes = 0u64;
        let mut done_bytes = 0u64;
        let mut total_files = 0u64;
        let mut done_files = 0u64;
        for t in transfers {
            let p = &t.progress;
            total_bytes += p.total_bytes_all.load(Ordering::Relaxed);
            done_bytes += p.bytes_done_all.load(Ordering::Relaxed);
            total_files += p.total_files.load(Ordering::Relaxed);
            done_files += p.files_done.load(Ordering::Relaxed);
        }
        let pct = if total_bytes > 0 {
            (done_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        format!(
            "{} transfers: {done_files}/{total_files} files [{}/{} {pct:.0}%] (p=progress)",
            transfers.len(),
            format_bytes(done_bytes),
            format_bytes(total_bytes),
        )
    }
}

/// Short label for a transfer (description or index).
fn transfer_label(transfers: &[BackgroundTransfer], idx: usize) -> String {
    transfers
        .get(idx)
        .map(|t| t.description.clone())
        .unwrap_or_else(|| format!("#{}", idx + 1))
}

/// Format a completion message for a finished background transfer.
fn format_transfer_result(result: &TransferResult, description: &str, is_move: bool) -> String {
    let verb_past = if is_move { "Moved" } else { "Copied" };
    let verb_noun = if is_move { "Move" } else { "Copy" };
    if result.copied == 0 && result.last_error.is_none() {
        format!("{verb_noun} cancelled")
    } else if let Some(ref err) = result.last_error {
        format!(
            "{verb_past} {}/{}, error: {err}",
            result.copied, result.total
        )
    } else if result.total == 1 {
        format!("{verb_past}: {description}")
    } else {
        format!("{verb_past} {} items", result.total)
    }
}

/// Run a file transfer using a pool of parallel SFTP workers.
/// Takes pre-opened SFTP sessions for concurrent transfers.
#[allow(clippy::too_many_arguments)]
async fn run_background_transfer(
    worker_sessions: Vec<SftpSession>,
    targets: Vec<TransferTarget>,
    progress: Arc<TransferProgress>,
    cancel: Arc<AtomicBool>,
    skip: Arc<AtomicBool>,
    direction: TransferDirection,
    overwrite_tx: tokio::sync::mpsc::Sender<OverwriteQuery>,
    overwrite_policy: Arc<AtomicU64>,
) -> TransferResult {
    assert!(
        !worker_sessions.is_empty(),
        "run_background_transfer requires at least one SFTP session"
    );

    // Use first session for scanning
    let scan_sftp = &worker_sessions[0];

    // Expand directories into flat file lists
    let targets = match expand_directory_targets(scan_sftp, targets, direction, &progress).await {
        Ok(t) => t,
        Err(e) => {
            progress.scanning.store(false, Ordering::Relaxed);
            return TransferResult {
                copied: 0,
                total: 0,
                last_error: Some(format!("Failed to expand directories: {e}")),
            };
        }
    };

    progress.scanning.store(false, Ordering::Relaxed);

    // Separate dirs (create sequentially) and files
    let mut dir_targets = Vec::new();
    let mut file_targets = Vec::new();
    for target in targets {
        if target.is_dir {
            dir_targets.push(target);
        } else {
            file_targets.push(target);
        }
    }

    // Create directories sequentially first (order matters for nested dirs)
    for dir_target in &dir_targets {
        if cancel.load(Ordering::Relaxed) {
            return TransferResult {
                copied: 0,
                total: file_targets.len(),
                last_error: None,
            };
        }
        let result = match direction {
            TransferDirection::LocalToRemote => scan_sftp
                .create_dir(&dir_target.dst_path)
                .await
                .with_context(|| format!("Failed to create remote dir: {}", dir_target.dst_path)),
            TransferDirection::RemoteToLocal => tokio::fs::create_dir_all(&dir_target.dst_path)
                .await
                .with_context(|| format!("Failed to create local dir: {}", dir_target.dst_path)),
        };
        if let Err(e) = result {
            tracing::error!("mkdir failed: {}: {e:#}", dir_target.dst_path);
        }
    }

    let total = file_targets.len();
    let total_bytes: u64 = file_targets.iter().map(|t| t.size).sum();
    progress.total_files.store(total as u64, Ordering::Relaxed);
    progress
        .total_bytes_all
        .store(total_bytes, Ordering::Relaxed);

    if total == 0 {
        return TransferResult {
            copied: 0,
            total: 0,
            last_error: None,
        };
    }

    let actual_workers = worker_sessions.len().min(total).min(TRANSFER_WORKERS);
    {
        let mut active = progress.active_files.lock().unwrap();
        active.resize(actual_workers, None);
    }

    // Set up work queue channel
    let (tx, rx) = tokio::sync::mpsc::channel::<TransferTarget>(actual_workers * 2);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    // Spawn workers
    let copied = Arc::new(AtomicU64::new(0));
    let last_error: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let mut join_set = tokio::task::JoinSet::new();

    for (worker_id, sftp) in worker_sessions.into_iter().take(actual_workers).enumerate() {
        let rx = Arc::clone(&rx);
        let progress = Arc::clone(&progress);
        let cancel = Arc::clone(&cancel);
        let skip = Arc::clone(&skip);
        let copied = Arc::clone(&copied);
        let last_error = Arc::clone(&last_error);
        let overwrite_tx = overwrite_tx.clone();
        let overwrite_policy = Arc::clone(&overwrite_policy);

        join_set.spawn(async move {
            loop {
                let target = {
                    let mut rx = rx.lock().await;
                    rx.recv().await
                };
                let Some(target) = target else {
                    break; // Channel closed, no more work
                };

                if cancel.load(Ordering::Relaxed) {
                    break;
                }

                // Check if destination file already exists
                let exists = match direction {
                    TransferDirection::LocalToRemote => {
                        sftp.metadata(&target.dst_path).await.is_ok()
                    }
                    TransferDirection::RemoteToLocal => {
                        tokio::fs::metadata(&target.dst_path).await.is_ok()
                    }
                };

                if exists {
                    let policy = overwrite_policy.load(Ordering::Relaxed);
                    if policy == 2 {
                        // Skip all — skip without asking
                        progress.files_done.fetch_add(1, Ordering::Relaxed);
                        progress
                            .bytes_done_all
                            .fetch_add(target.size, Ordering::Relaxed);
                        continue;
                    } else if policy == 0 {
                        // Ask the user
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        let query = OverwriteQuery {
                            name: target.name.clone(),
                            dst_path: target.dst_path.clone(),
                            size: target.size,
                            response: resp_tx,
                        };
                        if overwrite_tx.send(query).await.is_err() {
                            break; // UI gone
                        }
                        match resp_rx.await {
                            Ok(OverwriteAnswer::Overwrite) => {
                                // Proceed with this file
                            }
                            Ok(OverwriteAnswer::OverwriteAll) => {
                                overwrite_policy.store(1, Ordering::Relaxed);
                                // Proceed with this file
                            }
                            Ok(OverwriteAnswer::Skip) => {
                                progress.files_done.fetch_add(1, Ordering::Relaxed);
                                progress
                                    .bytes_done_all
                                    .fetch_add(target.size, Ordering::Relaxed);
                                continue;
                            }
                            Ok(OverwriteAnswer::SkipAll) => {
                                overwrite_policy.store(2, Ordering::Relaxed);
                                progress.files_done.fetch_add(1, Ordering::Relaxed);
                                progress
                                    .bytes_done_all
                                    .fetch_add(target.size, Ordering::Relaxed);
                                continue;
                            }
                            Ok(OverwriteAnswer::Cancel) | Err(_) => {
                                cancel.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    // policy == 1 (overwrite all) falls through to transfer
                }

                // Set active file info for this worker
                {
                    let mut active = progress.active_files.lock().unwrap();
                    if worker_id < active.len() {
                        active[worker_id] = Some(ActiveFile {
                            name: target.name.clone(),
                            src_path: target.src_path.clone(),
                            dst_path: target.dst_path.clone(),
                            bytes_done: 0,
                            bytes_total: target.size,
                        });
                    }
                }

                let result = match direction {
                    TransferDirection::LocalToRemote => {
                        transfer_local_to_remote(
                            &sftp,
                            &target.src_path,
                            &target.dst_path,
                            worker_id,
                            &progress,
                            &cancel,
                            &skip,
                        )
                        .await
                    }
                    TransferDirection::RemoteToLocal => {
                        transfer_remote_to_local(
                            &sftp,
                            &target.src_path,
                            &target.dst_path,
                            worker_id,
                            &progress,
                            &cancel,
                            &skip,
                        )
                        .await
                    }
                };

                // Clear active file slot
                {
                    let mut active = progress.active_files.lock().unwrap();
                    if worker_id < active.len() {
                        active[worker_id] = None;
                    }
                }

                match result {
                    Ok(()) => {
                        copied.fetch_add(1, Ordering::Relaxed);
                        progress.files_done.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) if cancel.load(Ordering::Relaxed) => break,
                    Err(e) if skip.swap(false, Ordering::Relaxed) => {
                        tracing::debug!("skipped: {}: {e:#}", target.name);
                    }
                    Err(e) => {
                        tracing::error!("transfer failed: {}: {e:#}", target.name);
                        let mut err = last_error.lock().unwrap();
                        *err = Some(format!("{}: {e}", target.name));
                    }
                }
            }
        });
    }

    // Feed work items into the channel
    for target in file_targets {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if tx.send(target).await.is_err() {
            break; // All workers gone
        }
    }
    drop(tx); // Close channel so workers exit when done

    // Wait for all workers to finish
    while let Some(result) = join_set.join_next().await {
        if let Err(e) = result {
            tracing::error!("worker task panicked: {e}");
        }
    }

    let final_copied = copied.load(Ordering::Relaxed) as usize;
    let final_error = last_error.lock().unwrap().clone();

    TransferResult {
        copied: final_copied,
        total,
        last_error: final_error,
    }
}

/// Upload a local file to a remote SFTP destination.
async fn transfer_local_to_remote(
    sftp: &SftpSession,
    local_path: &str,
    remote_path: &str,
    worker_id: usize,
    progress: &TransferProgress,
    cancel: &AtomicBool,
    skip: &AtomicBool,
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
        if skip.load(Ordering::Relaxed) {
            anyhow::bail!("File skipped");
        }
        let n = local_file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        remote_file.write_all(&buf[..n]).await?;
        progress
            .bytes_done_all
            .fetch_add(n as u64, Ordering::Relaxed);
        // Update per-worker active file progress
        {
            let mut active = progress.active_files.lock().unwrap();
            if let Some(Some(af)) = active.get_mut(worker_id) {
                af.bytes_done += n as u64;
            }
        }
    }

    remote_file.shutdown().await?;
    Ok(())
}

/// Download a remote SFTP file to a local destination.
async fn transfer_remote_to_local(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &str,
    worker_id: usize,
    progress: &TransferProgress,
    cancel: &AtomicBool,
    skip: &AtomicBool,
) -> Result<()> {
    // Ensure parent directory exists for nested files
    if let Some(parent) = std::path::Path::new(local_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create parent dir: {}", parent.display()))?;
    }

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
        if skip.load(Ordering::Relaxed) {
            anyhow::bail!("File skipped");
        }
        let n = remote_file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        local_file.write_all(&buf[..n]).await?;
        progress
            .bytes_done_all
            .fetch_add(n as u64, Ordering::Relaxed);
        {
            let mut active = progress.active_files.lock().unwrap();
            if let Some(Some(af)) = active.get_mut(worker_id) {
                af.bytes_done += n as u64;
            }
        }
    }

    Ok(())
}

/// Start a copy transfer from the CopyConfirm state. Opens an SFTP channel, expands
/// directory targets, and spawns the background transfer task.
/// If `background` is true, goes straight to Normal mode instead of TransferPopup.
#[allow(clippy::too_many_arguments)]
async fn start_copy_transfer(
    targets: Vec<(String, String, bool, u64)>,
    direction: TransferDirection,
    source_side: Side,
    dst_cwd: &str,
    left_pane: &mut PaneState,
    right_pane: &mut PaneState,
    state: &mut BrowserState,
    left: &mut Backend,
    right: &mut Backend,
    background: bool,
    is_move: bool,
) -> Result<()> {
    let (src_label, _) = match source_side {
        Side::Left => (state.left_label, state.right_label),
        Side::Right => (state.right_label, state.left_label),
    };

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

    let ssh_handle = remote_backend.ssh_handle();
    match ssh_handle {
        Some(handle) => {
            // Open SFTP worker sessions before spawning the transfer task.
            // We need at least one; try to open up to TRANSFER_WORKERS.
            let mut worker_sessions = Vec::new();
            for i in 0..TRANSFER_WORKERS {
                match open_sftp_from_handle(handle).await {
                    Ok(s) => worker_sessions.push(s),
                    Err(e) => {
                        if i == 0 {
                            // Can't open even one session — fail
                            state.status_message =
                                Some(format!("Failed to open SFTP session: {e}"));
                            state.input_mode = InputMode::Normal;
                            return Ok(());
                        }
                        tracing::warn!("Opened {i} SFTP sessions (wanted {TRANSFER_WORKERS}): {e}");
                        break;
                    }
                }
            }

            let dest_side = match source_side {
                Side::Left => Side::Right,
                Side::Right => Side::Left,
            };

            let transfer_targets: Vec<TransferTarget> = targets
                .iter()
                .map(|(src, name, is_dir, size)| TransferTarget {
                    src_path: src.clone(),
                    dst_path: format!("{}/{}", dst_cwd.trim_end_matches('/'), name),
                    name: name.clone(),
                    size: *size,
                    is_dir: *is_dir,
                })
                .collect();

            let description = if transfer_targets.len() == 1 {
                transfer_targets[0].name.clone()
            } else {
                format!("{} files", transfer_targets.len())
            };

            let total_bytes_all: u64 = transfer_targets.iter().map(|t| t.size).sum();
            let progress = Arc::new(TransferProgress::new(
                transfer_targets.len() as u64,
                total_bytes_all,
            ));
            let cancel = Arc::new(AtomicBool::new(false));
            let skip = Arc::new(AtomicBool::new(false));

            let bg_progress = Arc::clone(&progress);
            let bg_cancel = Arc::clone(&cancel);
            let bg_skip = Arc::clone(&skip);
            let (ow_tx, ow_rx) = tokio::sync::mpsc::channel::<OverwriteQuery>(1);
            let ow_policy = Arc::new(AtomicU64::new(0));
            let bg_ow_policy = Arc::clone(&ow_policy);
            let handle = tokio::spawn(async move {
                run_background_transfer(
                    worker_sessions,
                    transfer_targets,
                    bg_progress,
                    bg_cancel,
                    bg_skip,
                    direction,
                    ow_tx,
                    bg_ow_policy,
                )
                .await
            });

            // For move operations, record source paths to delete after transfer completes.
            let delete_sources = if is_move {
                Some(
                    targets
                        .iter()
                        .map(|(path, _, is_dir, _)| (path.clone(), *is_dir))
                        .collect(),
                )
            } else {
                None
            };

            state.background_transfers.push(BackgroundTransfer {
                handle,
                progress,
                cancel,
                skip,
                dest_side,
                direction,
                description,
                started_at: std::time::Instant::now(),
                overwrite_rx: ow_rx,
                overwrite_policy: ow_policy,
                delete_sources,
                source_side,
            });
            state.popup_transfer_index = state.background_transfers.len() - 1;

            // Clear source marks immediately
            let src_pane = match source_side {
                Side::Left => &mut *left_pane,
                Side::Right => &mut *right_pane,
            };
            src_pane.marked.clear();

            state.input_mode = if background {
                InputMode::Normal
            } else {
                InputMode::TransferPopup
            };
        }
        None => {
            state.status_message = Some("No SSH handle available for transfer".to_string());
            state.input_mode = InputMode::Normal;
        }
    }
    Ok(())
}

/// Recursively walk a remote directory via SFTP, returning all entries relative to `base`.
///
/// Issues up to [`SFTP_SCAN_CONCURRENCY`] `read_dir` calls concurrently to hide
/// network latency when scanning deep directory trees.
async fn walk_remote_dir(
    sftp: &SftpSession,
    path: &str,
    base: &str,
    scan_counter: Option<&AtomicU64>,
) -> Result<Vec<(String, bool, u64)>> {
    let mut result = Vec::new();
    let mut dirs_to_visit = vec![path.to_string()];
    let base_trimmed = base.trim_end_matches('/');

    while !dirs_to_visit.is_empty() {
        // Drain up to SFTP_SCAN_CONCURRENCY directories and read them concurrently.
        let batch: Vec<_> = dirs_to_visit
            .drain(..dirs_to_visit.len().min(SFTP_SCAN_CONCURRENCY))
            .collect();

        let futures: Vec<_> = batch
            .iter()
            .map(|dir_path| sftp.read_dir(dir_path.as_str()))
            .collect();
        let results = join_all(futures).await;

        for (dir_path, read_result) in batch.iter().zip(results) {
            let entries = read_result
                .with_context(|| format!("Failed to read remote directory: {dir_path}"))?;
            for entry in entries {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let full_path = format!("{}/{}", dir_path.trim_end_matches('/'), name);
                let relative = full_path
                    .strip_prefix(base_trimmed)
                    .unwrap_or(&full_path)
                    .trim_start_matches('/')
                    .to_string();
                let is_dir = entry.file_type().is_dir();
                let size = entry.metadata().size.unwrap_or(0);
                result.push((relative, is_dir, size));
                if let Some(counter) = scan_counter {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                if is_dir {
                    dirs_to_visit.push(full_path);
                }
            }
        }
    }
    Ok(result)
}

/// Recursively walk a local directory, returning all entries relative to `base`.
async fn walk_local_dir(
    path: &str,
    base: &str,
    scan_counter: Option<&AtomicU64>,
) -> Result<Vec<(String, bool, u64)>> {
    let mut result = Vec::new();
    let mut dirs_to_visit = vec![std::path::PathBuf::from(path)];

    while let Some(dir_path) = dirs_to_visit.pop() {
        let mut read_dir = tokio::fs::read_dir(&dir_path)
            .await
            .with_context(|| format!("Failed to read local directory: {}", dir_path.display()))?;
        while let Some(entry) = read_dir.next_entry().await? {
            let full_path = entry.path();
            let relative = full_path
                .strip_prefix(base)
                .unwrap_or(&full_path)
                .to_string_lossy()
                .to_string();
            let metadata = entry.metadata().await?;
            let is_dir = metadata.is_dir();
            let size = if is_dir { 0 } else { metadata.len() };
            result.push((relative, is_dir, size));
            if let Some(counter) = scan_counter {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            if is_dir {
                dirs_to_visit.push(full_path);
            }
        }
    }
    Ok(result)
}

/// Expand directory targets into flat file lists, creating destination directories as needed.
async fn expand_directory_targets(
    sftp: &SftpSession,
    targets: Vec<TransferTarget>,
    direction: TransferDirection,
    progress: &TransferProgress,
) -> Result<Vec<TransferTarget>> {
    let mut expanded = Vec::new();

    for target in targets {
        if !target.is_dir {
            expanded.push(target);
            continue;
        }

        // Walk the source directory
        let children = match direction {
            TransferDirection::LocalToRemote => {
                walk_local_dir(
                    &target.src_path,
                    &target.src_path,
                    Some(&progress.scan_entries_found),
                )
                .await?
            }
            TransferDirection::RemoteToLocal => {
                walk_remote_dir(
                    sftp,
                    &target.src_path,
                    &target.src_path,
                    Some(&progress.scan_entries_found),
                )
                .await?
            }
        };

        // Add the root directory itself
        expanded.push(TransferTarget {
            src_path: target.src_path.clone(),
            dst_path: target.dst_path.clone(),
            name: target.name.clone(),
            size: 0,
            is_dir: true,
        });

        // Add all children
        for (relative, is_dir, size) in children {
            let src = format!("{}/{}", target.src_path.trim_end_matches('/'), relative);
            let dst = format!("{}/{}", target.dst_path.trim_end_matches('/'), relative);
            let name = relative.rsplit('/').next().unwrap_or(&relative).to_string();
            expanded.push(TransferTarget {
                src_path: src,
                dst_path: dst,
                name,
                size,
                is_dir,
            });
        }
    }

    Ok(expanded)
}

/// Handle input modes: filter, mkdir prompt, rename prompt, confirm delete, pattern select,
/// copy confirm, transfer progress popup, transfer complete popup.
async fn handle_input_mode(
    key: KeyEvent,
    left_pane: &mut PaneState,
    right_pane: &mut PaneState,
    state: &mut BrowserState,
    left: &mut Backend,
    right: &mut Backend,
) -> Result<()> {
    // Transfer complete popup: any key dismisses
    if matches!(state.input_mode, InputMode::TransferComplete(_)) {
        state.input_mode = InputMode::Normal;
        return Ok(());
    }

    // Transfer progress popup: s=skip, b=background, Esc=cancel, Tab=next transfer
    if matches!(state.input_mode, InputMode::TransferPopup) {
        let idx = state
            .popup_transfer_index
            .min(state.background_transfers.len().saturating_sub(1));
        match key.code {
            KeyCode::Char('s') => {
                if let Some(t) = state.background_transfers.get(idx) {
                    t.skip.store(true, Ordering::Relaxed);
                }
            }
            KeyCode::Tab if state.background_transfers.len() > 1 => {
                state.popup_transfer_index =
                    (state.popup_transfer_index + 1) % state.background_transfers.len();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.input_mode = InputMode::Normal;
            }
            KeyCode::Char('b') => {
                state.input_mode = InputMode::Normal;
            }
            KeyCode::Esc => {
                if idx < state.background_transfers.len() {
                    let label = transfer_label(&state.background_transfers, idx);
                    let transfer = state.background_transfers.remove(idx);
                    transfer.cancel.store(true, Ordering::Relaxed);
                    transfer.handle.abort();
                    state.status_message = Some(format!("Cancelled transfer {label}"));
                    // Clamp popup index after removal
                    if state.popup_transfer_index >= state.background_transfers.len()
                        && !state.background_transfers.is_empty()
                    {
                        state.popup_transfer_index = state.background_transfers.len() - 1;
                    }
                }
                if state.background_transfers.is_empty() {
                    state.input_mode = InputMode::Normal;
                }
            }
            _ => {} // Ignore all other keys
        }
        return Ok(());
    }

    // Overwrite confirmation popup: 5 buttons
    if matches!(state.input_mode, InputMode::OverwriteConfirm { .. }) {
        let focus = state.popup_focus;
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                state.popup_focus = (state.popup_focus + 1) % 5;
                return Ok(());
            }
            KeyCode::BackTab | KeyCode::Left => {
                state.popup_focus = (state.popup_focus + 4) % 5;
                return Ok(());
            }
            KeyCode::Enter => {
                let answer = match focus {
                    0 => OverwriteAnswer::Overwrite,
                    1 => OverwriteAnswer::OverwriteAll,
                    2 => OverwriteAnswer::Skip,
                    3 => OverwriteAnswer::SkipAll,
                    _ => OverwriteAnswer::Cancel,
                };

                // For "all" choices, set the shared policy so all workers see it
                // immediately, then drain any queued queries with the same answer.
                let is_all = matches!(
                    answer,
                    OverwriteAnswer::OverwriteAll | OverwriteAnswer::SkipAll
                );
                if is_all {
                    let policy_val = match answer {
                        OverwriteAnswer::OverwriteAll => 1u64,
                        OverwriteAnswer::SkipAll => 2u64,
                        _ => 0,
                    };
                    // Set policy on ALL active transfers (acts as one logical operation)
                    for t in &mut state.background_transfers {
                        t.overwrite_policy.store(policy_val, Ordering::Relaxed);
                        // Drain pending queries and auto-answer them
                        while let Ok(q) = t.overwrite_rx.try_recv() {
                            let _ = q.response.send(answer);
                        }
                    }
                }

                // Answer the original query
                if let Some(tx) = state.overwrite_response_tx.take() {
                    let _ = tx.send(answer);
                }
                state.input_mode = InputMode::TransferPopup;
            }
            KeyCode::Esc => {
                if let Some(tx) = state.overwrite_response_tx.take() {
                    let _ = tx.send(OverwriteAnswer::Cancel);
                }
                state.input_mode = InputMode::Normal;
            }
            _ => {}
        }
        return Ok(());
    }

    // Popup button navigation: Tab/Right = next, Shift+Tab/Left = prev
    if matches!(
        state.input_mode,
        InputMode::MkdirPrompt(_)
            | InputMode::RenamePrompt { .. }
            | InputMode::ConfirmDelete { .. }
            | InputMode::CopyConfirm { .. }
            | InputMode::SelectPattern { .. }
    ) {
        let button_count = if matches!(state.input_mode, InputMode::CopyConfirm { .. }) {
            3
        } else {
            2
        };
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                state.popup_focus = (state.popup_focus + 1) % button_count;
                return Ok(());
            }
            KeyCode::Left | KeyCode::BackTab => {
                state.popup_focus = (state.popup_focus + button_count - 1) % button_count;
                return Ok(());
            }
            _ => {}
        }
    }

    // Copy/Move confirmation popup: Enter=focus-aware, b=background, Esc=cancel
    if matches!(state.input_mode, InputMode::CopyConfirm { .. }) {
        let focus = state.popup_focus;
        let mode = std::mem::replace(&mut state.input_mode, InputMode::Normal);
        if let InputMode::CopyConfirm {
            targets,
            direction,
            source_side,
            dst_cwd,
            is_move,
        } = mode
        {
            match key.code {
                KeyCode::Enter => {
                    match focus {
                        0 => {
                            // OK — foreground copy/move
                            start_copy_transfer(
                                targets,
                                direction,
                                source_side,
                                &dst_cwd,
                                left_pane,
                                right_pane,
                                state,
                                left,
                                right,
                                false,
                                is_move,
                            )
                            .await?;
                        }
                        1 => {
                            // Bkgnd — background copy/move
                            start_copy_transfer(
                                targets,
                                direction,
                                source_side,
                                &dst_cwd,
                                left_pane,
                                right_pane,
                                state,
                                left,
                                right,
                                true,
                                is_move,
                            )
                            .await?;
                        }
                        _ => {
                            // Cancel — already set to Normal
                        }
                    }
                }
                KeyCode::Char('b') => {
                    start_copy_transfer(
                        targets,
                        direction,
                        source_side,
                        &dst_cwd,
                        left_pane,
                        right_pane,
                        state,
                        left,
                        right,
                        true,
                        is_move,
                    )
                    .await?;
                }
                KeyCode::Esc => {
                    // Already set to Normal above
                }
                _ => {
                    // Put it back for unrecognized keys
                    state.input_mode = InputMode::CopyConfirm {
                        targets,
                        direction,
                        source_side,
                        dst_cwd,
                        is_move,
                    };
                }
            }
        }
        return Ok(());
    }

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
        let focus = state.popup_focus;
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
            KeyCode::Enter => {
                if focus == 0 {
                    // OK — perform delete
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
                } else {
                    // Cancel
                    state.status_message = Some("Delete cancelled.".to_string());
                }
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
                if state.popup_focus == 0 {
                    // OK — create directory
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
                // else: Cancel — mode already set to Normal
            }
            InputMode::RenamePrompt { input, source } => {
                if state.popup_focus == 0 {
                    // OK — rename/move
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
                                tracing::error!(
                                    "rename failed: {} → {new_path}: {e:#}",
                                    source.path
                                );
                                state.status_message = Some(format!("Rename error: {e}"));
                            }
                        }
                    }
                }
                // else: Cancel — mode already set to Normal
            }
            InputMode::SelectPattern { input, selecting } => {
                if state.popup_focus == 0 {
                    // OK
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
                // else: Cancel — mode already set to Normal
            }
            InputMode::Normal
            | InputMode::ConfirmDelete { .. }
            | InputMode::CopyConfirm { .. }
            | InputMode::TransferPopup
            | InputMode::OverwriteConfirm { .. }
            | InputMode::TransferComplete(_) => {}
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
    spans.extend(hint_pair("F6", "Move", theme));
    spans.extend(hint_pair("F7", "Mkdir", theme));
    spans.extend(hint_pair("F8", "Del", theme));
    spans.extend(hint_pair("v", "Mark", theme));
    spans.extend(hint_pair("/", "Filter", theme));
    spans.extend(hint_pair("Tab", "Switch", theme));
    spans.extend(hint_pair("Esc", "Quit", theme));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Open a new SFTP session from an SSH handle (standalone function for use in spawned tasks).
async fn open_sftp_from_handle(
    handle: &russh::client::Handle<crate::ssh::client::SshoreHandler>,
) -> Result<SftpSession> {
    let channel = handle
        .channel_open_session()
        .await
        .context("Failed to open SSH channel for SFTP")?;

    channel
        .request_subsystem(true, "sftp")
        .await
        .context("Failed to request SFTP subsystem")?;

    SftpSession::new(channel.into_stream())
        .await
        .context("Failed to initialize SFTP session")
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
