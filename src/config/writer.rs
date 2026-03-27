use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::config::model::AppConfig;

/// Atomically write config to disk using tempfile-then-rename.
///
/// 1. Serialize to TOML
/// 2. Create parent directories if needed
/// 3. Write to a temp file in the same directory (ensures same filesystem)
/// 4. Set permissions to 0600 on Unix
/// 5. Atomic rename into place
pub fn atomic_write(config: &AppConfig, path: &Path) -> Result<()> {
    let toml_str = toml::to_string_pretty(config).context("Failed to serialize config to TOML")?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    let parent = path
        .parent()
        .context("Config path has no parent directory")?;

    let temp_file =
        NamedTempFile::new_in(parent).context("Failed to create temporary config file")?;

    fs::write(temp_file.path(), &toml_str).context("Failed to write config to temporary file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(temp_file.path(), perms)
            .context("Failed to set config file permissions")?;
    }

    // Before replacing the existing config, create a .bak backup.
    // If the backup fails, warn but do not block the write — losing the backup
    // is acceptable; losing the config is not.
    if path.exists() {
        let backup_path = path.with_extension("toml.bak");
        if let Err(e) = fs::copy(path, &backup_path) {
            eprintln!(
                "Warning: failed to create config backup at {}: {e}",
                backup_path.display()
            );
        }
    }

    temp_file
        .persist(path)
        .context("Failed to atomically replace config file")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::AppConfig;

    #[test]
    fn test_atomic_write_creates_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = AppConfig::default();

        atomic_write(&config, &path).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: AppConfig = toml::from_str(&content).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("config.toml");
        let config = AppConfig::default();

        atomic_write(&config, &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_atomic_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = AppConfig::default();
        config.settings.default_user = Some("testuser".into());
        config.bookmarks.push(crate::config::model::Bookmark {
            name: "test-server".into(),
            host: "example.com".into(),
            user: Some("deploy".into()),
            port: 2222,
            env: "production".into(),
            tags: vec!["web".into()],
            identity_file: Some("~/.ssh/id_rsa".into()),
            proxy_jump: None,
            notes: None,
            last_connected: None,
            connect_count: 5,
            on_connect: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::HashMap::new(),
            profile: None,
        });

        atomic_write(&config, &path).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: AppConfig = toml::from_str(&content).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn test_atomic_write_creates_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup_path = dir.path().join("config.toml.bak");

        // First write — no backup should be created
        let config1 = AppConfig::default();
        atomic_write(&config1, &path).unwrap();
        assert!(
            !backup_path.exists(),
            ".bak should not exist after first write"
        );

        // Second write — backup of first config should appear
        let mut config2 = AppConfig::default();
        config2.settings.default_user = Some("newuser".into());
        atomic_write(&config2, &path).unwrap();

        assert!(backup_path.exists(), ".bak should exist after second write");
    }

    #[test]
    fn test_atomic_write_first_write_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup_path = dir.path().join("config.toml.bak");

        let config = AppConfig::default();
        atomic_write(&config, &path).unwrap();

        assert!(!backup_path.exists());
    }

    #[test]
    fn test_atomic_write_backup_content_matches_previous() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup_path = dir.path().join("config.toml.bak");

        // Write config A
        let mut config_a = AppConfig::default();
        config_a.settings.default_user = Some("user_a".into());
        atomic_write(&config_a, &path).unwrap();

        // Write config B — backup should contain config A
        let mut config_b = AppConfig::default();
        config_b.settings.default_user = Some("user_b".into());
        atomic_write(&config_b, &path).unwrap();

        // Verify backup deserializes to config A
        let backup_content = fs::read_to_string(&backup_path).unwrap();
        let backup_parsed: AppConfig = toml::from_str(&backup_content).unwrap();
        assert_eq!(backup_parsed, config_a);

        // Verify main file contains config B
        let main_content = fs::read_to_string(&path).unwrap();
        let main_parsed: AppConfig = toml::from_str(&main_content).unwrap();
        assert_eq!(main_parsed, config_b);
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Write initial config
        let config1 = AppConfig::default();
        atomic_write(&config1, &path).unwrap();

        // Overwrite with different config
        let mut config2 = AppConfig::default();
        config2.settings.default_user = Some("newuser".into());
        atomic_write(&config2, &path).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: AppConfig = toml::from_str(&content).unwrap();
        assert_eq!(parsed.settings.default_user, Some("newuser".into()));
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_write_permissions_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = AppConfig::default();

        atomic_write(&config, &path).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
