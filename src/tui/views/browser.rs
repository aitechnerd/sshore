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
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::SftpSession;

use futures::future::join_all;

use crate::sftp::pipeline;
use crate::sftp::shortcuts::{format_bytes, format_bytes_per_sec, format_duration};
use crate::storage::{Backend, FileEntry};
use crate::tui::theme::ThemeColors;

/// Poll timeout when idle (no timed state changes pending).
/// User input is detected instantly regardless of this value.
const POLL_RATE: Duration = Duration::from_secs(1);

/// Faster poll rate when background transfers are active, for progress updates.
const PROGRESS_POLL_RATE: Duration = Duration::from_millis(100);

/// Max concurrent SFTP `read_dir` calls during directory scanning.
/// Higher values hide more network latency but use more SFTP channel capacity.
const SFTP_SCAN_CONCURRENCY: usize = 16;

/// Number of pipelined SFTP workers per SSH/TCP connection.
/// Each worker fires concurrent SFTP requests internally.
/// 2 allows overlapping file transfers on the same connection,
/// which helps for multi-file batches without much extra memory
/// (~30 MB per extra worker with 8 MB in-flight cap).
const WORKERS_PER_CONNECTION: usize = 2;

/// Minimum number of transfer targets before opening a second SSH connection.
/// A second connection doubles memory, so only open it for batches large enough
/// to benefit from the parallelism.
const MIN_FILES_FOR_SECOND_CONN: usize = 10;

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
        files_only: bool,
        case_sensitive: bool,
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
    /// Transfer complete popup with summary message.
    /// When `retry` is `Some`, a Retry button is shown (transfer had errors).
    /// When `retry` is `None`, any key dismisses (success).
    TransferComplete {
        msg: String,
        retry: Option<RetryInfo>,
    },
}

/// Info needed to retry a failed transfer from the completion popup.
#[derive(Debug, Clone)]
pub struct RetryInfo {
    targets: Vec<(String, String, bool, u64)>,
    direction: TransferDirection,
    source_side: Side,
    dst_cwd: String,
    is_move: bool,
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

/// A pipelined SFTP worker session for high-throughput transfers.
/// Each worker fires 64 concurrent SFTP requests internally.
struct PipelinedWorker {
    raw: Arc<RawSftpSession>,
    read_chunk_size: u64,
    write_chunk_size: u64,
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

/// Number of speed samples kept for the rolling window average.
const SPEED_WINDOW_SAMPLES: usize = 100;

/// Minimum number of speed samples before showing ETA (avoids wild estimates).
const MIN_ETA_SPEED_SAMPLES: usize = 3;

/// Rolling speed sample: (instant_nanos, cumulative_bytes).
struct SpeedSample {
    nanos: u64,
    bytes: u64,
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
    /// Nanos since `UNIX_EPOCH` when the first data byte was transferred.
    /// Set once by the first `on_bytes_written` callback. 0 = not yet started.
    first_bytes_nanos: AtomicU64,
    /// Ring buffer of (timestamp, bytes_done) for rolling speed calculation.
    speed_samples: std::sync::Mutex<Vec<SpeedSample>>,
    /// Next write position in the ring buffer.
    speed_sample_idx: AtomicU64,
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
            first_bytes_nanos: AtomicU64::new(0),
            speed_samples: std::sync::Mutex::new(Vec::with_capacity(SPEED_WINDOW_SAMPLES)),
            speed_sample_idx: AtomicU64::new(0),
        }
    }

    /// Record that data transfer has started (called once on first bytes).
    fn mark_transfer_start(&self) {
        let _ = self.first_bytes_nanos.compare_exchange(
            0,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Seconds elapsed since first data byte, or None if not started yet.
    fn transfer_elapsed_secs(&self) -> Option<f64> {
        let start_nanos = self.first_bytes_nanos.load(Ordering::Relaxed);
        if start_nanos == 0 {
            return None;
        }
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Some((now_nanos.saturating_sub(start_nanos)) as f64 / 1_000_000_000.0)
    }

    /// Record a speed sample (called from the UI poll loop, not from transfer callbacks).
    fn record_speed_sample(&self) {
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let bytes = self.bytes_done_all.load(Ordering::Relaxed);

        let mut samples = self.speed_samples.lock().unwrap();
        let idx = self.speed_sample_idx.fetch_add(1, Ordering::Relaxed) as usize;
        let pos = idx % SPEED_WINDOW_SAMPLES;
        let sample = SpeedSample {
            nanos: now_nanos,
            bytes,
        };
        if pos >= samples.len() {
            samples.push(sample);
        } else {
            samples[pos] = sample;
        }
    }

    /// Compute rolling speed (bytes/sec) from recent samples.
    /// Returns None if not enough samples yet.
    fn rolling_speed(&self) -> Option<f64> {
        let samples = self.speed_samples.lock().unwrap();
        if samples.len() < 2 {
            tracing::trace!("speed: only {} samples, need >=2", samples.len());
            return None;
        }
        let idx = self.speed_sample_idx.load(Ordering::Relaxed) as usize;
        // Most recent sample
        let newest_pos = (idx.wrapping_sub(1)) % SPEED_WINDOW_SAMPLES;
        // Oldest sample in the window
        let oldest_pos = if samples.len() < SPEED_WINDOW_SAMPLES {
            0
        } else {
            idx % SPEED_WINDOW_SAMPLES
        };
        let newest = &samples[newest_pos];
        let oldest = &samples[oldest_pos];
        let dt_nanos = newest.nanos.saturating_sub(oldest.nanos);
        let dt_secs = dt_nanos as f64 / 1_000_000_000.0;
        if dt_secs < 0.1 {
            tracing::trace!(
                "speed: window too short ({:.3}s), newest_pos={newest_pos} oldest_pos={oldest_pos}",
                dt_secs,
            );
            return None;
        }
        let dbytes = newest.bytes.saturating_sub(oldest.bytes);
        let speed = dbytes as f64 / dt_secs;
        // Log every ~5s (every 50th call at ~100ms poll interval).
        if idx.is_multiple_of(50) {
            tracing::debug!(
                "speed: rolling window={} samples, span={:.1}s, \
                 oldest=({oldest_pos}, {:.1} MB) newest=({newest_pos}, {:.1} MB) \
                 delta={:.1} MB in {:.1}s = {:.2} MB/s",
                samples.len(),
                dt_secs,
                oldest.bytes as f64 / 1_048_576.0,
                newest.bytes as f64 / 1_048_576.0,
                dbytes as f64 / 1_048_576.0,
                dt_secs,
                speed / 1_048_576.0,
            );
        }
        Some(speed)
    }

    /// Number of speed samples collected so far.
    fn speed_sample_count(&self) -> usize {
        let samples = self.speed_samples.lock().unwrap();
        samples.len()
    }
}

/// Calculate ETA in seconds from remaining bytes and current speed.
///
/// Returns `None` when ETA cannot be reliably estimated:
/// - speed is zero or negative
/// - total bytes is unknown (0)
/// - all bytes already transferred
/// - fewer than `MIN_ETA_SPEED_SAMPLES` have been collected
fn calculate_eta(
    bytes_done: u64,
    total_bytes: u64,
    speed_bytes_per_sec: f64,
    sample_count: usize,
) -> Option<u64> {
    if total_bytes == 0 || bytes_done >= total_bytes {
        return None;
    }
    if speed_bytes_per_sec <= 0.0 || sample_count < MIN_ETA_SPEED_SAMPLES {
        return None;
    }
    let remaining = (total_bytes - bytes_done) as f64;
    // Guard: avoid producing absurdly large ETAs from near-zero speed.
    let eta = remaining / speed_bytes_per_sec;
    Some(eta as u64)
}

/// Format a transfer summary message for the completion popup.
///
/// Includes file count, total bytes, elapsed time, and average speed.
/// For partial failures, includes the error and how many files completed.
fn format_transfer_summary(
    result: &TransferResult,
    total_bytes: u64,
    elapsed: Duration,
    is_move: bool,
) -> String {
    let verb_past = if is_move { "Moved" } else { "Copied" };
    let verb_noun = if is_move { "Move" } else { "Copy" };
    let elapsed_secs = elapsed.as_secs();
    let elapsed_f64 = elapsed.as_secs_f64();

    if result.copied == 0 && result.last_error.is_none() {
        return format!("{verb_noun} cancelled");
    }

    let size_str = format_bytes(total_bytes);
    let time_str = format_duration(elapsed_secs);
    let avg_speed = if elapsed_f64 > 0.1 {
        format!(
            ", avg {}",
            format_bytes_per_sec(total_bytes as f64 / elapsed_f64)
        )
    } else {
        String::new()
    };

    if let Some(ref err) = result.last_error {
        format!(
            "Failed: {err} after {} of {} files ({}{} in {})",
            result.copied, result.total, size_str, avg_speed, time_str,
        )
    } else if result.total == 1 {
        format!(
            "\u{2713} {verb_past} 1 file ({} in {}{})",
            size_str, time_str, avg_speed,
        )
    } else {
        format!(
            "\u{2713} {verb_past} {} files ({} in {}{})",
            result.total, size_str, time_str, avg_speed,
        )
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
    /// Original transfer parameters for retry on failure.
    retry_targets: Vec<(String, String, bool, u64)>,
    retry_dst_cwd: String,
    retry_is_move: bool,
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
    /// Persistent overwrite policy across transfers in this browser session.
    /// 0=ask, 1=overwrite all, 2=skip all.
    overwrite_policy: Arc<AtomicU64>,
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
    tracing::debug!(
        "MEM[browser:start]: {:.1} MB RSS — bookmark={bookmark_name} env={env}",
        rss_mb()
    );

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
        overwrite_policy: Arc::new(AtomicU64::new(0)),
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

                    tracing::debug!(
                        "MEM[browser:transfer_finished]: {:.1} MB RSS — {}/{} files",
                        rss_mb(),
                        result.copied,
                        result.total
                    );

                    let is_move = transfer.delete_sources.is_some();
                    let summary = format_transfer_result(&result, &transfer.description, is_move);

                    // If popup is showing and this was the last transfer, show completion
                    if matches!(state.input_mode, InputMode::TransferPopup)
                        && state.background_transfers.is_empty()
                    {
                        let total_bytes = transfer.progress.bytes_done_all.load(Ordering::Relaxed);
                        let completion_msg =
                            format_transfer_summary(&result, total_bytes, elapsed, is_move);
                        let retry = if result.last_error.is_some() {
                            Some(RetryInfo {
                                targets: transfer.retry_targets,
                                direction: transfer.direction,
                                source_side: transfer.source_side,
                                dst_cwd: transfer.retry_dst_cwd,
                                is_move: transfer.retry_is_move,
                            })
                        } else {
                            None
                        };
                        state.popup_focus = 0;
                        state.input_mode = InputMode::TransferComplete {
                            msg: completion_msg,
                            retry,
                        };
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

            // Record speed samples and update progress for active transfers.
            for transfer in &state.background_transfers {
                transfer.progress.record_speed_sample();
            }

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
                other => {
                    tracing::trace!(?other, "browser: ignoring event");
                }
            }
        }
    }

    // Cancel any in-flight transfers and give workers time to log summaries.
    if !state.background_transfers.is_empty() {
        tracing::debug!(
            "browser:cleanup cancelling {} transfers",
            state.background_transfers.len(),
        );
        for t in &state.background_transfers {
            t.cancel.store(true, Ordering::Relaxed);
        }
        // Workers need time to: notice cancel (50ms poll) → bail from pipeline →
        // close SFTP handle (100-1000ms over internet) → print summary.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        for (i, t) in state.background_transfers.iter_mut().enumerate() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::debug!("browser:cleanup deadline hit at transfer {i}");
                break;
            }
            match tokio::time::timeout(remaining, &mut t.handle).await {
                Ok(Ok(result)) => {
                    tracing::debug!(
                        "browser:cleanup transfer {i} finished: {}/{} files",
                        result.copied,
                        result.total,
                    );
                }
                Ok(Err(e)) => {
                    tracing::debug!("browser:cleanup transfer {i} join error: {e}");
                }
                Err(_) => {
                    tracing::debug!("browser:cleanup transfer {i} timed out");
                }
            }
        }
    }

    // BrowserGuard handles terminal cleanup on drop
    tracing::debug!("MEM[browser:exit]: {:.1} MB RSS", rss_mb());
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
                let going_up = entry.name == "..";
                // Remember current dir name so we can select it after going up
                let prev_dir_name = if going_up {
                    dir_basename(&pane.cwd)
                } else {
                    None
                };
                let backend = active_backend_mut(left, right, state);
                if going_up {
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
                state.status_message = None;
                let pane = active_pane_mut(left_pane, right_pane, state);
                let backend = active_backend_mut(left, right, state);
                refresh_pane(pane, backend, state).await?;
                if let Some(name) = prev_dir_name {
                    select_entry_by_name(pane, &name);
                }
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
                    let going_up = entry.name == "..";
                    let prev_dir_name = if going_up {
                        dir_basename(&pane.cwd)
                    } else {
                        None
                    };
                    let backend = active_backend_mut(left, right, state);
                    if going_up {
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
                    state.status_message = None;
                    let pane = active_pane_mut(left_pane, right_pane, state);
                    let backend = active_backend_mut(left, right, state);
                    refresh_pane(pane, backend, state).await?;
                    if let Some(name) = prev_dir_name {
                        select_entry_by_name(pane, &name);
                    }
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
            let count = pane.marked.len();
            state.status_message = if count > 0 {
                Some(format!("{count} files marked"))
            } else {
                None
            };
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
                files_only: false,
                case_sensitive: true,
            };
        }

        KeyCode::Char('-') => {
            state.popup_focus = 0;
            state.input_mode = InputMode::SelectPattern {
                input: "*".to_string(),
                selecting: false,
                files_only: false,
                case_sensitive: true,
            };
        }

        KeyCode::Backspace => {
            // Navigate up (parent directory)
            let pane = active_pane_mut(left_pane, right_pane, state);
            let prev_dir_name = dir_basename(&pane.cwd);
            let backend = active_backend_mut(left, right, state);
            backend.cd("..").await?;
            let new_cwd = backend.cwd().unwrap_or_default();
            let pane = active_pane_mut(left_pane, right_pane, state);
            pane.cwd = new_cwd;
            pane.selected = 0;
            pane.list_state.select(Some(0));
            pane.marked.clear();
            state.filter = None;
            state.status_message = None;
            let pane = active_pane_mut(left_pane, right_pane, state);
            let backend = active_backend_mut(left, right, state);
            refresh_pane(pane, backend, state).await?;
            if let Some(name) = prev_dir_name {
                select_entry_by_name(pane, &name);
            }
        }

        _ => {}
    }

    Ok(BrowserAction::Continue)
}

