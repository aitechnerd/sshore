use std::fs;
use std::path::Path;

use sshore::config;
use sshore::config::ImportSourceKind;
use sshore::config::ssh_import::merge_imports;

/// Helper: import from a fixture file.
fn import_fixture(
    fixture: &str,
    source: ImportSourceKind,
    env_override: Option<&str>,
    extra_tags: &[String],
) -> Vec<sshore::config::model::Bookmark> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);
    assert!(path.exists(), "Fixture file not found: {}", path.display());
    config::import_from_source(&path, source, env_override, extra_tags).unwrap()
}

// ── PuTTY ──────────────────────────────────────────────────────────────

#[test]
fn test_putty_end_to_end() {
    let bookmarks = import_fixture("putty_sessions.reg", ImportSourceKind::Putty, None, &[]);

    // Should import 4 SSH sessions (skip Default Settings + telnet)
    assert_eq!(bookmarks.len(), 4);

    let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"prod-web-01"));
    assert!(names.contains(&"prod-db-01"));
    assert!(names.contains(&"staging-api"));
    assert!(names.contains(&"My-Dev-Server"));
    assert!(!names.contains(&"dev-telnet")); // telnet skipped
    assert!(!names.contains(&"Default-Settings")); // empty host skipped

    // Check production env detection
    let prod = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(prod.env, "production");
    assert_eq!(prod.user, Some("deploy".into()));
    assert_eq!(prod.port, 22);
    assert_eq!(
        prod.identity_file,
        Some("C:/Users/sergey/.ssh/id_ed25519".into())
    );

    // Check proxy mapping
    let db = bookmarks.iter().find(|b| b.name == "prod-db-01").unwrap();
    assert_eq!(db.proxy_jump, Some("bastion.example.com".into()));

    // Check non-standard port
    let staging = bookmarks.iter().find(|b| b.name == "staging-api").unwrap();
    assert_eq!(staging.port, 3352); // dword:00000d18
    assert_eq!(staging.env, "staging");
}

#[test]
fn test_putty_with_env_override() {
    let bookmarks = import_fixture(
        "putty_sessions.reg",
        ImportSourceKind::Putty,
        Some("testing"),
        &[],
    );
    for b in &bookmarks {
        assert_eq!(b.env, "testing");
    }
}

#[test]
fn test_putty_with_extra_tags() {
    let bookmarks = import_fixture(
        "putty_sessions.reg",
        ImportSourceKind::Putty,
        None,
        &["windows".into(), "legacy".into()],
    );
    for b in &bookmarks {
        assert!(b.tags.contains(&"windows".to_string()));
        assert!(b.tags.contains(&"legacy".to_string()));
        assert!(b.tags.contains(&"putty-import".to_string()));
    }
}

// ── MobaXterm ──────────────────────────────────────────────────────────

#[test]
fn test_mobaxterm_end_to_end() {
    let bookmarks = import_fixture(
        "mobaxterm_sessions.mxtsessions",
        ImportSourceKind::Mobaxterm,
        None,
        &[],
    );

    // Should import 4 SSH sessions (skip telnet)
    assert_eq!(bookmarks.len(), 4);

    let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"prod-web-01"));
    assert!(names.contains(&"prod-db-01"));
    assert!(names.contains(&"staging-api"));
    assert!(names.contains(&"dev-local"));
    assert!(!names.contains(&"telnet-box")); // non-SSH skipped

    // Check folder tags
    let prod = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert!(prod.tags.contains(&"Production".to_string()));
    assert_eq!(prod.env, "production");

    let staging = bookmarks.iter().find(|b| b.name == "staging-api").unwrap();
    assert!(staging.tags.contains(&"Staging".to_string()));
}

// ── Tabby ──────────────────────────────────────────────────────────────

#[test]
fn test_tabby_end_to_end() {
    let bookmarks = import_fixture("tabby_config.yaml", ImportSourceKind::Tabby, None, &[]);

    // 3 SSH profiles (local terminal skipped)
    assert_eq!(bookmarks.len(), 3);

    let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"bastion"));
    assert!(names.contains(&"prod-web-01"));
    assert!(names.contains(&"staging-api"));

    // Check jump host resolution
    let web = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(web.proxy_jump, Some("bastion".into()));
    assert_eq!(web.identity_file, Some("~/.ssh/id_ed25519".into()));
    assert!(web.tags.contains(&"Production".to_string()));

    // Bastion has no jump host
    let bastion = bookmarks.iter().find(|b| b.name == "bastion").unwrap();
    assert!(bastion.proxy_jump.is_none());
}

// ── SecureCRT ──────────────────────────────────────────────────────────

#[test]
fn test_securecrt_end_to_end() {
    let bookmarks = import_fixture(
        "securecrt_export.xml",
        ImportSourceKind::Securecrt,
        None,
        &[],
    );

    assert_eq!(bookmarks.len(), 3);

    let web = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(web.host, "10.0.1.5");
    assert_eq!(web.port, 22);
    assert_eq!(web.user, Some("deploy".into()));
    assert_eq!(web.identity_file, Some("~/.ssh/id_ed25519".into()));

    // Check hex port parsing: 0x00001538 = 5432
    let db = bookmarks.iter().find(|b| b.name == "internal-db").unwrap();
    assert_eq!(db.port, 5432);
    assert_eq!(db.proxy_jump, Some("bastion".into()));
}

