pub mod client;
pub mod known_hosts;
pub mod password;
pub mod snippet;
pub mod terminal_theme;
pub mod tunnel;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use russh::ChannelMsg;
use russh::client::AuthResult;
use russh::keys::PrivateKeyWithHashAlg;
use tokio::io::AsyncReadExt;
use zeroize::Zeroizing;

use crate::config;
use crate::config::model::{AppConfig, Bookmark};
use crate::keychain;

use self::client::{HostKeyCheckMode, SshoreHandler};
use self::password::PasswordDetector;

/// Result of executing a single command on a remote host.
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u32,
}

/// Default SSH key filenames to try, in priority order.
const DEFAULT_KEY_NAMES: &[&str] = &["id_ed25519", "id_rsa", "id_ecdsa"];

/// Default SSH connection timeout in seconds.
/// Can be overridden per-settings or per-bookmark.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Print a one-time high-visibility production banner for interactive operations.
pub fn print_production_banner(
    bookmark: &Bookmark,
    settings: &crate::config::model::Settings,
    context: &str,
) {
    if bookmark.env.eq_ignore_ascii_case("production") {
        let user: String = bookmark
            .effective_user(settings)
            .chars()
            .filter(|c| !c.is_ascii_control())
            .collect();
        let host: String = bookmark
            .host
            .chars()
            .filter(|c| !c.is_ascii_control())
            .collect();
        eprintln!(
            "\x1b[1;37;41m PROD \x1b[0m {}: {}@{}:{}",
            context, user, host, bookmark.port
        );
    }
}

/// Resolve the effective connection timeout for a bookmark.
/// Priority: bookmark.connect_timeout_secs → settings.connect_timeout_secs → default (15s).
fn effective_timeout(bookmark: &Bookmark, settings: &crate::config::model::Settings) -> u64 {
    bookmark
        .connect_timeout_secs
        .or(settings.connect_timeout_secs)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS)
}

/// Terminal cleanup guard — restores terminal state on drop.
/// Tracks whether raw mode was enabled by us, so we only disable
/// what we enabled.
struct TerminalGuard {
    was_raw: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Flush stdout before disabling raw mode
        let _ = std::io::stdout().flush();

        // Only disable raw mode if WE enabled it (it wasn't already on)
        if !self.was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }

        // Reset terminal theming
        terminal_theme::reset_theme();

        // Ensure cursor is visible and colors are reset
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::Show,
            crossterm::style::ResetColor,
        );

        // Final newline so the shell prompt starts clean
        let _ = writeln!(std::io::stdout());
        let _ = std::io::stdout().flush();
    }
}

/// Information about the current SSH session, used for save-as-bookmark.
pub struct SessionInfo {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_file: Option<String>,
    pub proxy_jump: Option<String>,
    /// Whether this is an existing bookmark or an ad-hoc connection.
    pub bookmark_name: Option<String>,
}

/// Parse a connection string: `[user@]host[:port]`
///
/// Returns (user, host, port) where user defaults to None and port defaults to 22.
pub fn parse_connection_string(target: &str) -> Result<(Option<String>, String, u16)> {
    let (user_part, host_port) = if target.contains('@') {
        let parts: Vec<&str> = target.splitn(2, '@').collect();
        if parts[0].is_empty() {
            bail!("Empty username in connection string: {target}");
        }
        (Some(parts[0].to_string()), parts[1].to_string())
    } else {
        (None, target.to_string())
    };

    if host_port.is_empty() {
        bail!("Empty hostname in connection string: {target}");
    }

    let (host, port) = if host_port.contains(':') {
        let parts: Vec<&str> = host_port.rsplitn(2, ':').collect();
        let port = parts[0]
            .parse::<u16>()
            .with_context(|| format!("Invalid port in connection string: {}", parts[0]))?;
        (parts[1].to_string(), port)
    } else {
        (host_port, 22u16)
    };

    Ok((user_part, host, port))
}

/// Infer a bookmark name from a hostname or IP address.
pub fn infer_bookmark_name(host: &str) -> String {
    if host.contains('.') && host.chars().all(|c| c.is_ascii_digit() || c == '.') {
        // IP address: 10.0.1.50 → server-10-0-1-50
        format!("server-{}", host.replace('.', "-"))
    } else if host.contains('.') {
        // FQDN: web-prod-01.example.com → web-prod-01
        host.split('.').next().unwrap_or(host).to_string()
    } else {
        host.to_string()
    }
}

