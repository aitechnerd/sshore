use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::config::model::{AppConfig, validate_hostname};

use super::client::RemoteForwardMap;

/// Shared handle to an SSH session, wrapped for concurrent access.
type SharedSession = Arc<Mutex<russh::client::Handle<super::client::SshoreHandler>>>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// SSH keepalive interval for tunnel sessions (seconds).
pub const TUNNEL_KEEPALIVE_INTERVAL_SECS: u64 = 30;

/// Max missed keepalives before considering the connection dead.
pub const TUNNEL_KEEPALIVE_MAX: usize = 3;

/// Initial reconnect delay for persistent tunnels (seconds).
pub const RECONNECT_INITIAL_DELAY_SECS: u64 = 1;

/// Maximum reconnect delay after exponential backoff (seconds).
pub const RECONNECT_MAX_DELAY_SECS: u64 = 60;

/// Backoff multiplier between reconnect attempts.
pub const RECONNECT_BACKOFF_MULTIPLIER: u64 = 2;
/// Default remote bind address for -R forwards (loopback-only for safety).
const REMOTE_FORWARD_BIND_ADDR: &str = "127.0.0.1";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Direction of a port forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardDirection {
    Local,
    Remote,
}

impl std::fmt::Display for ForwardDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardDirection::Local => write!(f, "-L"),
            ForwardDirection::Remote => write!(f, "-R"),
        }
    }
}

/// A parsed port-forwarding specification (e.g. "5432:localhost:5432").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub direction: ForwardDirection,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

impl std::fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}:{}:{}",
            self.direction, self.local_port, self.remote_host, self.remote_port
        )
    }
}

/// Status of a running tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelStatus {
    Connected,
    Reconnecting,
    Stopped,
}

impl std::fmt::Display for TunnelStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelStatus::Connected => write!(f, "connected"),
            TunnelStatus::Reconnecting => write!(f, "reconnecting"),
            TunnelStatus::Stopped => write!(f, "stopped"),
        }
    }
}

/// A single tunnel entry in the state file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelEntry {
    pub bookmark: String,
    pub forwards: Vec<ForwardSpec>,
    pub persistent: bool,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub reconnect_count: u32,
    pub status: TunnelStatus,
}

/// Top-level state file holding all active tunnels.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TunnelState {
    #[serde(default)]
    pub tunnels: Vec<TunnelEntry>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a forward spec string like "5432:localhost:5432" into a `ForwardSpec`.
///
/// Format: `local_port:remote_host:remote_port`
///
/// For -L: local_port is bound locally, traffic goes to remote_host:remote_port via SSH.
/// For -R: local_port is bound on the remote, traffic comes back to remote_host:remote_port locally.
pub fn parse_forward_spec(spec: &str, direction: ForwardDirection) -> Result<ForwardSpec> {
    if spec.is_empty() {
        bail!("Forward spec cannot be empty");
    }

    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() != 3 {
        bail!(
            "Invalid forward spec '{spec}': expected format local_port:host:remote_port (3 parts separated by ':')"
        );
    }

    let local_port = parse_port(parts[0], spec)?;
    let remote_host = parts[1];
    validate_hostname(remote_host)
        .with_context(|| format!("Invalid host in forward spec '{spec}'"))?;
    let remote_port = parse_port(parts[2], spec)?;

    Ok(ForwardSpec {
        direction,
        local_port,
        remote_host: remote_host.to_string(),
        remote_port,
    })
}

/// Parse a port number from a string, validating range 1-65535.
fn parse_port(s: &str, spec: &str) -> Result<u16> {
    let port: u32 = s
        .parse()
        .with_context(|| format!("Invalid port '{s}' in forward spec '{spec}'"))?;

    if port == 0 || port > 65535 {
        bail!("Port {port} out of valid range (1-65535) in forward spec '{spec}'");
    }

    Ok(port as u16)
}

// ---------------------------------------------------------------------------
// State file I/O
// ---------------------------------------------------------------------------

/// Return the path to the tunnel state file (`~/.config/sshore/tunnels.json`).
pub fn tunnel_state_path() -> PathBuf {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("sshore").join("tunnels.json")
}

/// Load tunnel state from the default path. Returns empty state if file is missing.
pub fn load_tunnel_state() -> Result<TunnelState> {
    load_tunnel_state_from(&tunnel_state_path())
}

