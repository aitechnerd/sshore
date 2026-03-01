use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A named command shortcut for a bookmark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snippet {
    /// Display name shown in the picker (e.g. "Tail app log").
    pub name: String,

    /// The command to execute (e.g. "tail -f /var/log/app/production.log").
    pub command: String,

    /// If true, press Enter automatically after injecting the command.
    /// Default: true. Set to false for commands the user may want to edit first.
    #[serde(default = "default_true")]
    pub auto_execute: bool,
}

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

    /// TUI color theme. Built-in: "tokyo-night", "catppuccin-mocha", "dracula", "default".
    /// Custom themes are loaded from `~/.config/sshore/themes/<name>.toml`.
    #[serde(default = "default_theme")]
    pub theme: String,

    /// Custom environment color definitions.
    #[serde(default = "default_env_colors")]
    pub env_colors: EnvColorMap,

    /// Escape sequence to trigger snippet picker during SSH session.
    /// Default: "~~" (double tilde).
    #[serde(default = "default_snippet_trigger")]
    pub snippet_trigger: String,

    /// Delay in milliseconds before sending on_connect command.
    /// Allows remote shell to initialize before injection.
    /// Default: 200
    #[serde(default = "default_on_connect_delay_ms")]
    pub on_connect_delay_ms: u64,

    /// Global snippets available in all SSH sessions.
    #[serde(default)]
    pub snippets: Vec<Snippet>,

    /// Escape sequence to trigger save-as-bookmark during SSH session.
    /// Default: "~b" (tilde-b, following OpenSSH's ~ escape convention).
    #[serde(default = "default_bookmark_trigger")]
    pub bookmark_trigger: String,

    /// Host key checking mode: "strict" (default), "accept-new", "off".
    /// - "strict": reject changed keys, prompt for unknown
    /// - "accept-new": auto-accept unknown keys, reject changed keys
    /// - "off": accept all keys (insecure, for testing only)
    #[serde(default = "default_host_key_checking")]
    pub host_key_checking: String,

    /// SSH connection timeout in seconds. Default: 15.
    /// Set higher for slow networks, lower for fast local connections.
    pub connect_timeout_secs: Option<u64>,

    /// Whether the first-run import wizard has been dismissed.
    /// Set to true after the user skips or completes the wizard.
    #[serde(default)]
    pub import_wizard_dismissed: bool,
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

    /// Command to run automatically after SSH session starts.
    /// Runs before interactive shell — the shell remains interactive after.
    /// Example: "cd /var/www/app && exec $SHELL"
    pub on_connect: Option<String>,

    /// Named command shortcuts for this bookmark.
    #[serde(default)]
    pub snippets: Vec<Snippet>,

    /// Connection timeout override for this specific bookmark (seconds).
    /// Falls back to settings.connect_timeout_secs, then 15s default.
    pub connect_timeout_secs: Option<u64>,

    /// Additional SSH options parsed from ssh_config but not modeled as
    /// dedicated fields. Applied at connection time.
    #[serde(default)]
    pub ssh_options: HashMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_user: None,
            sort_by_name: default_true(),
            tab_title_template: default_tab_title_template(),
            show_env_column: default_true(),
            theme: default_theme(),
            env_colors: default_env_colors(),
            snippet_trigger: default_snippet_trigger(),
            bookmark_trigger: default_bookmark_trigger(),
            on_connect_delay_ms: default_on_connect_delay_ms(),
            snippets: Vec::new(),
            host_key_checking: default_host_key_checking(),
            connect_timeout_secs: None,
            import_wizard_dismissed: false,
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

    /// Resolve identity file path with tilde AND environment variable expansion.
    /// Supports: ~/path, $HOME/path, ${SSHKEY}, $VAR/subpath
    /// Returns None if the field is not set.
    /// Returns Err if env var expansion fails (undefined variable).
    pub fn resolved_identity_file(&self) -> Option<Result<String>> {
        self.identity_file.as_ref().map(|p| {
            shellexpand::full(p)
                .map(|expanded| expanded.to_string())
                .map_err(|e| anyhow!("Failed to expand identity file path '{}': {}", p, e))
        })
    }
}

/// Expand shell variables and tilde in a string.
///
/// Returns `Err` if expansion fails (e.g., undefined environment variable),
/// consistent with `Bookmark::resolved_identity_file()`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn expand_path(input: &str) -> Result<String> {
    shellexpand::full(input)
        .map(|s| s.to_string())
        .map_err(|e| anyhow!("Variable expansion failed for '{}': {}", input, e))
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

/// Sanitize a string for use as a bookmark name.
///
/// Replaces spaces and invalid characters with hyphens, collapses consecutive
/// hyphens, and trims leading/trailing hyphens. Used by all importers.
pub fn sanitize_bookmark_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || BOOKMARK_NAME_EXTRA_CHARS.contains(&c) {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive hyphens
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_hyphen = false;
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    result.trim_matches('-').to_string()
}

fn default_port() -> u16 {
    22
}

fn default_true() -> bool {
    true
}

fn default_theme() -> String {
    "tokyo-night".to_string()
}

fn default_tab_title_template() -> String {
    "{badge} {label} — {name}".to_string()
}

fn default_snippet_trigger() -> String {
    "~~".to_string()
}

fn default_bookmark_trigger() -> String {
    "~b".to_string()
}

fn default_on_connect_delay_ms() -> u64 {
    200
}

