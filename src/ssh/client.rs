use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use russh::client;
use russh::keys::PublicKey;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use super::known_hosts::{self, HostKeyStatus};

/// Map from (bound_address, bound_port) → (local_host, local_port) for -R forwards.
/// When the server opens a forwarded channel, we look up the destination here.
pub type RemoteForwardMap = Arc<Mutex<HashMap<(String, u32), (String, u16)>>>;

/// Host key checking mode.
#[derive(Debug, Clone, PartialEq)]
pub enum HostKeyCheckMode {
    /// Prompt for unknown, reject changed.
    Strict,
    /// Auto-accept unknown, reject changed.
    AcceptNew,
    /// Accept all keys (insecure, for testing only).
    Off,
}

impl HostKeyCheckMode {
    /// Parse from string setting value.
    pub fn from_str_setting(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "accept-new" => Self::AcceptNew,
            "off" => Self::Off,
            _ => Self::Strict,
        }
    }
}

/// SSH client handler for sshore connections.
/// Implements the russh `client::Handler` trait to handle SSH protocol events.
pub struct SshoreHandler {
    /// Registered remote forwards: maps server-side (address, port) to local (host, port).
    pub remote_forwards: RemoteForwardMap,
    /// Target hostname for known_hosts checking.
    pub hostname: String,
    /// Target port for known_hosts checking.
    pub port: u16,
    /// Host key checking mode.
    pub host_key_check_mode: HostKeyCheckMode,
}

impl Default for SshoreHandler {
    fn default() -> Self {
        Self {
            remote_forwards: Arc::new(Mutex::new(HashMap::new())),
            hostname: String::new(),
            port: 22,
            host_key_check_mode: HostKeyCheckMode::Strict,
        }
    }
}

impl SshoreHandler {
    /// Create a handler configured for a specific host connection.
    pub fn for_host(hostname: &str, port: u16, check_mode: HostKeyCheckMode) -> Self {
        Self {
            remote_forwards: Arc::new(Mutex::new(HashMap::new())),
            hostname: hostname.to_string(),
            port,
            host_key_check_mode: check_mode,
        }
    }
}

impl client::Handler for SshoreHandler {
    type Error = anyhow::Error;

    /// Called when the server presents its host key.
    /// Checks against ~/.ssh/known_hosts with configurable strictness.
    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // "off" mode accepts everything
        if self.host_key_check_mode == HostKeyCheckMode::Off {
            return Ok(true);
        }

        // If hostname is empty (shouldn't happen, but be safe), accept
        if self.hostname.is_empty() {
            return Ok(true);
        }

        match known_hosts::check_host_key(&self.hostname, self.port, server_public_key)? {
            HostKeyStatus::Known => Ok(true),

            HostKeyStatus::Unknown {
                fingerprint,
                key_type,
            } => {
                if self.host_key_check_mode == HostKeyCheckMode::AcceptNew {
                    // Auto-accept and save
                    known_hosts::add_host_key(&self.hostname, self.port, server_public_key)?;
                    eprintln!(
                        "Warning: Permanently added '{}' ({}) to the list of known hosts.",
                        self.hostname, key_type
                    );
                    return Ok(true);
                }

                // Strict mode: prompt user
                eprintln!(
                    "The authenticity of host '{}' can't be established.",
                    self.hostname
                );
                eprintln!("{} key fingerprint is {}.", key_type, fingerprint);
                eprint!("Are you sure you want to continue connecting (yes/no)? ");
                let _ = std::io::stderr().flush();

                let mut response = String::new();
                std::io::stdin().read_line(&mut response)?;

                if response.trim().eq_ignore_ascii_case("yes") {
                    known_hosts::add_host_key(&self.hostname, self.port, server_public_key)?;
                    eprintln!(
                        "Warning: Permanently added '{}' ({}) to the list of known hosts.",
                        self.hostname, key_type
                    );
                    Ok(true)
                } else {
                    eprintln!("Host key verification failed.");
                    Ok(false)
                }
            }

            HostKeyStatus::Changed {
                fingerprint_new,
                known_hosts_line,
            } => {
                // CRITICAL WARNING — matches OpenSSH behavior
                eprintln!("@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@");
                eprintln!("@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @");
                eprintln!("@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@");
                eprintln!("IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!");
                eprintln!(
                    "Someone could be eavesdropping on you right now (man-in-the-middle attack)!"
                );
                eprintln!(
                    "The fingerprint for the new host key is: {}",
                    fingerprint_new
                );
                eprintln!(
                    "The offending key is in ~/.ssh/known_hosts line {}.",
                    known_hosts_line
                );
                eprintln!(
                    "Remove the old key with: sed -i '' '{}d' ~/.ssh/known_hosts",
                    known_hosts_line
                );
                eprintln!("Host key verification failed.");
                Ok(false)
            }
        }
    }

    /// Called when the server opens a forwarded-tcpip channel (for -R remote forwards).
    /// Connects to the local target and bridges the two streams.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        let key = (connected_address.to_string(), connected_port);
        let target = {
            let map = self.remote_forwards.lock().await;
            map.get(&key).cloned()
        };

        let Some((local_host, local_port)) = target else {
            eprintln!(
                "Warning: received forwarded connection for unknown {}:{}, ignoring",
                connected_address, connected_port
            );
            return Ok(());
        };

        // Spawn a task to bridge the channel and the local TCP connection
        tokio::spawn(async move {
            match TcpStream::connect(format!("{local_host}:{local_port}")).await {
                Ok(mut tcp_stream) => {
                    let mut channel_stream = channel.into_stream();
                    if let Err(e) = copy_bidirectional(&mut tcp_stream, &mut channel_stream).await {
                        // Connection closed or errored — this is normal for short-lived forwards
                        let _ = e;
                    }
                }
                Err(e) => {
                    eprintln!("Warning: failed to connect to local {local_host}:{local_port}: {e}");
                }
            }
        });

        Ok(())
    }
}