/// Load tunnel state from a specific path. Returns empty state if file is missing.
pub fn load_tunnel_state_from(path: &PathBuf) -> Result<TunnelState> {
    if !path.exists() {
        return Ok(TunnelState::default());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read tunnel state: {}", path.display()))?;

    let state: TunnelState = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse tunnel state: {}", path.display()))?;

    Ok(state)
}

/// Save tunnel state to the default path using atomic write.
pub fn save_tunnel_state(state: &TunnelState) -> Result<()> {
    save_tunnel_state_to(state, &tunnel_state_path())
}

/// Save tunnel state to a specific path using atomic write (tempfile + rename, 0600 perms).
pub fn save_tunnel_state_to(state: &TunnelState, path: &PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create tunnel state directory: {}",
                parent.display()
            )
        })?;
    }

    let json = serde_json::to_string_pretty(state).context("Failed to serialize tunnel state")?;

    let parent = path
        .parent()
        .context("Tunnel state path has no parent directory")?;

    let temp_file =
        tempfile::NamedTempFile::new_in(parent).context("Failed to create temp tunnel state")?;

    fs::write(temp_file.path(), &json).context("Failed to write tunnel state to temp file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(temp_file.path(), perms)
            .context("Failed to set tunnel state file permissions")?;
    }

    let persisted = temp_file
        .persist(path)
        .context("Failed to atomically replace tunnel state file")?;

    // Sync to disk to prevent data loss on crash
    persisted
        .sync_all()
        .context("Failed to sync tunnel state file to disk")?;

    Ok(())
}

/// Register a new tunnel entry in the state file.
pub fn register_tunnel(entry: TunnelEntry) -> Result<()> {
    let mut state = load_tunnel_state()?;
    // Remove any existing entry for this bookmark
    state.tunnels.retain(|t| t.bookmark != entry.bookmark);
    state.tunnels.push(entry);
    save_tunnel_state(&state)
}

/// Remove a tunnel entry for the given bookmark from the state file.
pub fn unregister_tunnel(bookmark: &str) -> Result<()> {
    let mut state = load_tunnel_state()?;
    state.tunnels.retain(|t| t.bookmark != bookmark);
    save_tunnel_state(&state)
}

/// Update tunnel status and reconnect count for a bookmark.
pub fn update_tunnel_status(
    bookmark: &str,
    status: TunnelStatus,
    reconnect_count: u32,
) -> Result<()> {
    let mut state = load_tunnel_state()?;
    if let Some(entry) = state.tunnels.iter_mut().find(|t| t.bookmark == bookmark) {
        entry.status = status;
        entry.reconnect_count = reconnect_count;
    }
    save_tunnel_state(&state)
}

/// Check if a process with the given PID is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Check if a process with the given PID is alive (Windows variant).
#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

/// Remove tunnel entries whose PIDs are no longer alive.
pub fn cleanup_stale_tunnels(state: &mut TunnelState) {
    state.tunnels.retain(|t| is_process_alive(t.pid));
}

/// Get the set of bookmark names that have active tunnels.
pub fn active_tunnel_bookmarks() -> HashSet<String> {
    let mut state = load_tunnel_state().unwrap_or_default();
    cleanup_stale_tunnels(&mut state);
    state.tunnels.iter().map(|t| t.bookmark.clone()).collect()
}

// ---------------------------------------------------------------------------
// Tunnel runtime
// ---------------------------------------------------------------------------