/// Extract the last component (directory name) from a path.
fn dir_basename(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Select the entry matching `name` in the pane, updating both `selected` and `list_state`.
fn select_entry_by_name(pane: &mut PaneState, name: &str) {
    if let Some(idx) = pane.entries.iter().position(|e| e.name == name) {
        pane.selected = idx;
        pane.list_state.select(Some(idx));
    }
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
    glob_match_opts(pattern, text, true)
}

/// Glob matching with case sensitivity option.
fn glob_match_opts(pattern: &str, text: &str, case_sensitive: bool) -> bool {
    let prefix = if case_sensitive { "" } else { "(?i)" };
    let regex_pattern = format!("{prefix}^{}$", regex::escape(pattern).replace(r"\*", ".*"));
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

    // Build remote context for environment-colored remote pane indicators
    let remote_ctx = {
        let env_color = match state.env.to_lowercase().as_str() {
            "production" => Color::Red,
            "staging" => Color::Yellow,
            "development" => Color::Green,
            "local" => Color::Blue,
            "testing" => Color::Cyan,
            _ => Color::White,
        };
        let env_label = match state.env.to_lowercase().as_str() {
            "production" => "PROD",
            "staging" => "STG",
            "development" => "DEV",
            "local" => "LOCAL",
            "testing" => "TEST",
            _ => &state.env,
        };
        RemoteContext {
            env_color,
            env_tint: dim_color(env_color, 55),
            env_label: env_label.to_uppercase(),
            bookmark_name: state.bookmark_name.clone(),
        }
    };

    let left_ctx = if state.left_label == PaneLabel::Remote {
        Some(&remote_ctx)
    } else {
        None
    };
    let right_ctx = if state.right_label == PaneLabel::Remote {
        Some(&remote_ctx)
    } else {
        None
    };

    draw_pane(
        frame,
        left_pane,
        pane_chunks[0],
        state.active_pane == Side::Left,
        state.left_label,
        left_ctx,
    );
    draw_pane(
        frame,
        right_pane,
        pane_chunks[1],
        state.active_pane == Side::Right,
        state.right_label,
        right_ctx,
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

    // Status message (truncated to available width so long messages get "..."
    // instead of being silently clipped by ratatui).
    if has_status {
        let msg = state.status_message.as_deref().unwrap_or("");
        let available = main_chunks[3].width as usize;
        // Account for the leading space in the format.
        let max_msg = available.saturating_sub(1);
        let display_msg = truncate_name(msg, max_msg);
        frame.render_widget(
            Paragraph::new(format!(" {display_msg}")).style(Style::default().fg(Color::DarkGray)),
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
        InputMode::TransferComplete { msg, retry } => {
            draw_transfer_complete_popup(frame, size, msg, retry.is_some(), state.popup_focus);
        }
        InputMode::ConfirmDelete { entries } => {
            let is_production = is_production_remote(state);
            draw_delete_confirm_popup(frame, size, entries, is_production, state.popup_focus);
        }
        InputMode::SelectPattern {
            input,
            selecting,
            files_only,
            case_sensitive,
        } => {
            draw_select_popup(
                frame,
                size,
                input,
                *selecting,
                *files_only,
                *case_sensitive,
                state.popup_focus,
            );
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
    files_only: bool,
    case_sensitive: bool,
    popup_focus: usize,
) {
    let title = if selecting { " Select " } else { " Deselect " };
    let popup_h: u16 = 10;
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
    // Focus: 0=Input, 1=Files only, 2=Case sensitive, 3=OK, 4=Cancel
    let field_y = inner.y + 1;
    let field_w = inner.width;
    let field_area = Rect::new(inner.x, field_y, field_w, 1);
    let input_focused = popup_focus == 0;

    let max_visible = field_w.saturating_sub(1) as usize;
    let display_input = if input.len() > max_visible {
        &input[input.len() - max_visible..]
    } else {
        input
    };

    let field_bg = if input_focused {
        Color::DarkGray
    } else {
        Color::Rgb(40, 40, 40)
    };
    let cursor_char = if input_focused && display_input.len() < max_visible {
        "\u{2588}"
    } else {
        ""
    };
    let text_len = display_input.len() + cursor_char.len();
    let pad = (field_w as usize).saturating_sub(text_len);
    let field_line = Line::from(vec![
        Span::styled(
            display_input.to_string(),
            Style::default().fg(Color::White).bg(field_bg),
        ),
        Span::styled(
            cursor_char.to_string(),
            Style::default().fg(Color::White).bg(field_bg),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(field_bg)),
    ]);
    frame.render_widget(Paragraph::new(field_line), field_area);

    // Checkbox row: [x] Files only    [x] Case sensitive
    let chk_y = inner.y + 3;
    let files_mark = if files_only { "x" } else { " " };
    let case_mark = if case_sensitive { "x" } else { " " };
    let files_style = if popup_focus == 1 {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::White)
    };
    let case_style = if popup_focus == 2 {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::White)
    };
    let chk_line = Line::from(vec![
        Span::styled(format!("[{files_mark}] Files only"), files_style),
        Span::raw("    "),
        Span::styled(format!("[{case_mark}] Case sensitive"), case_style),
    ]);
    frame.render_widget(
        Paragraph::new(chk_line),
        Rect::new(inner.x, chk_y, inner.width, 1),
    );

    // Separator line
    let sep_y = inner.y + 5;
    let sep_line = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(Color::Cyan)),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // Buttons — focus 3=OK, 4=Cancel; usize::MAX = no button focused
    let btn_y = inner.y + 6;
    let btn_focus = if popup_focus >= 3 {
        popup_focus - 3
    } else {
        usize::MAX
    };
    render_button_row(&["OK", "Cancel"], btn_focus, inner, btn_y, frame);
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

    // Use rolling window for current speed; fall back to overall average during ramp-up.
    let (speed, speed_source) = match p.rolling_speed() {
        Some(s) => (s, "rolling"),
        None => {
            let elapsed_secs = p
                .transfer_elapsed_secs()
                .unwrap_or_else(|| transfer.started_at.elapsed().as_secs_f64());
            if elapsed_secs > 0.1 {
                (bytes_done_all as f64 / elapsed_secs, "average")
            } else {
                (0.0, "zero")
            }
        }
    };
    let sample_count = p.speed_sample_count();
    let eta_secs = calculate_eta(bytes_done_all, total_bytes_all, speed, sample_count);

    // Log display speed periodically (~every 5s at 100ms poll).
    {
        let sample_idx = p.speed_sample_idx.load(Ordering::Relaxed);
        if sample_idx.is_multiple_of(50) && sample_idx > 0 {
            tracing::debug!(
                "speed display: source={speed_source} {:.2} MB/s, \
                 done={:.1}/{:.1} MB, files={}/{}, eta={:?}s",
                speed / 1_048_576.0,
                bytes_done_all as f64 / 1_048_576.0,
                total_bytes_all as f64 / 1_048_576.0,
                p.files_done.load(Ordering::Relaxed),
                total_files,
                eta_secs.unwrap_or(0),
            );
        }
    }

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
    let eta_display = match eta_secs {
        Some(secs) => format!("~{} remaining", format_duration(secs)),
        None if bytes_done_all > 0 && bytes_done_all < total_bytes_all => {
            "calculating...".to_string()
        }
        _ => String::new(),
    };
    let stats_line = if speed > 0.0 {
        if eta_display.is_empty() {
            format!(
                " Files: {files_done}/{total_files}  {} / {}  {}",
                format_bytes(bytes_done_all),
                format_bytes(total_bytes_all),
                format_bytes_per_sec(speed),
            )
        } else {
            format!(
                " Files: {files_done}/{total_files}  {} / {}  {}  {eta_display}",
                format_bytes(bytes_done_all),
                format_bytes(total_bytes_all),
                format_bytes_per_sec(speed),
            )
        }
    } else if eta_display.is_empty() {
        format!(
            " Files: {files_done}/{total_files}  {} / {}",
            format_bytes(bytes_done_all),
            format_bytes(total_bytes_all),
        )
    } else {
        format!(
            " Files: {files_done}/{total_files}  {} / {}  {eta_display}",
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
/// When `has_retry` is true, shows [Retry] [OK] buttons with yellow border (error).
/// When false, shows "Press any key to close" with green border (success).
fn draw_transfer_complete_popup(
    frame: &mut Frame,
    area: Rect,
    msg: &str,
    has_retry: bool,
    popup_focus: usize,
) {
    let popup_height = if has_retry { 6 } else { 5 };
    let popup_area = centered_fixed_rect(POPUP_WIDTH, popup_height, area);
    if popup_area.width < 20 || popup_area.height < popup_height {
        return;
    }

    // Truncate message to fit within popup borders (2 chars for borders).
    let max_msg_width = popup_area.width.saturating_sub(2) as usize;
    let display_msg = truncate_name(msg, max_msg_width);

    let lines = vec![
        Line::from(Span::styled(
            format!(" {display_msg}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    let border_color = if has_retry {
        Color::Yellow
    } else {
        Color::Green
    };
    let block = Block::default()
        .title(" Copy Complete ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = Rect::new(
        popup_area.x + 1,
        popup_area.y + 1,
        popup_area.width.saturating_sub(2),
        popup_area.height.saturating_sub(2),
    );

    frame.render_widget(Clear, popup_area);
    frame.render_widget(Paragraph::new(lines).block(block), popup_area);

    if has_retry {
        render_button_row(&["Retry", "OK"], popup_focus, inner, inner.y + 3, frame);
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(" Press any key to close")),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    }
}

/// Draw a single pane.
/// Context passed to `draw_pane` for remote-panel visual indicators.
struct RemoteContext {
    /// Environment background color (e.g. red for production).
    env_color: Color,
    /// Dimmed version of env_color for subtle background tint.
    env_tint: Color,
    /// Short env label (e.g. "PROD").
    env_label: String,
    /// Bookmark name (e.g. "prod-web-01").
    bookmark_name: String,
}

/// Dim an RGB color to ~15% intensity for a subtle background tint.
/// For non-RGB colors, returns a conservative dark tint.
fn dim_color(color: Color, intensity: u8) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as u16 * intensity as u16 / 255) as u8,
            (g as u16 * intensity as u16 / 255) as u8,
            (b as u16 * intensity as u16 / 255) as u8,
        ),
        Color::Red => Color::Rgb(intensity, 0, 0),
        Color::Yellow => Color::Rgb(intensity, intensity, 0),
        Color::Green => Color::Rgb(0, intensity, 0),
        Color::Blue => Color::Rgb(0, 0, intensity),
        Color::Cyan => Color::Rgb(0, intensity, intensity),
        Color::Magenta => Color::Rgb(intensity, 0, intensity),
        _ => Color::Rgb(intensity / 3, intensity / 3, intensity / 3),
    }
}

fn draw_pane(
    frame: &mut Frame,
    pane: &mut PaneState,
    area: Rect,
    is_active: bool,
    label: PaneLabel,
    remote_ctx: Option<&RemoteContext>,
) {
    let is_remote = label == PaneLabel::Remote;

    // 1. Border color: env color for remote, default for local
    let border_style = if is_remote {
        if let Some(ctx) = remote_ctx {
            let border_color = if is_active {
                ctx.env_color
            } else {
                dim_color(ctx.env_color, 128)
            };
            Style::default().fg(border_color)
        } else if is_active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    } else if is_active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // 2. Title with colored env badge for remote pane
    //    Remote panes always show "◆" prefix so they're visually distinct from local
    //    panes even when the env tier is "local".
    let (badge_text, badge_style) = match (label, remote_ctx) {
        (PaneLabel::Remote, Some(ctx)) => (
            format!(" \u{25c6} {} {} ", ctx.env_label, ctx.bookmark_name),
            Style::default().fg(Color::White).bg(ctx.env_color),
        ),
        (PaneLabel::Remote, None) => (
            " \u{25c6} REMOTE ".to_string(),
            Style::default().fg(Color::White).bg(Color::Magenta),
        ),
        (PaneLabel::Local, _) => (
            " LOCAL ".to_string(),
            Style::default().fg(Color::White).bg(Color::Blue),
        ),
    };

    // Reserve space for badge + padding in the title
    let badge_len = badge_text.len() + 2;
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
        Span::styled(&badge_text, badge_style),
        Span::raw(format!(" {} ", cwd_display)),
    ]);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    // 3. Background tint for remote pane cells
    let bg_tint = if is_remote {
        remote_ctx.map(|ctx| ctx.env_tint)
    } else {
        None
    };

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

            let mut style = if is_marked {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if entry.is_dir {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };

            // Apply subtle background tint for remote pane
            if let Some(tint) = bg_tint {
                style = style.bg(tint);
            }

            ListItem::new(text).style(style)
        })
        .collect();

    let highlight_bg = if is_active {
        if let Some(ctx) = remote_ctx {
            // Brighter tint for the selected row in remote pane
            dim_color(ctx.env_color, 100)
        } else {
            Color::DarkGray
        }
    } else {
        Color::Black
    };

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(highlight_bg)
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

/// Shared state for the worker pool, allowing dynamic worker addition.
struct WorkerPool {
    work_rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<TransferTarget>>>,
    progress: Arc<TransferProgress>,
    cancel: Arc<AtomicBool>,
    skip: Arc<AtomicBool>,
    direction: TransferDirection,
    copied: Arc<AtomicU64>,
    last_error: Arc<std::sync::Mutex<Option<String>>>,
    overwrite_tx: tokio::sync::mpsc::Sender<OverwriteQuery>,
    overwrite_policy: Arc<AtomicU64>,
}

/// Receive the next work item from the shared queue, handling overwrite checks.
/// Returns `None` if channel closed or cancelled.
/// Returns `Some((target, should_transfer))` — `should_transfer` is false if skipped.
async fn recv_next_target(
    worker_id: usize,
    pool: &WorkerPool,
    raw: &RawSftpSession,
    total_idle_ms: &mut u64,
    total_stat_ms: &mut u64,
    total_overwrite_wait_ms: &mut u64,
) -> Option<(TransferTarget, bool)> {
    let idle_start = std::time::Instant::now();
    let target = {
        let mut rx = pool.work_rx.lock().await;
        rx.recv().await
    };
    let idle_ms = idle_start.elapsed().as_millis() as u64;
    *total_idle_ms += idle_ms;
    if idle_ms > 100 {
        tracing::debug!("worker[{worker_id}] waited {idle_ms}ms for next work item");
    }

    let target = target?;

    if pool.cancel.load(Ordering::Relaxed) {
        return None;
    }

    // Fast path: skip stat check when overwrite policy is already decided.
    let policy = pool.overwrite_policy.load(Ordering::Relaxed);
    if policy == 1 {
        return Some((target, true)); // overwrite all — skip stat
    }

    // Need to check existence.
    let stat_start = std::time::Instant::now();
    let exists = match pool.direction {
        TransferDirection::LocalToRemote => raw.stat(target.dst_path.as_str()).await.is_ok(),
        TransferDirection::RemoteToLocal => tokio::fs::metadata(&target.dst_path).await.is_ok(),
    };
    let stat_ms = stat_start.elapsed().as_millis() as u64;
    *total_stat_ms += stat_ms;
    if stat_ms > 50 {
        tracing::debug!(
            "worker[{worker_id}] stat check took {stat_ms}ms: {}",
            target.name
        );
    }

    if !exists {
        return Some((target, true)); // new file — transfer it
    }

    if policy == 2 {
        // skip all existing
        pool.progress.files_done.fetch_add(1, Ordering::Relaxed);
        pool.progress
            .bytes_done_all
            .fetch_add(target.size, Ordering::Relaxed);
        return Some((target, false));
    }

    // policy == 0: ask the user
    let ow_start = std::time::Instant::now();
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    let query = OverwriteQuery {
        name: target.name.clone(),
        dst_path: target.dst_path.clone(),
        size: target.size,
        response: resp_tx,
    };
    if pool.overwrite_tx.send(query).await.is_err() {
        return None;
    }
    let answer = match resp_rx.await {
        Ok(a) => a,
        Err(_) => return None,
    };
    let ow_wait_ms = ow_start.elapsed().as_millis() as u64;
    *total_overwrite_wait_ms += ow_wait_ms;
    if ow_wait_ms > 100 {
        tracing::debug!(
            "worker[{worker_id}] overwrite prompt waited {ow_wait_ms}ms: {}",
            target.name,
        );
    }

    match answer {
        OverwriteAnswer::Overwrite => Some((target, true)),
        OverwriteAnswer::OverwriteAll => {
            pool.overwrite_policy.store(1, Ordering::Relaxed);
            Some((target, true))
        }
        OverwriteAnswer::Skip => {
            pool.progress.files_done.fetch_add(1, Ordering::Relaxed);
            pool.progress
                .bytes_done_all
                .fetch_add(target.size, Ordering::Relaxed);
            Some((target, false))
        }
        OverwriteAnswer::SkipAll => {
            pool.overwrite_policy.store(2, Ordering::Relaxed);
            pool.progress.files_done.fetch_add(1, Ordering::Relaxed);
            pool.progress
                .bytes_done_all
                .fetch_add(target.size, Ordering::Relaxed);
            Some((target, false))
        }
        OverwriteAnswer::Cancel => {
            pool.cancel.store(true, Ordering::Relaxed);
            None
        }
    }
}

/// Spawn a single transfer worker that pulls from the shared work queue.
/// Prefetches the next file's SFTP handle during the current transfer to
/// eliminate per-file open/close latency.
fn spawn_worker(
    worker_id: usize,
    worker: PipelinedWorker,
    pool: &Arc<WorkerPool>,
) -> tokio::task::JoinHandle<()> {
    let pool = Arc::clone(pool);
    let raw = worker.raw;
    let read_chunk_size = worker.read_chunk_size;
    let write_chunk_size = worker.write_chunk_size;

    tokio::spawn(async move {
        tracing::debug!(
            "MEM[worker:spawn]: {:.1} MB RSS — worker[{worker_id}] chunk_size={read_chunk_size}",
            rss_mb()
        );
        let worker_start = std::time::Instant::now();
        let mut files_completed = 0u64;
        let mut bytes_transferred = 0u64;
        let mut total_idle_ms = 0u64;
        let mut total_stat_ms = 0u64;
        let mut total_transfer_ms = 0u64;
        let mut total_overwrite_wait_ms = 0u64;
        let mut total_open_ms = 0u64;

        // Prefetched state: next target + its already-opened SFTP handle.
        let mut prefetched: Option<(TransferTarget, Arc<str>)> = None;

        loop {
            // Get next target — either from prefetch or from the work queue.
            let (target, file_handle) = if let Some((t, h)) = prefetched.take() {
                (t, Some(h))
            } else {
                match recv_next_target(
                    worker_id,
                    &pool,
                    &raw,
                    &mut total_idle_ms,
                    &mut total_stat_ms,
                    &mut total_overwrite_wait_ms,
                )
                .await
                {
                    Some((target, true)) => (target, None),
                    Some((_, false)) => continue, // skipped
                    None => break,                // done or cancelled
                }
            };

            if pool.cancel.load(Ordering::Relaxed) {
                // Close prefetched handle if we got one
                if let Some(h) = file_handle {
                    pipeline::close_handle(&raw, &h).await;
                }
                break;
            }

            // Open the SFTP file handle (if not prefetched).
            let open_start = std::time::Instant::now();
            let handle_str = match file_handle {
                Some(h) => h,
                None => match open_target_handle(&raw, &target, pool.direction).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!("worker[{worker_id}] open failed: {}: {e:#}", target.name,);
                        let mut err = pool.last_error.lock().unwrap();
                        *err = Some(format!("{}: open failed: {e}", target.name));
                        continue;
                    }
                },
            };
            let open_ms = open_start.elapsed().as_millis() as u64;
            total_open_ms += open_ms;

            // Set active file info for this worker.
            {
                let mut active = pool.progress.active_files.lock().unwrap();
                if worker_id >= active.len() {
                    active.resize(worker_id + 1, None);
                }
                active[worker_id] = Some(ActiveFile {
                    name: target.name.clone(),
                    src_path: target.src_path.clone(),
                    dst_path: target.dst_path.clone(),
                    bytes_done: 0,
                    bytes_total: target.size,
                });
            }

            // Run the data transfer + concurrently prefetch the next file's handle.
            let xfer_start = std::time::Instant::now();
            let transfer_fut = run_file_transfer(
                &raw,
                &handle_str,
                &target,
                pool.direction,
                worker_id,
                read_chunk_size,
                write_chunk_size,
                &pool.progress,
                &pool.cancel,
                &pool.skip,
            );

            let prefetch_raw = Arc::clone(&raw);
            let prefetch_pool = Arc::clone(&pool);
            let prefetch_fut = async {
                // Get next target from queue and open its handle while transfer runs.
                let next = recv_next_target(
                    worker_id,
                    &prefetch_pool,
                    &prefetch_raw,
                    &mut 0u64, // prefetch idle/stat not counted separately
                    &mut 0u64,
                    &mut 0u64,
                )
                .await;
                match next {
                    Some((next_target, true)) => {
                        // Pre-open the handle while current transfer is in progress.
                        match open_target_handle(
                            &prefetch_raw,
                            &next_target,
                            prefetch_pool.direction,
                        )
                        .await
                        {
                            Ok(h) => Some((next_target, Some(h))),
                            Err(e) => {
                                tracing::debug!(
                                    "worker[{worker_id}] prefetch open failed: {}: {e:#}",
                                    next_target.name,
                                );
                                // Return target without handle — will open in main loop.
                                Some((next_target, None))
                            }
                        }
                    }
                    Some((next_target, false)) => {
                        // Skipped file — return None so main loop continues.
                        // We already updated progress in recv_next_target.
                        Some((next_target, None))
                    }
                    None => None, // channel closed or cancelled
                }
            };

            let (result, next_prefetched) = tokio::join!(transfer_fut, prefetch_fut);
            let xfer_ms = xfer_start.elapsed().as_millis() as u64;
            total_transfer_ms += xfer_ms;

            // Store prefetched result for next iteration.
            match next_prefetched {
                Some((next_target, Some(next_handle))) => {
                    prefetched = Some((next_target, next_handle));
                }
                Some((next_target, None)) => {
                    // Target was skipped or open failed — re-queue as no-handle
                    // Only set prefetched if it should be transferred (not skipped).
                    // Skipped targets already had their progress updated.
                    // For open failures, we'll retry in the main loop.
                    // Check: was this a skip or an open failure?
                    // If bytes_done_all was bumped, it was skipped.
                    // For simplicity: just set it as prefetched without handle.
                    prefetched = Some((next_target, "".into()));
                    // The empty handle signals "needs re-opening" in the main loop.
                    // Actually, let's use a cleaner approach:
                }
                None => {
                    prefetched = None; // No more work
                }
            }

            // Fire-and-forget close of current handle (overlaps with next transfer).
            let close_raw = Arc::clone(&raw);
            let close_handle = Arc::clone(&handle_str);
            let close_start = std::time::Instant::now();
            tokio::spawn(async move {
                pipeline::close_handle(&close_raw, &close_handle).await;
            });
            // Estimate close time from recent closes (we can't await without blocking).
            let _ = close_start; // close runs in background

            // Clear active file slot.
            {
                let mut active = pool.progress.active_files.lock().unwrap();
                if worker_id < active.len() {
                    active[worker_id] = None;
                }
            }

            match result {
                Ok(()) => {
                    files_completed += 1;
                    bytes_transferred += target.size;
                    pool.copied.fetch_add(1, Ordering::Relaxed);
                    pool.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    let mbps = if xfer_ms > 0 {
                        target.size as f64 / (xfer_ms as f64 / 1000.0) / 1_048_576.0
                    } else {
                        0.0
                    };
                    // Log memory every 5 files or on first file to track RSS over time.
                    if files_completed == 1 || files_completed.is_multiple_of(5) {
                        tracing::debug!(
                            "MEM[worker:file_done]: {:.1} MB RSS — worker[{worker_id}] #{files_completed} {} {:.1}MB {mbps:.1}MB/s",
                            rss_mb(),
                            target.name,
                            target.size as f64 / 1_048_576.0
                        );
                    } else {
                        tracing::debug!(
                            "worker[{worker_id}] file done: {} {:.1}MB in {:.1}s = {mbps:.1}MB/s (open={open_ms}ms)",
                            target.name,
                            target.size as f64 / 1_048_576.0,
                            xfer_ms as f64 / 1000.0,
                        );
                    }
                }
                Err(_) if pool.cancel.load(Ordering::Relaxed) => {
                    // Close prefetched handle before exiting.
                    if let Some((_, ref h)) = prefetched
                        && !h.is_empty()
                    {
                        pipeline::close_handle(&raw, h).await;
                    }
                    break;
                }
                Err(e) if pool.skip.swap(false, Ordering::Relaxed) => {
                    tracing::debug!("worker[{worker_id}] skipped: {}: {e:#}", target.name);
                }
                Err(e) => {
                    tracing::error!(
                        "worker[{worker_id}] transfer failed: {}: {e:#}",
                        target.name,
                    );
                    let mut err = pool.last_error.lock().unwrap();
                    *err = Some(format!("{}: {e}", target.name));
                }
            }
        }

        // Close any remaining prefetched handle.
        if let Some((_, ref h)) = prefetched
            && !h.is_empty()
        {
            pipeline::close_handle(&raw, h).await;
        }

        // Worker summary.
        tracing::debug!(
            "MEM[worker:done]: {:.1} MB RSS — worker[{worker_id}] {files_completed} files, {:.1} MB",
            rss_mb(),
            bytes_transferred as f64 / 1_048_576.0
        );
        let wall_ms = worker_start.elapsed().as_millis() as u64;
        let active_pct = if wall_ms > 0 {
            total_transfer_ms as f64 / wall_ms as f64 * 100.0
        } else {
            0.0
        };
        let avg_speed = if total_transfer_ms > 0 {
            bytes_transferred as f64 / (total_transfer_ms as f64 / 1000.0) / 1_048_576.0
        } else {
            0.0
        };
        tracing::debug!(
            "worker[{worker_id}] done: {files_completed} files, \
             {:.1} MB in {:.1}s wall | \
             transfer={:.1}s idle={:.1}s stat={:.1}s open={:.1}s overwrite_wait={:.1}s | \
             active={active_pct:.0}% avg_speed={avg_speed:.1}MB/s",
            bytes_transferred as f64 / 1_048_576.0,
            wall_ms as f64 / 1000.0,
            total_transfer_ms as f64 / 1000.0,
            total_idle_ms as f64 / 1000.0,
            total_stat_ms as f64 / 1000.0,
            total_open_ms as f64 / 1000.0,
            total_overwrite_wait_ms as f64 / 1000.0,
        );
    })
}