/// Connect to an ad-hoc target (not a bookmark) and run an interactive SSH session.
/// Creates a temporary bookmark from the parsed connection string.
pub async fn connect_adhoc(
    config: &mut AppConfig,
    user: Option<String>,
    host: String,
    port: u16,
    cfg_override: Option<&str>,
) -> Result<()> {
    let _effective_user = user
        .clone()
        .or_else(|| config.settings.default_user.clone())
        .unwrap_or_else(|| whoami::username().to_string());

    let inferred_name = infer_bookmark_name(&host);
    let env = crate::config::env::detect_env(&inferred_name, &host);

    let bookmark = Bookmark {
        name: inferred_name,
        host: host.clone(),
        user: user.clone(),
        port,
        env,
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

    // Temporarily add the bookmark for connection, then remove it
    config.bookmarks.push(bookmark);
    let index = config.bookmarks.len() - 1;

    let result = connect(config, index, cfg_override).await;

    // Remove the temporary bookmark (unless it was saved via ~b during the session)
    if index < config.bookmarks.len() {
        config.bookmarks.remove(index);
    }

    result
}

/// Establish an authenticated SSH session to a bookmark.
/// Returns the session handle for opening channels (shell, SFTP, etc.).
pub async fn establish_session(
    config: &AppConfig,
    bookmark_index: usize,
) -> Result<russh::client::Handle<SshoreHandler>> {
    let bookmark = &config.bookmarks[bookmark_index];
    let settings = &config.settings;

    let user = bookmark.effective_user(settings);
    let host = &bookmark.host;
    let port = bookmark.port;

    eprintln!("Connecting to {user}@{host}:{port}...");

    // Load SSH keys
    let keys = load_keys(bookmark)?;

    // Build handler with host key checking
    let check_mode = HostKeyCheckMode::from_str_setting(&settings.host_key_checking);
    let handler = SshoreHandler::for_host(host, port, check_mode);

    // Configurable connection timeout
    let timeout_secs = effective_timeout(bookmark, settings);

    // Connect to SSH server with timeout
    let ssh_config = russh::client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(timeout_secs)),
        ..<_>::default()
    };

    let connect_future =
        russh::client::connect(Arc::new(ssh_config), (host.as_str(), port), handler);

    let mut session =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), connect_future)
            .await
        {
            Ok(result) => result.with_context(|| format!("Failed to connect to {host}:{port}"))?,
            Err(_) => {
                bail!(
                    "Connection to {host}:{port} timed out after {timeout_secs}s. \
                 Adjust timeout with connect_timeout_secs in config.toml."
                );
            }
        };

    // Authenticate
    let authenticated = authenticate(&mut session, &user, &keys).await?;
    if !authenticated {
        bail!("Authentication failed for {user}@{host}:{port}");
    }

    Ok(session)
}

/// Establish an SSH session configured for tunnel keepalives.
/// Returns the session handle and the remote forward map for -R support.
pub async fn establish_tunnel_session(
    config: &AppConfig,
    bookmark_index: usize,
) -> Result<(
    russh::client::Handle<SshoreHandler>,
    client::RemoteForwardMap,
)> {
    let bookmark = &config.bookmarks[bookmark_index];
    let settings = &config.settings;

    let user = bookmark.effective_user(settings);
    let host = &bookmark.host;
    let port = bookmark.port;

    eprintln!("Connecting tunnel to {user}@{host}:{port}...");

    let keys = load_keys(bookmark)?;

    let check_mode = HostKeyCheckMode::from_str_setting(&settings.host_key_checking);
    let handler = SshoreHandler::for_host(host, port, check_mode);
    let remote_map = handler.remote_forwards.clone();

    let ssh_config = russh::client::Config {
        inactivity_timeout: None, // Tunnels stay open indefinitely
        keepalive_interval: Some(std::time::Duration::from_secs(
            tunnel::TUNNEL_KEEPALIVE_INTERVAL_SECS,
        )),
        keepalive_max: tunnel::TUNNEL_KEEPALIVE_MAX,
        ..<_>::default()
    };

    let timeout_secs = effective_timeout(bookmark, settings);
    let connect_future =
        russh::client::connect(Arc::new(ssh_config), (host.as_str(), port), handler);

    let mut session =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), connect_future)
            .await
        {
            Ok(result) => result.with_context(|| format!("Failed to connect to {host}:{port}"))?,
            Err(_) => {
                bail!("Tunnel connection to {host}:{port} timed out after {timeout_secs}s.");
            }
        };

    let authenticated = authenticate(&mut session, &user, &keys).await?;
    if !authenticated {
        bail!("Authentication failed for {user}@{host}:{port}");
    }

    Ok((session, remote_map))
}

