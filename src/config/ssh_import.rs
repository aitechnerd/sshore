use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::env::detect_env;
use crate::config::model::{AppConfig, Bookmark};

/// Default SSH port.
const DEFAULT_SSH_PORT: u16 = 22;

/// Result of merging imported bookmarks into existing config.
#[derive(Debug)]
pub struct ImportResult {
    pub imported: Vec<Bookmark>,
    /// Number of wildcard Host entries skipped during parsing.
    #[allow(dead_code)]
    pub skipped_wildcards: usize,
    pub already_existed: usize,
}

/// Intermediate representation while parsing an SSH config Host block.
#[derive(Default)]
struct HostBlock {
    name: Option<String>,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
    proxy_jump: Option<String>,
    on_connect: Option<String>,
    connect_timeout_secs: Option<u64>,
    ssh_options: std::collections::HashMap<String, String>,
}

impl HostBlock {
    /// Convert to a Bookmark if we have at least a name.
    fn into_bookmark(self) -> Option<Bookmark> {
        let name = self.name?;
        let host = self.hostname.unwrap_or_else(|| name.clone());
        let env = detect_env(&name, &host);

        Some(Bookmark {
            name,
            host,
            user: self.user,
            port: self.port.unwrap_or(DEFAULT_SSH_PORT),
            env,
            tags: vec![],
            identity_file: self.identity_file,
            proxy_jump: self.proxy_jump,
            notes: None,
            last_connected: None,
            connect_count: 0,
            on_connect: self.on_connect,
            snippets: vec![],
            connect_timeout_secs: self.connect_timeout_secs,
            ssh_options: self.ssh_options,
        })
    }
}

/// Import bookmarks from a file, auto-detecting format.
/// Supports: `~/.ssh/config` format (SSH config) and sshore TOML export files.
pub fn import_from_file(path: &Path) -> Result<Vec<Bookmark>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read import file: {}", path.display()))?;

    if is_sshore_toml(&content) {
        import_from_toml(&content)
    } else {
        parse_ssh_config(path)
    }
}

/// Detect whether the file content is a sshore TOML export (vs ssh_config).
fn is_sshore_toml(content: &str) -> bool {
    content.contains("[[bookmarks]]")
}

/// Parse a sshore TOML export file into bookmarks.
fn import_from_toml(content: &str) -> Result<Vec<Bookmark>> {
    let config: AppConfig =
        toml::from_str(content).context("Failed to parse sshore TOML export file")?;
    Ok(config.bookmarks)
}

/// Parse an SSH config file into a list of bookmarks.
///
/// Handles Host blocks, supported directives, wildcard skipping, and
/// recursive Include directives with circular-include protection.
pub fn parse_ssh_config(path: &Path) -> Result<Vec<Bookmark>> {
    let mut visited = HashSet::new();
    parse_ssh_config_recursive(path, &mut visited)
}

