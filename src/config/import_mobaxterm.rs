use std::collections::HashMap;

use anyhow::Result;

use crate::config::env::detect_env;
use crate::config::model::{Bookmark, sanitize_bookmark_name, validate_hostname};

/// MobaXterm SSH session type identifier.
const MOBA_SSH_TYPE: u16 = 109;

/// Parse a MobaXterm .mxtsessions file into sshore bookmarks.
///
/// The file uses an INI-like format where sessions are stored as packed
/// `%`-delimited strings under `[Bookmarks]` or `[Bookmarks_N]` sections.
/// The `SubRep` key in each section defines the folder name, which maps to a tag.
pub fn parse_mxtsessions(
    content: &str,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    let sessions = parse_sessions(content)?;

    let bookmarks: Vec<Bookmark> = sessions
        .into_iter()
        .filter(|s| {
            if validate_hostname(&s.host).is_err() {
                eprintln!(
                    "Warning: skipping MobaXterm session '{}': invalid hostname '{}'",
                    s.name, s.host
                );
                return false;
            }
            true
        })
        .map(|s| session_to_bookmark(s, env_override, extra_tags))
        .collect();

    Ok(bookmarks)
}

/// A parsed MobaXterm session.
#[derive(Debug)]
struct MobaSession {
    name: String,
    folder: String,
    host: String,
    port: u16,
    username: Option<String>,
}

/// Parse sessions from .mxtsessions INI-like content.
fn parse_sessions(content: &str) -> Result<Vec<MobaSession>> {
    let mut sessions = Vec::new();
    let mut current_folder = String::new();

    for line in content.lines() {
        let line = line.trim();

        // Section header: [Bookmarks] or [Bookmarks_N]
        if line.starts_with('[') && line.ends_with(']') {
            current_folder = String::new();
            continue;
        }

        // SubRep=FolderName — the folder/group name
        if let Some(folder) = line.strip_prefix("SubRep=") {
            current_folder = folder.trim().to_string();
            continue;
        }

        // Skip non-session lines
        if line.starts_with("ImgNum=") || line.is_empty() || !line.contains("=#") {
            continue;
        }

        // Session line: name=#type#flags%host%port%%%user%...
        if let Some((name, packed)) = line.split_once("=#")
            && let Some(session) = parse_moba_packed(name.trim(), packed, &current_folder)
        {
            sessions.push(session);
        }
    }

    Ok(sessions)
}

/// Parse the packed MobaXterm session string.
///
/// Format: `<type>#<flags>%<host>%<port>%%%<username>%...#<trailing>`
///
/// The data between the first `#` and the second `#` contains all fields
/// as `%`-delimited values. `flags` is at index 0.
fn parse_moba_packed(name: &str, packed: &str, folder: &str) -> Option<MobaSession> {
    // Split on '#' to get: [type, flags%host%port%%%user%..., trailing...]
    let parts: Vec<&str> = packed.splitn(3, '#').collect();
    if parts.len() < 2 {
        return None;
    }

    let session_type: u16 = parts[0].parse().unwrap_or(0);

    // Only import SSH sessions
    if session_type != MOBA_SSH_TYPE {
        return None;
    }

    // The field data is in parts[1]: flags%host%port%%%username%...
    let fields: Vec<&str> = parts[1].split('%').collect();

    // Field positions:
    // [0]=flags, [1]=host, [2]=port, [3]=empty, [4]=empty, [5]=username
    let host = fields.get(1).unwrap_or(&"").to_string();
    let port: u16 = fields.get(2).and_then(|p| p.parse().ok()).unwrap_or(22);
    let username = fields.get(5).and_then(|u| {
        let u = u.to_string();
        if u.is_empty() { None } else { Some(u) }
    });

    if host.is_empty() {
        return None;
    }

    Some(MobaSession {
        name: name.to_string(),
        folder: folder.to_string(),
        host,
        port,
        username,
    })
}

