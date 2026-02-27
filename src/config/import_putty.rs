use std::collections::HashMap;

use anyhow::Result;

use crate::config::env::detect_env;
use crate::config::model::{Bookmark, sanitize_bookmark_name, validate_hostname};

/// A parsed PuTTY session from a .reg file.
#[derive(Debug)]
struct PuttySession {
    name: String,
    hostname: String,
    port: u16,
    username: Option<String>,
    protocol: String,
    proxy_host: Option<String>,
    proxy_port: Option<u16>,
    proxy_username: Option<String>,
    identity_file: Option<String>,
}

/// Parse a PuTTY .reg export file and convert to sshore bookmarks.
///
/// Skips non-SSH sessions (telnet, serial) and sessions with empty hostnames
/// (like "Default Settings").
pub fn parse_putty_reg(
    content: &str,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    let sessions = parse_sessions(content)?;

    let bookmarks: Vec<Bookmark> = sessions
        .into_iter()
        .filter(|s| {
            if validate_hostname(&s.hostname).is_err() {
                eprintln!("Warning: skipping PuTTY session '{}': invalid hostname '{}'", s.name, s.hostname);
                return false;
            }
            true
        })
        .map(|s| session_to_bookmark(s, env_override, extra_tags))
        .collect();

    Ok(bookmarks)
}

/// Parse .reg content into intermediate PuTTY session structs.
fn parse_sessions(content: &str) -> Result<Vec<PuttySession>> {
    let mut sessions = Vec::new();
    let mut current: Option<PuttySession> = None;

    for line in content.lines() {
        let line = line.trim();

        // Skip header, blanks, comments
        if line.is_empty()
            || line.starts_with("Windows Registry Editor")
            || line.starts_with("REGEDIT")
            || line.starts_with(';')
        {
            // Save previous session before moving on
            if line.is_empty()
                && let Some(session) = current.take()
                && is_valid_ssh_session(&session)
            {
                sessions.push(session);
            }
            continue;
        }

        // Section header: [HKEY_CURRENT_USER\...\Sessions\<name>]
        if line.starts_with('[') && line.ends_with(']') {
            // Save previous session
            if let Some(session) = current.take()
                && is_valid_ssh_session(&session)
            {
                sessions.push(session);
            }

            // Extract session name from registry path
            let path = &line[1..line.len() - 1];
            // Only parse PuTTY Sessions keys
            if let Some(sessions_pos) = path.rfind("\\Sessions\\") {
                let name_part = &path[sessions_pos + "\\Sessions\\".len()..];
                let name = url_decode(name_part);
                current = Some(PuttySession {
                    name,
                    hostname: String::new(),
                    port: 22,
                    username: None,
                    protocol: "ssh".to_string(),
                    proxy_host: None,
                    proxy_port: None,
                    proxy_username: None,
                    identity_file: None,
                });
            }
            continue;
        }

        // Key-value pair within a session
        if let Some(session) = current.as_mut()
            && let Some((key, value)) = parse_reg_line(line)
        {
            match key.as_str() {
                "HostName" => session.hostname = value,
                "PortNumber" => {
                    session.port = parse_dword(&value).unwrap_or(22) as u16;
                }
                "UserName" => session.username = non_empty(value),
                "Protocol" => session.protocol = value,
                "ProxyHost" => session.proxy_host = non_empty(value),
                "ProxyPort" => {
                    session.proxy_port = parse_dword(&value).map(|v| v as u16);
                }
                "ProxyUsername" => session.proxy_username = non_empty(value),
                "PublicKeyFile" => {
                    // Convert Windows path to Unix-style
                    session.identity_file = non_empty(value).map(|p| p.replace('\\', "/"));
                }
                _ => {}
            }
        }
    }

    // Don't forget the last session
    if let Some(session) = current
        && is_valid_ssh_session(&session)
    {
        sessions.push(session);
    }

    Ok(sessions)
}

/// Check if a session is a valid SSH session worth importing.
fn is_valid_ssh_session(session: &PuttySession) -> bool {
    !session.hostname.is_empty() && session.protocol == "ssh"
}

/// Parse a .reg value line like `"Key"="Value"` or `"Key"=dword:XXXXXXXX`.
fn parse_reg_line(line: &str) -> Option<(String, String)> {
    // Format: "Key"="Value" or "Key"=dword:XXXXXXXX
    if !line.starts_with('"') {
        return None;
    }

    // Find the closing quote of the key
    let key_end = line[1..].find('"')?;
    let key = line[1..key_end + 1].to_string();

    // After key closing quote, expect "="
    let rest = &line[key_end + 2..];
    if !rest.starts_with('=') {
        return None;
    }
    let value_part = &rest[1..];

    let value = if value_part.starts_with("dword:") {
        // Keep as-is for parse_dword to handle
        value_part.to_string()
    } else if value_part.starts_with('"') && value_part.ends_with('"') && value_part.len() >= 2 {
        // Quoted string — strip quotes and unescape backslashes
        value_part[1..value_part.len() - 1]
            .replace("\\\\", "\\")
            .to_string()
    } else {
        value_part.to_string()
    };

    Some((key, value))
}

/// Parse `dword:XXXXXXXX` to u32.
fn parse_dword(s: &str) -> Option<u32> {
    s.strip_prefix("dword:")
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
}

