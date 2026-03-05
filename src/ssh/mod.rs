pub mod client;
pub mod known_hosts;
pub mod password;
pub mod snippet;
pub mod stdin_reader;
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
use zeroize::Zeroizing;

use crate::config;
use crate::config::model::{AppConfig, Bookmark};
use crate::keychain;

use self::client::{HostKeyCheckMode, SshoreHandler};
use self::password::{PasswordDetector, PromptKind};

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

/// SSH keepalive interval for interactive/SFTP sessions (seconds).
/// Sends a keepalive packet if no data is exchanged within this period.
const KEEPALIVE_INTERVAL_SECS: u64 = 60;

/// Maximum consecutive keepalive failures before dropping the connection.
const KEEPALIVE_MAX: usize = 3;

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

/// Print available escape sequence hints at session start.
fn print_escape_hints(
    snippet_trigger: &str,
    bookmark_trigger: &str,
    browser_trigger: &str,
    has_snippets: bool,
) {
    let mut hints = Vec::new();
    if has_snippets && !snippet_trigger.is_empty() {
        hints.push(format!("{snippet_trigger} snippets"));
    }
    if !bookmark_trigger.is_empty() {
        hints.push(format!("{bookmark_trigger} bookmark"));
    }
    if !browser_trigger.is_empty() {
        hints.push(format!("{browser_trigger} file browser"));
    }
    if !hints.is_empty() {
        eprintln!("\x1b[2m[sshore] {}\x1b[0m", hints.join("  "));
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
    tracing::debug!(host, port, user, "establishing SSH session");

    // Load SSH keys
    let keys = load_keys(bookmark)?;
    tracing::debug!(key_count = keys.len(), "loaded SSH keys");

    // Build handler with host key checking
    let check_mode = HostKeyCheckMode::from_str_setting(&settings.host_key_checking);
    let handler = SshoreHandler::for_host(host, port, check_mode);

    // Configurable connection timeout
    let timeout_secs = effective_timeout(bookmark, settings);

    // Connect to SSH server with timeout.
    // No inactivity_timeout — interactive sessions must never be killed due to
    // idle time (e.g. waiting at a `su -` password prompt). Keepalives detect
    // dead connections without cutting live ones.
    let ssh_config = russh::client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS)),
        keepalive_max: KEEPALIVE_MAX,
        ..<_>::default()
    };

    tracing::debug!(
        timeout_secs,
        keepalive_interval = KEEPALIVE_INTERVAL_SECS,
        keepalive_max = KEEPALIVE_MAX,
        "connecting with timeout"
    );

    let connect_future =
        russh::client::connect(Arc::new(ssh_config), (host.as_str(), port), handler);

    let mut session =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), connect_future)
            .await
        {
            Ok(result) => {
                tracing::debug!("TCP connection established");
                result.with_context(|| format!("Failed to connect to {host}:{port}"))?
            }
            Err(_) => {
                tracing::debug!("connection timed out");
                bail!(
                    "Connection to {host}:{port} timed out after {timeout_secs}s. \
                 Adjust timeout with connect_timeout_secs in config.toml."
                );
            }
        };

    // Authenticate
    let ctx = AuthContext {
        bookmark_name: Some(&bookmark.name),
        env: Some(&bookmark.env),
        has_identity_file: bookmark.identity_file.is_some(),
    };
    let authenticated = authenticate(&mut session, &user, &keys, &ctx).await?;
    if !authenticated {
        tracing::debug!("authentication failed");
        bail!("Authentication failed for {user}@{host}:{port}");
    }

    tracing::debug!("session established and authenticated");
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

    let ctx = AuthContext {
        bookmark_name: Some(&bookmark.name),
        env: Some(&bookmark.env),
        has_identity_file: bookmark.identity_file.is_some(),
    };
    let authenticated = authenticate(&mut session, &user, &keys, &ctx).await?;
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
    let session = Arc::new(establish_session(config, bookmark_index).await?);

    // Apply terminal theming
    terminal_theme::apply_theme(&config.bookmarks[bookmark_index], &config.settings);
    print_production_banner(
        &config.bookmarks[bookmark_index],
        &config.settings,
        "SSH session",
    );

    // Open session channel
    tracing::debug!("opening session channel");
    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open SSH session channel")?;

    // Request PTY with current terminal size
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    tracing::debug!(cols, rows, term = "xterm-256color", "requesting PTY");
    channel
        .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
        .context("Failed to request PTY")?;

    // Request shell
    tracing::debug!("requesting shell");
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
    let detector = PasswordDetector::new(true);

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
    let browser_trigger = config.settings.browser_trigger.clone();
    let bookmark_env = bookmark.env.clone();
    let theme_name = config.settings.theme.clone();

    // Print available escape triggers as a dim hint
    print_escape_hints(
        &snippet_trigger,
        &bookmark_trigger,
        &browser_trigger,
        !bookmark_snippets.is_empty() || !config.settings.snippets.is_empty(),
    );

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
        browser_trigger,
        session_info,
        Arc::clone(&session),
        &bookmark_env,
        &theme_name,
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
/// Prompts for passphrase when an encrypted key is encountered.
fn load_keys(bookmark: &Bookmark) -> Result<Vec<PrivateKeyWithHashAlg>> {
    let mut keys = Vec::new();

    if bookmark.identity_file.is_some() {
        match bookmark.resolved_identity_file() {
            Some(Ok(path)) => {
                let expanded = PathBuf::from(&path);
                if expanded.exists() {
                    match load_key_from_path(&path) {
                        Ok(loaded) => keys.extend(loaded),
                        Err(_) => {
                            // Key failed to load without passphrase — prompt for one
                            match load_key_with_passphrase_prompt(&path) {
                                Ok(loaded) => keys.extend(loaded),
                                Err(e) => {
                                    eprintln!("Warning: failed to load key {path}: {e}");
                                }
                            }
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
                    Ok(loaded) => keys.extend(loaded),
                    Err(_) => continue, // Silently skip default keys that fail to load
                }
            }
        }
    }

    Ok(keys)
}

/// Wrap a loaded private key into one or more `PrivateKeyWithHashAlg` entries.
/// For RSA keys, returns SHA-256 and SHA-512 variants (modern algorithms first)
/// so the auth loop tries `rsa-sha2-256` before the deprecated `ssh-rsa` (SHA-1).
/// For non-RSA keys (Ed25519, ECDSA), returns a single entry with no hash override.
fn wrap_key(key: russh::keys::PrivateKey) -> Vec<PrivateKeyWithHashAlg> {
    use russh::keys::HashAlg;

    let arc = Arc::new(key);
    if arc.algorithm().is_rsa() {
        // Try SHA-256 first (most widely accepted), then SHA-512
        vec![
            PrivateKeyWithHashAlg::new(Arc::clone(&arc), Some(HashAlg::Sha256)),
            PrivateKeyWithHashAlg::new(Arc::clone(&arc), Some(HashAlg::Sha512)),
        ]
    } else {
        vec![PrivateKeyWithHashAlg::new(arc, None)]
    }
}

/// Load a single private key from a file path (no passphrase).
fn load_key_from_path(path: &str) -> Result<Vec<PrivateKeyWithHashAlg>> {
    let key = russh::keys::load_secret_key(path, None)
        .with_context(|| format!("Failed to load SSH key: {path}"))?;
    Ok(wrap_key(key))
}

/// Prompt for a passphrase and load an encrypted private key.
fn load_key_with_passphrase_prompt(path: &str) -> Result<Vec<PrivateKeyWithHashAlg>> {
    eprintln!("Key {path} requires a passphrase.");
    eprint!("Passphrase: ");
    std::io::stderr().flush()?;

    crossterm::terminal::enable_raw_mode()?;
    let mut passphrase = Zeroizing::new(String::new());
    loop {
        if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
            match key.code {
                crossterm::event::KeyCode::Enter => break,
                crossterm::event::KeyCode::Char(c) => passphrase.push(c),
                crossterm::event::KeyCode::Backspace => {
                    passphrase.pop();
                }
                crossterm::event::KeyCode::Esc => {
                    crossterm::terminal::disable_raw_mode()?;
                    eprintln!();
                    bail!("Passphrase entry cancelled");
                }
                _ => {}
            }
        }
    }
    crossterm::terminal::disable_raw_mode()?;
    eprintln!(); // newline after hidden input

    let key = russh::keys::load_secret_key(path, Some(passphrase.as_str()))
        .with_context(|| format!("Failed to decrypt SSH key: {path}"))?;
    Ok(wrap_key(key))
}

/// Context passed to `authenticate()` for keychain-aware auth.
struct AuthContext<'a> {
    bookmark_name: Option<&'a str>,
    env: Option<&'a str>,
    /// When true, the bookmark has an explicit identity_file configured.
    /// Password auth should not be attempted as a fallback.
    has_identity_file: bool,
}