fn parse_ssh_config_recursive(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<Vec<Bookmark>> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    if !visited.insert(canonical.clone()) {
        // Circular include — silently skip
        return Ok(vec![]);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read SSH config: {}", path.display()))?;

    let parent_dir = path.parent().unwrap_or(Path::new("."));
    let mut bookmarks = Vec::new();
    let mut current_block: Option<HostBlock> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (directive, value) = match split_directive(trimmed) {
            Some(pair) => pair,
            None => continue,
        };

        match directive.to_lowercase().as_str() {
            "host" => {
                // Finalize previous block
                if let Some(block) = current_block.take()
                    && let Some(bookmark) = block.into_bookmark()
                {
                    bookmarks.push(bookmark);
                }

                // Use first alias only for multi-alias Host lines
                let first_alias = value.split_whitespace().next().unwrap_or("");

                // Skip wildcard entries
                if first_alias.contains('*') || first_alias.contains('?') {
                    current_block = None;
                } else {
                    current_block = Some(HostBlock {
                        name: Some(first_alias.to_string()),
                        ..HostBlock::default()
                    });
                }
            }
            "hostname" => {
                if let Some(ref mut block) = current_block {
                    block.hostname = Some(value.to_string());
                }
            }
            "user" => {
                if let Some(ref mut block) = current_block {
                    block.user = Some(value.to_string());
                }
            }
            "port" => {
                if let Some(ref mut block) = current_block
                    && let Ok(port) = value.parse::<u16>()
                {
                    block.port = Some(port);
                }
            }
            "identityfile" => {
                if let Some(ref mut block) = current_block {
                    block.identity_file = Some(value.to_string());
                }
            }
            "proxyjump" => {
                if let Some(ref mut block) = current_block {
                    block.proxy_jump = Some(value.to_string());
                }
            }
            // Phase 10: additional directives
            "connecttimeout" => {
                if let Some(ref mut block) = current_block {
                    block.connect_timeout_secs = value.parse().ok();
                }
            }
            "remotecommand" => {
                if let Some(ref mut block) = current_block {
                    block.on_connect = Some(value.to_string());
                }
            }
            "localforward"
            | "remoteforward"
            | "serveraliveinterval"
            | "serveralivecountmax"
            | "addkeystoagent"
            | "forwardagent"
            | "compression"
            | "stricthostkeychecking"
            | "requesttty" => {
                if let Some(ref mut block) = current_block {
                    block
                        .ssh_options
                        .insert(directive.to_string(), value.to_string());
                }
            }
            "include" => {
                let included = resolve_includes(value, parent_dir, visited)?;
                bookmarks.extend(included);
            }
            _ => {
                // Ignore unsupported directives
            }
        }
    }

    // Finalize last block
    if let Some(block) = current_block.take()
        && let Some(bookmark) = block.into_bookmark()
    {
        bookmarks.push(bookmark);
    }

    Ok(bookmarks)
}

/// Split an SSH config line into (directive, value).
fn split_directive(line: &str) -> Option<(&str, &str)> {
    // SSH config supports both "Directive value" and "Directive=value"
    let (directive, rest) = if let Some(eq_pos) = line.find('=') {
        let (d, v) = line.split_at(eq_pos);
        (d.trim(), v[1..].trim())
    } else {
        let mut parts = line.splitn(2, char::is_whitespace);
        let directive = parts.next()?.trim();
        let value = parts.next().map(|v| v.trim()).unwrap_or("");
        (directive, value)
    };

    if directive.is_empty() {
        return None;
    }

    Some((directive, rest))
}

/// Resolve Include directive paths (tilde expansion + glob expansion).
fn resolve_includes(
    pattern: &str,
    parent_dir: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<Vec<Bookmark>> {
    let expanded = shellexpand::tilde(pattern);
    let full_pattern = if Path::new(expanded.as_ref()).is_absolute() {
        expanded.to_string()
    } else {
        parent_dir
            .join(expanded.as_ref())
            .to_string_lossy()
            .to_string()
    };

    let mut bookmarks = Vec::new();

    let paths: Vec<PathBuf> = glob::glob(&full_pattern)
        .with_context(|| format!("Invalid Include glob pattern: {}", full_pattern))?
        .filter_map(|entry| entry.ok())
        .collect();

    for path in paths {
        if path.is_file() {
            let included = parse_ssh_config_recursive(&path, visited)?;
            bookmarks.extend(included);
        }
    }

    Ok(bookmarks)
}

/// Merge imported bookmarks into an existing bookmark list.
///
/// Skips bookmarks whose name already exists unless `overwrite` is true.
pub fn merge_imports(
    existing: &mut Vec<Bookmark>,
    imported: Vec<Bookmark>,
    overwrite: bool,
) -> ImportResult {
    let mut result = ImportResult {
        imported: Vec::new(),
        skipped_wildcards: 0,
        already_existed: 0,
    };

    let existing_names: HashSet<String> = existing.iter().map(|b| b.name.clone()).collect();

    for bookmark in imported {
        if existing_names.contains(&bookmark.name) {
            if overwrite {
                // Remove the old one and add the new one
                existing.retain(|b| b.name != bookmark.name);
                result.imported.push(bookmark.clone());
                existing.push(bookmark);
            } else {
                result.already_existed += 1;
            }
        } else {
            result.imported.push(bookmark.clone());
            existing.push(bookmark);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_ssh_config(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_parse_basic_host_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host prod-web-01
    HostName 10.0.1.5
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_ed25519
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);

        let b = &bookmarks[0];
        assert_eq!(b.name, "prod-web-01");
        assert_eq!(b.host, "10.0.1.5");
        assert_eq!(b.user, Some("deploy".into()));
        assert_eq!(b.port, 2222);
        assert_eq!(b.identity_file, Some("~/.ssh/id_ed25519".into()));
        assert_eq!(b.env, "production");
    }

    #[test]
    fn test_parse_multiple_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host web-server
    HostName web.example.com
    User admin

Host db-server
    HostName db.example.com
    User postgres
    Port 5432
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].name, "web-server");
        assert_eq!(bookmarks[1].name, "db-server");
    }

    #[test]
    fn test_parse_wildcard_skip() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host *
    ServerAliveInterval 60