/// Convert a MobaXterm session to a sshore Bookmark.
fn session_to_bookmark(
    session: MobaSession,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Bookmark {
    let name = sanitize_bookmark_name(&session.name);

    // Use folder name in env detection for better results
    let detect_input = if session.folder.is_empty() {
        name.clone()
    } else {
        format!("{} {}", name, session.folder)
    };
    let env = env_override
        .map(String::from)
        .unwrap_or_else(|| detect_env(&detect_input, &session.host));

    let mut tags = extra_tags.to_vec();
    // Add folder as a tag if non-empty
    if !session.folder.is_empty() && !tags.contains(&session.folder) {
        tags.push(session.folder.clone());
    }
    if !tags.contains(&"mobaxterm-import".to_string()) {
        tags.push("mobaxterm-import".to_string());
    }

    Bookmark {
        name,
        host: session.host,
        user: session.username,
        port: session.port,
        env,
        tags,
        identity_file: None,
        proxy_jump: None,
        notes: Some(format!(
            "Imported from MobaXterm{}",
            if session.folder.is_empty() {
                String::new()
            } else {
                format!(" (folder: {})", session.folder)
            }
        )),
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
    fn test_parse_simple_mxtsessions() {
        let content = r#"[Bookmarks]
SubRep=
ImgNum=42

[Bookmarks_1]
SubRep=Production
ImgNum=41
prod-web-01=#109#0%10.0.1.5%22%%%deploy%-1%%%22%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        let b = &bookmarks[0];
        assert_eq!(b.name, "prod-web-01");
        assert_eq!(b.host, "10.0.1.5");
        assert_eq!(b.port, 22);
        assert_eq!(b.user, Some("deploy".into()));
    }

    #[test]
    fn test_parse_folder_as_tag() {
        let content = r#"[Bookmarks_1]
SubRep=Production
ImgNum=41
prod-web-01=#109#0%10.0.1.5%22%%%deploy%-1%%%22%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, None, &[]).unwrap();
        assert!(bookmarks[0].tags.contains(&"Production".to_string()));
        assert_eq!(bookmarks[0].env, "production");
    }

    #[test]
    fn test_skip_non_ssh_type() {
        let content = r#"[Bookmarks_1]
SubRep=
ImgNum=41
telnet-server=#91#0%10.0.1.5%23%%%%-1%%%23%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, None, &[]).unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_multiple_folders() {
        let content = r#"[Bookmarks_1]
SubRep=Production
ImgNum=41
prod-web=#109#0%10.0.1.5%22%%%deploy%-1%%%22%%0%0%0%%%-1%0#0#

[Bookmarks_2]
SubRep=Staging
ImgNum=41
staging-api=#109#0%10.0.2.10%22%%%deploy%-1%%%22%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert!(bookmarks[0].tags.contains(&"Production".to_string()));
        assert!(bookmarks[1].tags.contains(&"Staging".to_string()));
    }

    #[test]
    fn test_empty_folder_no_tag() {
        let content = r#"[Bookmarks]
SubRep=
ImgNum=42
myserver=#109#0%10.0.1.5%22%%%admin%-1%%%22%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, None, &[]).unwrap();
        // Should only have mobaxterm-import tag, no empty folder tag
        assert_eq!(bookmarks[0].tags, vec!["mobaxterm-import".to_string()]);
    }

    #[test]
    fn test_empty_host_skipped() {
        let content = r#"[Bookmarks_1]
SubRep=
ImgNum=41
empty=#109#0%%22%%%%-1%%%22%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, None, &[]).unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_env_override() {
        let content = r#"[Bookmarks_1]
SubRep=Production
ImgNum=41
myserver=#109#0%10.0.1.5%22%%%deploy%-1%%%22%%0%0%0%%%-1%0#0#
"#;
        let bookmarks = parse_mxtsessions(content, Some("testing"), &[]).unwrap();
        assert_eq!(bookmarks[0].env, "testing");
    }
}
