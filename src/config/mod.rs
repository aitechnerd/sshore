pub mod env;
pub mod import_csv;
pub mod import_json;
pub mod import_mobaxterm;
pub mod import_putty;
pub mod import_securecrt;
pub mod import_tabby;
pub mod model;
pub mod ssh_import;
pub mod writer;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use std::collections::HashSet;

use crate::config::model::{AppConfig, Bookmark, Settings};
use crate::config::writer::atomic_write;

/// Return the XDG-compliant config file path.
///
/// - Linux/macOS: ~/.config/sshore/config.toml
/// - Windows: %APPDATA%\sshore\config.toml
pub fn config_path() -> PathBuf {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("sshore").join("config.toml")
}

/// Load config with an optional custom path override.
/// Priority: custom_path → XDG default.
pub fn load_with_override(custom_path: Option<&str>) -> Result<AppConfig> {
    let path = resolve_config_path(custom_path);
    load_from(&path)
}

/// Save config with an optional custom path override.
pub fn save_with_override(config: &AppConfig, custom_path: Option<&str>) -> Result<()> {
    let path = resolve_config_path(custom_path);
    save_to(config, &path)
}

/// Resolve the effective config path from an optional override.
fn resolve_config_path(custom_path: Option<&str>) -> PathBuf {
    match custom_path {
        Some(p) => PathBuf::from(shellexpand::tilde(p).to_string()),
        None => config_path(),
    }
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

    warn_duplicate_names(&config.bookmarks);

    Ok(config)
}

/// Save config to a specific path via atomic write.
pub fn save_to(config: &AppConfig, path: &Path) -> Result<()> {
    atomic_write(config, path)
}

/// Warn to stderr if any bookmarks share the same name.
fn warn_duplicate_names(bookmarks: &[Bookmark]) {
    let mut seen = HashSet::new();
    for b in bookmarks {
        if !seen.insert(&b.name) {
            eprintln!(
                "Warning: duplicate bookmark name '{}' — last occurrence will be used in lookups",
                b.name
            );
        }
    }
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

/// Supported import source formats.
///
/// This mirrors `cli::ImportSource` but lives in the library crate so the
/// config module can route imports without depending on the CLI crate.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportSourceKind {
    /// Auto-detect between ssh_config and sshore TOML (legacy behavior).
    Auto,
    /// OpenSSH config (~/.ssh/config).
    SshConfig,
    /// sshore TOML export file.
    Sshore,
    /// PuTTY registry export (.reg).
    Putty,
    /// MobaXterm session export (.mxtsessions).
    Mobaxterm,
    /// Tabby terminal config (config.yaml).
    Tabby,
    /// SecureCRT XML export.
    Securecrt,
    /// CSV file.
    Csv,
    /// JSON file.
    Json,
}

/// Import bookmarks from a file using the specified source format.
pub fn import_from_source(
    path: &Path,
    source: ImportSourceKind,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    match source {
        ImportSourceKind::Auto => {
            // Auto-detect: sshore TOML vs ssh_config (existing behavior)
            ssh_import::import_from_file(path)
        }
        ImportSourceKind::SshConfig => ssh_import::parse_ssh_config(path),
        ImportSourceKind::Sshore => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            let config: AppConfig =
                toml::from_str(&content).context("Failed to parse sshore TOML export file")?;
            Ok(config.bookmarks)
        }
        ImportSourceKind::Putty => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            import_putty::parse_putty_reg(&content, env_override, extra_tags)
        }
        ImportSourceKind::Mobaxterm => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            import_mobaxterm::parse_mxtsessions(&content, env_override, extra_tags)
        }
        ImportSourceKind::Tabby => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            import_tabby::parse_tabby_config(&content, env_override, extra_tags)
        }
        ImportSourceKind::Securecrt => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            import_securecrt::parse_securecrt_xml(&content, env_override, extra_tags)
        }
        ImportSourceKind::Csv => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            import_csv::parse_csv(&content, env_override, extra_tags)
        }
        ImportSourceKind::Json => {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read import file: {}", path.display()))?;
            import_json::parse_json_bookmarks(&content, env_override, extra_tags)
        }
    }
}

