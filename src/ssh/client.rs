use russh::client;
use russh::keys::PublicKey;

/// SSH client handler for sshore connections.
/// Implements the russh `client::Handler` trait to handle SSH protocol events.
pub struct SshoreHandler;

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
}