/// Try to authenticate using available keys, then keychain password, then user prompt.
async fn authenticate(
    session: &mut russh::client::Handle<SshoreHandler>,
    user: &str,
    keys: &[PrivateKeyWithHashAlg],
    ctx: &AuthContext<'_>,
) -> Result<bool> {
    // 1. Try public key auth with each available key
    for (i, key) in keys.iter().enumerate() {
        tracing::debug!(key_index = i, "trying public key auth");
        match session.authenticate_publickey(user, key.clone()).await {
            Ok(AuthResult::Success) => {
                tracing::debug!(key_index = i, "public key auth succeeded");
                return Ok(true);
            }
            Ok(AuthResult::Failure { .. }) => {
                tracing::debug!(key_index = i, "public key rejected");
                continue;
            }
            Err(e) => {
                tracing::debug!(key_index = i, error = %e, "public key auth error");
                continue;
            }
        }
    }

    // If the bookmark has an explicit identity_file AND we actually tried
    // keys, don't fall through to password auth — the user intended key-based
    // authentication. But if no keys were loaded (file missing, wrong format,
    // passphrase cancelled), still allow password fallback.
    if ctx.has_identity_file && !keys.is_empty() {
        tracing::debug!("identity_file configured and keys were tried, skipping password fallback");
        return Ok(false);
    }

    // 2. Try keychain password (if bookmark name is available)
    if let Some(name) = ctx.bookmark_name
        && let Ok(Some(stored)) = keychain::get_password(name)
    {
        tracing::debug!("trying keychain password");
        match session.authenticate_password(user, &stored).await {
            Ok(AuthResult::Success) => {
                tracing::debug!("keychain password accepted");
                return Ok(true);
            }
            Ok(AuthResult::Failure { .. }) => {
                tracing::debug!("keychain password rejected, deleting stale entry");
                let _ = keychain::delete_password(name);
                eprintln!("Stored password rejected for '{name}', removed from keychain.");
            }
            Err(e) => {
                tracing::debug!(error = %e, "keychain auth error");
                eprintln!("Warning: keychain auth error: {e}");
            }
        }
    }

    // 3. Prompt user for password
    tracing::debug!("prompting for password");
    let password = prompt_password(user)?;
    match session.authenticate_password(user, password.as_str()).await {
        Ok(AuthResult::Success) => {
            tracing::debug!("password auth succeeded");
            // Offer to save the password to keychain
            offer_save_password(ctx, &password);
            Ok(true)
        }
        Ok(AuthResult::Failure { .. }) => {
            tracing::debug!("password auth failed");
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// After successful password auth, offer to save the password to the keychain.
/// Prompts on stderr with y/N. Silent on I/O failure (e.g. daemon tunnels).
fn offer_save_password(ctx: &AuthContext<'_>, password: &str) {
    let Some(name) = ctx.bookmark_name else {
        return;
    };

    // Check if there's already a stored password — no need to re-save the same thing
    if let Ok(Some(_)) = keychain::get_password(name) {
        return;
    }

    let env_label = ctx
        .env
        .filter(|e| e.eq_ignore_ascii_case("production"))
        .map(|_| " \x1b[31m(PRODUCTION)\x1b[0m")
        .unwrap_or("");

    eprint!("Save password to keychain for '{name}'{env_label}? [y/N] ");
    if std::io::Write::flush(&mut std::io::stderr()).is_err() {
        return;
    }

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return;
    }

    if input.trim().eq_ignore_ascii_case("y") {
        match keychain::set_password(name, password) {
            Ok(()) => eprintln!("Password saved for '{name}'."),
            Err(e) => eprintln!("Warning: failed to save password: {e}"),
        }
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

/// Grace period after sending a captured sudo password before auto-saving.
/// If another password prompt appears within this window, auth failed and
/// the password is discarded. If the window elapses without a new prompt,
/// auth succeeded and the password is saved to the keychain.
const SUDO_SAVE_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

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

/// Whether the inner proxy loop should exit entirely or switch to browser mode.
enum ProxyAction {
    Exit,
    Browser,
}

/// Run the interactive terminal proxy loop.
/// Routes stdin through the main `tokio::select!` loop to enable password injection
/// without race conditions. When a password prompt is detected in SSH output,
/// the user can press Enter to inject the stored password or Esc to skip.
///
/// Supports an outer/inner loop pattern: when `~f` is typed, the inner loop
/// breaks to launch the file browser, then the outer loop respawns the stdin
/// reader and re-enters the proxy.
#[allow(clippy::too_many_arguments)]
async fn run_proxy_loop(
    channel: russh::Channel<russh::client::Msg>,
    mut detector: PasswordDetector,
    mut stored_password: Option<Zeroizing<String>>,
    bookmark_snippets: Vec<crate::config::model::Snippet>,
    global_snippets: Vec<crate::config::model::Snippet>,
    snippet_trigger: String,
    bookmark_trigger: String,
    browser_trigger: String,
    session_info: SessionInfo,
    session: Arc<russh::client::Handle<SshoreHandler>>,
    bookmark_env: &str,
    theme_name: &str,
    cfg_override: Option<&str>,
) -> Result<()> {
    tracing::debug!("entering interactive proxy loop");

    // Put terminal in raw mode with cleanup guard
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    crossterm::terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let _guard = TerminalGuard { was_raw };

    let (mut channel_rx, channel_tx) = channel.split();

    // Create a writer for stdin forwarding (clones the internal sender)
    let mut writer = channel_tx.make_writer();

    let mut stdout = std::io::stdout();
    let mut osc_stripper = terminal_theme::OscTitleStripper::new();
    let mut awaiting_confirm = false;
    let mut capturing_pw: Option<Zeroizing<String>> = None;
    // Captured sudo password waiting for auth confirmation from the remote.
    // After the password is sent, we watch remote output: if another password
    // prompt appears, auth failed and we discard; if the grace period elapses
    // without a new prompt, auth succeeded and we auto-save to keychain.
    let mut pending_save_pw: Option<Zeroizing<String>> = None;
    // Deadline for saving captured sudo password. Set to far future when inactive.
    let save_pw_deadline = tokio::time::sleep(std::time::Duration::from_secs(86400));
    tokio::pin!(save_pw_deadline);
    // Set after auto-filling from keychain; if the next prompt re-appears,
    // it means the stored password is stale and should be deleted.
    let mut autofill_pending_verify = false;

    // Combined escape handler for snippets, bookmark save, and browser
    use self::snippet::{SessionAction, SessionEscapeHandler};
    let mut escape_handler =
        SessionEscapeHandler::new(&snippet_trigger, &bookmark_trigger, &browser_trigger);
    let has_snippets = !bookmark_snippets.is_empty() || !global_snippets.is_empty();
    let has_escape_triggers =
        has_snippets || !bookmark_trigger.is_empty() || !browser_trigger.is_empty();

    // Signal handlers for graceful shutdown
    let mut shutdown_rx = setup_ssh_signal_handlers().await?;

    // SIGWINCH listener — inlined so channel_tx stays available after browser exits
    #[cfg(unix)]
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();

    // Bookmark name for browser display
    let browser_name = session_info
        .bookmark_name
        .clone()
        .unwrap_or_else(|| session_info.host.clone());

    loop {
        // Spawn stdin reader for this iteration
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let mut stdin_reader = stdin_reader::StdinReader::spawn(stdin_tx);

        let action = 'proxy: loop {
            tokio::select! {
                // Timer to save captured sudo password after grace period
                () = &mut save_pw_deadline, if pending_save_pw.is_some() => {
                    if let Some(pw) = pending_save_pw.take() {
                        if let Some(ref name) = session_info.bookmark_name {
                            match keychain::set_password(name, &pw) {
                                Ok(()) => {
                                    tracing::debug!("password auto-saved to keychain (timer)");
                                    let mut stderr = std::io::stderr();
                                    let _ = write!(stderr, "\r\n[sshore] Password saved to keychain for '{name}'.\r\n");
                                    let _ = stderr.flush();
                                    stored_password = Some(pw);
                                }
                                Err(e) => {
                                    let mut stderr = std::io::stderr();
                                    let _ = write!(stderr, "\r\n[sshore] Failed to save password to keychain: {e}\r\n");
                                    let _ = stderr.flush();
                                }
                            }
                        }
                        detector.clear();
                    }
                    // Reset deadline to far future
                    save_pw_deadline.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(86400));
                }

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        tracing::debug!("received shutdown signal");
                        break 'proxy ProxyAction::Exit;
                    }
                }

                // Inline SIGWINCH handling (Unix only)
                _ = async {
                    #[cfg(unix)]
                    {
                        if let Some(ref mut sw) = sigwinch {
                            sw.recv().await
                        } else {
                            std::future::pending::<Option<()>>().await
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        std::future::pending::<Option<()>>().await
                    }
                } => {
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let _ = channel_tx.window_change(cols as u32, rows as u32, 0, 0).await;
                }

                msg = channel_rx.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { ref data }) => {
                            // Strip remote shell title sequences to preserve sshore's tab title
                            let filtered = osc_stripper.strip(data);
                            match stdout.write_all(&filtered) {
                                Ok(()) => { let _ = stdout.flush(); }
                                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                                    tracing::debug!("stdout broken pipe");
                                    break 'proxy ProxyAction::Exit;
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "stdout write error");
                                    eprintln!("stdout write error: {e}");
                                    break 'proxy ProxyAction::Exit;
                                }
                            }

                            // Feed data to password detector
                            let sudo_active = awaiting_confirm
                                || capturing_pw.is_some();
                            if !sudo_active {
                                if pending_save_pw.is_some() {
                                    // Check if auth failed (another prompt appeared)
                                    let prompt = detector.feed(data);
                                    if prompt == PromptKind::Sudo {
                                        tracing::debug!("auth failed — another sudo prompt, discarding captured password");
                                        pending_save_pw = None;
                                        // Reset timer
                                        save_pw_deadline.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(86400));
                                        capturing_pw = Some(Zeroizing::new(String::new()));
                                        let mut stderr = std::io::stderr();
                                        let _ = write!(stderr, "\r\n[sshore] Wrong password. Type it again:\r\n");
                                        let _ = stderr.flush();
                                        detector.clear();
                                    }
                                    // Timer handles the save after grace period
                                } else {
                                    let prompt = detector.feed(data);
                                    if prompt.detected() {
                                        tracing::debug!(?prompt, "password prompt detected");
                                        if autofill_pending_verify && stored_password.is_some() {
                                            // Stored password was just auto-filled but rejected —
                                            // delete the stale keychain entry and fall through to capture.
                                            autofill_pending_verify = false;
                                            tracing::debug!("stored password rejected by remote, deleting stale keychain entry");
                                            if let Some(ref name) = session_info.bookmark_name {
                                                let _ = keychain::delete_password(name);
                                            }
                                            stored_password = None;
                                            let mut stderr = std::io::stderr();
                                            let _ = write!(stderr, "\r\n[sshore] Stored password was rejected. Removed from keychain.\r\n");
                                            let _ = stderr.flush();
                                            if prompt == PromptKind::Sudo
                                                && session_info.bookmark_name.is_some()
                                            {
                                                capturing_pw =
                                                    Some(Zeroizing::new(String::new()));
                                            }
                                            detector.clear();
                                        } else if stored_password.is_some() {
                                            // Auto-fill works for any prompt type
                                            awaiting_confirm = true;
                                            let mut stderr = std::io::stderr();
                                            let _ = write!(stderr, "\r\n[sshore] Password found in keychain. Press Enter to auto-fill, Esc to skip.\r\n");
                                            let _ = stderr.flush();
                                        } else if prompt == PromptKind::Sudo
                                            && session_info.bookmark_name.is_some()
                                        {
                                            // Only capture-and-save for explicit sudo prompts;
                                            // generic "Password:" could be su, mysql, etc.
                                            tracing::debug!("no stored password — capturing sudo password");
                                            capturing_pw =
                                                Some(Zeroizing::new(String::new()));
                                        }
                                    } else {
                                        // Non-prompt output after auto-fill means it worked
                                        autofill_pending_verify = false;
                                    }
                                }
                            }
                        }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            std::io::stderr().write_all(&data)?;
                            std::io::stderr().flush()?;
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            tracing::debug!(exit_status, "remote exited");
                            break 'proxy ProxyAction::Exit;
                        }
                        Some(ChannelMsg::Eof) => {
                            tracing::debug!("channel EOF");
                            break 'proxy ProxyAction::Exit;
                        }
                        Some(ChannelMsg::Close) => {
                            tracing::debug!("channel closed by server");
                            break 'proxy ProxyAction::Exit;
                        }
                        Some(_) => {}
                        None => {
                            tracing::debug!("channel stream ended (connection lost)");
                            break 'proxy ProxyAction::Exit;
                        }
                    }
                }

                Some(bytes) = stdin_rx.recv() => {
                    if awaiting_confirm {
                        if bytes.first() == Some(&0x0d)
                            && let Some(ref pw) = stored_password
                        {
                            let mut payload = Zeroizing::new(pw.as_bytes().to_vec());
                            payload.push(b'\n');
                            let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &payload).await;
                            autofill_pending_verify = true;
                        } else {
                            // Esc or other key — user skipped auto-fill
                            autofill_pending_verify = false;
                        }
                        // Erase the [sshore] hint line: move up, clear to end of screen
                        let mut stderr = std::io::stderr();
                        let _ = write!(stderr, "\x1b[A\x1b[2K\r");
                        let _ = stderr.flush();
                        awaiting_confirm = false;
                        detector.clear();
                    } else if capturing_pw.is_some() {
                        // Capture typed password while forwarding to remote
                        let buffer = capturing_pw.as_mut().unwrap();
                        let mut enter_pressed = false;
                        for &b in &bytes {
                            match b {
                                0x0d | 0x0a => {
                                    enter_pressed = true;
                                    break;
                                }
                                0x7f | 0x08 => {
                                    buffer.pop();
                                }
                                b if b >= 0x20 => {
                                    buffer.push(b as char);
                                }
                                _ => {}
                            }
                        }
                        // Forward all bytes to remote
                        if tokio::io::AsyncWriteExt::write_all(&mut writer, &bytes)
                            .await
                            .is_err()
                        {
                            break 'proxy ProxyAction::Exit;
                        }
                        if enter_pressed {
                            let pw = capturing_pw.take().unwrap();
                            if !pw.is_empty()
                                && session_info.bookmark_name.is_some()
                            {
                                tracing::debug!("password captured, waiting to confirm auth");
                                pending_save_pw = Some(pw);
                                // Arm the save timer
                                save_pw_deadline.as_mut().reset(tokio::time::Instant::now() + SUDO_SAVE_GRACE);
                            }
                            detector.clear();
                        }
                    } else if pending_save_pw.is_some() {
                        // User is typing into the session after sudo — auth succeeded.
                        // Let the timer handle the save; just forward input.
                        if tokio::io::AsyncWriteExt::write_all(&mut writer, &bytes)
                            .await
                            .is_err()
                        {
                            break 'proxy ProxyAction::Exit;
                        }
                    } else if has_escape_triggers {
                        let mut forward_batch = Vec::new();
                        let mut should_break = false;
                        for &byte in &bytes {
                            match escape_handler.feed(byte) {
                                SessionAction::Forward(fwd) => {
                                    forward_batch.extend(fwd);
                                }
                                SessionAction::Buffer => {
                                    if !forward_batch.is_empty() {
                                        if tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await.is_err() {
                                            should_break = true;
                                            break;
                                        }
                                        forward_batch.clear();
                                    }
                                }
                                SessionAction::ShowSnippets => {
                                    if !forward_batch.is_empty() {
                                        let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await;
                                        forward_batch.clear();
                                    }
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
                                    if !forward_batch.is_empty() {
                                        let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await;
                                        forward_batch.clear();
                                    }
                                    if let Ok(Some(new_bookmark)) = snippet::show_save_bookmark_form(
                                        &mut stdout,
                                        &session_info,
                                    ) {
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
                                SessionAction::ShowBrowser => {
                                    if !forward_batch.is_empty() {
                                        let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await;
                                        forward_batch.clear();
                                    }
                                    break 'proxy ProxyAction::Browser;
                                }
                            }
                        }
                        if should_break {
                            break 'proxy ProxyAction::Exit;
                        }
                        if !forward_batch.is_empty()
                            && tokio::io::AsyncWriteExt::write_all(&mut writer, &forward_batch).await.is_err()
                        {
                            break 'proxy ProxyAction::Exit;
                        }
                    } else if tokio::io::AsyncWriteExt::write_all(&mut writer, &bytes).await.is_err() {
                        break 'proxy ProxyAction::Exit;
                    }
                }
            }
        };

        // Clean up stdin reader for this iteration — guaranteed thread exit
        drop(stdin_rx);
        stdin_reader.stop();

        // Save pending password on session end or browser switch
        if let Some(pw) = pending_save_pw.take()
            && let Some(ref name) = session_info.bookmark_name
        {
            match keychain::set_password(name, &pw) {
                Ok(()) => {
                    tracing::debug!("password auto-saved to keychain (on session end)");
                    let mut stderr = std::io::stderr();
                    let _ = write!(
                        stderr,
                        "\r\n[sshore] Password saved to keychain for '{name}'.\r\n"
                    );
                    let _ = stderr.flush();
                    stored_password = Some(pw);
                }
                Err(e) => {
                    let mut stderr = std::io::stderr();
                    let _ = write!(stderr, "\r\n[sshore] Failed to save password: {e}\r\n");
                    let _ = stderr.flush();
                }
            }
        }

        match action {
            ProxyAction::Exit => break,
            ProxyAction::Browser => {
                // Launch in-session SFTP file browser
                match launch_browser(
                    Arc::clone(&session),
                    &browser_name,
                    bookmark_env,
                    theme_name,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::error!("browser launch failed: {e:#}");
                        let _ =
                            write!(stdout, "\r\n\x1b[31m[sshore] Browser error: {e}\x1b[0m\r\n");
                        let _ = stdout.flush();
                    }
                }

                // Re-apply SSH session theming (BrowserGuard resets it on drop)
                if let Ok(cfg) = config::load_with_override(cfg_override) {
                    let bm_name = session_info.bookmark_name.as_deref();
                    if let Some(bm) = cfg
                        .bookmarks
                        .iter()
                        .find(|b| Some(b.name.as_str()) == bm_name)
                    {
                        terminal_theme::reapply_theme(bm, &cfg.settings);
                    }
                }

                // Re-enable raw mode (BrowserGuard disables it on drop)
                crossterm::terminal::enable_raw_mode()
                    .context("Failed to re-enable raw mode after browser")?;

                // Sync terminal size with remote PTY
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let _ = channel_tx
                    .window_change(cols as u32, rows as u32, 0, 0)
                    .await;

                // Outer loop continues — new stdin reader spawned at top
            }
        }
    }

    Ok(())
}

