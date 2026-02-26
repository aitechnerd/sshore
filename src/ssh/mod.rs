pub mod client;
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

use crate::config;
use crate::config::model::{AppConfig, Bookmark};
use crate::keychain;

use self::client::SshoreHandler;
use self::password::PasswordDetector;
use self::snippet::{EscapeAction, EscapeDetector};

/// Result of executing a single command on a remote host.
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u32,
}

/// Default SSH key filenames to try, in priority order.
const DEFAULT_KEY_NAMES: &[&str] = &["id_ed25519", "id_rsa", "id_ecdsa"];

/// SSH connection timeout in seconds.
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Terminal cleanup guard — restores terminal state on drop.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        terminal_theme::reset_theme();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    }
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

    // Connect to SSH server
    let ssh_config = russh::client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)),
        ..<_>::default()
    };

    let mut session = russh::client::connect(
        Arc::new(ssh_config),
        (host.as_str(), port),
        SshoreHandler::new(),
    )
    .await
    .with_context(|| format!("Failed to connect to {host}:{port}"))?;

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

    let handler = SshoreHandler::new();
    let remote_map = handler.remote_forwards.clone();

    let ssh_config = russh::client::Config {
        inactivity_timeout: None, // Tunnels stay open indefinitely
        keepalive_interval: Some(std::time::Duration::from_secs(
            tunnel::TUNNEL_KEEPALIVE_INTERVAL_SECS,
        )),
        keepalive_max: tunnel::TUNNEL_KEEPALIVE_MAX,
        ..<_>::default()
    };

    let mut session = russh::client::connect(Arc::new(ssh_config), (host.as_str(), port), handler)
        .await
        .with_context(|| format!("Failed to connect to {host}:{port}"))?;

    let authenticated = authenticate(&mut session, &user, &keys).await?;
    if !authenticated {
        bail!("Authentication failed for {user}@{host}:{port}");
    }

    Ok((session, remote_map))
}

/// Connect to a bookmark and run an interactive SSH session.
/// Updates last_connected/connect_count after a successful session.
pub async fn connect(config: &mut AppConfig, bookmark_index: usize) -> Result<()> {
    let session = establish_session(config, bookmark_index).await?;

    // Apply terminal theming
    terminal_theme::apply_theme(&config.bookmarks[bookmark_index], &config.settings);

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

    // Check keychain for stored password and create detector
    let bookmark_name = &config.bookmarks[bookmark_index].name;
    let stored_password = keychain::get_password(bookmark_name).unwrap_or_else(|e| {
        eprintln!("Warning: failed to read keychain: {e}");
        None
    });
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
    let bookmark_snippets = config.bookmarks[bookmark_index].snippets.clone();
    let global_snippets = config.settings.snippets.clone();
    let snippet_trigger = config.settings.snippet_trigger.clone();

    // Run the interactive proxy loop
    run_proxy_loop(
        channel,
        detector,
        stored_password,
        bookmark_snippets,
        global_snippets,
        snippet_trigger,
    )
    .await?;

    // Update bookmark stats
    config.bookmarks[bookmark_index].last_connected = Some(Utc::now());
    config.bookmarks[bookmark_index].connect_count += 1;
    if let Err(e) = config::save(config) {
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
            let _permit = sem.acquire().await.unwrap();

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
/// If bookmark has identity_file, load that. Otherwise try default keys.
fn load_keys(bookmark: &Bookmark) -> Result<Vec<PrivateKeyWithHashAlg>> {
    let mut keys = Vec::new();

    if let Some(ref identity) = bookmark.identity_file {
        let expanded = shellexpand::tilde(identity).to_string();
        match load_key_from_path(&expanded) {
            Ok(key) => keys.push(key),
            Err(e) => {
                eprintln!("Warning: failed to load key {expanded}: {e}");
            }
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
    match session.authenticate_password(user, &password).await {
        Ok(AuthResult::Success) => Ok(true),
        Ok(AuthResult::Failure { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Prompt the user for a password on stderr (so it doesn't interfere with SSH I/O).
fn prompt_password(user: &str) -> Result<String> {
    eprint!("{user}'s password: ");
    std::io::stderr().flush()?;

    // Read password without echo
    crossterm::terminal::enable_raw_mode()?;
    let mut password = String::new();
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

/// Run the interactive terminal proxy loop.
/// Routes stdin through the main `tokio::select!` loop to enable password injection
/// without race conditions. When a password prompt is detected in SSH output,
/// the user can press Enter to inject the stored password or Esc to skip.
async fn run_proxy_loop(
    channel: russh::Channel<russh::client::Msg>,
    mut detector: PasswordDetector,
    stored_password: Option<String>,
    bookmark_snippets: Vec<crate::config::model::Snippet>,
    global_snippets: Vec<crate::config::model::Snippet>,
    snippet_trigger: String,
) -> Result<()> {
    // Put terminal in raw mode with cleanup guard
    crossterm::terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let _guard = TerminalGuard;

    let (mut channel_rx, channel_tx) = channel.split();

    // Create a writer for stdin forwarding (clones the internal sender)
    let mut writer = channel_tx.make_writer();

    // Stdin flows through an mpsc channel so the main loop controls forwarding
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_SIZE);

    // Spawn stdin reader — sends raw bytes to the mpsc channel
    let stdin_handle = tokio::spawn(read_stdin(stdin_tx));

    // Spawn resize handler (takes ownership of write half)
    let resize_handle = tokio::spawn(handle_resize(channel_tx));

    let mut stdout = std::io::stdout();
    let mut awaiting_confirm = false;

    // Escape detector for snippet trigger
    let mut escape_detector = EscapeDetector::new(&snippet_trigger);
    let has_snippets = !bookmark_snippets.is_empty() || !global_snippets.is_empty();

    loop {
        tokio::select! {
            msg = channel_rx.wait() => {
                match msg {
                    Some(ChannelMsg::Data { ref data }) => {
                        stdout.write_all(data)?;
                        stdout.flush()?;

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
                        let mut payload = pw.as_bytes().to_vec();
                        payload.push(b'\n');
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &payload).await;
                    }
                    awaiting_confirm = false;
                    detector.clear();
                } else if has_snippets {
                    // Snippet escape detection: feed bytes one at a time
                    for &byte in &bytes {
                        match escape_detector.feed(byte) {
                            EscapeAction::Forward(fwd) => {
                                if tokio::io::AsyncWriteExt::write_all(&mut writer, &fwd).await.is_err() {
                                    break;
                                }
                            }
                            EscapeAction::Buffer => {
                                // Hold — might be start of escape sequence
                            }
                            EscapeAction::Trigger => {
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
                        }
                    }
                } else {
                    // No snippets configured — fast path, skip escape detection
                    if tokio::io::AsyncWriteExt::write_all(&mut writer, &bytes).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Clean up spawned tasks
    stdin_handle.abort();
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
async fn handle_resize(channel_tx: russh::ChannelWriteHalf<russh::client::Msg>) {
    let mut event_stream = crossterm::event::EventStream::new();
    use tokio_stream::StreamExt;

    while let Some(Ok(event)) = event_stream.next().await {
        if let crossterm::event::Event::Resize(cols, rows) = event {
            let _ = channel_tx
                .window_change(cols as u32, rows as u32, 0, 0)
                .await;
        }
    }
}