/// Connect to a bookmark and run an interactive SSH session.
/// Updates last_connected/connect_count after a successful session.
pub async fn connect(
    config: &mut AppConfig,
    bookmark_index: usize,
    cfg_override: Option<&str>,
) -> Result<()> {
    let session = establish_session(config, bookmark_index).await?;

    // Apply terminal theming
    terminal_theme::apply_theme(&config.bookmarks[bookmark_index], &config.settings);
    print_production_banner(
        &config.bookmarks[bookmark_index],
        &config.settings,
        "SSH session",
    );

    // Open session channel
    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open SSH session channel")?;

    // Request PTY with current terminal size
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    channel
        .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
        .context("Failed to request PTY")?;

    // Request shell
    channel
        .request_shell(true)
        .await
        .context("Failed to request shell")?;

    // Check keychain for stored password and create detector.
    // Wrap in Zeroizing so password memory is wiped on drop.
    let bookmark_name = &config.bookmarks[bookmark_index].name;
    let stored_password: Option<Zeroizing<String>> = keychain::get_password(bookmark_name)
        .unwrap_or_else(|e| {
            eprintln!("Warning: failed to read keychain: {e}");
            None
        })
        .map(Zeroizing::new);
    let detector = PasswordDetector::new(stored_password.is_some());

    // Send on_connect command if configured
    let bookmark = &config.bookmarks[bookmark_index];
    if let Some(ref on_connect) = bookmark.on_connect {
        let delay = config.settings.on_connect_delay_ms;
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        channel
            .data(format!("{on_connect}\n").as_bytes())
            .await
            .context("Failed to send on_connect command")?;
    }

    // Collect snippet info for escape detection
    let bookmark = &config.bookmarks[bookmark_index];
    let bookmark_snippets = bookmark.snippets.clone();
    let global_snippets = config.settings.snippets.clone();
    let snippet_trigger = config.settings.snippet_trigger.clone();
    let bookmark_trigger = config.settings.bookmark_trigger.clone();

    // Build session info for save-as-bookmark
    let session_info = SessionInfo {
        host: bookmark.host.clone(),
        user: bookmark.effective_user(&config.settings),
        port: bookmark.port,
        identity_file: bookmark.identity_file.clone(),
        proxy_jump: bookmark.proxy_jump.clone(),
        bookmark_name: Some(bookmark.name.clone()),
    };

    // Run the interactive proxy loop
    run_proxy_loop(
        channel,
        detector,
        stored_password,
        bookmark_snippets,
        global_snippets,
        snippet_trigger,
        bookmark_trigger,
        session_info,
        cfg_override,
    )
    .await?;

    // Update bookmark stats
    config.bookmarks[bookmark_index].last_connected = Some(Utc::now());
    config.bookmarks[bookmark_index].connect_count += 1;
    if let Err(e) = config::save_with_override(config, cfg_override) {
        eprintln!("Warning: failed to save connection stats: {e}");
    }

    Ok(())
}

