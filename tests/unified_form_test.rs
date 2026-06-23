/// Integration tests for the unified bookmark/group form.
///
/// Tests the unified form behavior:
/// - Adding a bookmark (0 sessions)
/// - Adding a group (>=1 sessions via Ctrl+Enter)
/// - Editing existing bookmark/group
/// - Removing last session reverts to bookmark mode
/// - Name conflict across bookmark/group types

use sshore::config::model::{AppConfig, Bookmark, BookmarkGroup, Settings, Session};
use sshore::tui::views::form::{EditTarget, FormState, UnifiedForm, UnifiedEntry};

fn sample_bookmark() -> Bookmark {
    Bookmark {
        name: "test-server".into(),
        host: "10.0.1.5".into(),
        user: Some("deploy".into()),
        port: 22,
        env: "production".into(),
        tags: vec!["web".into()],
        identity_file: None,
        proxy_jump: None,
        notes: None,
        last_connected: None,
        connect_count: 0,
        on_connect: None,
        on_connect_prompt_pattern: None,
        snippets: vec![],
        connect_timeout_secs: None,
        ssh_options: std::collections::BTreeMap::new(),
        profile: None,
    }
}

fn sample_group() -> BookmarkGroup {
    BookmarkGroup {
        name: "test-group".into(),
        host: "10.0.1.5".into(),
        user: Some("deploy".into()),
        port: 22,
        env: "production".into(),
        tags: vec!["web".into()],
        identity_file: None,
        proxy_jump: None,
        notes: None,
        profile: None,
        on_connect: None,
        on_connect_prompt_pattern: None,
        snippets: vec![],
        connect_timeout_secs: None,
        ssh_options: std::collections::BTreeMap::new(),
        sessions: vec![
            Session {
                name: "session-a".into(),
                on_connect: Some("tail -f /var/log/app.log".into()),
                ..Session::default()
            },
            Session {
                name: "session-b".into(),
                ..Session::default()
            },
        ],
    }
}

#[test]
fn unified_form_add_saves_as_bookmark() {
    let config = AppConfig::default();
    let settings = Settings::default();
    let mut form = UnifiedForm::new_add(&settings, &[]);
    form.fields[0] = "new-server".into(); // NAME
    form.fields[1] = "10.0.1.5".into();   // HOST

    // No sessions added - should save as bookmark
    let entry = form.validate_and_build(&config).unwrap();
    match entry {
        UnifiedEntry::Bookmark(b) => {
            assert_eq!(b.name, "new-server");
            assert_eq!(b.host, "10.0.1.5");
        }
        UnifiedEntry::Group(_) => panic!("Expected bookmark, got group"),
    }
}

#[test]
fn unified_form_add_with_sessions_saves_as_group() {
    let config = AppConfig::default();
    let settings = Settings::default();
    let mut form = UnifiedForm::new_add(&settings, &[]);
    form.fields[0] = "new-group".into();  // NAME
    form.fields[1] = "10.0.1.5".into();   // HOST

    // Add session via Ctrl+Enter simulation
    form.add_session_line();
    form.sessions[0].name = "my-session".into();

    // Has sessions - should save as group
    let entry = form.validate_and_build(&config).unwrap();
    match entry {
        UnifiedEntry::Group(g) => {
            assert_eq!(g.name, "new-group");
            assert_eq!(g.host, "10.0.1.5");
            assert_eq!(g.sessions.len(), 1);
            assert_eq!(g.sessions[0].name, "my-session");
        }
        UnifiedEntry::Bookmark(_) => panic!("Expected group, got bookmark"),
    }
}

#[test]
fn unified_form_edit_bookmark_preserves_type() {
    let bookmark = sample_bookmark();
    let mut form = UnifiedForm::new_edit_bookmark(&bookmark, &[]);
    let config = AppConfig::default();

    // Should build as bookmark (no sessions)
    let entry = form.validate_and_build(&config).unwrap();
    match entry {
        UnifiedEntry::Bookmark(b) => {
            assert_eq!(b.name, "test-server");
        }
        UnifiedEntry::Group(_) => panic!("Expected bookmark"),
    }
}

#[test]
fn unified_form_edit_group_preserves_type() {
    let group = sample_group();
    let mut form = UnifiedForm::new_edit_group(&group, &[]);
    let config = AppConfig::default();

    // Should build as group (has sessions)
    let entry = form.validate_and_build(&config).unwrap();
    match entry {
        UnifiedEntry::Group(g) => {
            assert_eq!(g.name, "test-group");
            assert_eq!(g.sessions.len(), 2);
        }
        UnifiedEntry::Bookmark(_) => panic!("Expected group"),
    }
}

#[test]
fn unified_form_remove_last_session_reverts_to_bookmark() {
    let settings = Settings::default();
    let mut form = UnifiedForm::new_add(&settings, &[]);
    form.fields[0] = "test".into();
    form.fields[1] = "10.0.1.5".into();

    // Add a session
    form.add_session_line();
    assert!(form.is_group());

    // Remove the last session
    form.remove_session_line();
    assert!(form.is_bookmark());
    assert!(form.sessions_collapsed);
}

#[test]
fn unified_form_name_conflict_bookmark_vs_group() {
    let mut config = AppConfig::default();
    config.groups.push(sample_group());

    let settings = Settings::default();
    let mut form = UnifiedForm::new_add(&settings, &[]);
    form.fields[0] = "test-group".into(); // Same name as existing group
    form.fields[1] = "10.0.1.5".into();

    // Should fail due to name conflict
    let result = form.validate_and_build_bookmark(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("already exists"));
}

#[test]
fn unified_form_name_conflict_group_vs_bookmark() {
    let mut config = AppConfig::default();
    config.bookmarks.push(sample_bookmark());

    let settings = Settings::default();
    let mut form = UnifiedForm::new_add(&settings, &[]);
    form.fields[0] = "test-server".into(); // Same name as existing bookmark
    form.fields[1] = "10.0.1.5".into();
    form.add_session_line();
    form.sessions[0].name = "s1".into();

    // Should fail due to name conflict
    let result = form.validate_and_build_group(&config);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("already exists"));
}

#[test]
fn form_state_new_edit_with_bookmark_target() {
    let bookmark = sample_bookmark();
    let state = FormState::new_edit(0, EditTarget::Bookmark, &bookmark, &[]);

    match state {
        FormState::Edit(_, EditTarget::Bookmark, f) => {
            assert!(f.is_edit);
            assert!(f.sessions.is_empty());
            assert!(f.sessions_collapsed);
        }
        _ => panic!("Expected Edit with Bookmark target"),
    }
}

#[test]
fn form_state_new_edit_with_group_target() {
    let group = sample_group();
    let state = FormState::new_edit(0, EditTarget::Group, &group, &[]);

    match state {
        FormState::Edit(_, EditTarget::Group, f) => {
            assert!(f.is_edit);
            assert_eq!(f.sessions.len(), 2);
            assert!(!f.sessions_collapsed);
        }
        _ => panic!("Expected Edit with Group target"),
    }
}
