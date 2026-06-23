pub mod theme;
pub mod views;
pub mod widgets;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use std::sync::atomic::Ordering;

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
use crate::keychain;
use crate::ssh;
use crate::tui::theme::{ThemeColors, resolve_theme};
use crate::tui::views::browser::truncate_name;
use crate::tui::views::confirm::{ConfirmState, ConfirmTarget};
use crate::tui::views::form::{EditTarget, FIELD_ENV, FIELD_PROFILE, FormState, UnifiedEntry};
use crate::tui::views::{confirm, form, help, import_wizard, list};
use crate::tui::widgets::{search_bar, status_bar};

/// Duration before status messages auto-clear.
const STATUS_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll timeout when a status message is pending (need to detect expiry).
const TICK_RATE_ACTIVE: Duration = Duration::from_millis(100);

/// Poll timeout when idle (no timed state changes pending).
/// User input is detected instantly regardless of this value.
const TICK_RATE_IDLE: Duration = Duration::from_secs(1);

/// If `event::poll()` returns this many times in a row faster than the requested
/// timeout (i.e. the fd is in a POLLHUP/error state), assume the terminal is gone.
/// Raised from 10 to 100 to avoid false positives on some Mac terminals.
const RAPID_POLL_LIMIT: u32 = 100;

/// Threshold: if a poll that requested ≥100ms returns in under this duration,
/// it counts as suspiciously fast (broken fd returning immediately).
const RAPID_POLL_THRESHOLD: Duration = Duration::from_millis(10);

/// Interval at which the terminal watchdog thread checks if the terminal is alive.
const TERMINAL_WATCHDOG_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum number of zero-timeout events to drain when re-entering the TUI.
/// Prevents a dead terminal fd from spinning forever in `drain_events()`.
const DRAIN_EVENTS_LIMIT: usize = 64;

/// Number of items to jump with Page Up/Down.
const PAGE_JUMP: usize = 10;

/// TUI screen states.
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    List,
    AddForm,
    EditForm(EditTarget, usize),
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

/// Action returned by the event loop to signal leaving the TUI for SSH or SFTP.
enum LoopAction {
    Quit,
    Connect(usize),
    Browse(usize),
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
    /// Set when the user presses 'f' to browse; signals the event loop to exit.
    browse_request: Option<usize>,
    /// Scroll offset for the help overlay.
    help_scroll: u16,
    /// Which screen the user was on when they opened help (for context-aware content).
    help_source: Option<Screen>,
    /// Config file path override (from --config flag or SSHORE_CONFIG env var).
    config_path_override: Option<String>,
    /// Groups with collapsed session lists (set of group indices).
    pub collapsed_groups: HashSet<usize>,
    /// Currently selected session as (group_index, session_index) within the group.
    /// `None` means no session selected (e.g., only bookmarks or empty config).
    pub selected_session: Option<(usize, usize)>,
    matcher: SkimMatcherV2,
}

impl App {
    /// Create a new App from loaded config.
    pub fn new(config: AppConfig) -> Self {
        let matcher = SkimMatcherV2::default();
        let filtered_indices = search_bar::filter_bookmarks(&matcher, &config.bookmarks, "", None);
        let theme = resolve_theme(&config.settings.theme);
        let tunnel_bookmarks = crate::ssh::tunnel::active_tunnel_bookmarks();

        // Initialize selected_session if groups exist
        let selected_session = config
            .groups
            .iter()
            .enumerate()
            .find(|(_, g)| !g.sessions.is_empty())
            .map(|(group_idx, _)| (group_idx, 0));

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
            browse_request: None,
            help_scroll: 0,
            help_source: None,
            config_path_override: None,
            collapsed_groups: HashSet::new(),
            selected_session,
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
    let mut drained = 0usize;
    while drained < DRAIN_EVENTS_LIMIT
        && !crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
        && event::poll(Duration::ZERO).unwrap_or(false)
    {
        if event::read().is_err() {
            break;
        }
        drained += 1;
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
    tracing::debug!(bookmarks = config.bookmarks.len(), "starting TUI");

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

    // Subscribe to global shutdown signal (SIGHUP/SIGTERM).
    // The watch receiver is polled each iteration to detect terminal close.
    let shutdown_rx = crate::subscribe_shutdown();

    // Start the terminal watchdog: a background thread that monitors whether
    // stdin is still a valid TTY. If the terminal emulator closes, crossterm's
    // event::poll() can block indefinitely inside read() on the dead fd.
    // The watchdog detects this independently and sets SHUTDOWN_REQUESTED
    // to force the event loop to exit on its next iteration.
    // Note: poll_stdin() below already handles dead TTY detection via
    // libc::poll(), but the watchdog provides an additional safety net
    // for edge cases where libc::poll() might also block.
    let _watchdog = TerminalWatchdog::spawn();

    loop {
        if crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            tracing::debug!("shutdown requested before entering TUI");
            break;
        }

        let mut terminal = match enter_tui() {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "failed to enter TUI, terminal likely gone");
                break;
            }
        };
        let action = event_loop(&mut terminal, &mut app)?;
        let _ = leave_tui(&mut terminal);