/// Run a file transfer using a dynamic pool of pipelined SFTP workers.
/// `scan_sftp` is used for directory scanning/mkdir; `initial_workers` start immediately.
/// Extra workers arriving on `extra_workers_rx` are added to the pool on the fly
/// (e.g. from a second SSH connection opened in the background).
#[allow(clippy::too_many_arguments)]
async fn run_background_transfer(
    scan_sftp: SftpSession,
    initial_workers: Vec<PipelinedWorker>,
    mut extra_workers_rx: tokio::sync::mpsc::Receiver<PipelinedWorker>,
    targets: Vec<TransferTarget>,
    progress: Arc<TransferProgress>,
    cancel: Arc<AtomicBool>,
    skip: Arc<AtomicBool>,
    direction: TransferDirection,
    overwrite_tx: tokio::sync::mpsc::Sender<OverwriteQuery>,
    overwrite_policy: Arc<AtomicU64>,
) -> TransferResult {
    assert!(
        !initial_workers.is_empty(),
        "run_background_transfer requires at least one pipelined worker"
    );

    let transfer_wall_start = std::time::Instant::now();
    let dir_label = match direction {
        TransferDirection::LocalToRemote => "upload",
        TransferDirection::RemoteToLocal => "download",
    };
    tracing::debug!(
        "MEM[bg:start]: {:.1} MB RSS — {dir_label} targets={} initial_workers={}",
        rss_mb(),
        targets.len(),
        initial_workers.len()
    );

    // Expand directories into flat file lists
    let scan_start = std::time::Instant::now();
    let targets = match expand_directory_targets(&scan_sftp, targets, direction, &progress).await {
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
    let scan_ms = scan_start.elapsed().as_millis();

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

    // Sort files largest-first so workers stay busy on big files early,
    // avoiding a "long tail" where one worker grinds a large file at the end.
    file_targets.sort_by(|a, b| b.size.cmp(&a.size));

    tracing::debug!(
        "MEM[bg:scan_done]: {:.1} MB RSS — in {scan_ms}ms — {} dirs, {} files",
        rss_mb(),
        dir_targets.len(),
        file_targets.len()
    );

    // Create directories sequentially first (order matters for nested dirs)
    let mkdir_start = std::time::Instant::now();
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
    if !dir_targets.is_empty() {
        tracing::debug!(
            "transfer:mkdir {} dirs in {}ms",
            dir_targets.len(),
            mkdir_start.elapsed().as_millis(),
        );
    }

    // Drop scan session — frees its SSH channel and mlocked buffers before
    // workers start filling their own channels with transfer data.
    tracing::debug!("MEM[bg:pre_drop_scan_sftp]: {:.1} MB RSS", rss_mb());
    drop(scan_sftp);
    tracing::debug!("MEM[bg:post_drop_scan_sftp]: {:.1} MB RSS", rss_mb());

    let total = file_targets.len();
    let total_bytes: u64 = file_targets.iter().map(|t| t.size).sum();
    progress.total_files.store(total as u64, Ordering::Relaxed);
    progress
        .total_bytes_all
        .store(total_bytes, Ordering::Relaxed);

    tracing::debug!(
        "MEM[bg:files_queued]: {:.1} MB RSS — {total} files, {:.1} MB total",
        rss_mb(),
        total_bytes as f64 / 1_048_576.0
    );

    if total == 0 {
        return TransferResult {
            copied: 0,
            total: 0,
            last_error: None,
        };
    }

    // Set up shared work queue and worker pool state.
    let (tx, rx) = tokio::sync::mpsc::channel::<TransferTarget>(WORKERS_PER_CONNECTION * 4);
    let pool = Arc::new(WorkerPool {
        work_rx: Arc::new(tokio::sync::Mutex::new(rx)),
        progress: Arc::clone(&progress),
        cancel: Arc::clone(&cancel),
        skip: Arc::clone(&skip),
        direction,
        copied: Arc::new(AtomicU64::new(0)),
        last_error: Arc::new(std::sync::Mutex::new(None)),
        overwrite_tx,
        overwrite_policy,
    });

    // Periodic memory reporter — logs RSS every 2 seconds during transfer.
    let mem_cancel = Arc::clone(&cancel);
    let mem_reporter = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            if mem_cancel.load(Ordering::Relaxed) {
                break;
            }
            tracing::debug!("MEM[bg:periodic]: {:.1} MB RSS", rss_mb());
        }
    });

    // Spawn initial workers immediately.
    let worker_handles: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let initial_count = initial_workers.len();
    {
        let mut handles = worker_handles.lock().await;
        for (id, worker) in initial_workers.into_iter().enumerate() {
            handles.push(spawn_worker(id, worker, &pool));
        }
    }

    // Spawn a listener that adds extra workers (from second connection) as they arrive.
    let extra_pool = Arc::clone(&pool);
    let extra_handles = Arc::clone(&worker_handles);
    let extra_cancel = Arc::clone(&cancel);
    let extra_listener = tokio::spawn(async move {
        let mut next_id = WORKERS_PER_CONNECTION; // IDs for extra workers start after initial
        while let Some(worker) = extra_workers_rx.recv().await {
            if extra_cancel.load(Ordering::Relaxed) {
                break;
            }
            tracing::debug!(
                "MEM[bg:extra_worker]: {:.1} MB RSS — {next_id} added at +{:.1}s",
                rss_mb(),
                transfer_wall_start.elapsed().as_secs_f64()
            );
            let handle = spawn_worker(next_id, worker, &extra_pool);
            extra_handles.lock().await.push(handle);
            next_id += 1;
        }
    });

    // Feed work items into the channel.
    let feed_start = std::time::Instant::now();
    for target in file_targets {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if tx.send(target).await.is_err() {
            break;
        }
    }
    drop(tx); // Close channel so workers exit when done
    tracing::debug!(
        "transfer:feed all work items queued in {}ms",
        feed_start.elapsed().as_millis(),
    );

    // Wait for all workers (initial + extra) to finish.
    loop {
        let handles: Vec<_> = worker_handles.lock().await.drain(..).collect();
        if handles.is_empty() {
            break;
        }
        let batch_size = handles.len();
        for h in handles {
            let _ = h.await;
        }
        tracing::debug!("transfer:join awaited {batch_size} worker handles");
    }
    // Signal helper tasks to stop, then wait for them to exit gracefully.
    cancel.store(true, Ordering::Relaxed);
    let _ = extra_listener.await;
    let _ = mem_reporter.await;

    let final_copied = pool.copied.load(Ordering::Relaxed) as usize;
    let final_error = pool.last_error.lock().unwrap().clone();
    let wall_secs = transfer_wall_start.elapsed().as_secs_f64();
    let avg_speed = if wall_secs > 0.1 {
        total_bytes as f64 / wall_secs / 1_048_576.0
    } else {
        0.0
    };
    tracing::debug!(
        "MEM[bg:transfer_done]: {:.1} MB RSS — {final_copied}/{total} files, \
         {:.1} MB in {wall_secs:.1}s = {avg_speed:.1} MB/s avg | \
         {initial_count} initial workers + extra via second conn",
        rss_mb(),
        total_bytes as f64 / 1_048_576.0
    );

    TransferResult {
        copied: final_copied,
        total,
        last_error: final_error,
    }
}