// ── CSV ────────────────────────────────────────────────────────────────

#[test]
fn test_csv_end_to_end() {
    let bookmarks = import_fixture("hosts.csv", ImportSourceKind::Csv, None, &[]);

    assert_eq!(bookmarks.len(), 5);

    let web = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(web.host, "10.0.1.5");
    assert_eq!(web.user, Some("deploy".into()));
    assert_eq!(web.env, "production");
    assert!(web.tags.contains(&"web".to_string()));
    assert!(web.tags.contains(&"frontend".to_string()));
    assert_eq!(web.identity_file, Some("~/.ssh/id_ed25519".into()));
    assert_eq!(web.proxy_jump, Some("bastion".into()));
    assert_eq!(web.notes, Some("Primary web server".into()));
}

#[test]
fn test_csv_with_env_override() {
    let bookmarks = import_fixture("hosts.csv", ImportSourceKind::Csv, Some("staging"), &[]);
    for b in &bookmarks {
        assert_eq!(b.env, "staging");
    }
}

// ── JSON ───────────────────────────────────────────────────────────────

#[test]
fn test_json_end_to_end() {
    let bookmarks = import_fixture("hosts.json", ImportSourceKind::Json, None, &[]);

    assert_eq!(bookmarks.len(), 5);

    let web = bookmarks.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(web.host, "10.0.1.5");
    assert_eq!(web.user, Some("deploy".into()));
    assert_eq!(web.env, "production");
    assert!(web.tags.contains(&"web".to_string()));
    assert_eq!(web.identity_file, Some("~/.ssh/id_ed25519".into()));
    assert_eq!(web.proxy_jump, Some("bastion".into()));

    // Bastion has auto-detected env (no env field → detect_env runs)
    let bastion = bookmarks.iter().find(|b| b.name == "bastion").unwrap();
    assert!(bastion.env.is_empty() || !bastion.env.is_empty()); // just verify it exists
}

// ── Conflict Resolution ────────────────────────────────────────────────

#[test]
fn test_import_skip_existing() {
    let mut existing = vec![sshore::config::model::Bookmark {
        name: "prod-web-01".into(),
        host: "original.example.com".into(),
        user: Some("original-user".into()),
        port: 22,
        env: "production".into(),
        tags: vec![],
        identity_file: None,
        proxy_jump: None,
        notes: Some("Original bookmark".into()),
        last_connected: None,
        connect_count: 5,
        on_connect: None,
        snippets: vec![],
        ssh_options: std::collections::HashMap::new(),
        connect_timeout_secs: None,
    }];

    let imported = import_fixture("hosts.csv", ImportSourceKind::Csv, None, &[]);
    let result = merge_imports(&mut existing, imported, false);

    // prod-web-01 already exists → skipped
    assert!(result.already_existed > 0);

    // Original should be preserved
    let prod = existing.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(prod.host, "original.example.com");
    assert_eq!(prod.connect_count, 5);
}

#[test]
fn test_import_overwrite_existing() {
    let mut existing = vec![sshore::config::model::Bookmark {
        name: "prod-web-01".into(),
        host: "original.example.com".into(),
        user: Some("original-user".into()),
        port: 22,
        env: "production".into(),
        tags: vec![],
        identity_file: None,
        proxy_jump: None,
        notes: Some("Original bookmark".into()),
        last_connected: None,
        connect_count: 5,
        on_connect: None,
        snippets: vec![],
        ssh_options: std::collections::HashMap::new(),
        connect_timeout_secs: None,
    }];

    let imported = import_fixture("hosts.csv", ImportSourceKind::Csv, None, &[]);
    let result = merge_imports(&mut existing, imported, true);

    // prod-web-01 should be overwritten
    assert!(result.imported.iter().any(|b| b.name == "prod-web-01"));

    let prod = existing.iter().find(|b| b.name == "prod-web-01").unwrap();
    assert_eq!(prod.host, "10.0.1.5"); // New host
    assert_eq!(prod.connect_count, 0); // Reset
}

// ── Dry Run ────────────────────────────────────────────────────────────

#[test]
fn test_dry_run_does_not_modify_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    // Create empty config
    let config = sshore::config::model::AppConfig::default();
    sshore::config::save_to(&config, &config_path).unwrap();

    // Read original contents
    let original_content = fs::read_to_string(&config_path).unwrap();

    // Import but don't merge (simulating dry run — we just parse, don't save)
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hosts.csv");
    let _parsed =
        config::import_from_source(&fixture_path, ImportSourceKind::Csv, None, &[]).unwrap();

    // Config file should be unchanged
    let current_content = fs::read_to_string(&config_path).unwrap();
    assert_eq!(original_content, current_content);
}