        // Check shutdown after leaving TUI — if the terminal was closed
        // during an SSH session, don't try to re-enter the TUI on a dead fd.
        if crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) || *shutdown_rx.borrow() {
            tracing::debug!("shutdown requested after leaving TUI");
            break;
        }

        match action {
            LoopAction::Quit => {
                tracing::debug!("user quit TUI");
                break;
            }
            LoopAction::Connect(index) => {
                // Session indices are encoded as: group_idx * 10000 + session_idx
                // If index >= 10000, it's a session connection
                if index >= 10000 {
                    let display_name = if index / 10000 < app.config.groups.len() {
                        let g = &app.config.groups[index / 10000];
                        let s_idx = index % 10000;
                        if s_idx < g.sessions.len() {
                            g.sessions[s_idx].display_name(g)
                        } else {
                            format!("session-{}", s_idx)
                        }
                    } else {
                        format!("session-{}", index)
                    };
                    tracing::debug!(
                        session = display_name.as_str(),
                        encoded_index = index,
                        "connecting session from TUI"
                    );
                    if let Err(e) = ssh::connect_session(
                        &mut app.config,
                        index,
                        app.config_path_override.as_deref(),
                    )
                    .await
                    {
                        tracing::debug!(error = %e, "SSH session ended with error");
                        eprintln!("SSH error: {e:#}");
                        if !crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                            eprintln!("Press Enter to return to sshore...");
                            let _ = wait_for_enter();
                        }
                    }
                    tracing::debug!("returned to TUI after SSH session");
                } else {
                    let name = &app.config.bookmarks[index].name;
                    tracing::debug!(
                        bookmark = name,
                        index,
                        "connecting from TUI"
                    );
                    if let Err(e) = ssh::connect(
                        &mut app.config,
                        index,
                        app.config_path_override.as_deref(),
                    )
                    .await
                    {
                        tracing::debug!(error = %e, "SSH session ended with error");
                        eprintln!("SSH error: {e:#}");
                        if !crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                            eprintln!("Press Enter to return to sshore...");
                            let _ = wait_for_enter();
                        }
                    }
                    tracing::debug!("returned to TUI after SSH session");
                }
            }
            LoopAction::Browse(bookmark_index) => {
                let name = &app.config.bookmarks[bookmark_index].name;
                tracing::debug!(bookmark = name, index = bookmark_index, "browsing from TUI");
                if let Err(e) = launch_browse(&app.config, bookmark_index).await {
                    tracing::debug!(error = %e, "browse ended with error");
                    eprintln!("Browse error: {e:#}");
                    if !crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        eprintln!("Press Enter to return to sshore...");
                        let _ = wait_for_enter();
                    }
                }
                tracing::debug!("returned to TUI after browse");
            }
        }
    }

    // Write back updated config (connection stats may have changed)
    *config = app.config;

    Ok(())
}

/// Wait for the user to press Enter (used after SSH error messages).
/// Returns `Ok(())` on dead terminal (stdin closed) rather than propagating
/// an error, since a dead terminal is an expected exit path.
fn wait_for_enter() -> Result<()> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // stdin was closed (terminal gone) — not an error
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Launch the file browser for a bookmark from the TUI.
async fn launch_browse(config: &AppConfig, bookmark_index: usize) -> Result<()> {
    use crate::storage;

    let bookmark = &config.bookmarks[bookmark_index];
    ssh::print_production_banner(bookmark, &config.settings, &config.profiles, "SFTP browser");
    ssh::terminal_theme::apply_theme(bookmark, &config.settings);

    let theme = resolve_theme(&config.settings.theme);

    let remote_sftp = storage::sftp_backend::SftpBackend::new(config, bookmark_index).await?;
    let local_fs = storage::local_backend::LocalBackend::new(".")?;

    let mut left = storage::Backend::Sftp(remote_sftp);
    let mut right = storage::Backend::Local(local_fs);

    views::browser::run(
        &mut left,
        &mut right,
        &bookmark.name,
        &bookmark.env,
        false,
        &theme,
    )
    .await?;

    ssh::terminal_theme::reset_theme();
    Ok(())
}

/// Main event loop: draw, poll, handle. Returns action when the loop exits.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<LoopAction> {
    // Reset requests from any previous iteration
    app.connect_request = None;
    app.browse_request = None;

    // Always draw the first frame
    let mut needs_redraw = true;
    // Consecutive suspiciously-fast poll returns (detects broken/closed terminal fd).
    let mut rapid_polls: u32 = 0;

    loop {
        // Check shutdown at the top of the loop so we break out immediately
        // even if poll() returns events on a dead fd.
        if crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            tracing::debug!("shutdown requested during event loop");
            return Ok(LoopAction::Quit);
        }

        // Detect dead terminal: when the terminal emulator closes, stdin is
        // no longer a valid TTY. This is the most reliable cross-platform way
        // to detect that the user has closed the terminal window/tab.
        if !is_terminal_active() || is_terminal_dead() {
            tracing::debug!("stdin is no longer a terminal, exiting event loop");
            return Ok(LoopAction::Quit);
        }

        if needs_redraw {
            // Draw on a dead terminal may fail; treat it as a signal to exit
            // rather than propagating the error and crashing.
            if terminal.draw(|frame| draw(frame, app)).is_err() {
                tracing::debug!("draw failed, terminal likely gone");
                return Ok(LoopAction::Quit);
            }
            needs_redraw = false;
        }

        // Use a shorter poll timeout when a status message needs expiry checking
        let poll_timeout = if app.status_message.is_some() {
            TICK_RATE_ACTIVE
        } else {
            TICK_RATE_IDLE
        };

        // Use our own poll() instead of crossterm's to avoid blocking
        // indefinitely inside read() on a dead TTY. crossterm's event::poll()
        // internally calls read() on stdin, which can block forever when the
        // terminal is closed (POLLHUP state). Our libc::poll() has a bounded
        // timeout and checks for POLLHUP/POLLERR directly.
        let before_poll = Instant::now();
        let has_event = poll_stdin(poll_timeout);

        if has_event {
            match event::read() {
                Ok(Event::Key(key)) => {
                    handle_key_event(app, key);
                    needs_redraw = true;
                    rapid_polls = 0;
                }
                Ok(Event::Resize(_, _)) => {
                    needs_redraw = true;
                    rapid_polls = 0;
                }
                Ok(other) => {
                    tracing::trace!(?other, "TUI: ignoring event");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "read failed, terminal likely gone");
                    return Ok(LoopAction::Quit);
                }
            }
        }

        // Detect broken terminal: when the fd is in POLLHUP/error state,
        // poll() returns immediately regardless of the requested timeout.
        // Only count rapid polls when there was NO event (legitimate input resets counter).
        // Can be disabled via SSHORE_NO_POLL_CHECK=1 (e.g. for problematic terminals).
        let poll_check_disabled = std::env::var("SSHORE_NO_POLL_CHECK").is_ok();
        if !poll_check_disabled
            && !has_event
            && poll_timeout >= TICK_RATE_ACTIVE
            && before_poll.elapsed() < RAPID_POLL_THRESHOLD
        {
            rapid_polls += 1;
            if rapid_polls >= RAPID_POLL_LIMIT {
                tracing::debug!(
                    rapid_polls,
                    "terminal fd appears dead (poll returning instantly), exiting"
                );
                return Ok(LoopAction::Quit);
            }
        } else {
            rapid_polls = 0;
        }

        // Check if status message expired (only source of timed state change)
        let had_status = app.status_message.is_some();
        app.tick();
        if had_status && app.status_message.is_none() {
            needs_redraw = true;
        }

        if app.should_quit || crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            return Ok(LoopAction::Quit);
        }

        if let Some(idx) = app.connect_request.take() {
            return Ok(LoopAction::Connect(idx));
        }
        if let Some(idx) = app.browse_request.take() {
            return Ok(LoopAction::Browse(idx));
        }
    }
}