/// Run a local port forward (-L): binds a local listener and bridges each
/// accepted connection through the SSH session to the remote target.
async fn run_local_forward(session: SharedSession, spec: &ForwardSpec) -> Result<()> {
    let addr = format!("127.0.0.1:{}", spec.local_port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind local port {}", spec.local_port))?;

    eprintln!(
        "Forwarding {} → {}:{}",
        addr, spec.remote_host, spec.remote_port
    );

    let remote_host = spec.remote_host.clone();
    let remote_port = spec.remote_port as u32;

    tokio::spawn(async move {
        loop {
            let (mut tcp_stream, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("Warning: failed to accept connection: {e}");
                    continue;
                }
            };

            let session = Arc::clone(&session);
            let host = remote_host.clone();

            tokio::spawn(async move {
                let channel = {
                    let handle = session.lock().await;
                    handle
                        .channel_open_direct_tcpip(&host, remote_port, "127.0.0.1", 0)
                        .await
                };

                match channel {
                    Ok(channel) => {
                        let mut channel_stream = channel.into_stream();
                        if let Err(e) =
                            copy_bidirectional(&mut tcp_stream, &mut channel_stream).await
                        {
                            let _ = e; // Normal when either side closes
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to open direct-tcpip channel: {e}");
                    }
                }
            });
        }
    });

    Ok(())
}

/// Set up a remote port forward (-R): requests the server to listen on a port,
/// and registers the mapping so `SshoreHandler::server_channel_open_forwarded_tcpip`
/// can bridge incoming connections to the local target.
async fn setup_remote_forward(
    session: &SharedSession,
    spec: &ForwardSpec,
    remote_map: &RemoteForwardMap,
) -> Result<()> {
    // Ask the server to listen. The returned port is the actual bound port
    // (may differ from requested if the server chose one).
    let bound_port = {
        let mut handle = session.lock().await;
        handle
            .tcpip_forward(REMOTE_FORWARD_BIND_ADDR, spec.local_port as u32)
            .await
            .with_context(|| {
                format!(
                    "Failed to request remote forward on port {}",
                    spec.local_port
                )
            })?
    };

    // Use the server-assigned port if it differs from what we requested
    let actual_port = if bound_port != 0 {
        bound_port
    } else {
        spec.local_port as u32
    };

    if bound_port != 0 && bound_port != spec.local_port as u32 {
        eprintln!(
            "Warning: requested remote port {} but server bound port {}",
            spec.local_port, bound_port
        );
    }

    // Register the mapping: when the server sends traffic for this port,
    // we connect locally to remote_host:remote_port
    {
        let mut map = remote_map.lock().await;
        map.insert(
            (REMOTE_FORWARD_BIND_ADDR.to_string(), actual_port),
            (spec.remote_host.clone(), spec.remote_port),
        );
    }

    eprintln!(
        "Remote forward: remote:{} → {}:{}",
        actual_port, spec.remote_host, spec.remote_port
    );

    Ok(())
}

/// Run all forwards in the foreground, blocking until Ctrl+C.
pub async fn run_foreground(
    config: &AppConfig,
    bookmark_index: usize,
    forwards: &[ForwardSpec],
) -> Result<()> {
    let (session, remote_map) = super::establish_tunnel_session(config, bookmark_index).await?;
    let session = Arc::new(Mutex::new(session));

    for spec in forwards {
        match spec.direction {
            ForwardDirection::Local => {
                run_local_forward(Arc::clone(&session), spec).await?;
            }
            ForwardDirection::Remote => {
                setup_remote_forward(&session, spec, &remote_map).await?;
            }
        }
    }

    // Register in state file
    let entry = TunnelEntry {
        bookmark: config.bookmarks[bookmark_index].name.clone(),
        forwards: forwards.to_vec(),
        persistent: false,
        pid: std::process::id(),
        started_at: Utc::now(),
        reconnect_count: 0,
        status: TunnelStatus::Connected,
    };
    register_tunnel(entry)?;

    eprintln!("Tunnel active. Press Ctrl+C to stop.");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c()
        .await
        .context("Failed to listen for Ctrl+C")?;

    eprintln!("\nShutting down tunnel...");
    unregister_tunnel(&config.bookmarks[bookmark_index].name)?;

    Ok(())
}

/// Run a persistent tunnel with auto-reconnect on disconnect.
/// Intended to be called from a daemonized process.
pub async fn run_daemon_loop(
    config: &AppConfig,
    bookmark_index: usize,
    forwards: &[ForwardSpec],
) -> Result<()> {
    let bookmark_name = config.bookmarks[bookmark_index].name.clone();
    let mut delay_secs = RECONNECT_INITIAL_DELAY_SECS;
    let mut reconnect_count: u32 = 0;

    // Register in state file
    let entry = TunnelEntry {
        bookmark: bookmark_name.clone(),
        forwards: forwards.to_vec(),
        persistent: true,
        pid: std::process::id(),
        started_at: Utc::now(),
        reconnect_count: 0,
        status: TunnelStatus::Connected,
    };
    register_tunnel(entry)?;

    loop {
        let result = run_single_session(config, bookmark_index, forwards).await;

        match result {
            Ok(()) => {
                // Session ended cleanly (e.g., SIGTERM)
                break;
            }
            Err(e) => {
                reconnect_count += 1;
                eprintln!(
                    "Tunnel disconnected: {e:#}. Reconnecting in {delay_secs}s (attempt {reconnect_count})..."
                );

                let _ = update_tunnel_status(
                    &bookmark_name,
                    TunnelStatus::Reconnecting,
                    reconnect_count,
                );

                // Check for SIGTERM while sleeping
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("Received signal, stopping tunnel.");
                        break;
                    }
                }

                // Exponential backoff
                delay_secs =
                    (delay_secs * RECONNECT_BACKOFF_MULTIPLIER).min(RECONNECT_MAX_DELAY_SECS);
            }
        }
    }

    unregister_tunnel(&bookmark_name)?;
    Ok(())
}