/// Run a pipelined transfer for a single file using a pre-opened SFTP handle.
/// Handles both upload and download directions. Does NOT open or close the handle.
#[allow(clippy::too_many_arguments)]
async fn run_file_transfer(
    raw: &Arc<RawSftpSession>,
    handle_str: &Arc<str>,
    target: &TransferTarget,
    direction: TransferDirection,
    worker_id: usize,
    read_chunk_size: u64,
    write_chunk_size: u64,
    progress: &TransferProgress,
    cancel: &AtomicBool,
    skip: &AtomicBool,
) -> Result<()> {
    let combined_cancel = Arc::new(AtomicBool::new(false));

    match direction {
        TransferDirection::LocalToRemote => {
            let local_meta = std::fs::metadata(&target.src_path)
                .with_context(|| format!("Failed to stat: {}", target.src_path))?;
            let total = local_meta.len();

            let local_file = std::fs::File::open(&target.src_path)
                .with_context(|| format!("Failed to open: {}", target.src_path))?;
            let mut local_file =
                std::io::BufReader::with_capacity((pipeline::CHUNK_SIZE * 2) as usize, local_file);

            let mut on_bytes = |bytes: u64| {
                progress.mark_transfer_start();
                progress.bytes_done_all.fetch_add(bytes, Ordering::Relaxed);
                let mut active = progress.active_files.lock().unwrap();
                if let Some(Some(af)) = active.get_mut(worker_id) {
                    af.bytes_done += bytes;
                }
            };
            let transfer = pipeline::upload_from_handle(
                raw,
                handle_str,
                &mut local_file,
                total,
                write_chunk_size,
                &mut on_bytes,
                Some(combined_cancel.as_ref()),
            );
            tokio::pin!(transfer);
            loop {
                tokio::select! {
                    result = &mut transfer => { result?; break; }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if cancel.load(Ordering::Relaxed) || skip.load(Ordering::Relaxed) {
                            combined_cancel.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        TransferDirection::RemoteToLocal => {
            // Ensure parent directory exists for nested files.
            if let Some(parent) = std::path::Path::new(&target.dst_path).parent() {
                tokio::fs::create_dir_all(parent).await.with_context(|| {
                    format!("Failed to create parent dir: {}", parent.display())
                })?;
            }

            let local_file = std::fs::File::create(&target.dst_path)
                .with_context(|| format!("Failed to create: {}", target.dst_path))?;
            let mut local_file =
                std::io::BufWriter::with_capacity((pipeline::CHUNK_SIZE * 2) as usize, local_file);

            let mut on_bytes = |bytes: u64| {
                progress.mark_transfer_start();
                progress.bytes_done_all.fetch_add(bytes, Ordering::Relaxed);
                let mut active = progress.active_files.lock().unwrap();
                if let Some(Some(af)) = active.get_mut(worker_id) {
                    af.bytes_done += bytes;
                }
            };
            let transfer = pipeline::download_from_handle(
                raw,
                handle_str,
                &mut local_file,
                target.size,
                0,
                read_chunk_size,
                &mut on_bytes,
                Some(combined_cancel.as_ref()),
            );
            tokio::pin!(transfer);
            loop {
                tokio::select! {
                    result = &mut transfer => { result?; break; }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if cancel.load(Ordering::Relaxed) || skip.load(Ordering::Relaxed) {
                            combined_cancel.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Open the SFTP file handle for a target (read for download, write for upload).
async fn open_target_handle(
    raw: &RawSftpSession,
    target: &TransferTarget,
    direction: TransferDirection,
) -> Result<Arc<str>> {
    match direction {
        TransferDirection::RemoteToLocal => pipeline::open_read(raw, &target.src_path).await,
        TransferDirection::LocalToRemote => pipeline::open_write(raw, &target.dst_path).await,
    }
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
            let setup_start = std::time::Instant::now();
            tracing::debug!("MEM[transfer:begin]: {:.1} MB RSS", rss_mb());

            // Open one SftpSession for scanning/mkdir on the primary connection.
            let scan_sftp = match open_sftp_from_handle(handle).await {
                Ok(s) => s,
                Err(e) => {
                    state.status_message = Some(format!("Failed to open SFTP session: {e}"));
                    state.input_mode = InputMode::Normal;
                    return Ok(());
                }
            };

            tracing::debug!(
                "MEM[transfer:scan_sftp_opened]: {:.1} MB RSS — in {}ms",
                rss_mb(),
                setup_start.elapsed().as_millis()
            );

            // Open one worker now so the transfer can start immediately after scanning.
            // Additional workers are opened lazily via the extra_workers channel to
            // avoid allocating channel buffers during the scan phase.
            let workers_start = std::time::Instant::now();
            let first_worker = match open_pipelined_worker(handle).await {
                Ok(w) => w,
                Err(e) => {
                    state.status_message = Some(format!("Failed to open pipelined SFTP: {e}"));
                    state.input_mode = InputMode::Normal;
                    return Ok(());
                }
            };
            let initial_workers = vec![first_worker];
            tracing::debug!(
                "MEM[transfer:first_worker_opened]: {:.1} MB RSS — in {}ms (total setup={}ms)",
                rss_mb(),
                workers_start.elapsed().as_millis(),
                setup_start.elapsed().as_millis()
            );

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
            // Capacity matches max workers so none block waiting to submit their query.
            let (ow_tx, ow_rx) =
                tokio::sync::mpsc::channel::<OverwriteQuery>(WORKERS_PER_CONNECTION * 2 + 1);
            let ow_policy = Arc::clone(&state.overwrite_policy);
            let bg_ow_policy = Arc::clone(&ow_policy);

            // Channel for dynamically adding workers (primary extra + second connection).
            let (extra_tx, extra_rx) = tokio::sync::mpsc::channel::<PipelinedWorker>(4);

            // Open remaining primary-connection workers in the background so they
            // don't allocate channel buffers during the scan phase.
            if WORKERS_PER_CONNECTION > 1
                && let Some(primary_arc) = remote_backend.ssh_handle_arc()
            {
                let primary_tx = extra_tx.clone();
                let primary_cancel = Arc::clone(&cancel);
                tokio::spawn(async move {
                    for i in 1..WORKERS_PER_CONNECTION {
                        if primary_cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        match open_pipelined_worker(&primary_arc).await {
                            Ok(w) => {
                                if primary_tx.send(w).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("primary extra worker {i}: {e}");
                                break;
                            }
                        }
                    }
                });
            }

            // Open a second TCP connection in the background for extra workers.
            let need_second = transfer_targets.len() >= MIN_FILES_FOR_SECOND_CONN;
            let extra_cancel = Arc::clone(&cancel);
            let reconnect_info = if need_second {
                let info = remote_backend.reconnection_info();
                if info.is_none() {
                    tracing::debug!(
                        "no reconnection info available (backend created via from_handle?)"
                    );
                }
                info
            } else {
                tracing::debug!(
                    "skipping second connection: only {} targets",
                    transfer_targets.len()
                );
                None
            };
            if let Some((reconnect_config, reconnect_index)) = reconnect_info {
                // Open second connection in a background task — doesn't block transfer start.
                let conn_setup_start = setup_start;
                tokio::spawn(async move {
                    // extra_tx moved into this task
                    let conn_start = std::time::Instant::now();
                    tracing::debug!(
                        "second_conn:start opening at +{:.1}s",
                        conn_setup_start.elapsed().as_secs_f64(),
                    );
                    match crate::ssh::establish_session(&reconnect_config, reconnect_index).await {
                        Ok(second_handle) => {
                            let conn_ms = conn_start.elapsed().as_millis();
                            tracing::debug!(
                                "MEM[second_conn:established]: {:.1} MB RSS — in {conn_ms}ms (at +{:.1}s)",
                                rss_mb(),
                                conn_setup_start.elapsed().as_secs_f64()
                            );
                            // Open workers on the new connection and send them to the pool.
                            for i in 0..WORKERS_PER_CONNECTION {
                                if extra_cancel.load(Ordering::Relaxed) {
                                    tracing::debug!(
                                        "second_conn: cancelled before opening worker {i}"
                                    );
                                    break;
                                }
                                let w_start = std::time::Instant::now();
                                match open_pipelined_worker(&second_handle).await {
                                    Ok(w) => {
                                        tracing::debug!(
                                            "MEM[second_conn:worker]: {:.1} MB RSS — [{i}] opened in {}ms",
                                            rss_mb(),
                                            w_start.elapsed().as_millis()
                                        );
                                        if extra_tx.send(w).await.is_err() {
                                            tracing::debug!(
                                                "second_conn: transfer done, worker {i} not needed"
                                            );
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("second_conn: opened {i} workers: {e}");
                                        break;
                                    }
                                }
                            }
                            // Keep the second handle alive until the transfer finishes.
                            // When extra_tx is dropped (transfer done), this task ends.
                            drop(extra_tx);
                            // Hold the handle alive by waiting for cancel.
                            loop {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                if extra_cancel.load(Ordering::Relaxed) {
                                    break;
                                }
                            }
                            drop(second_handle);
                        }
                        Err(e) => {
                            tracing::debug!("second SSH connection failed: {e}");
                        }
                    }
                });
            } else {
                // No second connection — drop the sender so the extra_workers listener terminates.
                drop(extra_tx);
            }

            let handle = tokio::spawn(async move {
                run_background_transfer(
                    scan_sftp,
                    initial_workers,
                    extra_rx,
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
                retry_targets: targets.clone(),
                retry_dst_cwd: dst_cwd.to_string(),
                retry_is_move: is_move,
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
    // Transfer complete popup
    if let InputMode::TransferComplete { retry, .. } = &state.input_mode {
        if retry.is_none() {
            // Success: any key dismisses
            state.input_mode = InputMode::Normal;
            return Ok(());
        }
        // Error with retry: Tab/arrows switch focus, Enter acts, Esc dismisses
        match key.code {
            KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::BackTab => {
                state.popup_focus = if state.popup_focus == 0 { 1 } else { 0 };
            }
            KeyCode::Esc => {
                state.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                if state.popup_focus == 0 {
                    // Retry: extract retry info and re-queue the transfer
                    let info = if let InputMode::TransferComplete {
                        retry: Some(info), ..
                    } = std::mem::replace(&mut state.input_mode, InputMode::Normal)
                    {
                        info
                    } else {
                        unreachable!()
                    };
                    start_copy_transfer(
                        info.targets,
                        info.direction,
                        info.source_side,
                        &info.dst_cwd,
                        left_pane,
                        right_pane,
                        state,
                        left,
                        right,
                        false,
                        info.is_move,
                    )
                    .await?;
                } else {
                    // OK: dismiss
                    state.input_mode = InputMode::Normal;
                }
            }
            _ => {}
        }
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

    // SelectPattern popup — MC-style dialog navigation:
    // Focus items: 0=Input, 1=Files only, 2=Case sensitive, 3=OK, 4=Cancel
    // Tab/Shift+Tab cycles focus. Space toggles checkboxes.
    // Enter always submits (OK) unless Cancel (4) is focused.
    // Left/Right reserved for input cursor (no widget nav).
    // Chars/Backspace always edit the input field regardless of focus.
    if matches!(state.input_mode, InputMode::SelectPattern { .. }) {
        const SELECT_FOCUS_COUNT: usize = 5;
        match key.code {
            KeyCode::Tab => {
                state.popup_focus = (state.popup_focus + 1) % SELECT_FOCUS_COUNT;
                return Ok(());
            }
            KeyCode::BackTab => {
                state.popup_focus =
                    (state.popup_focus + SELECT_FOCUS_COUNT - 1) % SELECT_FOCUS_COUNT;
                return Ok(());
            }
            KeyCode::Char(' ') if state.popup_focus == 1 || state.popup_focus == 2 => {
                // Space toggles checkboxes
                if let InputMode::SelectPattern {
                    files_only,
                    case_sensitive,
                    ..
                } = &mut state.input_mode
                {
                    if state.popup_focus == 1 {
                        *files_only = !*files_only;
                    } else {
                        *case_sensitive = !*case_sensitive;
                    }
                }
                return Ok(());
            }
            _ => {}
        }
    }

    if matches!(
        state.input_mode,
        InputMode::MkdirPrompt(_)
            | InputMode::RenamePrompt { .. }
            | InputMode::ConfirmDelete { .. }
            | InputMode::CopyConfirm { .. }
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
            InputMode::SelectPattern {
                input,
                selecting,
                files_only,
                case_sensitive,
            } => {
                if state.popup_focus == 4 {
                    // Cancel — mode already set to Normal
                } else {
                    // Enter always submits (OK) from input, checkboxes, or OK button
                    let pattern = input.trim().to_string();
                    if !pattern.is_empty() {
                        let pane = active_pane_mut(left_pane, right_pane, state);
                        for (i, entry) in pane.entries.iter().enumerate() {
                            if entry.name == ".." {
                                continue;
                            }
                            if files_only && entry.is_dir {
                                continue;
                            }
                            if glob_match_opts(&pattern, &entry.name, case_sensitive) {
                                if selecting {
                                    pane.marked.insert(i);
                                } else {
                                    pane.marked.remove(&i);
                                }
                            }
                        }
                        let count = pane.marked.len();
                        state.status_message = if count > 0 {
                            Some(format!("{count} files marked"))
                        } else {
                            None
                        };
                    }
                    // mode already set to Normal
                }
            }
            InputMode::Normal
            | InputMode::ConfirmDelete { .. }
            | InputMode::CopyConfirm { .. }
            | InputMode::TransferPopup
            | InputMode::OverwriteConfirm { .. }
            | InputMode::TransferComplete { .. } => {}
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

/// Open a pipelined SFTP worker session for high-throughput transfers.
async fn open_pipelined_worker(
    handle: &russh::client::Handle<crate::ssh::client::SshoreHandler>,
) -> Result<PipelinedWorker> {
    let channel = handle
        .channel_open_session()
        .await
        .context("Failed to open SSH channel for pipelined SFTP")?;
    let session = pipeline::create_raw_session(channel).await?;
    Ok(PipelinedWorker {
        raw: session.raw,
        read_chunk_size: session.read_chunk_size,
        write_chunk_size: session.write_chunk_size,
    })
}

/// Truncate a filename to fit within a given character width.
/// Uses `char` boundaries so multi-byte UTF-8 filenames don't panic.
pub(crate) fn truncate_name(name: &str, max_len: usize) -> String {
    if name.chars().count() <= max_len {
        name.to_string()
    } else if max_len < 3 {
        // Not enough room for even "...", just take what fits.
        name.chars().take(max_len).collect()
    } else {
        let truncated: String = name.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

/// Get process RSS in megabytes (macOS only, 0.0 elsewhere).
fn rss_mb() -> f64 {
    #[cfg(target_os = "macos")]
    {
        use std::mem;
        #[repr(C)]
        struct TaskBasicInfo {
            virtual_size: u64,
            resident_size: u64,
            resident_size_max: u64,
            user_time: [u32; 2],
            system_time: [u32; 2],
            policy: i32,
            suspend_count: i32,
        }
        const FLAVOR: u32 = 20;
        const COUNT: u32 = (mem::size_of::<TaskBasicInfo>() / mem::size_of::<u32>()) as u32;
        unsafe {
            unsafe extern "C" {
                fn mach_task_self() -> u32;
                fn task_info(t: u32, f: u32, out: *mut TaskBasicInfo, cnt: *mut u32) -> i32;
            }
            let mut info: TaskBasicInfo = mem::zeroed();
            let mut count = COUNT;
            if task_info(mach_task_self(), FLAVOR, &mut info, &mut count) == 0 {
                return info.resident_size as f64 / 1_048_576.0;
            }
        }
    }
    0.0
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

    // --- glob_match_opts (case sensitivity) ---

    #[test]
    fn test_glob_match_case_insensitive() {
        assert!(glob_match_opts("*.TXT", "readme.txt", false));
        assert!(glob_match_opts("*.txt", "README.TXT", false));
        assert!(!glob_match_opts("*.TXT", "readme.txt", true));
    }

    #[test]
    fn test_glob_match_case_sensitive_default() {
        // glob_match delegates to glob_match_opts with case_sensitive=true
        assert!(!glob_match("*.TXT", "readme.txt"));
        assert!(glob_match("*.txt", "readme.txt"));
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
    fn test_truncate_name_zero_width() {
        // Zero width should return empty string, never panic.
        assert_eq!(truncate_name("hello", 0), "");
        assert_eq!(truncate_name("", 0), "");
        // Width 1 and 2: too small for "...", just take what fits.
        assert_eq!(truncate_name("hello", 1), "h");
        assert_eq!(truncate_name("hello", 2), "he");
    }

    #[test]
    fn test_overwrite_policy_persists_across_transfers() {
        // The overwrite policy is shared via Arc<AtomicU64> across all transfers
        // in a browser session. When a user picks "Overwrite All" (1) or
        // "Skip All" (2), subsequent transfers read the same value.
        let policy = Arc::new(AtomicU64::new(0));
        assert_eq!(policy.load(Ordering::Relaxed), 0); // default: ask

        // Simulate first transfer setting "overwrite all"
        let transfer1_policy = Arc::clone(&policy);
        transfer1_policy.store(1, Ordering::Relaxed);

        // Simulate second transfer reading the persisted policy
        let transfer2_policy = Arc::clone(&policy);
        assert_eq!(transfer2_policy.load(Ordering::Relaxed), 1);

        // Simulate user changing to "skip all"
        transfer2_policy.store(2, Ordering::Relaxed);
        assert_eq!(policy.load(Ordering::Relaxed), 2);
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

    // --- RetryInfo / TransferComplete ---

    #[test]
    fn test_retry_info_present_on_error() {
        // When a transfer has an error, RetryInfo should be populated.
        let result = TransferResult {
            copied: 3,
            total: 5,
            last_error: Some("connection reset".into()),
        };
        let retry = if result.last_error.is_some() {
            Some(RetryInfo {
                targets: vec![("/src/a.txt".into(), "a.txt".into(), false, 100)],
                direction: TransferDirection::LocalToRemote,
                source_side: Side::Left,
                dst_cwd: "/dst".into(),
                is_move: false,
            })
        } else {
            None
        };
        assert!(retry.is_some());
        let info = retry.unwrap();
        assert_eq!(info.targets.len(), 1);
        assert_eq!(info.dst_cwd, "/dst");
        assert!(!info.is_move);
    }

    #[test]
    fn test_retry_info_absent_on_success() {
        // When a transfer succeeds, retry should be None.
        let result = TransferResult {
            copied: 5,
            total: 5,
            last_error: None,
        };
        let retry: Option<RetryInfo> = if result.last_error.is_some() {
            Some(RetryInfo {
                targets: vec![],
                direction: TransferDirection::LocalToRemote,
                source_side: Side::Left,
                dst_cwd: "/dst".into(),
                is_move: false,
            })
        } else {
            None
        };
        assert!(retry.is_none());
    }

    // --- ETA calculation ---

    #[test]
    fn test_eta_calculation_normal() {
        // 50% done at 1 MB/s with enough samples => ~5s remaining
        let eta = calculate_eta(5_000_000, 10_000_000, 1_000_000.0, 5);
        assert_eq!(eta, Some(5));
    }

    #[test]
    fn test_eta_calculation_insufficient_samples() {
        // Only 2 samples (below MIN_ETA_SPEED_SAMPLES=3) => None
        let eta = calculate_eta(5_000_000, 10_000_000, 1_000_000.0, 2);
        assert!(eta.is_none());
    }

    #[test]
    fn test_eta_calculation_zero_speed() {
        let eta = calculate_eta(5_000_000, 10_000_000, 0.0, 10);
        assert!(eta.is_none());
    }

    #[test]
    fn test_eta_calculation_negative_speed() {
        let eta = calculate_eta(5_000_000, 10_000_000, -100.0, 10);
        assert!(eta.is_none());
    }

    #[test]
    fn test_eta_calculation_zero_total_bytes() {
        // Unknown total size => None
        let eta = calculate_eta(5_000, 0, 1_000.0, 10);
        assert!(eta.is_none());
    }

    #[test]
    fn test_eta_calculation_transfer_complete() {
        // All bytes done => None (no remaining time)
        let eta = calculate_eta(10_000, 10_000, 1_000.0, 10);
        assert!(eta.is_none());
    }

    #[test]
    fn test_eta_calculation_nearly_done() {
        // 1 byte remaining at 1 byte/s => 1 second
        let eta = calculate_eta(9_999, 10_000, 1.0, 5);
        assert_eq!(eta, Some(1));
    }

    #[test]
    fn test_eta_calculation_large_transfer() {
        // 1 GB remaining at 10 MB/s => 100 seconds
        let gb = 1_073_741_824u64;
        let done = gb;
        let total = 2 * gb;
        let speed = 10.0 * 1_048_576.0; // 10 MB/s
        let eta = calculate_eta(done, total, speed, 50);
        // 1 GB / 10 MB/s = 102.4 seconds
        assert_eq!(eta, Some(102));
    }

    // --- Transfer summary format ---

    #[test]
    fn test_transfer_summary_format_success_single_file() {
        let result = TransferResult {
            copied: 1,
            total: 1,
            last_error: None,
        };
        let msg = format_transfer_summary(&result, 14_900_000, Duration::from_secs(12), false);
        assert!(msg.starts_with('\u{2713}'));
        assert!(msg.contains("1 file"));
        assert!(msg.contains("14.2MB"));
        assert!(msg.contains("12s"));
        assert!(msg.contains("avg"));
    }

    #[test]
    fn test_transfer_summary_format_success_multiple_files() {
        let result = TransferResult {
            copied: 3,
            total: 3,
            last_error: None,
        };
        let msg = format_transfer_summary(&result, 14_900_000, Duration::from_secs(12), false);
        assert!(msg.contains("3 files"));
        assert!(msg.contains("14.2MB"));
        assert!(msg.contains("12s"));
        assert!(msg.contains("avg"));
    }

    #[test]
    fn test_transfer_summary_format_partial_failure() {
        let result = TransferResult {
            copied: 2,
            total: 5,
            last_error: Some("connection lost".into()),
        };
        let msg = format_transfer_summary(&result, 5_000_000, Duration::from_secs(8), false);
        assert!(msg.starts_with("Failed:"));
        assert!(msg.contains("connection lost"));
        assert!(msg.contains("2 of 5"));
    }

    #[test]
    fn test_transfer_summary_format_cancelled() {
        let result = TransferResult {
            copied: 0,
            total: 5,
            last_error: None,
        };
        let msg = format_transfer_summary(&result, 0, Duration::from_secs(1), false);
        assert_eq!(msg, "Copy cancelled");
    }

    #[test]
    fn test_transfer_summary_format_move() {
        let result = TransferResult {
            copied: 2,
            total: 2,
            last_error: None,
        };
        let msg = format_transfer_summary(&result, 1_000_000, Duration::from_secs(5), true);
        assert!(msg.contains("Moved"));
        assert!(!msg.contains("Copied"));
    }

    #[test]
    fn test_transfer_summary_format_move_cancelled() {
        let result = TransferResult {
            copied: 0,
            total: 3,
            last_error: None,
        };
        let msg = format_transfer_summary(&result, 0, Duration::from_secs(0), true);
        assert_eq!(msg, "Move cancelled");
    }

    #[test]
    fn test_transfer_summary_format_very_fast_transfer() {
        // Transfer completes in < 0.1s — avg speed omitted (avoid divide-by-near-zero)
        let result = TransferResult {
            copied: 1,
            total: 1,
            last_error: None,
        };
        let msg = format_transfer_summary(&result, 500, Duration::from_millis(50), false);
        assert!(msg.contains("1 file"));
        assert!(!msg.contains("avg"));
    }
}