/// Execute a single command on a bookmark and return the result.
/// Does NOT allocate a PTY — runs as an exec channel.
pub async fn exec_command(
    config: &AppConfig,
    bookmark_index: usize,
    command: &str,
) -> Result<ExecResult> {
    let session = establish_session(config, bookmark_index).await?;

    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open exec channel")?;

    channel
        .exec(true, command)
        .await
        .context("Failed to execute remote command")?;

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut exit_code: Option<u32> = None;

    let (mut channel_rx, _channel_tx) = channel.split();

    loop {
        match channel_rx.wait().await {
            Some(ChannelMsg::Data { ref data }) => {
                stdout_buf.extend_from_slice(data);
                std::io::stdout().write_all(data)?;
                std::io::stdout().flush()?;
            }
            Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                stderr_buf.extend_from_slice(&data);
                std::io::stderr().write_all(&data)?;
                std::io::stderr().flush()?;
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = Some(exit_status);
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) => break,
            Some(_) => {}
            None => break,
        }
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        exit_code: exit_code.unwrap_or(1),
    })
}

/// Execute a command on multiple bookmarks concurrently.
/// Output is printed with per-host headers, interleaved as results arrive.
pub async fn exec_multi(
    config: &AppConfig,
    indices: &[usize],
    command: &str,
    concurrency: usize,
) -> Result<()> {
    use tokio::sync::Semaphore;

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut handles = Vec::new();

    for &idx in indices {
        let sem = semaphore.clone();
        let config = config.clone();
        let command = command.to_string();

        let handle = tokio::spawn(async move {
            let _permit = match sem.acquire().await {
                Ok(permit) => permit,
                Err(e) => {
                    eprintln!("\x1b[31mError: failed to acquire semaphore permit: {e}\x1b[0m");
                    return;
                }
            };

            let bookmark = &config.bookmarks[idx];
            let header = format!("\x1b[1m── {} ──\x1b[0m", bookmark.name);

            match exec_command_quiet(&config, idx, &command).await {
                Ok(result) => {
                    let mut output = format!("{header}\n{}", result.stdout);
                    if !result.stderr.is_empty() {
                        output.push_str(&format!("\x1b[31m{}\x1b[0m", result.stderr));
                    }
                    if result.exit_code != 0 {
                        output.push_str(&format!(
                            "\x1b[31m(exit code: {})\x1b[0m\n",
                            result.exit_code
                        ));
                    }
                    print!("{output}");
                }
                Err(e) => {
                    eprintln!("{header}\n\x1b[31mError: {e}\x1b[0m\n");
                }
            }
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await?;
    }

    Ok(())
}

/// Execute a single command on a bookmark without streaming to stdout.
/// Used by `exec_multi` to collect output atomically per host.
async fn exec_command_quiet(
    config: &AppConfig,
    bookmark_index: usize,
    command: &str,
) -> Result<ExecResult> {
    let session = establish_session(config, bookmark_index).await?;

    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open exec channel")?;

    channel
        .exec(true, command)
        .await
        .context("Failed to execute remote command")?;

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut exit_code: Option<u32> = None;

    let (mut channel_rx, _channel_tx) = channel.split();

    loop {
        match channel_rx.wait().await {
            Some(ChannelMsg::Data { ref data }) => {
                stdout_buf.extend_from_slice(data);
            }
            Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                stderr_buf.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = Some(exit_status);
            }
            Some(ChannelMsg::Eof | ChannelMsg::Close) => break,
            Some(_) => {}
            None => break,
        }
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        exit_code: exit_code.unwrap_or(1),
    })
}

/// Load SSH private keys for authentication.
/// If bookmark has identity_file, load that (with env var expansion). Otherwise try default keys.
fn load_keys(bookmark: &Bookmark) -> Result<Vec<PrivateKeyWithHashAlg>> {
    let mut keys = Vec::new();

    if bookmark.identity_file.is_some() {
        match bookmark.resolved_identity_file() {
            Some(Ok(path)) => {
                let expanded = PathBuf::from(&path);
                if expanded.exists() {
                    match load_key_from_path(&path) {
                        Ok(key) => keys.push(key),
                        Err(e) => {
                            eprintln!("Warning: failed to load key {path}: {e}");
                        }
                    }
                } else {
                    eprintln!(
                        "Warning: identity file not found: {} (expanded from {:?})",
                        path,
                        bookmark.identity_file.as_deref().unwrap_or("")
                    );
                }
            }
            Some(Err(e)) => {
                eprintln!("Warning: {e}");
            }
            None => {}
        }
    } else {
        // Try default key locations
        let ssh_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ssh");

        for name in DEFAULT_KEY_NAMES {
            let path = ssh_dir.join(name);
            if path.exists() {
                match load_key_from_path(&path.to_string_lossy()) {
                    Ok(key) => keys.push(key),
                    Err(_) => continue, // Silently skip keys that fail to load
                }
            }
        }
    }

    Ok(keys)
}