Host prod-web
    HostName 10.0.1.5

Host *.example.com
    User admin
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "prod-web");
    }

    #[test]
    fn test_parse_hostname_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host myserver.example.com
    User deploy
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        // No HostName directive — Host name used as hostname
        assert_eq!(bookmarks[0].host, "myserver.example.com");
    }

    #[test]
    fn test_parse_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host minimal
    HostName example.com
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert!(bookmarks[0].user.is_none());
        assert_eq!(bookmarks[0].port, 22);
        assert!(bookmarks[0].identity_file.is_none());
        assert!(bookmarks[0].proxy_jump.is_none());
    }

    #[test]
    fn test_parse_port_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host myhost
    HostName example.com
    Port 8022
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks[0].port, 8022);
    }

    #[test]
    fn test_parse_comments_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
# This is a comment
Host myhost
    # Another comment
    HostName example.com
    User admin # inline text after value is included by SSH, but we handle it
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].host, "example.com");
    }

    #[test]
    fn test_parse_case_insensitive_directives() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
host myhost
    hostname example.com
    user admin
    port 2222
    identityfile ~/.ssh/id_rsa
    proxyjump bastion
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].host, "example.com");
        assert_eq!(bookmarks[0].user, Some("admin".into()));
        assert_eq!(bookmarks[0].port, 2222);
        assert_eq!(bookmarks[0].identity_file, Some("~/.ssh/id_rsa".into()));
        assert_eq!(bookmarks[0].proxy_jump, Some("bastion".into()));
    }

    #[test]
    fn test_parse_proxy_jump() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host internal
    HostName 10.0.0.5
    ProxyJump bastion.example.com
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks[0].proxy_jump, Some("bastion.example.com".into()));
    }

    #[test]
    fn test_parse_multi_alias_host_uses_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host myhost alias1 alias2
    HostName example.com
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "myhost");
    }

    #[test]
    fn test_parse_equals_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host myhost
    HostName=example.com
    User=admin
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks[0].host, "example.com");
        assert_eq!(bookmarks[0].user, Some("admin".into()));
    }

    #[test]
    fn test_parse_env_detection_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host staging-api
    HostName api.stg.example.com

