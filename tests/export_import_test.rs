use std::fs;

use sshore::config;
use sshore::config::model::{AppConfig, Bookmark, Settings, Snippet};
use sshore::config::ssh_import::{import_from_file, merge_imports};

fn sample_bookmark(name: &str, env: &str) -> Bookmark {
    Bookmark {
        name: name.into(),
        host: format!("{name}.example.com"),
        user: Some("deploy".into()),
        port: 22,
        env: env.into(),
        tags: vec!["web".into()],
        identity_file: None,
        proxy_jump: None,
        notes: None,
        last_connected: Some(chrono::Utc::now()),
        connect_count: 10,
        on_connect: None,
        snippets: vec![],
        connect_timeout_secs: None,
        ssh_options: std::collections::HashMap::new(),
    }
}

fn sample_config() -> AppConfig {
    AppConfig {
        settings: Settings::default(),
        bookmarks: vec![
            sample_bookmark("prod-web-01", "production"),
            sample_bookmark("staging-api", "staging"),
            sample_bookmark("dev-local", "development"),
        ],
    }
}

#[test]
fn test_export_import_roundtrip() {
    let original = sample_config();

    // Export all bookmarks
    let exported = config::export_bookmarks(&original, None, &[], None, false).unwrap();

    // Write to a temp file
    let dir = tempfile::tempdir().unwrap();
    let export_path = dir.path().join("export.toml");
    fs::write(&export_path, &exported).unwrap();

    // Import into a fresh config
    let imported = import_from_file(&export_path).unwrap();
    assert_eq!(imported.len(), 3);

    // Verify names and hosts match
    for orig_bm in &original.bookmarks {
        let found = imported.iter().find(|b| b.name == orig_bm.name);
        assert!(found.is_some(), "Missing bookmark: {}", orig_bm.name);
        let found = found.unwrap();
        assert_eq!(found.host, orig_bm.host);
        assert_eq!(found.env, orig_bm.env);
        assert_eq!(found.user, orig_bm.user);
        assert_eq!(found.port, orig_bm.port);
        // Usage data should be zeroed in export
        assert!(found.last_connected.is_none());
        assert_eq!(found.connect_count, 0);
    }
}

#[test]
fn test_export_import_with_env_filter() {
    let config = sample_config();

    // Export only production
    let exported = config::export_bookmarks(&config, Some("production"), &[], None, false).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prod.toml");
    fs::write(&path, &exported).unwrap();

    let imported = import_from_file(&path).unwrap();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].name, "prod-web-01");
    assert_eq!(imported[0].env, "production");
}

#[test]
fn test_import_overwrite_vs_skip() {
    let mut existing = vec![Bookmark {
        name: "server-a".into(),
        host: "old.example.com".into(),
        user: Some("olduser".into()),
        port: 22,
        env: "production".into(),
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
            user: Some("newuser".into()),
            port: 22,
            env: "staging".into(),
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

    // Without overwrite: server-a keeps old values, server-b is added
    let result = merge_imports(&mut existing, imported.clone(), false);
    assert_eq!(result.already_existed, 1);
    assert_eq!(result.imported.len(), 1);
    assert_eq!(existing.len(), 2);
    assert_eq!(existing[0].host, "old.example.com");
    assert_eq!(existing[1].name, "server-b");

    // Reset and try with overwrite
    let mut existing2 = vec![Bookmark {
        name: "server-a".into(),
        host: "old.example.com".into(),
        user: Some("olduser".into()),
        port: 22,
        env: "production".into(),
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

    let result2 = merge_imports(&mut existing2, imported, true);
    assert_eq!(result2.imported.len(), 2);
    // server-a should now have new values
    let server_a = existing2.iter().find(|b| b.name == "server-a").unwrap();
    assert_eq!(server_a.host, "new.example.com");
    assert_eq!(server_a.user, Some("newuser".into()));
}

#[test]
fn test_export_import_with_snippets() {
    let mut config = sample_config();
    config.bookmarks[0].on_connect = Some("cd /var/www && exec $SHELL".into());
    config.bookmarks[0].snippets = vec![
        Snippet {
            name: "Tail log".into(),
            command: "tail -f /var/log/app.log".into(),
            auto_execute: true,
        },
        Snippet {
            name: "Git status".into(),
            command: "cd /var/www && git status".into(),
            auto_execute: false,
        },
    ];

    let exported = config::export_bookmarks(&config, Some("production"), &[], None, false).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("with_snippets.toml");
    fs::write(&path, &exported).unwrap();

    let imported = import_from_file(&path).unwrap();
    assert_eq!(imported.len(), 1);

    let bm = &imported[0];
    assert_eq!(bm.on_connect, Some("cd /var/www && exec $SHELL".into()));
    assert_eq!(bm.snippets.len(), 2);
    assert_eq!(bm.snippets[0].name, "Tail log");
    assert!(bm.snippets[0].auto_execute);
    assert_eq!(bm.snippets[1].name, "Git status");
    assert!(!bm.snippets[1].auto_execute);
}