/// Poll stdin for readability using libc::poll() with a bounded timeout.
/// Returns `true` if stdin has data available to read.
///
/// This replaces `crossterm::event::poll()` to avoid the problem where
/// crossterm blocks indefinitely inside `read()` on a dead TTY fd.
///
/// Unlike crossterm's implementation, this function:
/// - Uses `libc::poll()` which has a guaranteed timeout
/// - Detects POLLHUP/POLLERR and returns false (not true) for dead fds
/// - Never blocks indefinitely
#[cfg(unix)]
fn poll_stdin(timeout: Duration) -> bool {
    use std::os::unix::io::AsRawFd;

    let timeout_ms = if timeout.is_zero() {
        0
    } else {
        // Clamp to a maximum of 2 seconds to prevent any single poll
        // from blocking for too long. This ensures the watchdog and
        // shutdown checks fire at least every 2 seconds.
        (timeout.as_millis() as i32).min(2000)
    };

    let mut fds = libc::pollfd {
        fd: std::io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    let ret = unsafe { libc::poll(&mut fds, 1, timeout_ms) };

    if ret < 0 {
        // EINTR — signal interrupted poll, treat as no events
        // (the signal handler will set SHUTDOWN_REQUESTED if needed)
        return false;
    }

    if ret == 0 {
        // Timeout — no events
        return false;
    }

    // Check for hangup or error — these indicate a dead terminal
    if fds.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return false;
    }

    // POLLIN set — data available
    fds.revents & libc::POLLIN != 0
}

#[cfg(not(unix))]
fn poll_stdin(timeout: Duration) -> bool {
    // On non-Unix platforms, fall back to crossterm's poll
    event::poll(timeout).unwrap_or(false)
}

/// Check if stdin is still connected to an active terminal.
/// Returns `false` when the terminal has been closed (e.g., window/tab closed),
/// which causes stdin to no longer be a TTY device.
///
/// This is the primary defense against the "zombie process" problem where
/// sshore spins at 100% CPU after the terminal emulator is terminated.
/// The signal-based approach (SIGHUP) is unreliable because:
/// - SIGHUP may not be delivered if the process was started from a non-login shell
/// - The tokio signal handler runs on the async runtime, not the blocking poll thread
/// - macOS terminal emulators may not send SIGHUP to child processes on window close
#[cfg(unix)]
fn is_terminal_active() -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    // isatty() returns 0 if fd is not connected to a terminal
    let result = unsafe { libc::isatty(fd) };
    result != 0
}

#[cfg(not(unix))]
fn is_terminal_active() -> bool {
    true
}

/// Try to detect a dead terminal by performing a non-blocking poll on stdin.
/// On macOS, when the terminal window closes, the PTY slave fd may still pass
/// isatty() but poll() returns POLLHUP | POLLERR. This is non-invasive:
/// it doesn't consume any data from the fd.
#[cfg(unix)]
fn is_terminal_dead() -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    // First check: if isatty returns false, terminal is definitely gone
    if unsafe { libc::isatty(fd) } == 0 {
        return true;
    }
    // Second check: use poll() to detect HUP/ERR on the fd.
    // This is non-invasive — it doesn't read any data.
    let mut fds = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // Zero timeout — just check current state, don't block
    let ret = unsafe { libc::poll(&mut fds, 1, 0) };
    if ret < 0 {
        return false; // poll error, inconclusive
    }
    if ret == 0 {
        return false; // no events, terminal is fine
    }
    // Check for hangup or error flags
    if fds.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    false
}

#[cfg(not(unix))]
fn is_terminal_dead() -> bool {
    false
}

/// Install a no-op signal handler for SIGUSR1 so that raising it from the
/// terminal watchdog interrupts blocking syscalls (like read() in crossterm)
/// without terminating the process.
#[cfg(unix)]
fn install_sigusr1_noop_handler() {
    unsafe {
        libc::signal(libc::SIGUSR1, libc::SIG_IGN as libc::sighandler_t);
    }
}

/// Background thread that monitors whether the terminal is still alive.
///
/// This is a safety net: even though `poll_stdin()` uses `libc::poll()` with
/// a bounded timeout, the watchdog provides independent monitoring in case
/// the event loop gets stuck for any other reason.
struct TerminalWatchdog {
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl TerminalWatchdog {
    fn spawn() -> Self {
        // Install no-op handler for SIGUSR1 as a fallback interrupt mechanism.
        #[cfg(unix)]
        install_sigusr1_noop_handler();

        let handle = std::thread::Builder::new()
            .name("terminal-watchdog".into())
            .spawn(move || {
                loop {
                    // Exit if shutdown was requested by another mechanism
                    if crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                        break;
                    }
                    if !is_terminal_active() || is_terminal_dead() {
                        tracing::debug!("terminal watchdog: terminal closed, requesting shutdown");
                        crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                        // Interrupt any blocking syscall in the main thread.
                        // SIGUSR1 with SIG_IGN handler is a no-op that still
                        // causes EINTR on blocking syscalls.
                        #[cfg(unix)]
                        unsafe {
                            libc::raise(libc::SIGUSR1);
                        }
                        break;
                    }
                    std::thread::sleep(TERMINAL_WATCHDOG_INTERVAL);
                }
            })
            .expect("failed to spawn terminal watchdog thread");
        Self {
            _handle: Some(handle),
        }
    }
}

