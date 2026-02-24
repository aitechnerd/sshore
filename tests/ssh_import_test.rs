use std::fs;
use std::io::Write;
use std::path::Path;

use sshore::config::ssh_import::{merge_imports, parse_ssh_config};

fn write_file(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

/// Realistic multi-file SSH config simulating a typical setup.
#[test]
fn test_realistic_multi_file_import() {
    let dir = tempfile::tempdir().unwrap();
    let conf_d = dir.path().join("config.d");
    fs::create_dir(&conf_d).unwrap();

    // Main config with global settings and an Include
    let main_config = format!(
        r#"
# Global defaults
Host *
    ServerAliveInterval 60
    ServerAliveCountMax 3
    AddKeysToAgent yes

# Bastion host
Host bastion
    HostName jump.example.com
    User ops
    IdentityFile ~/.ssh/id_ed25519

Include {}/*.conf
"#,
        conf_d.display()
    );

    // Work config file
    write_file(
        &conf_d,
        "work.conf",
        r#"
Host prod-web-01
    HostName 10.0.1.10
    User deploy
    Port 22
    IdentityFile ~/.ssh/work_key
    ProxyJump bastion

Host prod-web-02
    HostName 10.0.1.11
    User deploy
    ProxyJump bastion

Host staging-api
    HostName staging-api.example.com
    User deploy

Host dev-worker
    HostName dev-worker.internal
    User developer
"#,
    );

    // Personal config file
    write_file(
        &conf_d,
        "personal.conf",
        r#"
Host homelab
    HostName 192.168.1.100
    User admin
    Port 2222

Host vps
    HostName vps.myhost.com
    User root
"#,
    );

    let main_path = write_file(dir.path(), "config", &main_config);

    let bookmarks = parse_ssh_config(&main_path).unwrap();

    // Should have: bastion + 4 work + 2 personal = 7 (wildcard skipped)
    assert_eq!(bookmarks.len(), 7);

    let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"bastion"));
    assert!(names.contains(&"prod-web-01"));
    assert!(names.contains(&"prod-web-02"));
    assert!(names.contains(&"staging-api"));
    assert!(names.contains(&"dev-worker"));
    assert!(names.contains(&"homelab"));
    assert!(names.contains(&"vps"));

    // Verify environment detection
    let prod_web = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(prod_web.env, "production");
    assert_eq!(prod_web.proxy_jump, Some("bastion".into()));

    let staging = bookmarks.iter().find(|b| b.name == "staging-api").unwrap();
    assert_eq!(staging.env, "staging");

    let dev = bookmarks.iter().find(|b| b.name == "dev-worker").unwrap();
    assert_eq!(dev.env, "development");

    // bastion has no env keyword
    let bastion = bookmarks.iter().find(|b| b.name == "bastion").unwrap();
    assert_eq!(bastion.env, "");
}

#[test]
fn test_merge_after_import() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        dir.path(),
        "config",
        r#"
Host server-a
    HostName a.example.com

Host server-b
    HostName b.example.com
"#,
    );

    let imported = parse_ssh_config(&path).unwrap();
    assert_eq!(imported.len(), 2);

    // Start with one existing bookmark
    let mut existing = vec![sshore::config::model::Bookmark {
        name: "server-a".into(),
        host: "old-a.example.com".into(),
        user: Some("olduser".into()),
        port: 22,
        env: String::new(),
        tags: vec![],
        identity_file: None,
        proxy_jump: None,
        notes: Some("Keep this".into()),
        last_connected: None,
        connect_count: 10,
    }];

    let result = merge_imports(&mut existing, imported, false);

    assert_eq!(result.already_existed, 1);
    assert_eq!(result.imported.len(), 1);
    assert_eq!(result.imported[0].name, "server-b");
    assert_eq!(existing.len(), 2);
    // server-a should still have old values
    assert_eq!(existing[0].host, "old-a.example.com");
    assert_eq!(existing[0].notes, Some("Keep this".into()));
}

#[test]
fn test_nested_includes() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();

    // Level 2: included by level 1
    let level2_path = write_file(
        &sub,
        "level2.conf",
        r#"
Host deep-host
    HostName deep.example.com
"#,
    );

    // Level 1: included by main, itself includes level 2
    write_file(
        dir.path(),
        "level1.conf",
        &format!(
            "Include {}\n\nHost mid-host\n    HostName mid.example.com\n",
            level2_path.display()
        ),
    );

    let main_path = write_file(
        dir.path(),
        "config",
        &format!(
            "Include {}\n\nHost top-host\n    HostName top.example.com\n",
            dir.path().join("level1.conf").display()
        ),
    );

    let bookmarks = parse_ssh_config(&main_path).unwrap();
    let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"deep-host"));
    assert!(names.contains(&"mid-host"));
    assert!(names.contains(&"top-host"));
}
