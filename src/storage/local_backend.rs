use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use super::FileEntry;

/// Local filesystem implementation of StorageBackend.
pub struct LocalBackend {
    cwd: PathBuf,
    #[allow(dead_code)]
    display_name: String,
}

impl LocalBackend {
    /// Create a new local backend starting at the given directory.
    pub fn new(start_dir: &str) -> Result<Self> {
        let cwd = PathBuf::from(shellexpand::tilde(start_dir).to_string())
            .canonicalize()
            .with_context(|| format!("Invalid local path: {start_dir}"))?;

        Ok(Self {
            cwd,
            display_name: "local".to_string(),
        })
    }
}

#[allow(dead_code)]
impl LocalBackend {
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn cwd(&self) -> Result<String> {
        Ok(self.cwd.to_string_lossy().to_string())
    }

    pub async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let dir = PathBuf::from(path);
        let mut entries = Vec::new();

        let mut read_dir = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("Failed to read directory: {}", dir.display()))?;

        while let Some(entry) = read_dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            let name = entry.file_name().to_string_lossy().to_string();
            let full_path = entry.path().to_string_lossy().to_string();
            let modified: Option<DateTime<Utc>> =
                metadata.modified().ok().map(DateTime::<Utc>::from);

            let permissions = format_local_permissions(&metadata);

            entries.push(FileEntry {
                name,
                path: full_path,
                is_dir: metadata.is_dir(),
                size: metadata.len(),
                modified,
                permissions: Some(permissions),
            });
        }

        Ok(entries)
    }

    pub async fn cd(&mut self, path: &str) -> Result<()> {
        let new_dir = if path == ".." {
            self.cwd.parent().unwrap_or(&self.cwd).to_path_buf()
        } else if PathBuf::from(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.cwd.join(path)
        };

        let canonical = new_dir
            .canonicalize()
            .with_context(|| format!("Invalid path: {}", new_dir.display()))?;

        if !canonical.is_dir() {
            anyhow::bail!("Not a directory: {}", canonical.display());
        }

        self.cwd = canonical;
        Ok(())
    }

    pub async fn download(&self, remote_path: &str, local_path: &Path) -> Result<()> {
        // For local backend, download is just a copy
        tokio::fs::copy(remote_path, local_path)
            .await
            .with_context(|| {
                format!("Failed to copy {} to {}", remote_path, local_path.display())
            })?;
        Ok(())
    }

    pub async fn upload(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        // For local backend, upload is just a copy
        tokio::fs::copy(local_path, remote_path)
            .await
            .with_context(|| {
                format!("Failed to copy {} to {}", local_path.display(), remote_path)
            })?;
        Ok(())
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("Failed to delete: {path}"))
    }

    pub async fn rmdir(&self, path: &str) -> Result<()> {
        tokio::fs::remove_dir_all(path)
            .await
            .with_context(|| format!("Failed to remove directory: {path}"))
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        tokio::fs::create_dir_all(path)
            .await
            .with_context(|| format!("Failed to create directory: {path}"))
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        tokio::fs::rename(from, to)
            .await
            .with_context(|| format!("Failed to rename {from} to {to}"))
    }
}

/// Format local file permissions as a string.
fn format_local_permissions(metadata: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        format_mode(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        String::new()
    }
}

/// Format a Unix mode as rwx string.
#[cfg(unix)]
fn format_mode(mode: u32) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_backend_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let backend = LocalBackend::new(dir.path().to_str().unwrap()).unwrap();
        let entries = backend.list(dir.path().to_str().unwrap()).await.unwrap();

        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"file.txt"));
        assert!(names.contains(&"subdir"));
    }

    #[tokio::test]
    async fn test_local_backend_cd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let mut backend = LocalBackend::new(dir.path().to_str().unwrap()).unwrap();
        backend.cd("sub").await.unwrap();
        assert!(backend.cwd().unwrap().ends_with("sub"));

        backend.cd("..").await.unwrap();
        assert_eq!(
            backend.cwd().unwrap(),
            dir.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[tokio::test]
    async fn test_local_backend_mkdir_rmdir() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(dir.path().to_str().unwrap()).unwrap();

        let new_dir = dir.path().join("newdir").to_string_lossy().to_string();
        backend.mkdir(&new_dir).await.unwrap();
        assert!(dir.path().join("newdir").is_dir());

        backend.rmdir(&new_dir).await.unwrap();
        assert!(!dir.path().join("newdir").exists());
    }

    #[tokio::test]
    async fn test_local_backend_rename() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("old.txt");
        let dst = dir.path().join("new.txt");
        std::fs::write(&src, "content").unwrap();

        let backend = LocalBackend::new(dir.path().to_str().unwrap()).unwrap();
        backend
            .rename(src.to_str().unwrap(), dst.to_str().unwrap())
            .await
            .unwrap();

        assert!(!src.exists());
        assert!(dst.exists());
    }

    #[tokio::test]
    async fn test_local_backend_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("visible"), "").unwrap();

        let backend = LocalBackend::new(dir.path().to_str().unwrap()).unwrap();
        let entries = backend.list(dir.path().to_str().unwrap()).await.unwrap();

        // Both should be listed — filtering is done at the TUI layer
        assert_eq!(entries.len(), 2);
    }
}
