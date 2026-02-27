use std::collections::HashMap;

use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::config::env::detect_env;
use crate::config::model::{Bookmark, sanitize_bookmark_name, validate_hostname};

/// A parsed SecureCRT session from XML export.
#[derive(Debug, Default)]
struct SecureCrtSession {
    name: String,
    hostname: String,
    port: u16,
    username: Option<String>,
    identity_file: Option<String>,
    firewall: Option<String>,
    folder_path: Vec<String>,
}

/// Parse SecureCRT XML export into sshore bookmarks.
///
/// SecureCRT exports sessions in a nested `<key>` structure under a
/// `<key name="Sessions">` root. Nested folders become tags. The `Firewall Name`
/// field maps to proxy_jump (SecureCRT's term for jump host).
pub fn parse_securecrt_xml(
    content: &str,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    let sessions = parse_sessions(content)?;

    let bookmarks: Vec<Bookmark> = sessions
        .into_iter()
        .filter(|s| !s.hostname.is_empty())
        .filter(|s| {
            if validate_hostname(&s.hostname).is_err() {
                eprintln!(
                    "Warning: skipping SecureCRT session '{}': invalid hostname '{}'",
                    s.name, s.hostname
                );
                return false;
            }
            true
        })
        .map(|s| session_to_bookmark(s, env_override, extra_tags))
        .collect();

    Ok(bookmarks)
}