impl Drop for TerminalWatchdog {
    fn drop(&mut self) {
        // Signal the watchdog to stop, then join
        crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
        if let Some(handle) = self._handle.take() {
            let _ = handle.join();
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
        let available = chunks[chunk_idx].width.saturating_sub(1) as usize;
        let display_msg = truncate_name(msg, available);
        let status_line = ratatui::text::Line::from(ratatui::text::Span::styled(
            format!(" {display_msg}"),
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
            let source = app.help_source.as_ref().unwrap_or(&Screen::List);
            let is_production_delete = match source {
                Screen::DeleteConfirm(_) => Some(
                    app.confirm_state
                        .as_ref()
                        .map(|s| s.is_production)
                        .unwrap_or(false),
                ),
                _ => None,
            };
            help::render_help(
                frame,
                frame.area(),
                source,
                app.search_active,
                is_production_delete,
                theme,
                app.help_scroll,
            );
        }
        Screen::AddForm | Screen::EditForm(_, _) => {
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
        Screen::AddForm | Screen::EditForm(_, _) => handle_unified_form_key(app, key),
        Screen::DeleteConfirm(_) => handle_confirm_key(app, key),
    }
}

/// Handle key events in the list view (not searching).
fn handle_list_key(app: &mut App, key: KeyEvent) {
    // If we have groups, use session navigation
    if !app.config.groups.is_empty() {
        handle_session_list_key(app, key);
        return;
    }

    // No groups — use bookmark navigation
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
            let profile_names: Vec<String> =
                app.config.profiles.iter().map(|p| p.name.clone()).collect();
            app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
            app.screen = Screen::AddForm;
        }
        KeyCode::Char('A') => {
            let profile_names: Vec<String> =
                app.config.profiles.iter().map(|p| p.name.clone()).collect();
            app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
            app.screen = Screen::AddForm;
        }
        KeyCode::Char('e') => {
            if let Some(idx) = app.selected_bookmark_index() {
                let profile_names: Vec<String> =
                    app.config.profiles.iter().map(|p| p.name.clone()).collect();
                let bookmark = &app.config.bookmarks[idx];
                app.form_state = Some(FormState::new_edit(idx, EditTarget::Bookmark, bookmark, &profile_names));
                app.screen = Screen::EditForm(EditTarget::Bookmark, idx);
            }
        }
        KeyCode::Char('d') => {
            if let Some(idx) = app.selected_bookmark_index() {
                let bookmark = &app.config.bookmarks[idx];
                app.confirm_state = Some(ConfirmState::new(bookmark));
                app.screen = Screen::DeleteConfirm(idx);
            }
        }
        KeyCode::Char('f') => {
            if let Some(idx) = app.selected_bookmark_index() {
                app.browse_request = Some(idx);
            }
        }

        // Help
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.help_source = Some(Screen::List);
            app.screen = Screen::Help;
        }

        _ => {}
    }
}

/// Handle key events in the session list view (when groups exist).
///
/// Navigation moves through sessions within groups, skipping group headers.
/// Space on a group header toggles collapse/expand.
/// Enter on a session triggers connection.
fn handle_session_list_key(app: &mut App, key: KeyEvent) {
    match key.code {
        // Quit
        KeyCode::Char('q') => app.should_quit = true,

        // Navigation: move through sessions
        KeyCode::Up | KeyCode::Char('k') => move_session_selection(app, -1),
        KeyCode::Down | KeyCode::Char('j') => move_session_selection(app, 1),
        KeyCode::Home | KeyCode::Char('g') => move_session_to_first(app),
        KeyCode::End | KeyCode::Char('G') => move_session_to_last(app),
        KeyCode::PageUp => move_session_selection(app, -(PAGE_JUMP as isize)),
        KeyCode::PageDown => move_session_selection(app, PAGE_JUMP as isize),

        // Toggle group collapse/expand
        KeyCode::Char(' ') => toggle_group_collapse(app),

        // Connect to selected session
        KeyCode::Enter => {
            if let Some((group_idx, session_idx)) = app.selected_session {
                // Encode as a single index for the connect_request
                // group_idx * 10000 + session_idx (assuming < 10000 sessions per group)
                let encoded = group_idx * 10000 + session_idx;
                app.connect_request = Some(encoded);
            }
        }

        // Search
        KeyCode::Char('/') => {
            app.search_active = true;
        }

        // Help
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.help_source = Some(Screen::List);
            app.screen = Screen::Help;
        }

        // Add group form (capital A)
        KeyCode::Char('A') => {
            let profile_names: Vec<String> =
                app.config.profiles.iter().map(|p| p.name.clone()).collect();
            app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
            app.screen = Screen::AddForm;
        }

        // Add bookmark form (lowercase a)
        KeyCode::Char('a') => {
            let profile_names: Vec<String> =
                app.config.profiles.iter().map(|p| p.name.clone()).collect();
            app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
            app.screen = Screen::AddForm;
        }

        // Edit selected group
        KeyCode::Char('e') => {
            if let Some((group_idx, _)) = app.selected_session {
                let profile_names: Vec<String> =
                    app.config.profiles.iter().map(|p| p.name.clone()).collect();
                let group = &app.config.groups[group_idx];
                app.form_state = Some(FormState::new_group_edit(group_idx, group, &profile_names));
                app.screen = Screen::EditForm(EditTarget::Group, group_idx);
            }
        }

        // Delete selected group
        KeyCode::Char('d') => {
            if let Some((group_idx, _)) = app.selected_session {
                let group = app.config.groups[group_idx].clone();
                app.confirm_state = Some(ConfirmState::new_group(&group));
                app.screen = Screen::DeleteConfirm(group_idx);
            }
        }

        _ => {}
    }
}

/// Move session selection by delta (positive = down, negative = up).
/// Skips group headers and respects collapsed groups.
fn move_session_selection(app: &mut App, delta: isize) {
    let sessions = visible_sessions(app);
    if sessions.is_empty() {
        return;
    }

    // Find current position in visible sessions list
    let current_pos = match app.selected_session {
        Some((g, s)) => sessions
            .iter()
            .position(|&(sg, ss)| sg == g && ss == s)
            .unwrap_or(0),
        None => 0,
    };

    let max = sessions.len() - 1;
    let new_pos = current_pos as isize + delta;
    let new_pos = new_pos.clamp(0, max as isize) as usize;

    if let Some(&(g, s)) = sessions.get(new_pos) {
        app.selected_session = Some((g, s));
    }
}

/// Move to the first visible session.
fn move_session_to_first(app: &mut App) {
    if let Some(&(g, s)) = visible_sessions(app).first() {
        app.selected_session = Some((g, s));
    }
}

