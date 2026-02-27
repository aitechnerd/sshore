use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

use crate::config::env::detect_env;
use crate::config::model::{Bookmark, sanitize_bookmark_name, validate_hostname};

/// A JSON bookmark with flexible/optional fields.
/// Missing fields get sensible defaults.
#[derive(Deserialize)]
struct JsonBookmark {
    name: String,
    host: Option<String>,
    #[serde(alias = "hostname")]
    hostname_alias: Option<String>,
    #[serde(alias = "username")]
    user: Option<String>,
    port: Option<u16>,
    env: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    identity_file: Option<String>,
    proxy_jump: Option<String>,
    notes: Option<String>,
}

/// Wrapper format: `{ "bookmarks": [...] }`
#[derive(Deserialize)]
struct JsonWrapper {
    bookmarks: Vec<JsonBookmark>,
}

/// Parse a JSON file into sshore bookmarks.
///
/// Accepts both a bare JSON array of bookmarks and a wrapped
/// `{"bookmarks": [...]}` format. Only `name` is strictly required;
/// `host` can also be specified as `hostname`.
pub fn parse_json_bookmarks(
    content: &str,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    let content = content.trim();

    if content.is_empty() {
        anyhow::bail!("File is empty, nothing to import");
    }

    // Try bare array first
    let json_bookmarks = if content.starts_with('[') {
        serde_json::from_str::<Vec<JsonBookmark>>(content)
            .map_err(|e| anyhow::anyhow!("Failed to parse JSON array: {}", e))?
    } else if content.starts_with('{') {
        // Try wrapped format
        let wrapper = serde_json::from_str::<JsonWrapper>(content)
            .map_err(|e| anyhow::anyhow!("Failed to parse JSON: {}", e))?;
        wrapper.bookmarks
    } else {
        anyhow::bail!("JSON must be an array of bookmarks or {{ \"bookmarks\": [...] }}");
    };

    let bookmarks: Vec<Bookmark> = json_bookmarks
        .into_iter()
        .filter_map(|jb| json_to_bookmark(jb, env_override, extra_tags))
        .collect();

    Ok(bookmarks)
}

/// Convert a JSON bookmark to a sshore Bookmark.
fn json_to_bookmark(
    jb: JsonBookmark,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Option<Bookmark> {
    let name = sanitize_bookmark_name(&jb.name);
    if name.is_empty() {
        return None;
    }

    let host = match jb.host.or(jb.hostname_alias) {
        Some(h) if !h.is_empty() => h,
        _ => {
            eprintln!("Warning: skipping JSON bookmark '{}': missing host field", name);
            return None;
        }
    };

    if validate_hostname(&host).is_err() {
        eprintln!("Warning: skipping JSON bookmark '{}': invalid hostname '{}'", name, host);
        return None;
    }

    let env = env_override
        .map(String::from)
        .or(jb.env)
        .unwrap_or_else(|| detect_env(&name, &host));

    let mut tags = jb.tags;
    tags.extend(extra_tags.iter().cloned());
    if !tags.contains(&"json-import".to_string()) {
        tags.push("json-import".to_string());
    }

    Some(Bookmark {
        name,
        host,
        user: jb.user,
        port: jb.port.unwrap_or(22),
        env,
        tags,
        identity_file: jb.identity_file,
        proxy_jump: jb.proxy_jump,
        notes: jb.notes,
        last_connected: None,
        connect_count: 0,
        on_connect: None,
        snippets: vec![],
        ssh_options: HashMap::new(),
        connect_timeout_secs: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_array() {
        let json = r#"[
            {
                "name": "prod-web-01",
                "host": "10.0.1.5",
                "user": "deploy",
                "port": 22,
                "env": "production",
                "tags": ["web", "frontend"],
                "identity_file": "~/.ssh/id_ed25519",
                "proxy_jump": "bastion",
                "notes": "Primary web server"
            },
            {
                "name": "staging-api",
                "host": "10.0.2.10",
                "user": "deploy",
                "port": 22,
                "env": "staging"
            }
        ]"#;

        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);

        let b0 = &bookmarks[0];
        assert_eq!(b0.name, "prod-web-01");
        assert_eq!(b0.host, "10.0.1.5");
        assert_eq!(b0.user, Some("deploy".into()));
        assert_eq!(b0.env, "production");
        assert!(b0.tags.contains(&"web".to_string()));
        assert!(b0.tags.contains(&"frontend".to_string()));
        assert_eq!(b0.identity_file, Some("~/.ssh/id_ed25519".into()));
        assert_eq!(b0.proxy_jump, Some("bastion".into()));
    }

    #[test]
    fn test_parse_wrapped() {
        let json = r#"{
            "bookmarks": [
                {
                    "name": "myserver",
                    "host": "10.0.1.5"
                }
            ]
        }"#;

        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "myserver");
    }

    #[test]
    fn test_missing_optional_fields() {
        let json = r#"[{"name": "myserver", "host": "10.0.1.5"}]"#;

        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].port, 22);
        assert!(bookmarks[0].user.is_none());
        assert!(bookmarks[0].identity_file.is_none());
        assert!(bookmarks[0].proxy_jump.is_none());
        assert!(bookmarks[0].notes.is_none());
    }

    #[test]
    fn test_env_auto_detect() {
        let json = r#"[{"name": "prod-web-01", "host": "10.0.1.5"}]"#;

        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks[0].env, "production");
    }

    #[test]
    fn test_env_override() {
        let json = r#"[{"name": "prod-web-01", "host": "10.0.1.5", "env": "production"}]"#;

        let bookmarks = parse_json_bookmarks(json, Some("staging"), &[]).unwrap();
        assert_eq!(bookmarks[0].env, "staging");
    }

    #[test]
    fn test_empty_file() {
        let result = parse_json_bookmarks("", None, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_json() {
        let result = parse_json_bookmarks("not json at all", None, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostname_alias() {
        let json = r#"[{"name": "myserver", "hostname": "10.0.1.5"}]"#;
        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks[0].host, "10.0.1.5");
    }

    #[test]
    fn test_extra_tags() {
        let json = r#"[{"name": "myserver", "host": "10.0.1.5", "tags": ["web"]}]"#;
        let bookmarks = parse_json_bookmarks(json, None, &["imported".into()]).unwrap();
        assert!(bookmarks[0].tags.contains(&"web".to_string()));
        assert!(bookmarks[0].tags.contains(&"imported".to_string()));
        assert!(bookmarks[0].tags.contains(&"json-import".to_string()));
    }

    #[test]
    fn test_skip_hostname_with_shell_metacharacters() {
        let json = r#"[
            {"name": "good", "host": "10.0.1.5"},
            {"name": "bad", "host": "host;evil"},
            {"name": "also-bad", "host": "$(whoami).evil.com"},
            {"name": "fine", "host": "example.com"}
        ]"#;
        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].host, "10.0.1.5");
        assert_eq!(bookmarks[1].host, "example.com");
    }

    #[test]
    fn test_skip_missing_host() {
        let json = r#"[
            {"name": "no-host"},
            {"name": "has-host", "host": "10.0.1.5"}
        ]"#;
        let bookmarks = parse_json_bookmarks(json, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "has-host");
    }
}