/// Launch the dual-pane file browser using the existing SSH session.
/// Detects the remote shell's current directory via `pwd` so the browser
/// starts where the user left off.
async fn launch_browser(
    session: Arc<russh::client::Handle<SshoreHandler>>,
    name: &str,
    env: &str,
    theme_name: &str,
) -> Result<()> {
    use crate::storage::{Backend, local_backend::LocalBackend, sftp_backend::SftpBackend};
    use crate::tui::theme::resolve_theme;
    use crate::tui::views::browser;

    let theme = resolve_theme(theme_name);
    let remote_cwd = detect_remote_cwd(&session).await;

    let mut sftp_backend = SftpBackend::from_handle(&session, name).await?;
    sftp_backend.set_ssh_handle(Arc::clone(&session));
    if let Some(ref cwd) = remote_cwd
        && let Err(e) = sftp_backend.cd(cwd).await
    {
        eprintln!("Warning: could not cd to {cwd}: {e}");
    }

    let local_backend = LocalBackend::new(".")?;

    let mut left = Backend::Sftp(sftp_backend);
    let mut right = Backend::Local(local_backend);

    browser::run(&mut left, &mut right, name, env, false, &theme).await?;

    Ok(())
}

/// Run `pwd` over a separate exec channel to detect the remote shell's
/// current working directory. Returns None on any failure (non-fatal).
async fn detect_remote_cwd(session: &russh::client::Handle<SshoreHandler>) -> Option<String> {
    let channel = session.channel_open_session().await.ok()?;
    channel.exec(true, "pwd").await.ok()?;

    let (mut rx, _tx) = channel.split();
    let mut output = Vec::new();

    loop {
        match rx.wait().await {
            Some(ChannelMsg::Data { ref data }) => output.extend_from_slice(data),
            Some(ChannelMsg::Eof | ChannelMsg::Close | ChannelMsg::ExitStatus { .. }) => break,
            Some(_) => {}
            None => break,
        }
    }

    let path = String::from_utf8_lossy(&output).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
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
