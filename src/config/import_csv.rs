use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::config::env::detect_env;
use crate::config::model::{Bookmark, sanitize_bookmark_name, validate_hostname};

/// Parse a CSV file into sshore bookmarks.
///
/// The first row must be a header. `name` and `host` columns are required;
/// all others are optional. Column order doesn't matter — matched by header name.
/// Common aliases are accepted: `hostname` = `host`, `username` = `user`, etc.
///
/// The content is stripped of a UTF-8 BOM if present.
pub fn parse_csv(
    content: &str,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Result<Vec<Bookmark>> {
    // Strip BOM if present
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    if content.trim().is_empty() {
        anyhow::bail!("File is empty, nothing to import");
    }

    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(content.as_bytes());

    let headers = reader.headers()?.clone();

    // Find column indices with flexible naming
    let col = |names: &[&str]| -> Option<usize> {
        for name in names {
            if let Some(pos) = headers.iter().position(|h| h.eq_ignore_ascii_case(name)) {
                return Some(pos);
            }
        }
        None
    };

    let name_col = col(&["name"]).context("CSV must have a 'name' column")?;
    let host_col =
        col(&["host", "hostname", "address", "ip"]).context("CSV must have a 'host' column")?;
    let user_col = col(&["user", "username", "login"]);
    let port_col = col(&["port"]);
    let env_col = col(&["env", "environment", "tier"]);
    let tags_col = col(&["tags", "labels", "groups"]);
    let identity_col = col(&["identity_file", "key", "ssh_key", "keyfile"]);
    let proxy_col = col(&["proxy_jump", "jump_host", "bastion", "proxy"]);
    let notes_col = col(&["notes", "description", "comment"]);

    let mut bookmarks = Vec::new();

    for (row_idx, result) in reader.records().enumerate() {
        let record = result.with_context(|| format!("Failed to parse CSV row {}", row_idx + 2))?;

        let get = |idx: Option<usize>| -> Option<String> {
            idx.and_then(|i| record.get(i))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let name = match get(Some(name_col)) {
            Some(n) => sanitize_bookmark_name(&n),
            None => continue, // Skip rows with empty name
        };

        let host = match get(Some(host_col)) {
            Some(h) => h,
            None => continue, // Skip rows with empty host
        };

        if validate_hostname(&host).is_err() {
            eprintln!("Warning: skipping CSV row {}: invalid hostname '{}'", row_idx + 2, host);
            continue;
        }

        let mut tags: Vec<String> = get(tags_col)
            .map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        tags.extend(extra_tags.iter().cloned());
        if !tags.contains(&"csv-import".to_string()) {
            tags.push("csv-import".to_string());
        }

        let env = env_override
            .map(String::from)
            .or_else(|| get(env_col))
            .unwrap_or_else(|| detect_env(&name, &host));

        let port = match get(port_col) {
            Some(p) => match p.parse::<u16>() {
                Ok(port) => port,
                Err(_) => {
                    eprintln!(
                        "Warning: invalid port '{}' in CSV row {}, defaulting to 22",
                        p,
                        row_idx + 2
                    );
                    22
                }
            },
            None => 22,
        };

        bookmarks.push(Bookmark {
            name,
            host,
            user: get(user_col),
            port,
            env,
            tags,
            identity_file: get(identity_col),
            proxy_jump: get(proxy_col),
            notes: get(notes_col),
            last_connected: None,
            connect_count: 0,
            on_connect: None,
            snippets: vec![],
            ssh_options: HashMap::new(),
            connect_timeout_secs: None,
        });
    }

    Ok(bookmarks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_standard_csv() {
        let csv = "name,host,user,port,env,tags,identity_file,proxy_jump,notes\n\
                   prod-web-01,10.0.1.5,deploy,22,production,\"web,frontend\",~/.ssh/id_ed25519,bastion,Primary web\n\
                   staging-api,10.0.2.10,deploy,22,staging,api,,,API server\n\
                   dev-local,192.168.1.50,sergey,22,development,,,,\n";

        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 3);

        let b0 = &bookmarks[0];
        assert_eq!(b0.name, "prod-web-01");
        assert_eq!(b0.host, "10.0.1.5");
        assert_eq!(b0.user, Some("deploy".into()));
        assert_eq!(b0.port, 22);
        assert_eq!(b0.env, "production");
        assert!(b0.tags.contains(&"web".to_string()));
        assert!(b0.tags.contains(&"frontend".to_string()));
        assert_eq!(b0.identity_file, Some("~/.ssh/id_ed25519".into()));
        assert_eq!(b0.proxy_jump, Some("bastion".into()));
        assert_eq!(b0.notes, Some("Primary web".into()));
    }

    #[test]
    fn test_flexible_column_names() {
        let csv = "name,hostname,username,port\n\
                   myserver,example.com,admin,2222\n";

        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].host, "example.com");
        assert_eq!(bookmarks[0].user, Some("admin".into()));
        assert_eq!(bookmarks[0].port, 2222);
    }

    #[test]
    fn test_missing_required_column() {
        let csv = "name,port\nmyserver,22\n";
        let result = parse_csv(csv, None, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_tags_within_quotes() {
        let csv = "name,host,tags\n\
                   myserver,example.com,\"web,frontend,api\"\n";

        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        let tags = &bookmarks[0].tags;
        assert!(tags.contains(&"web".to_string()));
        assert!(tags.contains(&"frontend".to_string()));
        assert!(tags.contains(&"api".to_string()));
    }

    #[test]
    fn test_extra_columns_ignored() {
        let csv = "name,host,datacenter,rack\n\
                   myserver,example.com,us-east-1,rack42\n";

        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "myserver");
    }

    #[test]
    fn test_env_auto_detect_when_missing() {
        let csv = "name,host\nprod-web-01,10.0.1.5\n";
        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks[0].env, "production");
    }

    #[test]
    fn test_env_override() {
        let csv = "name,host,env\nprod-web-01,10.0.1.5,production\n";
        let bookmarks = parse_csv(csv, Some("staging"), &[]).unwrap();
        assert_eq!(bookmarks[0].env, "staging");
    }

    #[test]
    fn test_bom_handling() {
        let csv = "\u{feff}name,host\nmyserver,example.com\n";
        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "myserver");
    }

    #[test]
    fn test_empty_file() {
        let result = parse_csv("", None, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_skip_empty_rows() {
        let csv = "name,host\nmyserver,example.com\n,,\n";
        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 1);
    }

    #[test]
    fn test_default_port() {
        let csv = "name,host\nmyserver,example.com\n";
        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks[0].port, 22);
    }

    #[test]
    fn test_skip_hostname_with_shell_metacharacters() {
        let csv = "name,host\ngood,example.com\nbad,host;rm -rf /\nalso-bad,host|evil\nfine,10.0.1.5\n";
        let bookmarks = parse_csv(csv, None, &[]).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].host, "example.com");
        assert_eq!(bookmarks[1].host, "10.0.1.5");
    }
}
