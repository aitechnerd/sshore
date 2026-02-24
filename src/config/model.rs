// Validation functions and identity_file resolution are used in tests now
// and will be called from TUI form validation in Phase 3.
#![allow(dead_code)]

use std::collections::HashMap;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Characters forbidden in hostnames to prevent shell injection.
const SHELL_METACHARACTERS: &[char] = &[
    ';', '|', '&', '$', '`', '(', ')', '{', '}', '<', '>', '\n', '\r',
];

/// Characters allowed in bookmark names beyond alphanumeric.
const BOOKMARK_NAME_EXTRA_CHARS: &[char] = &['-', '_', '.'];

/// Map of environment name to color configuration.
pub type EnvColorMap = HashMap<String, EnvColor>;

/// Top-level application configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
}

/// Global application settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Default SSH username when not specified per-bookmark.
    pub default_user: Option<String>,

    /// Sort bookmarks by name in the TUI list.
    #[serde(default = "default_true")]
    pub sort_by_name: bool,

    /// Template for terminal tab title when connected.
    /// Supports placeholders: {name}, {host}, {user}, {env}, {badge}, {label}
    #[serde(default = "default_tab_title_template")]
    pub tab_title_template: String,

    /// Show the environment column in TUI list.
    #[serde(default = "default_true")]
    pub show_env_column: bool,

    /// Custom environment color definitions.
    #[serde(default = "default_env_colors")]
    pub env_colors: EnvColorMap,
}

/// Color and badge configuration for an environment tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvColor {
    /// Foreground color (hex, e.g. "#FFFFFF").
    pub fg: String,
    /// Background color (hex, e.g. "#CC0000").
    pub bg: String,
    /// Emoji badge for TUI list (e.g. "🔴").
    pub badge: String,
    /// Short text label (e.g. "PROD").
    pub label: String,
}

/// A saved SSH connection bookmark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Unique display name (e.g. "prod-web-01").
    pub name: String,

    /// Hostname or IP address.
    pub host: String,

    /// SSH username (falls back to settings.default_user, then current OS user).
    pub user: Option<String>,

    /// SSH port (default 22).
    #[serde(default = "default_port")]
    pub port: u16,

    /// Environment tier: "production", "staging", "development", "local", "testing", or custom.
    #[serde(default)]
    pub env: String,

    /// Searchable tags.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Path to SSH private key file (supports ~ expansion).
    pub identity_file: Option<String>,

    /// ProxyJump host (equivalent to ssh -J).
    pub proxy_jump: Option<String>,

    /// Free-form notes.
    pub notes: Option<String>,

    /// Last successful connection time (auto-updated).
    pub last_connected: Option<DateTime<Utc>>,

    /// Connection count (auto-updated).
    #[serde(default)]
    pub connect_count: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_user: None,
            sort_by_name: default_true(),
            tab_title_template: default_tab_title_template(),
            show_env_column: default_true(),
            env_colors: default_env_colors(),
        }
    }
}

impl Bookmark {
    /// Resolve the effective username: bookmark -> settings default -> OS user.
    pub fn effective_user(&self, settings: &Settings) -> String {
        self.user
            .clone()
            .or_else(|| settings.default_user.clone())
            .unwrap_or_else(|| whoami::username().to_string())
    }

    /// Resolve identity file path with tilde expansion.
    pub fn resolved_identity_file(&self) -> Option<String> {
        self.identity_file
            .as_ref()
            .map(|p| shellexpand::tilde(p).to_string())
    }
}

/// Validate that a bookmark name contains only alphanumeric chars, hyphens, underscores, and dots.
pub fn validate_bookmark_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Bookmark name cannot be empty");
    }

    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || BOOKMARK_NAME_EXTRA_CHARS.contains(&c))
    {
        bail!(
            "Bookmark name '{}' contains invalid characters (allowed: alphanumeric, -, _, .)",
            name
        );
    }

    Ok(())
}

/// Validate that a hostname does not contain shell metacharacters.
pub fn validate_hostname(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("Hostname cannot be empty");
    }

    if let Some(bad_char) = host.chars().find(|c| SHELL_METACHARACTERS.contains(c)) {
        bail!(
            "Hostname '{}' contains forbidden character '{}'",
            host,
            bad_char
        );
    }

    Ok(())
}

fn default_port() -> u16 {
    22
}

fn default_true() -> bool {
    true
}

fn default_tab_title_template() -> String {
    "{badge} {label} — {name}".to_string()
}