/// Move to the last visible session.
fn move_session_to_last(app: &mut App) {
    if let Some(&(g, s)) = visible_sessions(app).last() {
        app.selected_session = Some((g, s));
    }
}

/// Get the list of visible (non-collapsed) sessions as (group_idx, session_idx) pairs.
fn visible_sessions(app: &App) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    for (group_idx, group) in app.config.groups.iter().enumerate() {
        if app.collapsed_groups.contains(&group_idx) {
            continue;
        }
        for session_idx in 0..group.sessions.len() {
            result.push((group_idx, session_idx));
        }
    }
    result
}

/// Toggle collapse/expand for the group containing the currently selected session.
fn toggle_group_collapse(app: &mut App) {
    if let Some((group_idx, _)) = app.selected_session {
        if app.collapsed_groups.contains(&group_idx) {
            app.collapsed_groups.remove(&group_idx);
        } else {
            app.collapsed_groups.insert(group_idx);
        }
    }
}

/// Handle key events in the add/edit form.
/// Handle key events in the unified form (handles both bookmark and group mode).
fn handle_unified_form_key(app: &mut App, key: KeyEvent) {
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
        KeyCode::Left if form.focused() == FIELD_ENV => form.cycle_env_left(),
        KeyCode::Right if form.focused() == FIELD_ENV => form.cycle_env_right(),
        KeyCode::Left if form.focused() == FIELD_PROFILE => form.cycle_profile_left(),
        KeyCode::Right if form.focused() == FIELD_PROFILE => form.cycle_profile_right(),
        KeyCode::Backspace => form.delete_char(),
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+O: add a new session line
            form.add_session_line();
        }
        KeyCode::Char(c) if c == '\x0f' => {
            // Ctrl+O sent as raw control character by some terminals
            form.add_session_line();
        }
        KeyCode::F(2) => {
            // F2: add a new session line (works on all terminals)
            form.add_session_line();
        }
        KeyCode::Enter => {
            // Attempt to save
            try_save_unified_form(app);
        }
        KeyCode::Char('-') => {
            // Remove current session line
            form.remove_session_line();
        }
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.help_source = Some(app.screen.clone());
            app.screen = Screen::Help;
        }
        KeyCode::Char(c) => form.insert_char(c),
        _ => {}
    }
}

