pub mod env;
pub mod model;
pub mod ssh_import;
pub mod writer;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::model::AppConfig;
use crate::config::writer::atomic_write;

/// Return the XDG-compliant config file path.
///
/// - Linux/macOS: ~/.config/sshore/config.toml
/// - Windows: %APPDATA%\sshore\config.toml
pub fn config_path() -> PathBuf {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("sshore").join("config.toml")
}

/// Load config from the default XDG path.
pub fn load() -> Result<AppConfig> {
    load_from(&config_path())
}

/// Save config to the default XDG path.
pub fn save(config: &AppConfig) -> Result<()> {
    save_to(config, &config_path())
}

/// Load config from a specific path.
///
/// If the file doesn't exist, creates a default config, saves it, and prints
/// a welcome message suggesting `sshore import`.
pub fn load_from(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        let config = AppConfig::default();
        save_to(&config, path)?;
        eprintln!(
            "Created default config at {}\nTip: run `sshore import` to import from ~/.ssh/config",
            path.display()
        );
        return Ok(config);
    }

    check_permissions(path);

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    Ok(config)
}

/// Save config to a specific path via atomic write.
pub fn save_to(config: &AppConfig, path: &Path) -> Result<()> {
    atomic_write(config, path)
}

/// Warn to stderr if the config file has permissions wider than 0600 on Unix.
#[cfg(unix)]
fn check_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            eprintln!(
                "Warning: config file {} has permissions {:o} (expected 600)",
                path.display(),
                mode
            );
        }
    }
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) {
    // No permission checking on non-Unix platforms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_path_contains_sshore() {
        let path = config_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("sshore"));
        assert!(path_str.ends_with("config.toml"));
    }

    #[test]
    fn test_load_creates_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sshore").join("config.toml");

        let config = load_from(&path).unwrap();

        assert!(path.exists());
        assert!(config.bookmarks.is_empty());
        assert_eq!(config.settings.env_colors.len(), 5);
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = AppConfig::default();
        config.settings.default_user = Some("testuser".into());
        save_to(&config, &path).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn test_load_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = AppConfig::default();
        config.bookmarks.push(model::Bookmark {
            name: "test".into(),
            host: "example.com".into(),
            user: None,
            port: 22,
            env: String::new(),
            tags: vec![],
            identity_file: None,
            proxy_jump: None,
            notes: None,
            last_connected: None,
            connect_count: 0,
        });
        save_to(&config, &path).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.bookmarks.len(), 1);
        assert_eq!(loaded.bookmarks[0].name, "test");
    }

    #[cfg(unix)]
    #[test]
    fn test_permissions_warning_for_wide_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = AppConfig::default();
        save_to(&config, &path).unwrap();

        // Widen permissions — check_permissions should warn (to stderr)
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        // Just verify it doesn't panic; the warning goes to stderr
        check_permissions(&path);
    }
}
