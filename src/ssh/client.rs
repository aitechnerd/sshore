use std::collections::HashMap;
use std::sync::Arc;

use russh::client;
use russh::keys::PublicKey;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// Map from (bound_address, bound_port) → (local_host, local_port) for -R forwards.
/// When the server opens a forwarded channel, we look up the destination here.
pub type RemoteForwardMap = Arc<Mutex<HashMap<(String, u32), (String, u16)>>>;

/// SSH client handler for sshore connections.
/// Implements the russh `client::Handler` trait to handle SSH protocol events.
pub struct SshoreHandler {
    /// Registered remote forwards: maps server-side (address, port) to local (host, port).
    pub remote_forwards: RemoteForwardMap,
}

impl Default for SshoreHandler {
    fn default() -> Self {
        Self {
            remote_forwards: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SshoreHandler {
    /// Create a new handler with an empty remote forward map.
    pub fn new() -> Self {
        Self::default()
    }
}

impl client::Handler for SshoreHandler {
    type Error = anyhow::Error;

    /// Called when the server presents its host key.
    /// Currently accepts all keys. TODO: known_hosts checking.
    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
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