fn default_env_colors() -> EnvColorMap {
    let mut map = EnvColorMap::new();
    map.insert(
        "production".into(),
        EnvColor {
            fg: "#FFFFFF".into(),
            bg: "#CC0000".into(),
            badge: "🔴".into(),
            label: "PROD".into(),
        },
    );
    map.insert(
        "staging".into(),
        EnvColor {
            fg: "#000000".into(),
            bg: "#CCCC00".into(),
            badge: "🟡".into(),
            label: "STG".into(),
        },
    );
    map.insert(
        "development".into(),
        EnvColor {
            fg: "#FFFFFF".into(),
            bg: "#00AA00".into(),
            badge: "🟢".into(),
            label: "DEV".into(),
        },
    );
    map.insert(
        "local".into(),
        EnvColor {
            fg: "#FFFFFF".into(),
            bg: "#0066CC".into(),
            badge: "🔵".into(),
            label: "LOCAL".into(),
        },
    );
    map.insert(
        "testing".into(),
        EnvColor {
            fg: "#FFFFFF".into(),
            bg: "#AA00AA".into(),
            badge: "🟣".into(),
            label: "TEST".into(),
        },
    );
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bookmark() -> Bookmark {
        Bookmark {
            name: "prod-web-01".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "production".into(),
            tags: vec!["web".into(), "frontend".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
            proxy_jump: Some("bastion".into()),
            notes: Some("Primary web server".into()),
            last_connected: None,
            connect_count: 0,
        }
    }

    fn sample_config() -> AppConfig {
        AppConfig {
            settings: Settings {
                default_user: Some("admin".into()),
                ..Settings::default()
            },
            bookmarks: vec![sample_bookmark()],
        }
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = sample_config();
        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        let deserialized: AppConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_default_config_has_five_env_colors() {
        let config = AppConfig::default();
        assert_eq!(config.settings.env_colors.len(), 5);
        assert!(config.settings.env_colors.contains_key("production"));
        assert!(config.settings.env_colors.contains_key("staging"));
        assert!(config.settings.env_colors.contains_key("development"));
        assert!(config.settings.env_colors.contains_key("local"));
        assert!(config.settings.env_colors.contains_key("testing"));
    }

    #[test]
    fn test_default_config_values() {
        let config = AppConfig::default();
        assert!(config.settings.sort_by_name);
        assert!(config.settings.show_env_column);
        assert_eq!(
            config.settings.tab_title_template,
            "{badge} {label} — {name}"
        );
        assert!(config.settings.default_user.is_none());
        assert!(config.bookmarks.is_empty());
    }

    #[test]
    fn test_effective_user_from_bookmark() {
        let bookmark = Bookmark {
            user: Some("deploy".into()),
            ..sample_bookmark()
        };
        let settings = Settings {
            default_user: Some("admin".into()),
            ..Settings::default()
        };
        assert_eq!(bookmark.effective_user(&settings), "deploy");
    }

    #[test]
    fn test_effective_user_from_settings_default() {
        let bookmark = Bookmark {
            user: None,
            ..sample_bookmark()
        };
        let settings = Settings {
            default_user: Some("admin".into()),
            ..Settings::default()
        };
        assert_eq!(bookmark.effective_user(&settings), "admin");
    }

    #[test]
    fn test_effective_user_falls_back_to_os() {
        let bookmark = Bookmark {
            user: None,
            ..sample_bookmark()
        };
        let settings = Settings {
            default_user: None,
            ..Settings::default()
        };
        let result = bookmark.effective_user(&settings);
        // Should return the OS username — just verify it's non-empty
        assert!(!result.is_empty());
    }

    #[test]
    fn test_resolved_identity_file_tilde() {
        let bookmark = Bookmark {
            identity_file: Some("~/.ssh/id_ed25519".into()),
            ..sample_bookmark()
        };
        let resolved = bookmark.resolved_identity_file().unwrap();
        assert!(!resolved.starts_with('~'));
        assert!(resolved.ends_with("/.ssh/id_ed25519"));
    }

    #[test]
    fn test_resolved_identity_file_none() {
        let bookmark = Bookmark {
            identity_file: None,
            ..sample_bookmark()
        };
        assert!(bookmark.resolved_identity_file().is_none());
    }

    #[test]
    fn test_validate_bookmark_name_valid() {
        assert!(validate_bookmark_name("prod-web-01").is_ok());
        assert!(validate_bookmark_name("my_server.local").is_ok());
        assert!(validate_bookmark_name("bastion").is_ok());
        assert!(validate_bookmark_name("A-Z.test_123").is_ok());
    }

    #[test]
    fn test_validate_bookmark_name_invalid() {
        assert!(validate_bookmark_name("").is_err());
        assert!(validate_bookmark_name("my server").is_err());
        assert!(validate_bookmark_name("host;rm -rf").is_err());
        assert!(validate_bookmark_name("test@host").is_err());
        assert!(validate_bookmark_name("a/b").is_err());
    }

    #[test]
    fn test_validate_hostname_valid() {
        assert!(validate_hostname("example.com").is_ok());
        assert!(validate_hostname("10.0.1.5").is_ok());
        assert!(validate_hostname("host-name.test").is_ok());
        assert!(validate_hostname("::1").is_ok());
        assert!(validate_hostname("127.0.0.1").is_ok());
    }

    #[test]
    fn test_validate_hostname_invalid() {
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("host;evil").is_err());
        assert!(validate_hostname("host|pipe").is_err());
        assert!(validate_hostname("host&bg").is_err());
        assert!(validate_hostname("$(cmd)").is_err());
        assert!(validate_hostname("host`cmd`").is_err());
        assert!(validate_hostname("host\nevil").is_err());
    }

    #[test]
    fn test_deserialize_with_missing_fields() {
        let minimal_toml = r#"
            [settings]

            [[bookmarks]]
            name = "test"
            host = "example.com"
        "#;
        let config: AppConfig = toml::from_str(minimal_toml).expect("deserialize minimal");
        assert_eq!(config.bookmarks[0].port, 22);
        assert!(config.bookmarks[0].tags.is_empty());
        assert_eq!(config.bookmarks[0].connect_count, 0);
        assert!(config.bookmarks[0].env.is_empty());
        assert!(config.settings.sort_by_name);
        assert_eq!(config.settings.env_colors.len(), 5);
    }

    #[test]
    fn test_default_port() {
        let toml_str = r#"
            name = "test"
            host = "example.com"
        "#;
        let bookmark: Bookmark = toml::from_str(toml_str).expect("deserialize");
        assert_eq!(bookmark.port, 22);
    }
}