/// Load a single private key from a file path.
fn load_key_from_path(path: &str) -> Result<PrivateKeyWithHashAlg> {
    let key = russh::keys::load_secret_key(path, None)
        .with_context(|| format!("Failed to load SSH key: {path}"))?;
    Ok(PrivateKeyWithHashAlg::new(Arc::new(key), None))
}

/// Try to authenticate using available keys, then fall back to password prompt.
async fn authenticate(
    session: &mut russh::client::Handle<SshoreHandler>,
    user: &str,
    keys: &[PrivateKeyWithHashAlg],
) -> Result<bool> {
    // Try public key auth with each available key
    for key in keys {
        match session.authenticate_publickey(user, key.clone()).await {
            Ok(AuthResult::Success) => return Ok(true),
            Ok(AuthResult::Failure { .. }) => continue,
            Err(_) => continue,
        }
    }

    // Fall back to password auth — prompt user
    let password = prompt_password(user)?;
    match session.authenticate_password(user, password.as_str()).await {
        Ok(AuthResult::Success) => Ok(true),
        Ok(AuthResult::Failure { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Prompt the user for a password on stderr (so it doesn't interfere with SSH I/O).
fn prompt_password(user: &str) -> Result<Zeroizing<String>> {
    eprint!("{user}'s password: ");
    std::io::stderr().flush()?;

    // Read password without echo
    crossterm::terminal::enable_raw_mode()?;
    let mut password = Zeroizing::new(String::new());
    loop {
        if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
            match key.code {
                crossterm::event::KeyCode::Enter => break,
                crossterm::event::KeyCode::Char(c) => password.push(c),
                crossterm::event::KeyCode::Backspace => {
                    password.pop();
                }
                crossterm::event::KeyCode::Esc => {
                    crossterm::terminal::disable_raw_mode()?;
                    eprintln!();
                    bail!("Authentication cancelled");
                }
                _ => {}
            }
        }
    }
    crossterm::terminal::disable_raw_mode()?;
    eprintln!(); // Newline after password entry

    Ok(password)
}

/// Capacity for the stdin mpsc channel.
const STDIN_CHANNEL_SIZE: usize = 64;

/// Grace period for spawned tasks to finish before aborting.
const TASK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Set up signal handlers for graceful SSH session shutdown.
#[cfg(unix)]
async fn setup_ssh_signal_handlers() -> Result<tokio::sync::watch::Receiver<bool>> {
    use tokio::signal::unix::{SignalKind, signal};

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // SIGTERM — kill <pid>
    let tx = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(mut sig) = signal(SignalKind::terminate()) {
            sig.recv().await;
            let _ = tx.send(true);
        }
    });

    // SIGHUP — terminal closed
    let tx = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(mut sig) = signal(SignalKind::hangup()) {
            sig.recv().await;
            let _ = tx.send(true);
        }
    });

    Ok(shutdown_rx)
}

#[cfg(not(unix))]
async fn setup_ssh_signal_handlers() -> Result<tokio::sync::watch::Receiver<bool>> {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    Ok(rx)
}