fn default_host_key_checking() -> String {
    "strict".to_string()
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
            on_connect: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::HashMap::new(),
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
        let resolved = bookmark.resolved_identity_file().unwrap().unwrap();
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
    #[serial_test::serial]
    fn test_resolved_identity_file_env_var() {
        // SAFETY: serial_test ensures no concurrent access to environment variables
        unsafe { std::env::set_var("SSHORE_TEST_HOME_RESOLVE", "/mock/home") };
        let bookmark = Bookmark {
            identity_file: Some("$SSHORE_TEST_HOME_RESOLVE/.ssh/id_ed25519".into()),
            ..sample_bookmark()
        };
        let resolved = bookmark.resolved_identity_file().unwrap().unwrap();
        assert!(!resolved.starts_with('$'));
        assert!(resolved.ends_with("/.ssh/id_ed25519"));
        assert!(resolved.starts_with("/mock/home"));
        unsafe { std::env::remove_var("SSHORE_TEST_HOME_RESOLVE") };
    }

    #[test]
    fn test_resolved_identity_file_undefined_var_returns_error() {
        let bookmark = Bookmark {
            identity_file: Some("${SSHORE_NONEXISTENT_VAR_12345}/key".into()),
            ..sample_bookmark()
        };
        let result = bookmark.resolved_identity_file().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn test_expand_path_tilde() {
        let result = expand_path("~/test").unwrap();
        assert!(!result.starts_with('~'));
        assert!(result.ends_with("/test"));
    }

    #[test]
    #[serial_test::serial]
    fn test_expand_path_env_var() {
        // SAFETY: serial_test ensures no concurrent access to environment variables
        unsafe { std::env::set_var("SSHORE_TEST_HOME_EXPAND", "/mock/home") };
        let result = expand_path("$SSHORE_TEST_HOME_EXPAND/test").unwrap();
        assert!(!result.starts_with('$'));
        assert!(result.ends_with("/test"));
        assert!(result.starts_with("/mock/home"));
        unsafe { std::env::remove_var("SSHORE_TEST_HOME_EXPAND") };
    }

    #[test]
    fn test_expand_path_undefined_var_returns_error() {
        let result = expand_path("${SSHORE_NONEXISTENT_VAR_12345}/test");
        assert!(result.is_err());
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

    #[test]
    fn test_snippet_serde_roundtrip() {
        let snippet = Snippet {
            name: "Tail app log".into(),
            command: "tail -f /var/log/app/production.log".into(),
            auto_execute: true,
        };
        let toml_str = toml::to_string_pretty(&snippet).expect("serialize");
        let deserialized: Snippet = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(snippet, deserialized);
    }

    #[test]
    fn test_bookmark_with_snippets_roundtrip() {
        let bookmark = Bookmark {
            on_connect: Some("cd /var/www/app && exec $SHELL".into()),
            snippets: vec![
                Snippet {
                    name: "Tail log".into(),
                    command: "tail -f /var/log/app.log".into(),
                    auto_execute: true,
                },
                Snippet {
                    name: "Git status".into(),
                    command: "cd /var/www/app && git status".into(),
                    auto_execute: false,
                },
            ],
            ..sample_bookmark()
        };
        let toml_str = toml::to_string_pretty(&bookmark).expect("serialize");
        let deserialized: Bookmark = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(bookmark, deserialized);
    }

    #[test]
    fn test_bookmark_without_snippets_defaults() {
        let toml_str = r#"
            name = "test"
            host = "example.com"
        "#;
        let bookmark: Bookmark = toml::from_str(toml_str).expect("deserialize");
        assert!(bookmark.snippets.is_empty());
        assert!(bookmark.on_connect.is_none());
    }

    #[test]
    fn test_settings_with_global_snippets_roundtrip() {
        let settings = Settings {
            snippet_trigger: "~~".into(),
            on_connect_delay_ms: 300,
            snippets: vec![Snippet {
                name: "System info".into(),
                command: "uname -a && uptime".into(),
                auto_execute: true,
            }],
            ..Settings::default()
        };
        let toml_str = toml::to_string_pretty(&settings).expect("serialize");
        let deserialized: Settings = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(settings, deserialized);
    }

    #[test]
    fn test_settings_without_new_fields_defaults() {
        let toml_str = r#"
            sort_by_name = true
        "#;
        let settings: Settings = toml::from_str(toml_str).expect("deserialize");
        assert_eq!(settings.snippet_trigger, "~~");
        assert_eq!(settings.on_connect_delay_ms, 200);
        assert!(settings.snippets.is_empty());
    }

    #[test]
    fn test_snippet_auto_execute_defaults_true() {
        let toml_str = r#"
            name = "Test"
            command = "uptime"
        "#;
        let snippet: Snippet = toml::from_str(toml_str).expect("deserialize");
        assert!(snippet.auto_execute);
    }

    #[test]
    fn test_deserialize_with_missing_fields_includes_snippet_defaults() {
        let minimal_toml = r#"
            [settings]

            [[bookmarks]]
            name = "test"
            host = "example.com"
        "#;
        let config: AppConfig = toml::from_str(minimal_toml).expect("deserialize minimal");
        assert!(config.bookmarks[0].snippets.is_empty());
        assert!(config.bookmarks[0].on_connect.is_none());
        assert_eq!(config.settings.snippet_trigger, "~~");
        assert_eq!(config.settings.on_connect_delay_ms, 200);
        assert!(config.settings.snippets.is_empty());
    }

    #[test]
    fn test_sanitize_bookmark_name() {
        assert_eq!(sanitize_bookmark_name("My Server"), "My-Server");
        assert_eq!(sanitize_bookmark_name("server #1"), "server-1");
        assert_eq!(sanitize_bookmark_name("  spaces  "), "spaces");
        assert_eq!(sanitize_bookmark_name("a--b"), "a-b");
        assert_eq!(
            sanitize_bookmark_name("valid-name_01.test"),
            "valid-name_01.test"
        );
        assert_eq!(sanitize_bookmark_name(""), "");
        assert_eq!(sanitize_bookmark_name("---"), "");
    }
}
