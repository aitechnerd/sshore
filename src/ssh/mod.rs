pub mod client;
pub mod terminal_theme;

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

use self::client::SshoreHandler;

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

/// Connect to a bookmark and run an interactive SSH session.
/// Updates last_connected/connect_count after a successful session.
pub async fn connect(config: &mut AppConfig, bookmark_index: usize) -> Result<()> {
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

    let mut session =
        russh::client::connect(Arc::new(ssh_config), (host.as_str(), port), SshoreHandler)
            .await
            .with_context(|| format!("Failed to connect to {host}:{port}"))?;

    // Authenticate
    let authenticated = authenticate(&mut session, &user, &keys).await?;
    if !authenticated {
        bail!("Authentication failed for {user}@{host}:{port}");
    }

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

    // Run the interactive proxy loop
    run_proxy_loop(channel).await?;

    // Update bookmark stats
    config.bookmarks[bookmark_index].last_connected = Some(Utc::now());
    config.bookmarks[bookmark_index].connect_count += 1;
    if let Err(e) = config::save(config) {
        eprintln!("Warning: failed to save connection stats: {e}");
    }

    Ok(())
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

/// Run the interactive terminal proxy loop.
/// Forwards stdin -> SSH channel and SSH channel -> stdout.
async fn run_proxy_loop(channel: russh::Channel<russh::client::Msg>) -> Result<()> {
    // Put terminal in raw mode with cleanup guard
    crossterm::terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let _guard = TerminalGuard;

    let (mut channel_rx, channel_tx) = channel.split();

    // Create a writer for stdin forwarding (clones the internal sender)
    let writer = channel_tx.make_writer();

    // Spawn stdin -> channel task
    let stdin_handle = tokio::spawn(forward_stdin(writer));

    // Spawn resize handler (takes ownership of write half)
    let resize_handle = tokio::spawn(handle_resize(channel_tx));

    // Main loop: channel -> stdout
    let mut stdout = std::io::stdout();
    loop {
        match channel_rx.wait().await {
            Some(ChannelMsg::Data { data }) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                // stderr
                std::io::stderr().write_all(&data)?;
                std::io::stderr().flush()?;
            }
            Some(ChannelMsg::ExitStatus { .. }) => break,
            Some(ChannelMsg::Eof | ChannelMsg::Close) => break,
            Some(_) => {}  // Ignore other messages
            None => break, // Channel closed
        }
    }

    // Clean up spawned tasks
    stdin_handle.abort();
    resize_handle.abort();

    Ok(())
}

/// Forward stdin to the SSH channel writer.
async fn forward_stdin(mut writer: impl tokio::io::AsyncWrite + Unpin) {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1024];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tokio::io::AsyncWriteExt::write_all(&mut writer, &buf[..n])
                    .await
                    .is_err()
                {
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