/// URL-decode PuTTY session names (%20 → space, etc.).
fn url_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .to_string()
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Convert a PuTTY session to a sshore Bookmark.
fn session_to_bookmark(
    session: PuttySession,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Bookmark {
    let name = sanitize_bookmark_name(&session.name);
    let env = env_override
        .map(String::from)
        .unwrap_or_else(|| detect_env(&name, &session.hostname));

    let proxy_jump = session.proxy_host.as_ref().map(|host| {
        let mut proxy = String::new();
        if let Some(user) = &session.proxy_username {
            proxy.push_str(user);
            proxy.push('@');
        }
        proxy.push_str(host);
        if let Some(port) = session.proxy_port
            && port != 22
        {
            proxy.push(':');
            proxy.push_str(&port.to_string());
        }
        proxy
    });

    let mut tags = extra_tags.to_vec();
    // Add "putty-import" tag for provenance
    if !tags.contains(&"putty-import".to_string()) {
        tags.push("putty-import".to_string());
    }

    Bookmark {
        name,
        host: session.hostname,
        user: session.username,
        port: session.port,
        env,
        tags,
        identity_file: session.identity_file,
        proxy_jump,
        notes: Some(format!("Imported from PuTTY session: {}", session.name)),
        last_connected: None,
        connect_count: 0,
        on_connect: None,
        snippets: vec![],
        ssh_options: HashMap::new(),
        connect_timeout_secs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_reg() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\prod-web-01]
"HostName"="10.0.1.5"
"PortNumber"=dword:00000016
"UserName"="deploy"
"Protocol"="ssh"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        let b = &bookmarks[0];
        assert_eq!(b.name, "prod-web-01");
        assert_eq!(b.host, "10.0.1.5");
        assert_eq!(b.port, 22);
        assert_eq!(b.user, Some("deploy".into()));
        assert_eq!(b.env, "production");
    }

    #[test]
    fn test_parse_multi_session_reg() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\prod-web-01]
"HostName"="10.0.1.5"
"PortNumber"=dword:00000016
"UserName"="deploy"
"Protocol"="ssh"

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\staging-api]
"HostName"="10.0.2.10"
"PortNumber"=dword:00000016
"UserName"="deploy"
"Protocol"="ssh"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].name, "prod-web-01");
        assert_eq!(bookmarks[1].name, "staging-api");
    }

    #[test]
    fn test_parse_dword_port() {
        assert_eq!(parse_dword("dword:00000016"), Some(22));
        assert_eq!(parse_dword("dword:00000050"), Some(80));
        assert_eq!(parse_dword("dword:00001f90"), Some(8080));
    }

    #[test]
    fn test_url_decode_session_name() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\My%20Server%20%231]
"HostName"="10.0.1.5"
"Protocol"="ssh"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        // "My Server #1" → sanitized to "My-Server-1"
        assert_eq!(bookmarks[0].name, "My-Server-1");
    }

    #[test]
    fn test_skip_non_ssh() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\telnet-server]
"HostName"="10.0.1.5"
"Protocol"="telnet"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_skip_default_settings() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\Default%20Settings]
"HostName"=""
"Protocol"="ssh"

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\real-server]
"HostName"="10.0.1.5"
"Protocol"="ssh"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "real-server");
    }

    #[test]
    fn test_windows_path_identity() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\myserver]
"HostName"="10.0.1.5"
"Protocol"="ssh"
"PublicKeyFile"="C:\\Users\\sergey\\.ssh\\id_ed25519"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert_eq!(
            bookmarks[0].identity_file,
            Some("C:/Users/sergey/.ssh/id_ed25519".into())
        );
    }

    #[test]
    fn test_proxy_host_mapping() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\internal]
"HostName"="10.0.0.5"
"Protocol"="ssh"
"ProxyHost"="bastion.example.com"
"ProxyPort"=dword:00000d18
"ProxyUsername"="admin"
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        assert_eq!(
            bookmarks[0].proxy_jump,
            Some("admin@bastion.example.com:3352".into())
        );
    }

    #[test]
    fn test_proxy_host_default_port() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\internal]
"HostName"="10.0.0.5"
"Protocol"="ssh"
"ProxyHost"="bastion.example.com"
"ProxyPort"=dword:00000016
"#;
        let bookmarks = parse_putty_reg(content, None, &[]).unwrap();
        // Port 22 should be omitted from proxy_jump
        assert_eq!(bookmarks[0].proxy_jump, Some("bastion.example.com".into()));
    }

    #[test]
    fn test_env_override() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\myserver]
"HostName"="10.0.1.5"
"Protocol"="ssh"
"#;
        let bookmarks = parse_putty_reg(content, Some("staging"), &[]).unwrap();
        assert_eq!(bookmarks[0].env, "staging");
    }

    #[test]
    fn test_extra_tags() {
        let content = r#"Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\myserver]
"HostName"="10.0.1.5"
"Protocol"="ssh"
"#;
        let bookmarks =
            parse_putty_reg(content, None, &["windows".into(), "legacy".into()]).unwrap();
        assert!(bookmarks[0].tags.contains(&"windows".to_string()));
        assert!(bookmarks[0].tags.contains(&"legacy".to_string()));
        assert!(bookmarks[0].tags.contains(&"putty-import".to_string()));
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
    }

    #[test]
    fn test_parse_reg_line_string() {
        let (key, value) = parse_reg_line(r#""HostName"="10.0.1.5""#).unwrap();
        assert_eq!(key, "HostName");
        assert_eq!(value, "10.0.1.5");
    }

    #[test]
    fn test_parse_reg_line_dword() {
        let (key, value) = parse_reg_line(r#""PortNumber"=dword:00000016"#).unwrap();
        assert_eq!(key, "PortNumber");
        assert_eq!(value, "dword:00000016");
    }

    #[test]
    fn test_parse_reg_line_escaped_backslash() {
        let (key, value) =
            parse_reg_line(r#""PublicKeyFile"="C:\\Users\\sergey\\.ssh\\id_ed25519""#).unwrap();
        assert_eq!(key, "PublicKeyFile");
        assert_eq!(value, "C:\\Users\\sergey\\.ssh\\id_ed25519");
    }
}