/// Export filtered bookmarks as a TOML string.
/// Passwords are NEVER exported (they live in OS keychain).
pub fn export_bookmarks(
    config: &AppConfig,
    env_filter: Option<&str>,
    tag_filters: &[String],
    name_pattern: Option<&str>,
    include_settings: bool,
) -> Result<String> {
    let filtered: Vec<&Bookmark> = config
        .bookmarks
        .iter()
        .filter(|b| {
            if let Some(env) = env_filter
                && !b.env.eq_ignore_ascii_case(env)
            {
                return false;
            }
            for tag in tag_filters {
                if !b.tags.contains(tag) {
                    return false;
                }
            }
            if let Some(pattern) = name_pattern
                && !glob_match(pattern, &b.name)
            {
                return false;
            }
            true
        })
        .collect();

    if filtered.is_empty() {
        anyhow::bail!("No bookmarks match the given filters");
    }

    let mut export_bookmarks: Vec<Bookmark> = filtered.into_iter().cloned().collect();

    // Scrub personal usage data
    for bookmark in &mut export_bookmarks {
        bookmark.last_connected = None;
        bookmark.connect_count = 0;
    }

    // Warn about dangling proxy_jump references
    let exported_names: HashSet<&str> = export_bookmarks.iter().map(|b| b.name.as_str()).collect();
    for bookmark in &export_bookmarks {
        if let Some(ref pj) = bookmark.proxy_jump
            && !exported_names.contains(pj.as_str())
        {
            eprintln!(
                "Warning: {} references proxy_jump \"{}\" which is not in the export.\n   \
                 The recipient will need to configure \"{}\" separately.",
                bookmark.name, pj, pj
            );
        }
    }

    let export_config = if include_settings {
        AppConfig {
            settings: config.settings.clone(),
            bookmarks: export_bookmarks,
        }
    } else {
        AppConfig {
            settings: Settings::default(),
            bookmarks: export_bookmarks,
        }
    };

    let toml_string =
        toml::to_string_pretty(&export_config).context("Failed to serialize config for export")?;

    let header = format!(
        "# sshore bookmark export\n\
         # Generated: {}\n\
         # Bookmarks: {}\n\
         # Passwords are stored in the OS keychain and are NOT included.\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        export_config.bookmarks.len(),
    );

    Ok(format!("{}{}", header, toml_string))
}

