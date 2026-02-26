pub mod local_backend;
pub mod sftp_backend;

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

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

    /// Upload a local file to a remote path.
    pub async fn upload(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        match self {
            Backend::Local(b) => b.upload(local_path, remote_path).await,
            Backend::Sftp(b) => b.upload(local_path, remote_path).await,
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
}
