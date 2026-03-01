use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::env::detect_env;
use crate::config::model::{Bookmark, sanitize_bookmark_name, validate_hostname};

#[derive(Deserialize)]
struct TabbyConfig {
    #[serde(default)]
    profiles: Vec<TabbyProfile>,
    #[serde(default)]
    groups: Vec<TabbyGroup>,
}

#[derive(Deserialize)]
struct TabbyGroup {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct TabbyProfile {
    #[serde(rename = "type")]
    profile_type: Option<String>,
    name: Option<String>,
    group: Option<String>,
    #[serde(default)]
    options: TabbyOptions,
    id: Option<String>,
}

#[derive(Deserialize, Default)]
struct TabbyOptions {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    #[serde(rename = "privateKeys")]
    private_keys: Option<Vec<String>>,
    #[serde(rename = "jumpHost")]
    jump_host: Option<String>,
}

/// Parse Tabby config.yaml into sshore bookmarks.
///
/// Filters to SSH profiles only, resolves jump host UUID references between
/// profiles, and maps group names to tags.
pub fn parse_tabby_config(
    content: &str,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    let config: TabbyConfig =
        serde_yaml::from_str(content).context("Failed to parse Tabby config.yaml")?;

    // Build group UUID → name lookup map
    let group_map: HashMap<String, String> = config
        .groups
        .iter()
        .filter_map(|g| match (&g.id, &g.name) {
            (Some(id), Some(name)) if !name.is_empty() => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();

    // Filter to SSH profiles with a valid host
    let ssh_profiles: Vec<&TabbyProfile> = config
        .profiles
        .iter()
        .filter(|p| p.profile_type.as_deref() == Some("ssh"))
        .filter(|p| p.options.host.is_some())
        .filter(|p| {
            let host = p.options.host.as_deref().unwrap_or("");
            if validate_hostname(host).is_err() {
                eprintln!(
                    "Warning: skipping Tabby profile {:?}: invalid hostname '{}'",
                    p.name.as_deref().unwrap_or("unnamed"),
                    host
                );
                return false;
            }
            true
        })
        .collect();

    if ssh_profiles.is_empty() {
        return Ok(vec![]);
    }

    // Build jump host resolution map: profile_id → bookmark name
    let jump_map = resolve_jump_hosts(&ssh_profiles);

    let bookmarks: Vec<Bookmark> = ssh_profiles
        .iter()
        .map(|p| profile_to_bookmark(p, &jump_map, &group_map, env_override, extra_tags))
        .collect();

    Ok(bookmarks)
}

/// Build a map from profile ID to resolved proxy_jump bookmark name.
///
/// Tabby's `jumpHost` is a profile UUID, not a hostname. We look up the
/// referenced profile's name to use as the sshore proxy_jump reference.
fn resolve_jump_hosts(profiles: &[&TabbyProfile]) -> HashMap<String, String> {
    // Map profile ID → profile name
    let id_to_name: HashMap<&str, &str> = profiles
        .iter()
        .filter_map(|p| match (&p.id, &p.name) {
            (Some(id), Some(name)) => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();

    // For each profile with a jumpHost, resolve to the target's name
    let mut jump_map = HashMap::new();
    for profile in profiles {
        if let (Some(id), Some(jump_id)) = (&profile.id, &profile.options.jump_host) {
            if let Some(jump_name) = id_to_name.get(jump_id.as_str()) {
                jump_map.insert(id.clone(), sanitize_bookmark_name(jump_name));
            } else {
                eprintln!(
                    "Warning: Tabby profile {:?} references jump host ID '{}' which was not found",
                    profile.name, jump_id
                );
            }
        }
    }

    jump_map
}

/// Strip `file://` or `file:///` URI prefix from a path.
fn strip_file_uri(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("file:///") {
        // file:///Users/... → /Users/... (Unix)
        // file:///C:/... → C:/... (Windows)
        if rest.starts_with('/') || rest.chars().nth(1) == Some(':') {
            // Windows drive letter like C:/ — keep as-is
            rest.to_string()
        } else {
            // Unix absolute path — restore leading slash
            format!("/{}", rest)
        }
    } else if let Some(rest) = path.strip_prefix("file://") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// Convert a Tabby profile to a sshore Bookmark.
fn profile_to_bookmark(
    profile: &TabbyProfile,
    jump_map: &HashMap<String, String>,
    group_map: &HashMap<String, String>,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Bookmark {
    let raw_name = profile.name.as_deref().unwrap_or("unnamed");
    let name = sanitize_bookmark_name(raw_name);
    let host = profile.options.host.clone().unwrap_or_default();

    // Resolve group UUID to human-readable name
    let resolved_group = profile
        .group
        .as_ref()
        .map(|g| group_map.get(g).cloned().unwrap_or_else(|| g.clone()));

    // Use group name in env detection
    let detect_input = if let Some(ref group) = resolved_group {
        format!("{} {}", name, group)
    } else {
        name.clone()
    };
    let env = env_override
        .map(String::from)
        .unwrap_or_else(|| detect_env(&detect_input, &host));

    let mut tags = extra_tags.to_vec();
    if let Some(ref group) = resolved_group
        && !group.is_empty()
        && !tags.contains(group)
    {
        tags.push(group.clone());
    }
    if !tags.contains(&"tabby-import".to_string()) {
        tags.push("tabby-import".to_string());
    }

    // Resolve jump host from profile ID
    let proxy_jump = profile.id.as_ref().and_then(|id| jump_map.get(id)).cloned();

    // Use first private key if available, stripping file:// URI prefix
    let identity_file = profile
        .options
        .private_keys
        .as_ref()
        .and_then(|keys| keys.first())
        .map(|k| strip_file_uri(k));

    Bookmark {
        name,
        host,
        user: profile.options.user.clone(),
        port: profile.options.port.unwrap_or(22),
        env,
        tags,
        identity_file,
        proxy_jump,
        notes: Some(format!("Imported from Tabby profile: {}", raw_name)),
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
    fn test_parse_ssh_profiles() {
        let yaml = r#"
profiles:
  - type: ssh
    name: prod-web-01
    group: Production
    options:
      host: 10.0.1.5
      port: 22
      user: deploy
    id: "abc-123"
  - type: ssh
    name: staging-api
    group: Staging
    options:
      host: 10.0.2.10
      port: 22
      user: deploy
    id: "def-456"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].name, "prod-web-01");
        assert_eq!(bookmarks[0].host, "10.0.1.5");
        assert_eq!(bookmarks[0].user, Some("deploy".into()));
        assert_eq!(bookmarks[0].port, 22);
        assert_eq!(bookmarks[0].env, "production");
        assert!(bookmarks[0].tags.contains(&"Production".to_string()));
    }

    #[test]
    fn test_skip_non_ssh_profiles() {
        let yaml = r#"
profiles:
  - type: local
    name: Local Terminal
    options: {}
  - type: ssh
    name: my-server
    options:
      host: 10.0.1.5
    id: "abc-123"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "my-server");
    }

    #[test]
    fn test_resolve_jump_host_id() {
        let yaml = r#"
profiles:
  - type: ssh
    name: bastion
    options:
      host: bastion.example.com
    id: "bastion-id"
  - type: ssh
    name: internal
    options:
      host: 10.0.0.5
      jumpHost: "bastion-id"
    id: "internal-id"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        let internal = bookmarks.iter().find(|b| b.name == "internal").unwrap();
        assert_eq!(internal.proxy_jump, Some("bastion".into()));
    }

    #[test]
    fn test_missing_jump_host_results_in_none() {
        let yaml = r#"
profiles:
  - type: ssh
    name: internal
    options:
      host: 10.0.0.5
      jumpHost: "nonexistent-id"
    id: "internal-id"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert!(bookmarks[0].proxy_jump.is_none());
    }

    #[test]
    fn test_private_key_mapping() {
        let yaml = r#"
profiles:
  - type: ssh
    name: my-server
    options:
      host: 10.0.1.5
      privateKeys:
        - "~/.ssh/id_ed25519"
        - "~/.ssh/id_rsa"
    id: "abc-123"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(bookmarks[0].identity_file, Some("~/.ssh/id_ed25519".into()));
    }

    #[test]
    fn test_empty_profiles() {
        let yaml = r#"
profiles: []
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_env_override() {
        let yaml = r#"
profiles:
  - type: ssh
    name: prod-web-01
    group: Production
    options:
      host: 10.0.1.5
    id: "abc-123"
"#;
        let bookmarks = parse_tabby_config(yaml, Some("testing"), &[]).unwrap();
        assert_eq!(bookmarks[0].env, "testing");
    }

    #[test]
    fn test_group_uuid_resolved_to_name() {
        let yaml = r#"
groups:
  - id: "2b47ab53-76f2-4768-a1ad-c2b972e73652"
    name: "DHP"
  - id: "e9626447-36bf-4885-9161-c60cc16e0827"
    name: "WorkHub"
profiles:
  - type: ssh
    name: Posterity-DHP-VM-Prod
    group: "2b47ab53-76f2-4768-a1ad-c2b972e73652"
    options:
      host: 48.216.242.39
      user: dhp
    id: "profile-001"
  - type: ssh
    name: WorkHub-self-hosted-server
    group: "e9626447-36bf-4885-9161-c60cc16e0827"
    options:
      host: 172.178.101.216
      user: admin
    id: "profile-002"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        // Should use resolved group name, not UUID
        assert!(bookmarks[0].tags.contains(&"DHP".to_string()));
        assert!(!bookmarks[0].tags.iter().any(|t| t.contains("2b47ab53")));
        assert!(bookmarks[1].tags.contains(&"WorkHub".to_string()));
        assert!(!bookmarks[1].tags.iter().any(|t| t.contains("e9626447")));
    }

    #[test]
    fn test_group_uuid_without_groups_section_falls_back() {
        // When there's no groups section, the raw group value is used as-is
        let yaml = r#"
profiles:
  - type: ssh
    name: my-server
    group: "some-uuid-here"
    options:
      host: 10.0.1.5
    id: "abc-123"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert!(bookmarks[0].tags.contains(&"some-uuid-here".to_string()));
    }

    #[test]
    fn test_private_key_file_uri_stripped() {
        let yaml = r#"
profiles:
  - type: ssh
    name: my-server
    options:
      host: 10.0.1.5
      privateKeys:
        - "file:///Users/sergeybelov/ssh/PosterityHealthDHP_key.pem"
    id: "abc-123"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(
            bookmarks[0].identity_file,
            Some("/Users/sergeybelov/ssh/PosterityHealthDHP_key.pem".into())
        );
    }

    #[test]
    fn test_strip_file_uri_variants() {
        // Unix absolute path
        assert_eq!(
            strip_file_uri("file:///Users/user/.ssh/key.pem"),
            "/Users/user/.ssh/key.pem"
        );
        // Windows path
        assert_eq!(
            strip_file_uri("file:///C:/Users/user/.ssh/key.pem"),
            "C:/Users/user/.ssh/key.pem"
        );
        // file:// (two slashes)
        assert_eq!(
            strip_file_uri("file:///home/user/.ssh/key.pem"),
            "/home/user/.ssh/key.pem"
        );
        // No prefix — unchanged
        assert_eq!(strip_file_uri("~/.ssh/id_ed25519"), "~/.ssh/id_ed25519");
        // Absolute path without prefix — unchanged
        assert_eq!(strip_file_uri("/home/user/.ssh/key"), "/home/user/.ssh/key");
    }

    #[test]
    fn test_group_uuid_used_for_env_detection() {
        // Group name "Production" should drive env detection even when stored as UUID
        let yaml = r#"
groups:
  - id: "uuid-prod"
    name: "Production"
profiles:
  - type: ssh
    name: my-server
    group: "uuid-prod"
    options:
      host: 10.0.1.5
    id: "abc-123"
"#;
        let bookmarks = parse_tabby_config(yaml, None, &[]).unwrap();
        assert_eq!(bookmarks[0].env, "production");
    }
}