/// Run the interactive terminal proxy loop.
/// Routes stdin through the main `tokio::select!` loop to enable password injection
/// without race conditions. When a password prompt is detected in SSH output,
/// the user can press Enter to inject the stored password or Esc to skip.
#[allow(clippy::too_many_arguments)]
async fn run_proxy_loop(
    channel: russh::Channel<russh::client::Msg>,
    mut detector: PasswordDetector,
    stored_password: Option<Zeroizing<String>>,
    bookmark_snippets: Vec<crate::config::model::Snippet>,
    global_snippets: Vec<crate::config::model::Snippet>,
    snippet_trigger: String,
    bookmark_trigger: String,
    session_info: SessionInfo,
    cfg_override: Option<&str>,
) -> Result<()> {
    // Put terminal in raw mode with cleanup guard
    // Capture pre-enable state first so guard is only created after successful enable
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    crossterm::terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let _guard = TerminalGuard { was_raw };

    let (mut channel_rx, channel_tx) = channel.split();

    // Create a writer for stdin forwarding (clones the internal sender)
    let mut writer = channel_tx.make_writer();

    // Stdin flows through an mpsc channel so the main loop controls forwarding
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_SIZE);

    // Spawn stdin reader — sends raw bytes to the mpsc channel
    let mut stdin_handle = tokio::spawn(read_stdin(stdin_tx));

    // Spawn resize handler (takes ownership of write half)
    let mut resize_handle = tokio::spawn(handle_resize(channel_tx));

    let mut stdout = std::io::stdout();
    let mut awaiting_confirm = false;

    // Combined escape handler for snippets and bookmark save
    use self::snippet::{SessionAction, SessionEscapeHandler};
    let mut escape_handler = SessionEscapeHandler::new(&snippet_trigger, &bookmark_trigger);
    let has_snippets = !bookmark_snippets.is_empty() || !global_snippets.is_empty();
    let has_escape_triggers = has_snippets || !bookmark_trigger.is_empty();

    // Signal handlers for graceful shutdown
    let mut shutdown_rx = setup_ssh_signal_handlers().await?;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    // Graceful shutdown — TerminalGuard will clean up on drop
                    break;
                }
            }
            msg = channel_rx.wait() => {
                match msg {
                    Some(ChannelMsg::Data { ref data }) => {
                        match stdout.write_all(data) {
                            Ok(()) => { let _ = stdout.flush(); }
                            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                                // Terminal is gone (window closed). Exit cleanly.
                                break;
                            }
                            Err(e) => {
                                eprintln!("stdout write error: {e}");
                                break;
                            }
                        }

                        // Feed data to password detector
                        if !awaiting_confirm && detector.feed(data) {
                            awaiting_confirm = true;
                            let mut stderr = std::io::stderr();
                            let _ = write!(stderr, "\r\n[sshore] Password found in keychain. Press Enter to auto-fill, Esc to skip.\r\n");
                            let _ = stderr.flush();
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        std::io::stderr().write_all(&data)?;
                        std::io::stderr().flush()?;
                    }
                    Some(ChannelMsg::ExitStatus { .. }) => break,
                    Some(ChannelMsg::Eof | ChannelMsg::Close) => break,
                    Some(_) => {}
                    None => break,
                }
            }
            Some(bytes) = stdin_rx.recv() => {
                if awaiting_confirm {
                    // Enter (0x0d) — inject the stored password
                    // Any other key — skip injection
                    // Don't forward the decision keystroke to the remote
                    if bytes.first() == Some(&0x0d)
                        && let Some(ref pw) = stored_password
                    {
                        let mut payload = Zeroizing::new(pw.as_bytes().to_vec());
                        payload.push(b'\n');
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &payload).await;
                        // payload is zeroed on drop via Zeroizing
                    }
                    awaiting_confirm = false;
                    detector.clear();
                } else if has_escape_triggers {
                    // Escape detection: feed bytes through combined handler.
                    // Batch forwarded bytes to minimize SSH channel writes.
                    let mut forward_batch = Vec::new();
                    for &byte in &bytes {
                        match escape_handler.feed(byte) {
                            SessionAction::Forward(fwd) => {
                                forward_batch.extend(fwd);
                            }
                            SessionAction::Buffer => {
                                // Flush accumulated batch before buffering
                                if !forward_batch.is_empty() {
                                    if tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await.is_err() {
                                        break;
                                    }
                                    forward_batch.clear();
                                }
                            }
                            SessionAction::ShowSnippets => {
                                // Flush batch before showing picker
                                if !forward_batch.is_empty() {
                                    let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await;
                                    forward_batch.clear();
                                }
                                // Show snippet picker, inject selected command
                                if let Ok(Some(command)) = snippet::show_snippet_picker(
                                    &mut stdout,
                                    &bookmark_snippets,
                                    &global_snippets,
                                ) {
                                    let _ = tokio::io::AsyncWriteExt::write_all(
                                        &mut writer,
                                        command.as_bytes(),
                                    )
                                    .await;
                                }
                            }
                            SessionAction::ShowSaveBookmark => {
                                // Flush batch before showing form
                                if !forward_batch.is_empty() {
                                    let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await;
                                    forward_batch.clear();
                                }
                                // Show save-as-bookmark form
                                if let Ok(Some(new_bookmark)) = snippet::show_save_bookmark_form(
                                    &mut stdout,
                                    &session_info,
                                ) {
                                    // Load, merge, save with file locking to prevent
                                    // concurrent sshore instances from losing changes
                                    match config::locked_modify(cfg_override, |app_config| {
                                        let bm_name = new_bookmark.name.clone();
                                        if let Some(idx) = app_config.bookmarks.iter().position(|b| b.name == bm_name) {
                                            app_config.bookmarks[idx] = new_bookmark;
                                            format!("\x1b[32mBookmark '{bm_name}' updated\x1b[0m\r\n")
                                        } else {
                                            app_config.bookmarks.push(new_bookmark);
                                            format!("\x1b[32mBookmark '{bm_name}' saved\x1b[0m\r\n")
                                        }
                                    }) {
                                        Ok(msg) => {
                                            let _ = write!(stdout, "{msg}");
                                        }
                                        Err(e) => {
                                            let _ = write!(stdout, "\x1b[31mError saving bookmark: {e}\x1b[0m\r\n");
                                        }
                                    }
                                    let _ = stdout.flush();
                                }
                            }
                        }
                    }
                    // Flush remaining batch
                    if !forward_batch.is_empty()
                        && tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await.is_err()
                    {
                        break;
                    }
                } else {
                    // No escape triggers configured — fast path, skip detection
                    if tokio::io::AsyncWriteExt::write_all(&mut writer, &bytes).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Clean up spawned tasks gracefully:
    // Drop the stdin receiver so read_stdin's send() fails and it exits.
    drop(stdin_rx);
    let _ = tokio::time::timeout(TASK_SHUTDOWN_TIMEOUT, &mut stdin_handle).await;
    stdin_handle.abort();
    // Resize handler has no cancellation signal (event stream); abort after grace period.
    let _ = tokio::time::timeout(TASK_SHUTDOWN_TIMEOUT, &mut resize_handle).await;
    resize_handle.abort();

    Ok(())
}