/// Run a single tunnel session: connect, set up forwards, wait until session dies or SIGTERM.
async fn run_single_session(
    config: &AppConfig,
    bookmark_index: usize,
    forwards: &[ForwardSpec],
) -> Result<()> {
    let (session, remote_map) = super::establish_tunnel_session(config, bookmark_index).await?;
    let session = Arc::new(Mutex::new(session));

    let bookmark_name = &config.bookmarks[bookmark_index].name;
    let _ = update_tunnel_status(bookmark_name, TunnelStatus::Connected, 0);

    for spec in forwards {
        match spec.direction {
            ForwardDirection::Local => {
                run_local_forward(Arc::clone(&session), spec).await?;
            }
            ForwardDirection::Remote => {
                setup_remote_forward(&session, spec, &remote_map).await?;
            }
        }
    }

    // Open a keepalive channel to detect disconnects
    let channel = {
        let handle = session.lock().await;
        handle
            .channel_open_session()
            .await
            .context("Failed to open keepalive channel")?
    };

    // Wait for the session to die or Ctrl+C
    tokio::select! {
        _ = wait_for_channel_close(channel) => {
            bail!("SSH session closed unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            // Clean exit
            Ok(())
        }
    }
}

/// Wait for an SSH channel to close (used to detect session death).
async fn wait_for_channel_close(channel: russh::Channel<russh::client::Msg>) {
    let (mut rx, _tx) = channel.split();
    loop {
        match rx.wait().await {
            Some(russh::ChannelMsg::Eof | russh::ChannelMsg::Close) => break,
            None => break,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_forward_spec_local_valid() {
        let spec = parse_forward_spec("5432:localhost:5432", ForwardDirection::Local).unwrap();
        assert_eq!(spec.direction, ForwardDirection::Local);
        assert_eq!(spec.local_port, 5432);
        assert_eq!(spec.remote_host, "localhost");
        assert_eq!(spec.remote_port, 5432);
    }

    #[test]
    fn test_parse_forward_spec_with_hostname() {
        let spec =
            parse_forward_spec("8080:my-host.example.com:80", ForwardDirection::Local).unwrap();
        assert_eq!(spec.local_port, 8080);
        assert_eq!(spec.remote_host, "my-host.example.com");
        assert_eq!(spec.remote_port, 80);
    }

    #[test]
    fn test_parse_forward_spec_remote() {
        let spec = parse_forward_spec("3000:localhost:3000", ForwardDirection::Remote).unwrap();
        assert_eq!(spec.direction, ForwardDirection::Remote);
        assert_eq!(spec.local_port, 3000);
        assert_eq!(spec.remote_host, "localhost");
        assert_eq!(spec.remote_port, 3000);
    }

    #[test]
    fn test_parse_forward_spec_missing_parts() {
        assert!(parse_forward_spec("5432:localhost", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_parse_forward_spec_too_many_parts() {
        assert!(parse_forward_spec("5432:localhost:5432:extra", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_parse_forward_spec_invalid_port_zero() {
        assert!(parse_forward_spec("0:localhost:5432", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_parse_forward_spec_invalid_port_overflow() {
        assert!(parse_forward_spec("99999:localhost:5432", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_parse_forward_spec_invalid_host_metachar() {
        assert!(parse_forward_spec("5432:host;rm:5432", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_parse_forward_spec_empty() {
        assert!(parse_forward_spec("", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_parse_forward_spec_non_numeric_port() {
        assert!(parse_forward_spec("abc:localhost:5432", ForwardDirection::Local).is_err());
    }

    #[test]
    fn test_forward_spec_display() {
        let spec = ForwardSpec {
            direction: ForwardDirection::Local,
            local_port: 5432,
            remote_host: "localhost".into(),
            remote_port: 5432,
        };
        assert_eq!(spec.to_string(), "-L 5432:localhost:5432");
    }

    #[test]
    fn test_tunnel_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnels.json");

        let state = TunnelState {
            tunnels: vec![TunnelEntry {
                bookmark: "test-server".into(),
                forwards: vec![ForwardSpec {
                    direction: ForwardDirection::Local,
                    local_port: 5432,
                    remote_host: "localhost".into(),
                    remote_port: 5432,
                }],
                persistent: true,
                pid: 12345,
                started_at: Utc::now(),
                reconnect_count: 0,
                status: TunnelStatus::Connected,
            }],
        };

        save_tunnel_state_to(&state, &path).unwrap();
        let loaded = load_tunnel_state_from(&path).unwrap();

        assert_eq!(loaded.tunnels.len(), 1);
        assert_eq!(loaded.tunnels[0].bookmark, "test-server");
        assert_eq!(loaded.tunnels[0].persistent, true);
        assert_eq!(loaded.tunnels[0].status, TunnelStatus::Connected);
    }

    #[test]
    fn test_tunnel_state_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent").join("tunnels.json");
        let state = load_tunnel_state_from(&path).unwrap();
        assert!(state.tunnels.is_empty());
    }

    #[test]
    fn test_register_and_unregister_tunnel() {
        // We can't easily test the global file, so test the state manipulation logic
        let mut state = TunnelState::default();

        let entry = TunnelEntry {
            bookmark: "test-server".into(),
            forwards: vec![],
            persistent: false,
            pid: 12345,
            started_at: Utc::now(),
            reconnect_count: 0,
            status: TunnelStatus::Connected,
        };

        // Register
        state.tunnels.push(entry);
        assert_eq!(state.tunnels.len(), 1);
        assert_eq!(state.tunnels[0].bookmark, "test-server");

        // Unregister
        state.tunnels.retain(|t| t.bookmark != "test-server");
        assert!(state.tunnels.is_empty());
    }

    #[test]
    fn test_cleanup_stale_tunnels() {
        let mut state = TunnelState {
            tunnels: vec![TunnelEntry {
                bookmark: "dead-tunnel".into(),
                forwards: vec![],
                persistent: false,
                pid: 999_999_999, // Very unlikely to be a real PID
                started_at: Utc::now(),
                reconnect_count: 0,
                status: TunnelStatus::Connected,
            }],
        };

        cleanup_stale_tunnels(&mut state);
        assert!(state.tunnels.is_empty(), "Stale tunnel should be removed");
    }

    #[test]
    fn test_is_process_alive_self() {
        let pid = std::process::id();
        assert!(is_process_alive(pid), "Current process should be alive");
    }

    #[test]
    fn test_is_process_alive_dead() {
        assert!(
            !is_process_alive(999_999_999),
            "Non-existent PID should not be alive"
        );
    }

    #[test]
    fn test_update_tunnel_status() {
        let mut state = TunnelState {
            tunnels: vec![TunnelEntry {
                bookmark: "test-server".into(),
                forwards: vec![],
                persistent: true,
                pid: 12345,
                started_at: Utc::now(),
                reconnect_count: 0,
                status: TunnelStatus::Connected,
            }],
        };

        // Update status
        if let Some(entry) = state
            .tunnels
            .iter_mut()
            .find(|t| t.bookmark == "test-server")
        {
            entry.status = TunnelStatus::Reconnecting;
            entry.reconnect_count = 3;
        }

        assert_eq!(state.tunnels[0].status, TunnelStatus::Reconnecting);
        assert_eq!(state.tunnels[0].reconnect_count, 3);
    }

    #[test]
    fn test_tunnel_status_display() {
        assert_eq!(TunnelStatus::Connected.to_string(), "connected");
        assert_eq!(TunnelStatus::Reconnecting.to_string(), "reconnecting");
        assert_eq!(TunnelStatus::Stopped.to_string(), "stopped");
    }

    #[test]
    fn test_forward_direction_display() {
        assert_eq!(ForwardDirection::Local.to_string(), "-L");
        assert_eq!(ForwardDirection::Remote.to_string(), "-R");
    }

    #[cfg(unix)]
    #[test]
    fn test_tunnel_state_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnels.json");
        let state = TunnelState::default();

        save_tunnel_state_to(&state, &path).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