/// Simple glob matching: `*` matches any sequence of characters.
fn glob_match(pattern: &str, text: &str) -> bool {
    let regex_pattern = format!("^{}$", regex::escape(pattern).replace(r"\*", ".*"));
    regex::Regex::new(&regex_pattern)
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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
            on_connect: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::HashMap::new(),
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

    fn sample_bookmark(name: &str, env: &str, tags: Vec<String>) -> Bookmark {
        Bookmark {
            name: name.into(),
            host: format!("{name}.example.com"),
            user: Some("deploy".into()),
            port: 22,
            env: env.into(),
            tags,
            identity_file: None,
            proxy_jump: None,
            notes: None,
            last_connected: Some(Utc::now()),
            connect_count: 5,
            on_connect: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::HashMap::new(),
        }
    }

    fn sample_config_with_bookmarks() -> AppConfig {
        AppConfig {
            settings: Settings::default(),
            bookmarks: vec![
                sample_bookmark("prod-web-01", "production", vec!["web".into()]),
                sample_bookmark("prod-db-01", "production", vec!["db".into()]),
                sample_bookmark("staging-api", "staging", vec!["web".into(), "api".into()]),
                sample_bookmark("dev-local", "development", vec![]),
            ],
        }
    }

    #[test]
    fn test_export_filters_by_env() {
        let config = sample_config_with_bookmarks();
        let result = export_bookmarks(&config, Some("production"), &[], None, false).unwrap();
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("prod-db-01"));
        assert!(!result.contains("staging-api"));
        assert!(!result.contains("dev-local"));
    }

    #[test]
    fn test_export_filters_by_tag_and_logic() {
        let config = sample_config_with_bookmarks();
        // Filter by "web" tag — should match prod-web-01 and staging-api
        let result = export_bookmarks(&config, None, &["web".into()], None, false).unwrap();
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("staging-api"));
        assert!(!result.contains("prod-db-01"));

        // Filter by "web" AND "api" tags — only staging-api has both
        let result =
            export_bookmarks(&config, None, &["web".into(), "api".into()], None, false).unwrap();
        assert!(result.contains("staging-api"));
        assert!(!result.contains("prod-web-01"));
    }

    #[test]
    fn test_export_glob_pattern() {
        let config = sample_config_with_bookmarks();
        let result = export_bookmarks(&config, None, &[], Some("prod-*"), false).unwrap();
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("prod-db-01"));
        assert!(!result.contains("staging-api"));
    }

    #[test]
    fn test_export_strips_usage_data() {
        let config = sample_config_with_bookmarks();
        let result = export_bookmarks(&config, Some("production"), &[], None, false).unwrap();

        // Parse back to verify usage data is zeroed
        let exported: AppConfig = toml::from_str(
            result
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        )
        .unwrap();
        for b in &exported.bookmarks {
            assert!(b.last_connected.is_none());
            assert_eq!(b.connect_count, 0);
        }
    }

    #[test]
    fn test_export_no_matches_returns_error() {
        let config = sample_config_with_bookmarks();
        let result = export_bookmarks(&config, Some("nonexistent"), &[], None, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_export_include_settings() {
        let config = sample_config_with_bookmarks();
        let result = export_bookmarks(&config, None, &[], None, true).unwrap();
        // Should contain env_colors from default settings
        assert!(result.contains("env_colors"));
    }

    #[test]
    fn test_glob_match_patterns() {
        assert!(glob_match("prod-*", "prod-web-01"));
        assert!(glob_match("prod-*", "prod-db-01"));
        assert!(!glob_match("prod-*", "staging-api"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "not-exact"));
        assert!(glob_match("*-web-*", "prod-web-01"));
    }

    /// Canary test: config load and bookmark filtering must never make network calls.
    /// If this test hangs on CI (run with network disabled), a dependency added
    /// a startup network call — that's a regression.
    #[test]
    fn test_no_network_at_startup() {
        // Write a config to a temp file
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = AppConfig {
            settings: Settings::default(),
            bookmarks: vec![sample_bookmark("prod-web-01", "production", vec![])],
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &toml_str).unwrap();

        // Load config — should not make any network calls
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.bookmarks.len(), 1);

        // Filter bookmarks — should not make any network calls
        let _filtered: Vec<&Bookmark> = loaded
            .bookmarks
            .iter()
            .filter(|b| b.env == "production")
            .collect();

        // Export bookmarks — should not make any network calls
        let _exported = export_bookmarks(&loaded, Some("production"), &[], None, false).unwrap();

        // If we get here without hanging or DNS timeout, the test passes.
    }

    #[test]
    fn test_load_with_override_uses_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom.toml");

        // First load creates the file
        let config = load_with_override(Some(custom_path.to_str().unwrap())).unwrap();
        assert!(custom_path.exists());
        assert!(config.bookmarks.is_empty());
    }

    #[test]
    fn test_save_load_with_override_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom.toml");
        let custom_str = custom_path.to_str().unwrap();

        let mut config = AppConfig::default();
        config.settings.default_user = Some("override_user".into());
        save_with_override(&config, Some(custom_str)).unwrap();

        let loaded = load_with_override(Some(custom_str)).unwrap();
        assert_eq!(loaded.settings.default_user, Some("override_user".into()));
    }

    #[test]
    fn test_resolve_config_path_none_uses_default() {
        let path = resolve_config_path(None);
        assert!(path.to_string_lossy().contains("sshore"));
    }

    #[test]
    fn test_resolve_config_path_with_tilde() {
        let path = resolve_config_path(Some("~/my-sshore.toml"));
        assert!(!path.to_string_lossy().contains('~'));
        assert!(path.to_string_lossy().ends_with("my-sshore.toml"));
    }
}
