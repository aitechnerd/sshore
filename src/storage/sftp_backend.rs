use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use russh_sftp::client::SftpSession;

use crate::config::model::AppConfig;
use crate::sftp::pipeline;
use crate::ssh;

use super::FileEntry;

/// SFTP implementation of StorageBackend.
pub struct SftpBackend {
    sftp: SftpSession,
    cwd: String,
    #[allow(dead_code)]
    display_name: String,
    /// SSH connection handle for spawning additional SFTP channels (background transfers).
    ssh_handle: Option<Arc<russh::client::Handle<crate::ssh::client::SshoreHandler>>>,
    /// Stored config for establishing additional SSH connections (e.g. parallel transfers).
    config: Option<Arc<AppConfig>>,
    /// Bookmark index within `config.bookmarks` for reconnection.
    bookmark_index: Option<usize>,
}

impl SftpBackend {
    /// Create a new SFTP backend connected to a bookmark.
    pub async fn new(config: &AppConfig, bookmark_index: usize) -> Result<Self> {
        let bookmark = &config.bookmarks[bookmark_index];
        let session = ssh::establish_session(config, bookmark_index).await?;

        let channel = session
            .channel_open_session()
            .await
            .context("Failed to open SSH channel for SFTP")?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .context("Failed to request SFTP subsystem")?;

        let sftp = SftpSession::new(channel.into_stream())
            .await
            .context("Failed to initialize SFTP session")?;

        let cwd = sftp
            .canonicalize(".")
            .await
            .unwrap_or_else(|_| "/".to_string());

        let display_name = format!("{} (SFTP)", bookmark.name);

        Ok(Self {
            sftp,
            cwd,
            display_name,
            ssh_handle: Some(Arc::new(session)),
            config: Some(Arc::new(config.clone())),
            bookmark_index: Some(bookmark_index),
        })
    }

    /// Create from an existing SSH session handle (reuses the connection).
    /// Opens a new SSH channel for SFTP on the already-authenticated session.
    pub async fn from_handle(
        session: &russh::client::Handle<crate::ssh::client::SshoreHandler>,
        display_name: &str,
    ) -> Result<Self> {
        let channel = session
            .channel_open_session()
            .await
            .context("Failed to open SSH channel for SFTP")?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .context("Failed to request SFTP subsystem")?;

        let sftp = SftpSession::new(channel.into_stream())
            .await
            .context("Failed to initialize SFTP session")?;

        let cwd = sftp
            .canonicalize(".")
            .await
            .unwrap_or_else(|_| "/".to_string());

        let display_name = format!("{display_name} (SFTP)");

        Ok(Self {
            sftp,
            cwd,
            display_name,
            ssh_handle: None,
            config: None,
            bookmark_index: None,
        })
    }

    /// Create from a bookmark reference with a specific starting path.
    pub async fn with_path(
        config: &AppConfig,
        bookmark_index: usize,
        start_path: &str,
    ) -> Result<Self> {
        let mut backend = Self::new(config, bookmark_index).await?;
        backend.cd(start_path).await?;
        Ok(backend)
    }