/// Try to validate and save the form. On success, return to list. On failure, show error.
/// Try to validate and save the unified form. Handles both bookmark and group mode.
fn try_save_unified_form(app: &mut App) {
    let Some(ref mut form) = app.form_state else {
        return;
    };

    let is_group = form.inner().is_group();

    if is_group {
        match form.validate_and_build_group(&app.config) {
            Ok(group) => {
                let name = group.name.clone();
                match app.screen {
                    Screen::AddForm => {
                        app.config.groups.push(group);
                    }
                    Screen::EditForm(EditTarget::Group, idx) => {
                        app.config.groups[idx] = group;
                    }
                    _ => {}
                }
                if let Err(e) = app.save_config() {
                    app.set_status(format!("Error saving config: {e}"));
                } else {
                    app.set_status(format!("Group '{name}' saved"));
                }
                app.form_state = None;
                app.screen = Screen::List;
                app.refilter();
            }
            Err(e) => {
                app.status_message = Some((e.to_string(), Instant::now()));
            }
        }
    } else {
        match form.validate_and_build(&app.config) {
            Ok(UnifiedEntry::Bookmark(bookmark)) => {
                let name = bookmark.name.clone();
                let password_value = form.password().to_string();
                let password_modified = form.password_modified();
                let has_stored = form.has_stored_password();

                let old_name = if let Screen::EditForm(EditTarget::Bookmark, idx) = app.screen {
                    let orig = &app.config.bookmarks[idx].name;
                    if orig != &name {
                        Some(orig.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };

                match app.screen {
                    Screen::AddForm => {
                        app.config.bookmarks.push(bookmark);
                    }
                    Screen::EditForm(EditTarget::Bookmark, idx) => {
                        let original = &app.config.bookmarks[idx];
                        let mut updated = bookmark;
                        updated.last_connected = original.last_connected;
                        updated.connect_count = original.connect_count;
                        app.config.bookmarks[idx] = updated;
                    }
                    _ => {}
                }

                if let Err(e) = app.save_config() {
                    app.set_status(format!("Error saving config: {e}"));
                } else {
                    app.set_status(format!("Bookmark '{name}' saved"));
                }

                if let Some(ref old) = old_name {
                    if let Ok(Some(pw)) = keychain::get_password(old) {
                        if let Err(e) = keychain::set_password(&name, &pw) {
                            app.set_status(format!("Warning: failed to migrate password: {e}"));
                        }
                        if let Err(e) = keychain::delete_password(old) {
                            app.set_status(format!(
                                "Warning: failed to remove old keychain entry: {e}"
                            ));
                        }
                    }
                }

                if password_modified {
                    if !password_value.is_empty() {
                        if let Err(e) = keychain::set_password(&name, &password_value) {
                            app.set_status(format!("Warning: failed to save password: {e}"));
                        }
                    } else if has_stored {
                        if let Err(e) = keychain::delete_password(&name) {
                            app.set_status(format!("Warning: failed to remove password: {e}"));
                        }
                    }
                }

                app.form_state = None;
                app.screen = Screen::List;
                app.refilter();
            }
            Ok(UnifiedEntry::Group(_)) => {
                // Should not happen in bookmark path (is_group check above)
                app.status_message = Some(("Unexpected group entry in bookmark path".to_string(), Instant::now()));
            }
            Err(e) => {
                app.status_message = Some((e.to_string(), Instant::now()));
            }
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
                // Determine if we're deleting a bookmark or group based on the target
                let was_group = state.target.is_group;

                if was_group {
                    let name = app.config.groups[idx].name.clone();
                    app.config.groups.remove(idx);

                    if let Err(e) = app.save_config() {
                        app.set_status(format!("Error saving config: {e}"));
                    } else {
                        app.set_status(format!("Group '{name}' deleted"));
                    }
                } else {
                    let name = app.config.bookmarks[idx].name.clone();
                    app.config.bookmarks.remove(idx);

                    if let Err(e) = app.save_config() {
                        app.set_status(format!("Error saving config: {e}"));
                    } else {
                        app.set_status(format!("Bookmark '{name}' deleted"));
                    }

                    // Clean up keychain entry — surface errors so user knows
                    if let Err(e) = keychain::delete_password(&name) {
                        app.set_status(format!(
                            "Bookmark deleted, but failed to remove keychain entry: {e}"
                        ));
                    }
                }

                app.confirm_state = None;
                app.screen = Screen::List;
                app.refilter();
            }
        }
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.help_source = Some(app.screen.clone());
            app.screen = Screen::Help;
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
        // Help (before Char catch-all — intercepts ? from search input)
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.help_source = Some(Screen::List);
            app.screen = Screen::Help;
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
            app.screen = app.help_source.take().unwrap_or(Screen::List);
            app.help_scroll = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.help_scroll = app.help_scroll.saturating_add(1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.help_scroll = app.help_scroll.saturating_sub(1);
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
    use crate::config::model::{Bookmark, BookmarkGroup, Session, Settings};
    use crate::tui::views::form::{FIELD_HOST, FIELD_NAME, FIELD_PORT};

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

    fn sample_app() -> App {
        let config = AppConfig {
            settings: Settings::default(),
            profiles: Vec::new(),
            bookmarks: vec![
                sample_bookmark("prod-web-01", "production"),
                sample_bookmark("staging-api", "staging"),
                sample_bookmark("dev-worker", "development"),
                sample_bookmark("local-docker", "local"),
                sample_bookmark("test-runner", "testing"),
            ],
            groups: vec![],
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
    fn test_drain_events_limit_is_positive() {
        assert!(DRAIN_EVENTS_LIMIT > 0);
    }

    #[test]
    fn test_selected_bookmark_index() {
        let app = sample_app();
        assert!(app.selected_bookmark_index().is_some());

        let empty_app = App::new(AppConfig::default());
        assert!(empty_app.selected_bookmark_index().is_none());
    }

    #[test]
    fn test_try_save_unified_form_validation_error_goes_to_status() {
        let mut app = sample_app();
        // Set up a form with invalid data: empty name triggers validation error
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        try_save_unified_form(&mut app);

        // Validation error should go to the status bar, not form.error
        assert!(
            app.status_message.is_some(),
            "expected validation error in status_message"
        );
        let (msg, _) = app.status_message.as_ref().unwrap();
        assert!(
            msg.contains("name") || msg.contains("Name"),
            "status message should mention name validation: {msg}"
        );
        // form.error should remain None (not used for validation errors anymore)
        let form = app.form_state.as_ref().unwrap();
        assert!(
            form.error().is_none(),
            "form.error should be None; validation errors go to status bar"
        );
    }

    #[test]
    fn test_open_add_form() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;
        assert!(app.form_state.is_some());
        assert_eq!(app.screen, Screen::AddForm);
    }

    #[test]
    fn test_open_edit_form() {
        let mut app = sample_app();
        if let Some(idx) = app.selected_bookmark_index() {
            let profile_names: Vec<String> =
                app.config.profiles.iter().map(|p| p.name.clone()).collect();
            let bookmark = app.config.bookmarks[idx].clone();
            app.form_state = Some(FormState::new_edit(idx, EditTarget::Bookmark, &bookmark, &profile_names));
            app.screen = Screen::EditForm(EditTarget::Bookmark, idx);
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
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            profile: None,
        };
        app.config.bookmarks.push(new_bookmark);
        app.refilter();

        assert_eq!(app.config.bookmarks.len(), initial_count + 1);
        assert!(app.config.bookmarks.iter().any(|b| b.name == "new-server"));
    }

    #[test]
    fn test_help_source_set_from_list() {
        let mut app = sample_app();
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        handle_key_event(&mut app, key);
        assert_eq!(app.screen, Screen::Help);
        assert_eq!(app.help_source, Some(Screen::List));
    }

    #[test]
    fn test_help_close_returns_to_origin_screen() {
        let mut app = sample_app();
        // Open form, then open help from form
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        handle_key_event(&mut app, key);
        assert_eq!(app.screen, Screen::Help);
        assert_eq!(app.help_source, Some(Screen::AddForm));

        // Close help — should return to AddForm, not List
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc);
        assert_eq!(app.screen, Screen::AddForm);
    }

    #[test]
    fn test_help_close_returns_to_delete_confirm() {
        let mut app = sample_app();
        let bookmark = &app.config.bookmarks[0];
        app.confirm_state = Some(ConfirmState::new(bookmark));
        app.screen = Screen::DeleteConfirm(0);

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        handle_key_event(&mut app, key);
        assert_eq!(app.screen, Screen::Help);
        assert_eq!(app.help_source, Some(Screen::DeleteConfirm(0)));

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc);
        assert_eq!(app.screen, Screen::DeleteConfirm(0));
    }

    #[test]
    fn test_help_scroll_resets_on_context_switch() {
        let mut app = sample_app();
        // Open help from list
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        handle_key_event(&mut app, key);
        assert_eq!(app.help_scroll, 0);

        // Scroll down
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key_event(&mut app, down);
        handle_key_event(&mut app, down);
        assert!(app.help_scroll > 0);

        // Close help
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc);

        // Open help from form — scroll should reset to 0
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;
        handle_key_event(&mut app, key);
        assert_eq!(
            app.help_scroll, 0,
            "scroll should reset when opening help from a different screen"
        );
    }

    #[test]
    fn test_help_from_search_mode() {
        let mut app = sample_app();
        app.search_active = true;
        app.screen = Screen::List;

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        handle_key_event(&mut app, key);
        assert_eq!(app.screen, Screen::Help);
        assert_eq!(app.help_source, Some(Screen::List));
        // search_active should still be true so help content shows search context
        assert!(app.search_active);

        // Close help — return to list with search still active
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc);
        assert_eq!(app.screen, Screen::List);
        assert!(app.search_active);
    }

    #[test]
    fn test_delete_confirm_invalid_index_help_no_panic() {
        let mut app = sample_app();
        // Set up confirm state for a stale/invalid index
        app.screen = Screen::DeleteConfirm(9999);
        app.confirm_state = None; // Stale state — confirm_state gone

        // This should not panic when help is rendered; help_source is set
        // and render_help uses confirm_state.is_production which defaults to false.
        // We can only test the state transitions here (rendering needs a terminal).
        app.help_scroll = 0;
        app.help_source = Some(Screen::DeleteConfirm(9999));
        app.screen = Screen::Help;

        // Close help — returns to delete confirm screen
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key_event(&mut app, esc);
        assert_eq!(app.screen, Screen::DeleteConfirm(9999));
    }

    // --- Session navigation tests (Task 003) ---

    fn sample_group() -> BookmarkGroup {
        BookmarkGroup {
            name: "prod-servers".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "production".into(),
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
            sessions: vec![
                Session {
                    name: "project-a".into(),
                    ..Session::default()
                },
                Session {
                    name: "project-b".into(),
                    ..Session::default()
                },
                Session {
                    name: "project-c".into(),
                    ..Session::default()
                },
            ],
        }
    }

    fn sample_group2() -> BookmarkGroup {
        BookmarkGroup {
            name: "staging-servers".into(),
            host: "10.0.2.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "staging".into(),
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
            sessions: vec![
                Session {
                    name: "frontend".into(),
                    ..Session::default()
                },
                Session {
                    name: "backend".into(),
                    ..Session::default()
                },
            ],
        }
    }

    fn app_with_groups(groups: Vec<BookmarkGroup>) -> App {
        let config = AppConfig {
            settings: Settings::default(),
            profiles: vec![],
            bookmarks: vec![],
            groups,
        };
        App::new(config)
    }

    #[test]
    fn test_app_selects_first_session_on_init() {
        let app = app_with_groups(vec![sample_group()]);
        assert_eq!(app.selected_session, Some((0, 0)));
    }

    #[test]
    fn test_app_no_selected_session_when_no_groups() {
        let app = sample_app();
        assert!(app.selected_session.is_none());
    }

    #[test]
    fn test_move_session_selection_down() {
        let mut app = app_with_groups(vec![sample_group()]);
        // Start at (0, 0)
        assert_eq!(app.selected_session, Some((0, 0)));

        // Move down
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key_event(&mut app, down);
        assert_eq!(app.selected_session, Some((0, 1)));

        // Move down again
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key_event(&mut app, down);
        assert_eq!(app.selected_session, Some((0, 2)));

        // At the end — should stay
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key_event(&mut app, down);
        assert_eq!(app.selected_session, Some((0, 2)));
    }

    #[test]
    fn test_move_session_selection_up() {
        let mut app = app_with_groups(vec![sample_group()]);
        // Start at (0, 0) — already at top
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        handle_key_event(&mut app, up);
        assert_eq!(app.selected_session, Some((0, 0)));

        // Move to (0, 2) first
        app.selected_session = Some((0, 2));
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        handle_key_event(&mut app, up);
        assert_eq!(app.selected_session, Some((0, 1)));

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        handle_key_event(&mut app, up);
        assert_eq!(app.selected_session, Some((0, 0)));
    }

    #[test]
    fn test_move_session_across_groups() {
        let mut app = app_with_groups(vec![sample_group(), sample_group2()]);
        // Start at (0, 0) — first session in first group
        assert_eq!(app.selected_session, Some((0, 0)));

        // Navigate down: (0,0) -> (0,1) -> (0,2) -> (1,0)
        for _ in 0..3 {
            let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
            handle_key_event(&mut app, down);
        }
        assert_eq!(app.selected_session, Some((1, 0)));

        // One more down: (1,0) -> (1,1)
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key_event(&mut app, down);
        assert_eq!(app.selected_session, Some((1, 1)));
    }

    #[test]
    fn test_toggle_group_collapse() {
        let mut app = app_with_groups(vec![sample_group()]);
        assert_eq!(app.selected_session, Some((0, 0)));
        assert!(app.collapsed_groups.is_empty());

        // Press space to toggle collapse
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        handle_key_event(&mut app, space);
        assert!(app.collapsed_groups.contains(&0));

        // Press space again to expand
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        handle_key_event(&mut app, space);
        assert!(!app.collapsed_groups.contains(&0));
    }

    #[test]
    fn test_navigation_skips_collapsed_group_sessions() {
        let mut app = app_with_groups(vec![sample_group(), sample_group2()]);
        // Collapse group 0
        app.collapsed_groups.insert(0);
        // Select first session of group 0 (even though it's collapsed)
        app.selected_session = Some((0, 0));

        // Move down — (0,0) is not visible, so current_pos falls to 0
        // which is (1,0). Then down moves to (1,1).
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key_event(&mut app, down);
        // Since (0,0) is not in visible list, position defaults to 0 = (1,0)
        // Then +1 delta goes to (1,1)
        assert_eq!(app.selected_session, Some((1, 1)));
    }

    #[test]
    fn test_enter_triggers_connect_request() {
        let mut app = app_with_groups(vec![sample_group()]);
        assert_eq!(app.selected_session, Some((0, 0)));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key_event(&mut app, enter);
        // connect_request is encoded as group_idx * 10000 + session_idx
        assert_eq!(app.connect_request, Some(0));
    }

    #[test]
    fn test_home_moves_to_first_session() {
        let mut app = app_with_groups(vec![sample_group(), sample_group2()]);
        // Move to last session
        app.selected_session = Some((1, 1));

        let g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        handle_key_event(&mut app, g);
        assert_eq!(app.selected_session, Some((0, 0)));
    }

    #[test]
    fn test_end_moves_to_last_session() {
        let mut app = app_with_groups(vec![sample_group(), sample_group2()]);
        app.selected_session = Some((0, 0));

        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        handle_key_event(&mut app, end);
        assert_eq!(app.selected_session, Some((1, 1)));
    }

    #[test]
    fn test_visible_sessions_excludes_collapsed() {
        let mut app = app_with_groups(vec![sample_group(), sample_group2()]);
        // 3 sessions in group 0 + 2 in group 1 = 5 total
        let visible = visible_sessions(&app);
        assert_eq!(visible.len(), 5);

        // Collapse group 0
        app.collapsed_groups.insert(0);
        let visible = visible_sessions(&app);
        assert_eq!(visible.len(), 2); // Only group 1's sessions
    }

    #[test]
    fn test_empty_groups_no_crash() {
        let app = app_with_groups(vec![]);
        assert!(app.selected_session.is_none());
        // Should not crash
    }

    // ─── Group form keyboard tests ───

    #[test]
    fn test_open_group_add_form() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;
        assert!(app.form_state.is_some());
        assert_eq!(app.screen, Screen::AddForm);
    }

    #[test]
    fn test_group_form_ctrl_enter_adds_session() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        // Simulate Ctrl+O
        let key = KeyEvent {
            code: KeyCode::Char('o'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        handle_unified_form_key(&mut app, key);

        if let Some(FormState::Add(f)) = &app.form_state {
            assert_eq!(f.sessions.len(), 2);
        } else {
            panic!("Expected GroupAdd form state");
        }
    }

    #[test]
    fn test_group_form_minus_removes_session() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        // Add an extra session first
        if let Some(ref mut form) = app.form_state {
            form.add_session_line();
        }

        // Simulate - key
        let key = KeyEvent {
            code: KeyCode::Char('-'),
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        handle_unified_form_key(&mut app, key);

        if let Some(FormState::Add(f)) = &app.form_state {
            // Started with 1, added 1 (cursor at index 1), removed current (index 1) = 1 left
            assert_eq!(f.sessions.len(), 1);
        } else {
            panic!("Expected GroupAdd form state");
        }
    }

    #[test]
    fn test_group_form_minus_removes_last_session_reverts_to_bookmark() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        // Simulate - key on form with only 1 session
        let key = KeyEvent {
            code: KeyCode::Char('-'),
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        handle_unified_form_key(&mut app, key);

        if let Some(FormState::Add(f)) = &app.form_state {
            // Last session removed, reverts to bookmark mode (0 sessions)
            assert_eq!(f.sessions.len(), 0);
            assert!(f.sessions_collapsed);
        } else {
            panic!("Expected Add form state");
        }
    }

    #[test]
    fn test_group_form_esc_returns_to_list() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        let key = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        handle_unified_form_key(&mut app, key);

        assert!(app.form_state.is_none());
        assert_eq!(app.screen, Screen::List);
    }

    // ─── Integration tests ───

    #[test]
    fn test_group_form_full_workflow_add_with_sessions() {
        let mut app = sample_app();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_group_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        // Fill in the form fields
        if let Some(FormState::Add(f)) = &mut app.form_state {
            f.fields[FIELD_NAME] = "test-group".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.fields[FIELD_PORT] = "2222".into();
            f.sessions[0].name = "session-a".into();
            f.sessions[0].on_connect = Some("tail -f /var/log/app.log".into());
            // Add second session
            f.add_session_line();
            f.sessions[1].name = "session-b".into();
            f.sessions[1].on_connect = Some("htop".into());
        }

        // Save the form
        try_save_unified_form(&mut app);

        // Verify the group was added
        assert_eq!(app.screen, Screen::List);
        assert!(app.form_state.is_none());
        assert_eq!(app.config.groups.len(), 1);
        let group = &app.config.groups[0];
        assert_eq!(group.name, "test-group");
        assert_eq!(group.host, "10.0.1.5");
        assert_eq!(group.port, 2222);
        assert_eq!(group.sessions.len(), 2);
        assert_eq!(group.sessions[0].name, "session-a");
        assert_eq!(group.sessions[0].on_connect, Some("tail -f /var/log/app.log".into()));
        assert_eq!(group.sessions[1].name, "session-b");
        assert_eq!(group.sessions[1].on_connect, Some("htop".into()));
    }

    #[test]
    fn test_group_form_edit_workflow() {
        let mut app = sample_app();
        // Add a group first
        app.config.groups.push(BookmarkGroup {
            name: "edit-me".into(),
            host: "10.0.1.5".into(),
            port: 22,
            sessions: vec![Session {
                name: "original".into(),
                on_connect: Some("echo hello".into()),
                ..Session::default()
            }],
            ..BookmarkGroup::default()
        });

        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        let group = app.config.groups[0].clone();
        app.form_state = Some(FormState::new_group_edit(0, &group, &profile_names));
        app.screen = Screen::EditForm(EditTarget::Group, 0);

        // Modify the session
        if let Some(FormState::Edit(_, EditTarget::Group, f)) = &mut app.form_state {
            f.sessions[0].on_connect = Some("echo world".into());
        }

        // Verify the form state was mutated
        if let Some(FormState::Edit(_, EditTarget::Group, f)) = &app.form_state {
            assert_eq!(f.sessions[0].on_connect, Some("echo world".into()));
        }

        // Build the group from form and verify
        if let Some(ref mut form) = app.form_state {
            let built = form.validate_and_build_group(&app.config).unwrap();
            assert_eq!(built.sessions[0].on_connect, Some("echo world".into()));
        }
    }

    #[test]
    fn test_group_form_delete_workflow() {
        let mut app = sample_app();
        // Add a group first
        app.config.groups.push(BookmarkGroup {
            name: "delete-me".into(),
            host: "10.0.1.5".into(),
            ..BookmarkGroup::default()
        });

        // Set up delete confirmation
        let group = &app.config.groups[0];
        app.confirm_state = Some(ConfirmState::new_group(group));
        app.screen = Screen::DeleteConfirm(0);

        // Confirm deletion (non-production, so Enter confirms immediately)
        let key = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        handle_confirm_key(&mut app, key);

        // Verify the group was removed
        assert!(app.config.groups.is_empty());
        assert_eq!(app.screen, Screen::List);
        assert!(app.confirm_state.is_none());
    }

    #[test]
    fn test_bookmark_form_unchanged() {
        let mut app = sample_app();
        let initial_count = app.config.bookmarks.len();
        let profile_names: Vec<String> =
            app.config.profiles.iter().map(|p| p.name.clone()).collect();
        app.form_state = Some(FormState::new_add(&app.config.settings, &profile_names));
        app.screen = Screen::AddForm;

        // Fill in the form fields
        if let Some(FormState::Add(f)) = &mut app.form_state {
            f.fields[FIELD_NAME] = "new-bookmark".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
        }

        // Save
        try_save_unified_form(&mut app);

        // Verify the bookmark was added
        assert_eq!(app.config.bookmarks.len(), initial_count + 1);
        let bookmark = app.config.bookmarks.iter().find(|b| b.name == "new-bookmark");
        assert!(bookmark.is_some());
        assert_eq!(bookmark.unwrap().host, "10.0.1.5");
    }
}