/// Parse SecureCRT XML into intermediate session structs.
fn parse_sessions(content: &str) -> Result<Vec<SecureCrtSession>> {
    let mut reader = Reader::from_str(content);
    let mut sessions = Vec::new();
    let mut key_stack: Vec<String> = Vec::new();
    let mut in_sessions = false;
    let mut current: Option<SecureCrtSession> = None;
    let mut current_attr_name = String::new();
    let mut current_tag_type = String::new();

    loop {
        match reader.read_event().context("Failed to read XML event")? {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();

                if tag == "key" {
                    let name_attr = extract_name_attr(&e);

                    if !in_sessions {
                        if name_attr.as_deref() == Some("Sessions") {
                            in_sessions = true;
                        }
                    } else {
                        // Inside Sessions tree — this is either a folder or a session
                        if let Some(name) = name_attr {
                            key_stack.push(name.clone());

                            // Every nested <key> starts as a potential session
                            // We'll only keep it if it has a Hostname
                            if let Some(prev) = current.take() {
                                // The previous key was a folder (no Hostname yet pushed it)
                                if !prev.hostname.is_empty() {
                                    sessions.push(prev);
                                }
                            }
                            current = Some(SecureCrtSession {
                                name,
                                hostname: String::new(),
                                port: 22,
                                username: None,
                                identity_file: None,
                                firewall: None,
                                folder_path: key_stack[..key_stack.len() - 1].to_vec(),
                            });
                        }
                    }
                } else if in_sessions && (tag == "string" || tag == "dword") {
                    current_tag_type = tag;
                    current_attr_name = extract_name_attr(&e).unwrap_or_default();
                }
            }
            Event::Text(e) => {
                if let Some(session) = current.as_mut() {
                    let text = e
                        .unescape()
                        .context("Failed to unescape XML text")?
                        .to_string();
                    match current_attr_name.as_str() {
                        "Hostname" => session.hostname = text,
                        "[SSH2] Port" => {
                            session.port = parse_hex_or_decimal(&text).unwrap_or(22) as u16;
                        }
                        "Username" => session.username = non_empty(text),
                        "Identity Filename V2" => session.identity_file = non_empty(text),
                        "Firewall Name" => session.firewall = non_empty(text),
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "key" && in_sessions {
                    if key_stack.is_empty() {
                        // Closing the Sessions root
                        in_sessions = false;
                        if let Some(session) = current.take()
                            && !session.hostname.is_empty()
                        {
                            sessions.push(session);
                        }
                    } else {
                        // Closing a session or folder entry
                        if let Some(session) = current.take()
                            && !session.hostname.is_empty()
                        {
                            sessions.push(session);
                        }
                        key_stack.pop();
                        // Re-create parent context if there are still keys on the stack
                        // (we're going back up to a folder level — no current session)
                        current = None;
                    }
                }
                // Clear attr state after end of string/dword tag
                if tag == "string" || tag == "dword" {
                    current_attr_name.clear();
                    current_tag_type.clear();
                }
            }
            Event::Empty(e) => {
                // Handle self-closing tags like <string name="Hostname"/>
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "key" && in_sessions {
                    // Empty key = session with no children (rare but possible)
                    if let Some(name) = extract_name_attr(&e) {
                        key_stack.push(name);
                        key_stack.pop();
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    // Handle any remaining session
    if let Some(session) = current
        && !session.hostname.is_empty()
    {
        sessions.push(session);
    }

    Ok(sessions)
}

/// Extract the `name` attribute from an XML element.
fn extract_name_attr(e: &quick_xml::events::BytesStart) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == b"name")
        .map(|a| String::from_utf8_lossy(&a.value).to_string())
}

/// Parse `0x00000016` or plain decimal `22` to u32.
fn parse_hex_or_decimal(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Convert a SecureCRT session to a sshore Bookmark.
fn session_to_bookmark(
    session: SecureCrtSession,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Bookmark {
    let name = sanitize_bookmark_name(&session.name);

    // Use folder path in env detection
    let folder_str = session.folder_path.join(" ");
    let detect_input = if folder_str.is_empty() {
        name.clone()
    } else {
        format!("{} {}", name, folder_str)
    };
    let env = env_override
        .map(String::from)
        .unwrap_or_else(|| detect_env(&detect_input, &session.hostname));

    let mut tags = extra_tags.to_vec();
    // Add folder path as tags (flattened)
    for folder in &session.folder_path {
        if !folder.is_empty() && !tags.contains(folder) {
            tags.push(folder.clone());
        }
    }
    if !tags.contains(&"securecrt-import".to_string()) {
        tags.push("securecrt-import".to_string());
    }

    Bookmark {
        name,
        host: session.hostname,
        user: session.username,
        port: session.port,
        env,
        tags,
        identity_file: session.identity_file,
        proxy_jump: session.firewall,
        notes: Some(format!("Imported from SecureCRT session: {}", session.name)),
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
    fn test_parse_simple_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke>
  <key name="Sessions">
    <key name="prod-web-01">
      <string name="Hostname">10.0.1.5</string>
      <dword name="[SSH2] Port">0x00000016</dword>
      <string name="Username">deploy</string>
    </key>
  </key>
</VanDyke>"#;

        let bookmarks = parse_securecrt_xml(xml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        let b = &bookmarks[0];
        assert_eq!(b.name, "prod-web-01");
        assert_eq!(b.host, "10.0.1.5");
        assert_eq!(b.port, 22);
        assert_eq!(b.user, Some("deploy".into()));
        assert_eq!(b.env, "production");
    }

    #[test]
    fn test_hex_port_parsing() {
        assert_eq!(parse_hex_or_decimal("0x00000016"), Some(22));
        assert_eq!(parse_hex_or_decimal("0x00001f90"), Some(8080));
        assert_eq!(parse_hex_or_decimal("22"), Some(22));
    }

    #[test]
    fn test_firewall_as_proxy() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke>
  <key name="Sessions">
    <key name="internal">
      <string name="Hostname">10.0.0.5</string>
      <dword name="[SSH2] Port">0x00000016</dword>
      <string name="Username">deploy</string>
      <string name="Firewall Name">bastion</string>
    </key>
  </key>
</VanDyke>"#;

        let bookmarks = parse_securecrt_xml(xml, None, &[]).unwrap();
        assert_eq!(bookmarks[0].proxy_jump, Some("bastion".into()));
    }

    #[test]
    fn test_identity_file() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke>
  <key name="Sessions">
    <key name="myserver">
      <string name="Hostname">10.0.1.5</string>
      <string name="Identity Filename V2">~/.ssh/id_ed25519</string>
    </key>
  </key>
</VanDyke>"#;

        let bookmarks = parse_securecrt_xml(xml, None, &[]).unwrap();
        assert_eq!(bookmarks[0].identity_file, Some("~/.ssh/id_ed25519".into()));
    }

    #[test]
    fn test_skip_empty_hostname() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke>
  <key name="Sessions">
    <key name="empty-session">
      <string name="Hostname"></string>
    </key>
    <key name="real-session">
      <string name="Hostname">10.0.1.5</string>
    </key>
  </key>
</VanDyke>"#;

        let bookmarks = parse_securecrt_xml(xml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "real-session");
    }

    #[test]
    fn test_multiple_sessions() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke>
  <key name="Sessions">
    <key name="web-01">
      <string name="Hostname">10.0.1.5</string>
      <string name="Username">deploy</string>
    </key>
    <key name="db-01">
      <string name="Hostname">10.0.1.6</string>
      <string name="Username">admin</string>
    </key>
  </key>
</VanDyke>"#;

        let bookmarks = parse_securecrt_xml(xml, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].name, "web-01");
        assert_eq!(bookmarks[1].name, "db-01");
    }

    #[test]
    fn test_env_override() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke>
  <key name="Sessions">
    <key name="myserver">
      <string name="Hostname">10.0.1.5</string>
    </key>
  </key>
</VanDyke>"#;

        let bookmarks = parse_securecrt_xml(xml, Some("staging"), &[]).unwrap();
        assert_eq!(bookmarks[0].env, "staging");
    }
}