Host bastion
    HostName jump.example.com
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks[0].env, "staging");
        assert_eq!(bookmarks[1].env, "");
    }

    #[test]
    fn test_include_basic() {
        let dir = tempfile::tempdir().unwrap();

        write_temp_ssh_config(
            dir.path(),
            "extra",
            r#"
Host included-host
    HostName included.example.com
"#,
        );

        let main_path = write_temp_ssh_config(
            dir.path(),
            "config",
            &format!(
                "Include {}\n\nHost main-host\n    HostName main.example.com\n",
                dir.path().join("extra").display()
            ),
        );

        let bookmarks = parse_ssh_config(&main_path).unwrap();
        assert_eq!(bookmarks.len(), 2);

        let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"included-host"));
        assert!(names.contains(&"main-host"));
    }

    #[test]
    fn test_include_glob() {
        let dir = tempfile::tempdir().unwrap();
        let conf_d = dir.path().join("config.d");
        fs::create_dir(&conf_d).unwrap();

        write_temp_ssh_config(
            &conf_d,
            "work.conf",
            r#"
Host work-server
    HostName work.example.com
"#,
        );

        write_temp_ssh_config(
            &conf_d,
            "personal.conf",
            r#"
Host personal-server
    HostName personal.example.com
"#,
        );

        let main_path = write_temp_ssh_config(
            dir.path(),
            "config",
            &format!("Include {}/*.conf\n", conf_d.display()),
        );

        let bookmarks = parse_ssh_config(&main_path).unwrap();
        assert_eq!(bookmarks.len(), 2);
    }

    #[test]
    fn test_include_circular_guard() {
        let dir = tempfile::tempdir().unwrap();

        // Create two files that include each other
        let path_a = dir.path().join("config_a");
        let path_b = dir.path().join("config_b");

        fs::write(
            &path_a,
            format!(
                "Include {}\n\nHost host-a\n    HostName a.example.com\n",
                path_b.display()
            ),
        )
        .unwrap();

        fs::write(
            &path_b,
            format!(
                "Include {}\n\nHost host-b\n    HostName b.example.com\n",
                path_a.display()
            ),
        )
        .unwrap();

        let bookmarks = parse_ssh_config(&path_a).unwrap();
        let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"host-a"));
        assert!(names.contains(&"host-b"));
        // No infinite loop
    }

    #[test]
    fn test_include_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();

        write_temp_ssh_config(
            &sub,
            "extra",
            r#"
Host relative-host
    HostName relative.example.com
"#,
        );

        let main_path = write_temp_ssh_config(&sub, "config", "Include extra\n");

        let bookmarks = parse_ssh_config(&main_path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "relative-host");
    }

    #[test]
    fn test_merge_no_overwrite() {
        let mut existing = vec![Bookmark {
            name: "server-a".into(),
            host: "old.example.com".into(),
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
        }];

        let imported = vec![
            Bookmark {
                name: "server-a".into(),
                host: "new.example.com".into(),
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
            },
            Bookmark {
                name: "server-b".into(),
                host: "b.example.com".into(),
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
            },
        ];

        let result = merge_imports(&mut existing, imported, false);
        assert_eq!(result.already_existed, 1);
        assert_eq!(result.imported.len(), 1);
        assert_eq!(existing.len(), 2);
        // server-a should still have old host
        assert_eq!(existing[0].host, "old.example.com");
    }

    #[test]
    fn test_merge_with_overwrite() {
        let mut existing = vec![Bookmark {
            name: "server-a".into(),
            host: "old.example.com".into(),
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
        }];

        let imported = vec![Bookmark {
            name: "server-a".into(),
            host: "new.example.com".into(),
            user: Some("newuser".into()),
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
        }];

        let result = merge_imports(&mut existing, imported, true);
        assert_eq!(result.already_existed, 0);
        assert_eq!(result.imported.len(), 1);
        assert_eq!(existing.len(), 1);
        // server-a should have new host
        assert_eq!(existing[0].host, "new.example.com");
        assert_eq!(existing[0].user, Some("newuser".into()));
    }

    #[test]
    fn test_parse_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(dir.path(), "config", "");

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_parse_only_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host *
    ServerAliveInterval 60
    ServerAliveCountMax 3
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_import_detects_toml_format() {
        assert!(is_sshore_toml("[[bookmarks]]\nname = \"test\""));
        assert!(!is_sshore_toml("Host myhost\n    HostName example.com"));
        assert!(!is_sshore_toml("# Just a comment"));
    }

    #[test]
    fn test_import_toml_parses_bookmarks() {
        let toml_content = r#"
[settings]

[[bookmarks]]
name = "web-server"
host = "10.0.1.5"
user = "deploy"
port = 22
env = "production"
tags = ["web"]
on_connect = "cd /var/www && exec $SHELL"

[[bookmarks.snippets]]
name = "Tail log"
command = "tail -f /var/log/app.log"

[[bookmarks.snippets]]
name = "Disk usage"
command = "df -h"
auto_execute = false

[[bookmarks]]
name = "db-server"
host = "10.0.1.6"
port = 5432
env = "staging"
"#;

        let bookmarks = import_from_toml(toml_content).unwrap();
        assert_eq!(bookmarks.len(), 2);

        let web = &bookmarks[0];
        assert_eq!(web.name, "web-server");
        assert_eq!(web.on_connect, Some("cd /var/www && exec $SHELL".into()));
        assert_eq!(web.snippets.len(), 2);
        assert_eq!(web.snippets[0].name, "Tail log");
        assert!(web.snippets[0].auto_execute);
        assert_eq!(web.snippets[1].name, "Disk usage");
        assert!(!web.snippets[1].auto_execute);

        let db = &bookmarks[1];
        assert_eq!(db.name, "db-server");
        assert_eq!(db.port, 5432);
        assert!(db.snippets.is_empty());
        assert!(db.on_connect.is_none());
    }

    #[test]
    fn test_import_from_file_auto_detects_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.toml");
        fs::write(
            &path,
            r#"
[[bookmarks]]
name = "from-toml"
host = "example.com"
port = 22
env = ""
"#,
        )
        .unwrap();

        let bookmarks = import_from_file(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "from-toml");
    }

    #[test]
    fn test_import_from_file_auto_detects_ssh_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            "Host from-ssh\n    HostName example.com\n",
        );

        let bookmarks = import_from_file(&path).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "from-ssh");
    }

    #[test]
    fn test_parse_connect_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host slow-vpn
    HostName slow.example.com
    ConnectTimeout 30
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(bookmarks[0].connect_timeout_secs, Some(30));
    }

    #[test]
    fn test_parse_remote_command_maps_to_on_connect() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host dev-server
    HostName dev.example.com
    RemoteCommand cd /var/www && exec $SHELL
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(
            bookmarks[0].on_connect,
            Some("cd /var/www && exec $SHELL".into())
        );
    }

    #[test]
    fn test_parse_additional_ssh_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host myhost
    HostName example.com
    ServerAliveInterval 60
    ServerAliveCountMax 3
    Compression yes
    ForwardAgent yes
    LocalForward 5432:localhost:5432
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        let opts = &bookmarks[0].ssh_options;
        assert_eq!(
            opts.get("ServerAliveInterval").map(String::as_str),
            Some("60")
        );
        assert_eq!(
            opts.get("ServerAliveCountMax").map(String::as_str),
            Some("3")
        );
        assert_eq!(opts.get("Compression").map(String::as_str), Some("yes"));
        assert_eq!(opts.get("ForwardAgent").map(String::as_str), Some("yes"));
        assert_eq!(
            opts.get("LocalForward").map(String::as_str),
            Some("5432:localhost:5432")
        );
    }

    #[test]
    fn test_parse_strict_host_key_checking_option() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_ssh_config(
            dir.path(),
            "config",
            r#"
Host myhost
    HostName example.com
    StrictHostKeyChecking accept-new
"#,
        );

        let bookmarks = parse_ssh_config(&path).unwrap();
        assert_eq!(
            bookmarks[0]
                .ssh_options
                .get("StrictHostKeyChecking")
                .map(String::as_str),
            Some("accept-new")
        );
    }
}