    /// Set the SSH handle for spawning additional SFTP sessions.
    /// Used when the handle was not available at construction time (e.g. `from_handle`).
    pub fn set_ssh_handle(
        &mut self,
        handle: Arc<russh::client::Handle<crate::ssh::client::SshoreHandler>>,
    ) {
        self.ssh_handle = Some(handle);
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn cwd(&self) -> Result<String> {
        Ok(self.cwd.clone())
    }

    pub async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let entries = self
            .sftp
            .read_dir(path)
            .await
            .with_context(|| format!("Failed to list: {path}"))?;

        Ok(entries
            .into_iter()
            .filter(|e| {
                let name = e.file_name();
                name != "." && name != ".."
            })
            .map(|e| {
                let attrs = e.metadata();
                let name = e.file_name().to_string();
                FileEntry {
                    path: format!("{}/{}", path.trim_end_matches('/'), name),
                    name,
                    is_dir: attrs.is_dir(),
                    size: attrs.size.unwrap_or(0),
                    modified: attrs
                        .mtime
                        .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0)),
                    permissions: attrs.permissions.map(format_sftp_permissions),
                }
            })
            .collect())
    }

    pub async fn cd(&mut self, path: &str) -> Result<()> {
        let new_path = if path == ".." {
            // Navigate up
            Path::new(&self.cwd)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".to_string())
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{}", self.cwd.trim_end_matches('/'), path)
        };

        // Verify the path exists and is a directory
        let canonical = self
            .sftp
            .canonicalize(&new_path)
            .await
            .with_context(|| format!("Path not found: {new_path}"))?;

        self.cwd = canonical;
        Ok(())
    }

    pub async fn download(&self, remote_path: &str, local_path: &Path) -> Result<()> {
        self.download_with_progress(remote_path, local_path, None, None)
            .await
    }

    /// Download with optional progress tracking and cancellation.
    /// Uses pipelined SFTP with bounded in-flight requests.
    pub async fn download_with_progress(
        &self,
        remote_path: &str,
        local_path: &Path,
        progress: Option<&AtomicU64>,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        let handle = self
            .ssh_handle
            .as_ref()
            .context("No SSH handle available for transfer")?;

        // Get file size for the pipeline.
        let meta = self
            .sftp
            .metadata(remote_path)
            .await
            .with_context(|| format!("Failed to stat remote file: {remote_path}"))?;
        let total = meta.size.unwrap_or(0);

        // Open a dedicated channel for the pipelined transfer.
        let channel = handle
            .channel_open_session()
            .await
            .context("Failed to open transfer channel")?;
        let session = pipeline::create_raw_session(channel).await?;

        let local_file = std::fs::File::create(local_path)
            .with_context(|| format!("Failed to create: {}", local_path.display()))?;
        let mut local_file =
            BufWriter::with_capacity((pipeline::CHUNK_SIZE * 2) as usize, local_file);

        pipeline::download(
            &session.raw,
            remote_path,
            &mut local_file,
            total,
            0,
            session.read_chunk_size,
            |bytes| {
                if let Some(p) = progress {
                    p.fetch_add(bytes, Ordering::Relaxed);
                }
            },
            cancel,
        )
        .await
    }

    pub async fn upload(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        self.upload_with_progress(local_path, remote_path, None, None)
            .await
    }

    /// Upload with optional progress tracking and cancellation.
    /// Uses pipelined SFTP with bounded in-flight requests.
    pub async fn upload_with_progress(
        &self,
        local_path: &Path,
        remote_path: &str,
        progress: Option<&AtomicU64>,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        let handle = self
            .ssh_handle
            .as_ref()
            .context("No SSH handle available for transfer")?;

        let local_meta = std::fs::metadata(local_path)
            .with_context(|| format!("Failed to stat: {}", local_path.display()))?;
        let total = local_meta.len();

        // Open a dedicated channel for the pipelined transfer.
        let channel = handle
            .channel_open_session()
            .await
            .context("Failed to open transfer channel")?;
        let session = pipeline::create_raw_session(channel).await?;

        let local_file = std::fs::File::open(local_path)
            .with_context(|| format!("Failed to open: {}", local_path.display()))?;
        let mut local_file =
            BufReader::with_capacity((pipeline::CHUNK_SIZE * 2) as usize, local_file);

        pipeline::upload(
            &session.raw,
            remote_path,
            &mut local_file,
            total,
            session.write_chunk_size,
            |bytes| {
                if let Some(p) = progress {
                    p.fetch_add(bytes, Ordering::Relaxed);
                }
            },
            cancel,
        )
        .await
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.sftp
            .remove_file(path)
            .await
            .with_context(|| format!("Failed to delete: {path}"))
    }

    pub async fn rmdir(&self, path: &str) -> Result<()> {
        // SFTP doesn't have recursive rmdir, so we need to walk the tree
        rmdir_recursive(&self.sftp, path).await
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.sftp
            .create_dir(path)
            .await
            .with_context(|| format!("Failed to create directory: {path}"))
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.sftp
            .rename(from, to)
            .await
            .with_context(|| format!("Failed to rename {from} to {to}"))
    }

    /// Open a new SFTP session on the existing SSH connection.
    /// Used for background transfers so they don't share a channel with the browser.
    pub async fn open_sftp_session(&self) -> Result<SftpSession> {
        let handle = self
            .ssh_handle
            .as_ref()
            .context("No SSH handle available for opening new SFTP sessions")?;

        let channel = handle
            .channel_open_session()
            .await
            .context("Failed to open SSH channel for background SFTP")?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .context("Failed to request SFTP subsystem")?;

        SftpSession::new(channel.into_stream())
            .await
            .context("Failed to initialize background SFTP session")
    }

    /// Get a reference to the SSH handle for spawning additional SFTP sessions.
    pub fn ssh_handle(&self) -> Option<&russh::client::Handle<crate::ssh::client::SshoreHandler>> {
        self.ssh_handle.as_deref()
    }

    /// Establish a new, independent SSH connection to the same host.
    /// Returns a fresh handle on a separate TCP socket — useful for parallel
    /// transfers that need independent flow control.
    pub async fn establish_new_connection(
        &self,
    ) -> Result<russh::client::Handle<crate::ssh::client::SshoreHandler>> {
        let config = self
            .config
            .as_ref()
            .context("No config stored for reconnection")?;
        let index = self
            .bookmark_index
            .context("No bookmark index stored for reconnection")?;
        ssh::establish_session(config, index).await
    }

    /// Get the reconnection info (config + bookmark index) for spawning
    /// independent SSH connections in background tasks.
    pub fn reconnection_info(&self) -> Option<(Arc<AppConfig>, usize)> {
        let config = self.config.as_ref()?.clone();
        let index = self.bookmark_index?;
        Some((config, index))
    }
}

/// Recursively remove a directory via SFTP.
async fn rmdir_recursive(sftp: &SftpSession, path: &str) -> Result<()> {
    let entries = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("Failed to list for delete: {path}"))?;

    for entry in entries {
        let name = entry.file_name().to_string();
        if name == "." || name == ".." {
            continue;
        }
        let full = format!("{}/{}", path.trim_end_matches('/'), name);
        if entry.metadata().is_dir() {
            Box::pin(rmdir_recursive(sftp, &full)).await?;
        } else {
            sftp.remove_file(&full)
                .await
                .with_context(|| format!("Failed to delete: {full}"))?;
        }
    }

    sftp.remove_dir(path)
        .await
        .with_context(|| format!("Failed to remove directory: {path}"))
}

/// Format SFTP permission bits as rwx string.
fn format_sftp_permissions(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for &(bit, ch) in &flags {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    s
}
