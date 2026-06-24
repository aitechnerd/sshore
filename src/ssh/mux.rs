//! Persistent mux shell support for mux mode.
//!
//! Opens a persistent SSH shell for a group session and allows sending
//! commands over the existing connection without re-authenticating.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::model::AppConfig;
use crate::ssh::client::SshoreHandler;

/// Handle for a persistent mux shell channel.
///
/// Holds the SSH session, the channel write half for sending commands,
/// and a sender for streaming output lines back to the TUI.
pub struct MuxChannel {
    /// The authenticated SSH session handle.
    #[allow(dead_code)]
    session: Arc<russh::client::Handle<SshoreHandler>>,
    /// Channel write half for sending data (commands).
    channel_tx: russh::ChannelWriteHalf<russh::client::Msg>,
    /// Sender for output lines from the reader task.
    output_tx: tokio::sync::mpsc::Sender<String>,
    /// Join handle for the background reader task.
    reader_handle: Option<tokio::task::JoinHandle<()>>,
}

impl MuxChannel {
    /// Send a command to the remote shell.
    pub async fn send_command(&self, command: &str) -> Result<()> {
        self.channel_tx
            .data(format!("{command}\n").as_bytes())
            .await
            .context("Failed to send command to mux channel")
    }

    /// Close the mux channel and stop the reader task.
    pub async fn close(self) {
        // Drop the output sender to signal the reader task to stop.
        drop(self.output_tx);
        // Wait for the reader task to finish.
        if let Some(handle) = self.reader_handle {
            let _ = handle.await;
        }
        // channel_tx and session drop after this (field drop order).
    }
}

/// Open a persistent shell channel for a session in a group.
///
/// Returns the MuxChannel for sending commands and an mpsc receiver
/// for reading output lines. The reader task runs in the background
/// and sends lines to the receiver until the channel is closed.
pub async fn mux_open_shell(
    config: &AppConfig,
    group_idx: usize,
    session_idx: usize,
) -> Result<(MuxChannel, tokio::sync::mpsc::Receiver<String>)> {
    let group = &config.groups[group_idx];
    let session = &group.sessions[session_idx];

    // Resolve effective parameters (same chain as connect_session)
    let user = session.effective_user(group, &config.settings, &config.profiles);
    let host = session.effective_host(group);
    let port = session.effective_port(group);
    let identity_file = session.effective_identity_file(group, &config.profiles);
    let proxy_jump = session.effective_proxy_jump(group, &config.profiles);
    let connect_timeout_secs =
        session.effective_connect_timeout(group, &config.settings, &config.profiles);

    // Build synthetic bookmark for connection
    let bookmark = crate::config::model::Bookmark {
        name: session.display_name(group),
        host: host.clone(),
        user: Some(user.clone()),
        port,
        env: session.effective_env(group),
        tags: group.tags.clone(),
        identity_file,
        proxy_jump,
        notes: group.notes.clone(),
        last_connected: None,
        connect_count: 0,
        on_connect: None, // Not sent on open; sent via send_command
        on_connect_prompt_pattern: None,
        snippets: session.effective_snippets(group),
        connect_timeout_secs,
        ssh_options: session.effective_ssh_options(group, &config.profiles),
        profile: group.profile.clone(),
    };

    // Establish SSH session via temp config (reuse establish_session)
    let mut temp_config = config.clone();
    let bm_idx = temp_config.bookmarks.len();
    temp_config.bookmarks.push(bookmark);

    let session_handle = Arc::new(crate::ssh::establish_session(&temp_config, bm_idx, true).await?);

    // Open session channel
    let channel = session_handle
        .channel_open_session()
        .await
        .context("Failed to open SSH session channel")?;

    // Request PTY
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

    // Split the channel: rx for reading output, tx for sending commands
    let (channel_rx, channel_tx) = channel.split();

    // Create output channel (buffer of 256 lines)
    let (output_tx, output_rx) = tokio::sync::mpsc::channel(256);

    // Spawn reader task to capture output
    let reader_handle = tokio::spawn(reader_task(channel_rx, output_tx.clone()));

    let mux_channel = MuxChannel {
        session: session_handle,
        channel_tx,
        output_tx,
        reader_handle: Some(reader_handle),
    };

    Ok((mux_channel, output_rx))
}

/// Background reader task: reads from the channel and sends lines via output_tx.
async fn reader_task(
    mut channel_rx: russh::ChannelReadHalf,
    output_tx: tokio::sync::mpsc::Sender<String>,
) {
    let mut buf = String::new();
    loop {
        // Use timeout to allow checking if output_tx is closed
        let item = tokio::time::timeout(std::time::Duration::from_millis(100), channel_rx.wait())
            .await
            .ok();

        match item {
            Some(Some(russh::ChannelMsg::Data { ref data })) => {
                buf.push_str(&String::from_utf8_lossy(data));
                flush_lines(&mut buf, &output_tx);
            }
            Some(Some(russh::ChannelMsg::ExtendedData { data, ext: 1 })) => {
                buf.push_str(&String::from_utf8_lossy(&data));
                flush_lines(&mut buf, &output_tx);
            }
            Some(Some(russh::ChannelMsg::Eof | russh::ChannelMsg::Close)) | Some(None) => {
                // Channel closed or EOF
                if !buf.is_empty() {
                    let _ = output_tx.try_send(buf);
                }
                return;
            }
            Some(Some(_)) => {
                // Other channel messages (exit status, etc.) — ignore
            }
            None => {
                // Timeout — check if receiver is still alive
                if output_tx.is_closed() {
                    return;
                }
                // Flush any partial line on timeout to keep output flowing
                if !buf.is_empty() {
                    let line = buf.clone();
                    if output_tx.try_send(line).is_err() {
                        return;
                    }
                    buf.clear();
                }
            }
        }
    }
}

/// Extract complete lines from buffer and send them via output_tx.
/// Returns true if the sender is closed (caller should exit).
fn flush_lines(buf: &mut String, tx: &tokio::sync::mpsc::Sender<String>) -> bool {
    loop {
        match buf.find('\n') {
            Some(pos) => {
                let line = buf[..=pos].to_string();
                if tx.try_send(line).is_err() {
                    return true; // Sender closed
                }
                buf.drain(..=pos);
            }
            None => return false,
        }
    }
}
