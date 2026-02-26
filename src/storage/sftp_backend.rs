use std::path::Path;

use anyhow::{Context, Result};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::model::AppConfig;
use crate::ssh;

use super::FileEntry;

/// SFTP implementation of StorageBackend.
pub struct SftpBackend {
    sftp: SftpSession,
    cwd: String,
    #[allow(dead_code)]
    display_name: String,
}

/// Buffer size for SFTP file transfers (32 KB).
const SFTP_CHUNK_SIZE: usize = 32 * 1024;

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
        let mut remote_file = self
            .sftp
            .open(remote_path)
            .await
            .with_context(|| format!("Failed to open remote file: {remote_path}"))?;

        let mut local_file = tokio::fs::File::create(local_path)
            .await
            .with_context(|| format!("Failed to create: {}", local_path.display()))?;

        let mut buf = vec![0u8; SFTP_CHUNK_SIZE];
        loop {
            let n = remote_file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            local_file.write_all(&buf[..n]).await?;
        }

        Ok(())
    }

    pub async fn upload(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let mut local_file = tokio::fs::File::open(local_path)
            .await
            .with_context(|| format!("Failed to open: {}", local_path.display()))?;

        let mut remote_file = self
            .sftp
            .create(remote_path)
            .await
            .with_context(|| format!("Failed to create remote file: {remote_path}"))?;

        let mut buf = vec![0u8; SFTP_CHUNK_SIZE];
        loop {
            let n = local_file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            remote_file.write_all(&buf[..n]).await?;
        }

        remote_file.shutdown().await?;
        Ok(())
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
