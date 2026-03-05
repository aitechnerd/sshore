pub mod local_backend;
pub mod sftp_backend;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};

use anyhow::Result;
use chrono::{DateTime, Utc};

use russh_sftp::client::SftpSession;

use self::local_backend::LocalBackend;
use self::sftp_backend::SftpBackend;

/// A file entry returned by list operations.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub permissions: Option<String>,
}

/// Unified storage backend enum. Wraps concrete implementations
/// to allow the browser TUI to work with any backend type.
pub enum Backend {
    Local(LocalBackend),
    Sftp(SftpBackend),
}

#[allow(dead_code)]
impl Backend {
    /// Display name for the UI header.
    pub fn display_name(&self) -> &str {
        match self {
            Backend::Local(b) => b.display_name(),
            Backend::Sftp(b) => b.display_name(),
        }
    }

    /// Current working directory.
    pub fn cwd(&self) -> Result<String> {
        match self {
            Backend::Local(b) => b.cwd(),
            Backend::Sftp(b) => b.cwd(),
        }
    }

    /// List entries in a directory.
    pub async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        match self {
            Backend::Local(b) => b.list(path).await,
            Backend::Sftp(b) => b.list(path).await,
        }
    }

    /// Change directory.
    pub async fn cd(&mut self, path: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.cd(path).await,
            Backend::Sftp(b) => b.cd(path).await,
        }
    }

    /// Download a file to a local path.
    pub async fn download(&self, remote_path: &str, local_path: &Path) -> Result<()> {
        match self {
            Backend::Local(b) => b.download(remote_path, local_path).await,
            Backend::Sftp(b) => b.download(remote_path, local_path).await,
        }
    }

    /// Download with progress tracking and cancellation support.
    pub async fn download_with_progress(
        &self,
        remote_path: &str,
        local_path: &Path,
        progress: &AtomicU64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        match self {
            Backend::Local(b) => {
                b.download_with_progress(remote_path, local_path, Some(progress), Some(cancel))
                    .await
            }
            Backend::Sftp(b) => {
                b.download_with_progress(remote_path, local_path, Some(progress), Some(cancel))
                    .await
            }
        }
    }

    /// Upload a local file to a remote path.
    pub async fn upload(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.upload(local_path, remote_path).await,
            Backend::Sftp(b) => b.upload(local_path, remote_path).await,
        }
    }

    /// Upload with progress tracking and cancellation support.
    pub async fn upload_with_progress(
        &self,
        local_path: &Path,
        remote_path: &str,
        progress: &AtomicU64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        match self {
            Backend::Local(b) => {
                b.upload_with_progress(local_path, remote_path, Some(progress), Some(cancel))
                    .await
            }
            Backend::Sftp(b) => {
                b.upload_with_progress(local_path, remote_path, Some(progress), Some(cancel))
                    .await
            }
        }
    }

    /// Delete a file.
    pub async fn delete(&self, path: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.delete(path).await,
            Backend::Sftp(b) => b.delete(path).await,
        }
    }

    /// Delete a directory (recursive).
    pub async fn rmdir(&self, path: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.rmdir(path).await,
            Backend::Sftp(b) => b.rmdir(path).await,
        }
    }

    /// Create a directory.
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.mkdir(path).await,
            Backend::Sftp(b) => b.mkdir(path).await,
        }
    }

    /// Rename / move a file or directory.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.rename(from, to).await,
            Backend::Sftp(b) => b.rename(from, to).await,
        }
    }

    /// Open a new SFTP session on the existing SSH connection.
    /// Returns an error for local backends.
    pub async fn open_sftp_session(&self) -> Result<SftpSession> {
        match self {
            Backend::Local(_) => anyhow::bail!("Local backend has no SFTP session"),
            Backend::Sftp(b) => b.open_sftp_session().await,
        }
    }

    /// Get a reference to the SSH handle for spawning additional SFTP channels.
    /// Returns `None` for local backends or SFTP backends without a handle.
    pub fn ssh_handle(&self) -> Option<&russh::client::Handle<crate::ssh::client::SshoreHandler>> {
        match self {
            Backend::Local(_) => None,
            Backend::Sftp(b) => b.ssh_handle(),
        }
    }
}