/// Read raw bytes from stdin and send to an mpsc channel.
/// Runs as a spawned task so the main loop can process stdin through `tokio::select!`.
async fn read_stdin(tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1024];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Handle terminal resize events and forward to SSH channel.
/// Uses SIGWINCH signal on Unix to avoid competing with the stdin reader
/// for stdin bytes (crossterm's EventStream reads from stdin too,
/// which causes dropped keystrokes).
#[cfg(unix)]
async fn handle_resize(channel_tx: russh::ChannelWriteHalf<russh::client::Msg>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigwinch = match signal(SignalKind::window_change()) {
        Ok(s) => s,
        Err(_) => return,
    };

    while sigwinch.recv().await.is_some() {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let _ = channel_tx
            .window_change(cols as u32, rows as u32, 0, 0)
            .await;
    }
}

/// Handle terminal resize events via polling on non-Unix platforms.
#[cfg(not(unix))]
async fn handle_resize(channel_tx: russh::ChannelWriteHalf<russh::client::Msg>) {
    let mut last_size = crossterm::terminal::size().unwrap_or((80, 24));
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok(size) = crossterm::terminal::size() {
            if size != last_size {
                last_size = size;
                let _ = channel_tx
                    .window_change(size.0 as u32, size.1 as u32, 0, 0)
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_connection_string ---

    #[test]
    fn test_parse_connection_string_full() {
        let (user, host, port) = parse_connection_string("deploy@10.0.1.50:2222").unwrap();
        assert_eq!(user, Some("deploy".to_string()));
        assert_eq!(host, "10.0.1.50");
        assert_eq!(port, 2222);
    }

    #[test]
    fn test_parse_connection_string_user_host() {
        let (user, host, port) = parse_connection_string("root@web-server.example.com").unwrap();
        assert_eq!(user, Some("root".to_string()));
        assert_eq!(host, "web-server.example.com");
        assert_eq!(port, 22);
    }

    #[test]
    fn test_parse_connection_string_host_only() {
        let (user, host, port) = parse_connection_string("10.0.1.50").unwrap();
        assert_eq!(user, None);
        assert_eq!(host, "10.0.1.50");
        assert_eq!(port, 22);
    }

    #[test]
    fn test_parse_connection_string_host_port() {
        let (user, host, port) = parse_connection_string("myserver:2222").unwrap();
        assert_eq!(user, None);
        assert_eq!(host, "myserver");
        assert_eq!(port, 2222);
    }

    #[test]
    fn test_parse_connection_string_empty_user_errors() {
        let result = parse_connection_string("@host");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_connection_string_empty_host_errors() {
        let result = parse_connection_string("user@");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_connection_string_invalid_port() {
        let result = parse_connection_string("host:notaport");
        assert!(result.is_err());
    }

    // --- infer_bookmark_name ---

    #[test]
    fn test_infer_bookmark_name_ip() {
        assert_eq!(infer_bookmark_name("10.0.1.50"), "server-10-0-1-50");
    }

    #[test]
    fn test_infer_bookmark_name_fqdn() {
        assert_eq!(
            infer_bookmark_name("web-prod-01.example.com"),
            "web-prod-01"
        );
    }

    #[test]
    fn test_infer_bookmark_name_short() {
        assert_eq!(infer_bookmark_name("myserver"), "myserver");
    }

    #[test]
    fn test_infer_bookmark_name_fqdn_with_subdomain() {
        assert_eq!(infer_bookmark_name("db.staging.internal.corp"), "db");
    }

    // --- effective_timeout ---

    fn sample_bookmark() -> Bookmark {
        Bookmark {
            name: "test".into(),
            host: "10.0.1.5".into(),
            user: None,
            port: 22,
            env: String::new(),
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
    fn test_effective_timeout_default() {
        let bookmark = sample_bookmark();
        let settings = crate::config::model::Settings::default();
        assert_eq!(
            effective_timeout(&bookmark, &settings),
            DEFAULT_CONNECT_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_effective_timeout_from_settings() {
        let bookmark = sample_bookmark();
        let settings = crate::config::model::Settings {
            connect_timeout_secs: Some(30),
            ..Default::default()
        };
        assert_eq!(effective_timeout(&bookmark, &settings), 30);
    }

    #[test]
    fn test_effective_timeout_bookmark_overrides_settings() {
        let bookmark = Bookmark {
            connect_timeout_secs: Some(5),
            ..sample_bookmark()
        };
        let settings = crate::config::model::Settings {
            connect_timeout_secs: Some(30),
            ..Default::default()
        };
        assert_eq!(effective_timeout(&bookmark, &settings), 5);
    }

    #[test]
    fn test_effective_timeout_bookmark_overrides_default() {
        let bookmark = Bookmark {
            connect_timeout_secs: Some(60),
            ..sample_bookmark()
        };
        let settings = crate::config::model::Settings::default();
        assert_eq!(effective_timeout(&bookmark, &settings), 60);
    }

    // --- parse_connection_string edge cases ---

    #[test]
    fn test_parse_connection_string_port_overflow() {
        let result = parse_connection_string("host:99999");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_connection_string_port_zero() {
        // Port 0 is technically valid in u16 parse
        let (_, _, port) = parse_connection_string("host:0").unwrap();
        assert_eq!(port, 0);
    }

    #[test]
    fn test_parse_connection_string_fqdn_with_port() {
        let (user, host, port) = parse_connection_string("admin@web.example.com:8022").unwrap();
        assert_eq!(user, Some("admin".to_string()));
        assert_eq!(host, "web.example.com");
        assert_eq!(port, 8022);
    }

    #[test]
    fn test_parse_connection_string_ipv4() {
        let (user, host, port) = parse_connection_string("root@192.168.1.1:22").unwrap();
        assert_eq!(user, Some("root".to_string()));
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 22);
    }

    // --- infer_bookmark_name edge cases ---

    #[test]
    fn test_infer_bookmark_name_single_char() {
        assert_eq!(infer_bookmark_name("a"), "a");
    }

    #[test]
    fn test_infer_bookmark_name_ip_v4_loopback() {
        assert_eq!(infer_bookmark_name("127.0.0.1"), "server-127-0-0-1");
    }
}
