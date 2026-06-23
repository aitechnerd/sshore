use std::path::Path;

use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use serde::{Deserialize, Serialize};

use crate::config::env::detect_env;
use crate::config::model::{
    AppConfig, Bookmark, BookmarkGroup, Session, Settings, validate_bookmark_name,
    validate_hostname,
};
use crate::config::validate_on_connect;
use crate::keychain;
use crate::tui::theme::ThemeColors;
use crate::tui::widgets::env_badge;

/// Number of editable fields in the form.
const FIELD_COUNT: usize = 12;

/// Environment options for the cycle selector.
const ENV_OPTIONS: &[&str] = &[
    "",
    "production",
    "staging",
    "development",
    "local",
    "testing",
];

/// Placeholder text shown in the Proxy Jump field when empty.
const PROXY_JUMP_PLACEHOLDER: &str = "(e.g. admin@bastion)";

/// Index of each form field.
pub(crate) const FIELD_NAME: usize = 0;
pub(crate) const FIELD_HOST: usize = 1;
const FIELD_USER: usize = 2;
pub(crate) const FIELD_PORT: usize = 3;
pub(crate) const FIELD_ENV: usize = 4;
const FIELD_TAGS: usize = 5;
const FIELD_IDENTITY: usize = 6;
const FIELD_PROXY: usize = 7;
const FIELD_NOTES: usize = 8;
const FIELD_ON_CONNECT: usize = 9;
const FIELD_PASSWORD: usize = 10;
pub(crate) const FIELD_PROFILE: usize = 11;

/// Target type for editing: a bookmark or a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditTarget {
    Bookmark,
    Group,
}

/// Unified entry type for form output: either a bookmark or a group.
#[derive(Debug, Clone)]
pub enum UnifiedEntry {
    Bookmark(Bookmark),
    Group(BookmarkGroup),
}

/// Trait for items that can be edited in the form.
/// Allows passing either a Bookmark or BookmarkGroup to FormState::new_edit.
pub trait EditableItem {
    fn as_bookmark(&self) -> Option<&Bookmark>;
    fn as_group(&self) -> Option<&BookmarkGroup>;
}

impl EditableItem for Bookmark {
    fn as_bookmark(&self) -> Option<&Bookmark> { Some(self) }
    fn as_group(&self) -> Option<&BookmarkGroup> { None }
}

impl EditableItem for BookmarkGroup {
    fn as_bookmark(&self) -> Option<&Bookmark> { None }
    fn as_group(&self) -> Option<&BookmarkGroup> { Some(self) }
}

/// Form fields for the unified bookmark/group form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedForm {
    pub fields: [String; FIELD_COUNT],
    pub focused: usize,
    pub env_index: usize,
    /// Index into `profile_options` for the profile cycle selector.
    pub profile_index: usize,
    /// Dynamic list of profile options: ["(none)", "profile-a", "profile-b", ...].
    pub profile_options: Vec<String>,
    pub is_edit: bool,
    /// Original name (for edit mode uniqueness check).
    pub original_name: Option<String>,
    /// Validation error to display.
    pub error: Option<String>,
    /// Whether a password is already stored in the keychain.
    pub has_stored_password: bool,
    /// Whether the user has modified the password field (typed or deleted chars).
    pub password_modified: bool,
    /// Session lines for this connection (groups have >= 1, bookmarks have 0).
    pub sessions: Vec<Session>,
    /// Current session line being edited.
    pub session_cursor: usize,
    /// Whether the sessions section is collapsed in the UI.
    pub sessions_collapsed: bool,
}

/// Form fields for bookmark add/edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkForm {
    pub fields: [String; FIELD_COUNT],
    pub focused: usize,
    pub env_index: usize,
    /// Index into `profile_options` for the profile cycle selector.
    pub profile_index: usize,
    /// Dynamic list of profile options: ["(none)", "profile-a", "profile-b", ...].
    pub profile_options: Vec<String>,
    pub is_edit: bool,
    /// Original bookmark name (for edit mode uniqueness check).
    pub original_name: Option<String>,
    /// Validation error to display.
    pub error: Option<String>,
    /// Whether a password is already stored in the keychain for this bookmark.
    pub has_stored_password: bool,
    /// Whether the user has modified the password field (typed or deleted chars).
    pub password_modified: bool,
}

/// Form fields for group add/edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupForm {
    pub fields: [String; FIELD_COUNT],
    pub focused: usize,
    pub env_index: usize,
    pub profile_index: usize,
    pub profile_options: Vec<String>,
    pub is_edit: bool,
    /// Original group name (for edit mode uniqueness check).
    pub original_name: Option<String>,
    /// Validation error to display.
    pub error: Option<String>,
    /// Session lines for this group.
    pub sessions: Vec<Session>,
    /// Current session line being edited.
    pub session_cursor: usize,
}

/// Form state enum covering the unified form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FormState {
    /// Adding a new connection (bookmark or group, determined by sessions count).
    Add(UnifiedForm),
    /// Editing an existing entry (bookmark or group, determined by EditTarget).
    Edit(usize, EditTarget, UnifiedForm),
}

// ─── BookmarkForm impl ───────────────────────────────────────────────

impl BookmarkForm {
    /// Create a blank form for adding a new bookmark.
    pub fn new_add(settings: &Settings, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_PORT] = "22".to_string();
        if let Some(ref user) = settings.default_user {
            fields[FIELD_USER] = user.clone();
        }

        let profile_options = build_profile_options(profile_names);

        Self {
            fields,
            focused: FIELD_NAME,
            env_index: 0,     // (none)
            profile_index: 0, // (none)
            profile_options,
            is_edit: false,
            original_name: None,
            error: None,
            has_stored_password: false,
            password_modified: false,
        }
    }

    /// Create a pre-populated form for editing an existing bookmark.
    pub fn new_edit(bookmark: &Bookmark, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_NAME] = bookmark.name.clone();
        fields[FIELD_HOST] = bookmark.host.clone();
        fields[FIELD_USER] = bookmark.user.clone().unwrap_or_default();
        fields[FIELD_PORT] = bookmark.port.to_string();
        fields[FIELD_TAGS] = bookmark.tags.join(", ");
        fields[FIELD_IDENTITY] = bookmark.identity_file.clone().unwrap_or_default();
        fields[FIELD_PROXY] = bookmark.proxy_jump.clone().unwrap_or_default();
        fields[FIELD_NOTES] = bookmark.notes.clone().unwrap_or_default();
        fields[FIELD_ON_CONNECT] = bookmark.on_connect.clone().unwrap_or_default();
        // Password field starts empty — never load actual password into memory.

        let env_index = ENV_OPTIONS
            .iter()
            .position(|&e| e == bookmark.env)
            .unwrap_or(0);

        let profile_options = build_profile_options(profile_names);

        // Find the bookmark's current profile in the options list.
        // If the profile was deleted since last edit, fall back to "(none)" (index 0).
        let profile_index = bookmark
            .profile
            .as_ref()
            .and_then(|p| profile_options.iter().position(|opt| opt == p))
            .unwrap_or(0);

        // Check keychain for stored password (best-effort)
        let has_stored_password = keychain::get_password(&bookmark.name)
            .unwrap_or(None)
            .is_some();

        Self {
            fields,
            focused: FIELD_NAME,
            env_index,
            profile_index,
            profile_options,
            is_edit: true,
            original_name: Some(bookmark.name.clone()),
            error: None,
            has_stored_password,
            password_modified: false,
        }
    }

    /// Move focus to the next field.
    pub fn next_field(&mut self) {
        if self.focused < FIELD_COUNT - 1 {
            self.focused += 1;
        }
    }

    /// Move focus to the previous field.
    pub fn prev_field(&mut self) {
        if self.focused > 0 {
            self.focused -= 1;
        }
    }

    /// Cycle environment selection forward.
    pub fn cycle_env_right(&mut self) {
        self.env_index = (self.env_index + 1) % ENV_OPTIONS.len();
    }

    /// Cycle environment selection backward.
    pub fn cycle_env_left(&mut self) {
        if self.env_index == 0 {
            self.env_index = ENV_OPTIONS.len() - 1;
        } else {
            self.env_index -= 1;
        }
    }

    /// Cycle profile selection forward.
    pub fn cycle_profile_right(&mut self) {
        if !self.profile_options.is_empty() {
            self.profile_index = (self.profile_index + 1) % self.profile_options.len();
        }
    }

    /// Cycle profile selection backward.
    pub fn cycle_profile_left(&mut self) {
        if !self.profile_options.is_empty() {
            if self.profile_index == 0 {
                self.profile_index = self.profile_options.len() - 1;
            } else {
                self.profile_index -= 1;
            }
        }
    }

    /// Get the selected profile name, or None if "(none)" is selected.
    pub fn selected_profile(&self) -> Option<&str> {
        if self.profile_index == 0 {
            None
        } else {
            Some(&self.profile_options[self.profile_index])
        }
    }

    /// Insert a character at the current field (except env and profile, which use cycling).
    pub fn insert_char(&mut self, c: char) {
        if self.focused == FIELD_ENV || self.focused == FIELD_PROFILE {
            return;
        }
        self.fields[self.focused].push(c);
        self.error = None;

        if self.focused == FIELD_PASSWORD {
            self.password_modified = true;
        }

        // Auto-detect env when name or host changes
        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Delete last character from the current field.
    pub fn delete_char(&mut self) {
        if self.focused == FIELD_ENV || self.focused == FIELD_PROFILE {
            return;
        }
        self.fields[self.focused].pop();
        self.error = None;

        if self.focused == FIELD_PASSWORD {
            self.password_modified = true;
        }

        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Auto-detect environment from name and host, updating env_index.
    fn auto_detect_env(&mut self) {
        let detected = detect_env(&self.fields[FIELD_NAME], &self.fields[FIELD_HOST]);
        if let Some(idx) = ENV_OPTIONS.iter().position(|&e| e == detected) {
            self.env_index = idx;
        }
    }

    /// Get the selected environment string.
    pub fn selected_env(&self) -> &str {
        ENV_OPTIONS[self.env_index]
    }

    /// Get the password field value.
    pub fn password(&self) -> &str {
        &self.fields[FIELD_PASSWORD]
    }

    /// Validate the form and build a Bookmark. Returns Err with a user-facing message on failure.
    pub fn validate_and_build(&mut self, config: &AppConfig) -> Result<Bookmark> {
        // Clear any previous warning (e.g., identity file not found) so
        // stale messages don't persist across validation attempts.
        self.error = None;

        let name = self.fields[FIELD_NAME].trim().to_string();
        let host = self.fields[FIELD_HOST].trim().to_string();

        // Validate name
        validate_bookmark_name(&name)?;

        // Uniqueness check (skip for edit if name unchanged)
        let is_rename = self
            .original_name
            .as_ref()
            .is_some_and(|orig| orig != &name);
        if (!self.is_edit || is_rename) && config.bookmarks.iter().any(|b| b.name == name) {
            anyhow::bail!("A bookmark named '{}' already exists", name);
        }

        // Validate host
        validate_hostname(&host)?;

        // Validate port
        let port: u16 = self.fields[FIELD_PORT]
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Port must be a number between 1 and 65535"))?;
        if port == 0 {
            anyhow::bail!("Port must be between 1 and 65535");
        }

        // Parse tags
        let tags: Vec<String> = self.fields[FIELD_TAGS]
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        // Identity file: warn if provided but doesn't exist
        let identity_file = non_empty_option(&self.fields[FIELD_IDENTITY]);
        if let Some(ref path_str) = identity_file {
            let expanded = shellexpand::tilde(path_str).to_string();
            if !Path::new(&expanded).exists() {
                // Warn but allow — file might be on a different machine or not yet created
                self.error = Some(format!("Warning: identity file not found: {expanded}"));
            }
        }

        let user = non_empty_option(&self.fields[FIELD_USER]);
        let proxy_jump = non_empty_option(&self.fields[FIELD_PROXY]);
        let notes = non_empty_option(&self.fields[FIELD_NOTES]);
        let on_connect = non_empty_option(&self.fields[FIELD_ON_CONNECT]);
        let env = self.selected_env().to_string();
        let profile = self.selected_profile().map(|s| s.to_string());

        Ok(Bookmark {
            name,
            host,
            user,
            port,
            env,
            tags,
            identity_file,
            proxy_jump,
            notes,
            last_connected: None,
            connect_count: 0,
            on_connect,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            profile,
        })
    }
}

// ─── UnifiedForm impl ─────────────────────────────────────────────────

impl UnifiedForm {
    /// Create a blank form for adding a new connection.
    pub fn new_add(settings: &Settings, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_PORT] = "22".to_string();
        if let Some(ref user) = settings.default_user {
            fields[FIELD_USER] = user.clone();
        }

        let profile_options = build_profile_options(profile_names);

        Self {
            fields,
            focused: FIELD_NAME,
            env_index: 0,     // (none)
            profile_index: 0, // (none)
            profile_options,
            is_edit: false,
            original_name: None,
            error: None,
            has_stored_password: false,
            password_modified: false,
            sessions: vec![],
            session_cursor: 0,
            sessions_collapsed: true,
        }
    }

    /// Create a pre-populated form for editing an existing bookmark.
    pub fn new_edit_bookmark(bookmark: &Bookmark, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_NAME] = bookmark.name.clone();
        fields[FIELD_HOST] = bookmark.host.clone();
        fields[FIELD_USER] = bookmark.user.clone().unwrap_or_default();
        fields[FIELD_PORT] = bookmark.port.to_string();
        fields[FIELD_TAGS] = bookmark.tags.join(", ");
        fields[FIELD_IDENTITY] = bookmark.identity_file.clone().unwrap_or_default();
        fields[FIELD_PROXY] = bookmark.proxy_jump.clone().unwrap_or_default();
        fields[FIELD_NOTES] = bookmark.notes.clone().unwrap_or_default();
        fields[FIELD_ON_CONNECT] = bookmark.on_connect.clone().unwrap_or_default();
        // Password field starts empty — never load actual password into memory.

        let env_index = ENV_OPTIONS
            .iter()
            .position(|&e| e == bookmark.env)
            .unwrap_or(0);

        let profile_options = build_profile_options(profile_names);

        let profile_index = bookmark
            .profile
            .as_ref()
            .and_then(|p| profile_options.iter().position(|opt| opt == p))
            .unwrap_or(0);

        let has_stored_password = keychain::get_password(&bookmark.name)
            .unwrap_or(None)
            .is_some();

        Self {
            fields,
            focused: FIELD_NAME,
            env_index,
            profile_index,
            profile_options,
            is_edit: true,
            original_name: Some(bookmark.name.clone()),
            error: None,
            has_stored_password,
            password_modified: false,
            sessions: vec![],
            session_cursor: 0,
            sessions_collapsed: true,
        }
    }

    /// Create a pre-populated form for editing an existing group.
    pub fn new_edit_group(group: &BookmarkGroup, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_NAME] = group.name.clone();
        fields[FIELD_HOST] = group.host.clone();
        fields[FIELD_USER] = group.user.clone().unwrap_or_default();
        fields[FIELD_PORT] = group.port.to_string();
        fields[FIELD_TAGS] = group.tags.join(", ");
        fields[FIELD_IDENTITY] = group.identity_file.clone().unwrap_or_default();
        fields[FIELD_PROXY] = group.proxy_jump.clone().unwrap_or_default();
        fields[FIELD_NOTES] = group.notes.clone().unwrap_or_default();
        fields[FIELD_ON_CONNECT] = group.on_connect.clone().unwrap_or_default();

        let env_index = ENV_OPTIONS
            .iter()
            .position(|&e| e == group.env)
            .unwrap_or(0);

        let profile_options = build_profile_options(profile_names);

        let profile_index = group
            .profile
            .as_ref()
            .and_then(|p| profile_options.iter().position(|opt| opt == p))
            .unwrap_or(0);

        let sessions = if group.sessions.is_empty() {
            vec![Session::default()]
        } else {
            group.sessions.clone()
        };

        Self {
            fields,
            focused: FIELD_NAME,
            env_index,
            profile_index,
            profile_options,
            is_edit: true,
            original_name: Some(group.name.clone()),
            error: None,
            has_stored_password: false,
            password_modified: false,
            sessions,
            session_cursor: 0,
            sessions_collapsed: false,
        }
    }

    /// Move focus to the next field.
    pub fn next_field(&mut self) {
        if self.focused < FIELD_COUNT - 1 {
            self.focused += 1;
        }
    }

    /// Move focus to the previous field.
    pub fn prev_field(&mut self) {
        if self.focused > 0 {
            self.focused -= 1;
        }
    }

    /// Cycle environment selection forward.
    pub fn cycle_env_right(&mut self) {
        self.env_index = (self.env_index + 1) % ENV_OPTIONS.len();
    }

    /// Cycle environment selection backward.
    pub fn cycle_env_left(&mut self) {
        if self.env_index == 0 {
            self.env_index = ENV_OPTIONS.len() - 1;
        } else {
            self.env_index -= 1;
        }
    }

    /// Cycle profile selection forward.
    pub fn cycle_profile_right(&mut self) {
        if !self.profile_options.is_empty() {
            self.profile_index = (self.profile_index + 1) % self.profile_options.len();
        }
    }

    /// Cycle profile selection backward.
    pub fn cycle_profile_left(&mut self) {
        if !self.profile_options.is_empty() {
            if self.profile_index == 0 {
                self.profile_index = self.profile_options.len() - 1;
            } else {
                self.profile_index -= 1;
            }
        }
    }

    /// Get the selected profile name, or None if "(none)" is selected.
    pub fn selected_profile(&self) -> Option<&str> {
        if self.profile_index == 0 {
            None
        } else {
            Some(&self.profile_options[self.profile_index])
        }
    }

    /// Insert a character at the current field (except env and profile, which use cycling).
    pub fn insert_char(&mut self, c: char) {
        if self.focused == FIELD_ENV || self.focused == FIELD_PROFILE {
            return;
        }
        self.fields[self.focused].push(c);
        self.error = None;

        if self.focused == FIELD_PASSWORD {
            self.password_modified = true;
        }

        // Auto-detect env when name or host changes
        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Delete last character from the current field.
    pub fn delete_char(&mut self) {
        if self.focused == FIELD_ENV || self.focused == FIELD_PROFILE {
            return;
        }
        self.fields[self.focused].pop();
        self.error = None;

        if self.focused == FIELD_PASSWORD {
            self.password_modified = true;
        }

        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Auto-detect environment from name and host, updating env_index.
    fn auto_detect_env(&mut self) {
        let detected = detect_env(&self.fields[FIELD_NAME], &self.fields[FIELD_HOST]);
        if let Some(idx) = ENV_OPTIONS.iter().position(|&e| e == detected) {
            self.env_index = idx;
        }
    }

    /// Get the selected environment string.
    pub fn selected_env(&self) -> &str {
        ENV_OPTIONS[self.env_index]
    }

    /// Get the password field value.
    pub fn password(&self) -> &str {
        &self.fields[FIELD_PASSWORD]
    }

    /// Add a new empty session line after the current cursor position.
    /// Expands the sessions section if this is the first session.
    pub fn add_session_line(&mut self) {
        let insert_at = self.session_cursor + 1;
        self.sessions.insert(
            insert_at.min(self.sessions.len()),
            Session::default(),
        );
        self.session_cursor = insert_at.min(self.sessions.len() - 1);
        self.sessions_collapsed = false;
    }

    /// Remove the current session line.
    /// Collapses the sessions section if this was the last session.
    pub fn remove_session_line(&mut self) {
        if !self.sessions.is_empty() {
            self.sessions.remove(self.session_cursor);
            if self.session_cursor >= self.sessions.len() {
                self.session_cursor = self.sessions.len().saturating_sub(1);
            }
            if self.sessions.is_empty() {
                self.sessions_collapsed = true;
            }
        }
    }

    /// Check if this form represents a group (has sessions) or bookmark (no sessions).
    pub fn is_group(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Check if this form represents a bookmark (no sessions).
    pub fn is_bookmark(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Validate and build the appropriate entry type based on sessions.
    /// Returns Bookmark if sessions is empty, BookmarkGroup otherwise.
    pub fn validate_and_build(&mut self, config: &AppConfig) -> Result<UnifiedEntry> {
        if self.is_group() {
            self.validate_and_build_group(config).map(UnifiedEntry::Group)
        } else {
            self.validate_and_build_bookmark(config).map(UnifiedEntry::Bookmark)
        }
    }

    /// Validate and build a Bookmark (when sessions is empty).
    pub fn validate_and_build_bookmark(&mut self, config: &AppConfig) -> Result<Bookmark> {
        self.error = None;

        let name = self.fields[FIELD_NAME].trim().to_string();
        let host = self.fields[FIELD_HOST].trim().to_string();

        validate_bookmark_name(&name)?;

        // Uniqueness check across both bookmarks and groups
        let is_rename = self
            .original_name
            .as_ref()
            .is_some_and(|orig| orig != &name);
        if (!self.is_edit || is_rename) {
            if config.bookmarks.iter().any(|b| b.name == name) {
                anyhow::bail!("A bookmark named '{}' already exists", name);
            }
            if config.groups.iter().any(|g| g.name == name) {
                anyhow::bail!("A group named '{}' already exists", name);
            }
        }

        validate_hostname(&host)?;

        let port: u16 = self.fields[FIELD_PORT]
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Port must be a number between 1 and 65535"))?;
        if port == 0 {
            anyhow::bail!("Port must be between 1 and 65535");
        }

        let tags: Vec<String> = self.fields[FIELD_TAGS]
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        let identity_file = non_empty_option(&self.fields[FIELD_IDENTITY]);
        if let Some(ref path_str) = identity_file {
            let expanded = shellexpand::tilde(path_str).to_string();
            if !Path::new(&expanded).exists() {
                self.error = Some(format!("Warning: identity file not found: {expanded}"));
            }
        }

        let user = non_empty_option(&self.fields[FIELD_USER]);
        let proxy_jump = non_empty_option(&self.fields[FIELD_PROXY]);
        let notes = non_empty_option(&self.fields[FIELD_NOTES]);
        let on_connect = non_empty_option(&self.fields[FIELD_ON_CONNECT]);
        let env = self.selected_env().to_string();
        let profile = self.selected_profile().map(|s| s.to_string());

        Ok(Bookmark {
            name,
            host,
            user,
            port,
            env,
            tags,
            identity_file,
            proxy_jump,
            notes,
            last_connected: None,
            connect_count: 0,
            on_connect,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            profile,
        })
    }

    /// Validate and build a BookmarkGroup (when sessions has entries).
    pub fn validate_and_build_group(&mut self, config: &AppConfig) -> Result<BookmarkGroup> {
        self.error = None;

        let name = self.fields[FIELD_NAME].trim().to_string();
        let host = self.fields[FIELD_HOST].trim().to_string();

        validate_bookmark_name(&name)?;

        // Uniqueness check across both bookmarks and groups
        let is_rename = self
            .original_name
            .as_ref()
            .is_some_and(|orig| orig != &name);
        if (!self.is_edit || is_rename) {
            if config.groups.iter().any(|g| g.name == name) {
                anyhow::bail!("A group named '{}' already exists", name);
            }
            if config.bookmarks.iter().any(|b| b.name == name) {
                anyhow::bail!("A bookmark named '{}' already exists", name);
            }
        }

        validate_hostname(&host)?;

        let port: u16 = self.fields[FIELD_PORT]
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Port must be a number between 1 and 65535"))?;
        if port == 0 {
            anyhow::bail!("Port must be between 1 and 65535");
        }

        // Validate session names are unique within the group
        let mut session_names = std::collections::HashSet::new();
        for session in &self.sessions {
            let session_name = session.name.trim();
            if session_name.is_empty() {
                anyhow::bail!("Session name cannot be empty");
            }
            if !session_names.insert(session_name) {
                anyhow::bail!("Duplicate session name '{}'", session_name);
            }
            if let Some(ref cmd) = session.on_connect {
                validate_on_connect(
                    cmd,
                    &format!("Session '{}' in group '{}'", session_name, name),
                )?;
            }
        }

        let tags: Vec<String> = self.fields[FIELD_TAGS]
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        let user = non_empty_option(&self.fields[FIELD_USER]);
        let proxy_jump = non_empty_option(&self.fields[FIELD_PROXY]);
        let notes = non_empty_option(&self.fields[FIELD_NOTES]);
        let on_connect = non_empty_option(&self.fields[FIELD_ON_CONNECT]);
        let env = self.selected_env().to_string();
        let profile = self.selected_profile().map(|s| s.to_string());

        Ok(BookmarkGroup {
            name,
            host,
            user,
            port,
            env,
            tags,
            identity_file: non_empty_option(&self.fields[FIELD_IDENTITY]),
            proxy_jump,
            notes,
            profile,
            on_connect,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            sessions: self.sessions.clone(),
        })
    }
}

// ─── GroupForm impl ──────────────────────────────────────────────────

impl GroupForm {
    /// Create a blank form for adding a new group.
    pub fn new_add(settings: &Settings, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_PORT] = "22".to_string();
        if let Some(ref user) = settings.default_user {
            fields[FIELD_USER] = user.clone();
        }

        let profile_options = build_profile_options(profile_names);

        Self {
            fields,
            focused: FIELD_NAME,
            env_index: 0,     // (none)
            profile_index: 0, // (none)
            profile_options,
            is_edit: false,
            original_name: None,
            error: None,
            sessions: vec![Session::default()], // Start with one empty session
            session_cursor: 0,
        }
    }

    /// Create a pre-populated form for editing an existing group.
    pub fn new_edit(group: &BookmarkGroup, profile_names: &[String]) -> Self {
        let mut fields = std::array::from_fn(|_| String::new());
        fields[FIELD_NAME] = group.name.clone();
        fields[FIELD_HOST] = group.host.clone();
        fields[FIELD_USER] = group.user.clone().unwrap_or_default();
        fields[FIELD_PORT] = group.port.to_string();
        fields[FIELD_TAGS] = group.tags.join(", ");
        fields[FIELD_IDENTITY] = group.identity_file.clone().unwrap_or_default();
        fields[FIELD_PROXY] = group.proxy_jump.clone().unwrap_or_default();
        fields[FIELD_NOTES] = group.notes.clone().unwrap_or_default();
        // on_connect field holds the group-level default on_connect
        fields[FIELD_ON_CONNECT] = group.on_connect.clone().unwrap_or_default();

        let env_index = ENV_OPTIONS
            .iter()
            .position(|&e| e == group.env)
            .unwrap_or(0);

        let profile_options = build_profile_options(profile_names);

        let profile_index = group
            .profile
            .as_ref()
            .and_then(|p| profile_options.iter().position(|opt| opt == p))
            .unwrap_or(0);

        let sessions = if group.sessions.is_empty() {
            vec![Session::default()]
        } else {
            group.sessions.clone()
        };

        Self {
            fields,
            focused: FIELD_NAME,
            env_index,
            profile_index,
            profile_options,
            is_edit: true,
            original_name: Some(group.name.clone()),
            error: None,
            sessions,
            session_cursor: 0,
        }
    }

    /// Move focus to the next field.
    pub fn next_field(&mut self) {
        if self.focused < FIELD_COUNT - 1 {
            self.focused += 1;
        }
    }

    /// Move focus to the previous field.
    pub fn prev_field(&mut self) {
        if self.focused > 0 {
            self.focused -= 1;
        }
    }

    /// Cycle environment selection forward.
    pub fn cycle_env_right(&mut self) {
        self.env_index = (self.env_index + 1) % ENV_OPTIONS.len();
    }

    /// Cycle environment selection backward.
    pub fn cycle_env_left(&mut self) {
        if self.env_index == 0 {
            self.env_index = ENV_OPTIONS.len() - 1;
        } else {
            self.env_index -= 1;
        }
    }

    /// Cycle profile selection forward.
    pub fn cycle_profile_right(&mut self) {
        if !self.profile_options.is_empty() {
            self.profile_index = (self.profile_index + 1) % self.profile_options.len();
        }
    }

    /// Cycle profile selection backward.
    pub fn cycle_profile_left(&mut self) {
        if !self.profile_options.is_empty() {
            if self.profile_index == 0 {
                self.profile_index = self.profile_options.len() - 1;
            } else {
                self.profile_index -= 1;
            }
        }
    }

    /// Get the selected profile name, or None if "(none)" is selected.
    pub fn selected_profile(&self) -> Option<&str> {
        if self.profile_index == 0 {
            None
        } else {
            Some(&self.profile_options[self.profile_index])
        }
    }

    /// Insert a character at the current field (except env and profile, which use cycling).
    pub fn insert_char(&mut self, c: char) {
        if self.focused == FIELD_ENV || self.focused == FIELD_PROFILE {
            return;
        }
        self.fields[self.focused].push(c);
        self.error = None;

        // Auto-detect env when name or host changes
        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Delete last character from the current field.
    pub fn delete_char(&mut self) {
        if self.focused == FIELD_ENV || self.focused == FIELD_PROFILE {
            return;
        }
        self.fields[self.focused].pop();
        self.error = None;

        if self.focused == FIELD_NAME || self.focused == FIELD_HOST {
            self.auto_detect_env();
        }
    }

    /// Auto-detect environment from name and host, updating env_index.
    fn auto_detect_env(&mut self) {
        let detected = detect_env(&self.fields[FIELD_NAME], &self.fields[FIELD_HOST]);
        if let Some(idx) = ENV_OPTIONS.iter().position(|&e| e == detected) {
            self.env_index = idx;
        }
    }

    /// Get the selected environment string.
    pub fn selected_env(&self) -> &str {
        ENV_OPTIONS[self.env_index]
    }

    /// Add a new empty session line after the current cursor position.
    pub fn add_session_line(&mut self) {
        let insert_at = self.session_cursor + 1;
        self.sessions.insert(
            insert_at.min(self.sessions.len()),
            Session::default(),
        );
        self.session_cursor = insert_at.min(self.sessions.len() - 1);
    }

    /// Remove the current session line (minimum 1 line enforced).
    pub fn remove_session_line(&mut self) {
        if self.sessions.len() > 1 {
            self.sessions.remove(self.session_cursor);
            if self.session_cursor >= self.sessions.len() {
                self.session_cursor = self.sessions.len() - 1;
            }
        }
    }

    /// Validate the form and build a BookmarkGroup. Returns Err with a user-facing message on failure.
    pub fn validate_and_build(&mut self, config: &AppConfig) -> Result<BookmarkGroup> {
        self.error = None;

        let name = self.fields[FIELD_NAME].trim().to_string();
        let host = self.fields[FIELD_HOST].trim().to_string();

        // Validate name
        validate_bookmark_name(&name)?;

        // Uniqueness check (skip for edit if name unchanged)
        let is_rename = self
            .original_name
            .as_ref()
            .is_some_and(|orig| orig != &name);
        if (!self.is_edit || is_rename) && config.groups.iter().any(|g| g.name == name) {
            anyhow::bail!("A group named '{}' already exists", name);
        }

        // Validate host
        validate_hostname(&host)?;

        // Validate port
        let port: u16 = self.fields[FIELD_PORT]
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Port must be a number between 1 and 65535"))?;
        if port == 0 {
            anyhow::bail!("Port must be between 1 and 65535");
        }

        // Validate session names are unique within the group
        let mut session_names = std::collections::HashSet::new();
        for session in &self.sessions {
            let session_name = session.name.trim();
            if session_name.is_empty() {
                anyhow::bail!("Session name cannot be empty");
            }
            if !session_names.insert(session_name) {
                anyhow::bail!("Duplicate session name '{}'", session_name);
            }
            // Validate session-level on_connect
            if let Some(ref cmd) = session.on_connect {
                validate_on_connect(
                    cmd,
                    &format!("Session '{}' in group '{}'", session_name, name),
                )?;
            }
        }

        // Parse tags
        let tags: Vec<String> = self.fields[FIELD_TAGS]
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        let user = non_empty_option(&self.fields[FIELD_USER]);
        let proxy_jump = non_empty_option(&self.fields[FIELD_PROXY]);
        let notes = non_empty_option(&self.fields[FIELD_NOTES]);
        let on_connect = non_empty_option(&self.fields[FIELD_ON_CONNECT]);
        let env = self.selected_env().to_string();
        let profile = self.selected_profile().map(|s| s.to_string());

        Ok(BookmarkGroup {
            name,
            host,
            user,
            port,
            env,
            tags,
            identity_file: non_empty_option(&self.fields[FIELD_IDENTITY]),
            proxy_jump,
            notes,
            profile,
            on_connect,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            sessions: self.sessions.clone(),
        })
    }
}

// ─── FormState enum impl (delegating methods) ────────────────────────

impl FormState {
    // ── Constructors ──

    /// Create a blank form for adding a new connection (bookmark or group).
    pub fn new_add(settings: &Settings, profile_names: &[String]) -> Self {
        Self::Add(UnifiedForm::new_add(settings, profile_names))
    }

    /// Create a pre-populated form for editing an existing entry.
    /// For bookmarks, passes the bookmark and creates form with no sessions.
    /// For groups, passes the group and pre-populates sessions.
    pub fn new_edit(idx: usize, target: EditTarget, item: &dyn EditableItem, profile_names: &[String]) -> Self {
        let form = match target {
            EditTarget::Bookmark => {
                UnifiedForm::new_edit_bookmark(item.as_bookmark().unwrap(), profile_names)
            }
            EditTarget::Group => {
                UnifiedForm::new_edit_group(item.as_group().unwrap(), profile_names)
            }
        };
        Self::Edit(idx, target, form)
    }

    // ── Shared navigation (delegated to inner form) ──

    /// Move focus to the next field.
    pub fn next_field(&mut self) {
        match self {
            Self::Add(f) => f.next_field(),
            Self::Edit(_, _, f) => f.next_field(),
        }
    }

    /// Move focus to the previous field.
    pub fn prev_field(&mut self) {
        match self {
            Self::Add(f) => f.prev_field(),
            Self::Edit(_, _, f) => f.prev_field(),
        }
    }

    /// Get the current focused field index.
    pub fn focused(&self) -> usize {
        match self {
            Self::Add(f) => f.focused,
            Self::Edit(_, _, f) => f.focused,
        }
    }

    /// Cycle environment selection forward.
    pub fn cycle_env_right(&mut self) {
        match self {
            Self::Add(f) => f.cycle_env_right(),
            Self::Edit(_, _, f) => f.cycle_env_right(),
        }
    }

    /// Cycle environment selection backward.
    pub fn cycle_env_left(&mut self) {
        match self {
            Self::Add(f) => f.cycle_env_left(),
            Self::Edit(_, _, f) => f.cycle_env_left(),
        }
    }

    /// Cycle profile selection forward.
    pub fn cycle_profile_right(&mut self) {
        match self {
            Self::Add(f) => f.cycle_profile_right(),
            Self::Edit(_, _, f) => f.cycle_profile_right(),
        }
    }

    /// Cycle profile selection backward.
    pub fn cycle_profile_left(&mut self) {
        match self {
            Self::Add(f) => f.cycle_profile_left(),
            Self::Edit(_, _, f) => f.cycle_profile_left(),
        }
    }

    /// Insert a character at the current field.
    pub fn insert_char(&mut self, c: char) {
        match self {
            Self::Add(f) => f.insert_char(c),
            Self::Edit(_, _, f) => f.insert_char(c),
        }
    }

    /// Delete last character from the current field.
    pub fn delete_char(&mut self) {
        match self {
            Self::Add(f) => f.delete_char(),
            Self::Edit(_, _, f) => f.delete_char(),
        }
    }

    /// Get the selected environment string.
    pub fn selected_env(&self) -> &str {
        match self {
            Self::Add(f) => f.selected_env(),
            Self::Edit(_, _, f) => f.selected_env(),
        }
    }

    /// Get the error message if any.
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Add(f) => f.error.as_deref(),
            Self::Edit(_, _, f) => f.error.as_deref(),
        }
    }

    /// Get the password field value.
    pub fn password(&self) -> &str {
        match self {
            Self::Add(f) => f.password(),
            Self::Edit(_, _, f) => f.password(),
        }
    }

    /// Check if password was modified.
    pub fn password_modified(&self) -> bool {
        match self {
            Self::Add(f) => f.password_modified,
            Self::Edit(_, _, f) => f.password_modified,
        }
    }

    /// Check if there's a stored password.
    pub fn has_stored_password(&self) -> bool {
        match self {
            Self::Add(f) => f.has_stored_password,
            Self::Edit(_, _, f) => f.has_stored_password,
        }
    }

    /// Add a new session line. Expands sessions section if first session.
    pub fn add_session_line(&mut self) {
        match self {
            Self::Add(f) => f.add_session_line(),
            Self::Edit(_, _, f) => f.add_session_line(),
        }
    }

    /// Remove the current session line. Collapses sessions if last removed.
    pub fn remove_session_line(&mut self) {
        match self {
            Self::Add(f) => f.remove_session_line(),
            Self::Edit(_, _, f) => f.remove_session_line(),
        }
    }

    /// Get the session cursor.
    pub fn session_cursor(&self) -> usize {
        match self {
            Self::Add(f) => f.session_cursor,
            Self::Edit(_, _, f) => f.session_cursor,
        }
    }

    /// Check if sessions section is collapsed.
    pub fn sessions_collapsed(&self) -> bool {
        match self {
            Self::Add(f) => f.sessions_collapsed,
            Self::Edit(_, _, f) => f.sessions_collapsed,
        }
    }

    /// Get reference to the inner form.
    pub fn inner(&self) -> &UnifiedForm {
        match self {
            Self::Add(f) => f,
            Self::Edit(_, _, f) => f,
        }
    }

    /// Get mutable reference to the inner form.
    pub fn inner_mut(&mut self) -> &mut UnifiedForm {
        match self {
            Self::Add(f) => f,
            Self::Edit(_, _, f) => f,
        }
    }

    /// Get the edit target (for Edit variants). Returns None for Add.
    pub fn edit_target(&self) -> Option<EditTarget> {
        match self {
            Self::Add(_) => None,
            Self::Edit(_, target, _) => Some(*target),
        }
    }

    /// Check if this form has sessions (group mode).
    pub fn is_group_form(&self) -> bool {
        match self {
            Self::Add(f) => f.is_group(),
            Self::Edit(_, target, _) => *target == EditTarget::Group,
        }
    }

    /// Check if this form is bookmark mode (no sessions).
    pub fn is_bookmark_form(&self) -> bool {
        !self.is_group_form()
    }

    /// Check if this is an add form (not editing existing).
    pub fn is_add(&self) -> bool {
        matches!(self, Self::Add(_))
    }

    // ── Legacy compatibility methods (used by tui/mod.rs, removed in task 003) ──

    /// Create a blank form for adding a new group (alias for new_add, adds one empty session).
    #[deprecated(note = "Use new_add() — sessions determine bookmark vs group")]
    pub fn new_group_add(settings: &Settings, profile_names: &[String]) -> Self {
        let mut state = Self::new_add(settings, profile_names);
        // Old GroupForm::new_add started with one empty session
        state.inner_mut().add_session_line();
        state
    }

    /// Create a pre-populated form for editing an existing group.
    #[deprecated(note = "Use new_edit(idx, EditTarget::Group, &group, profiles)")]
    pub fn new_group_edit(idx: usize, group: &BookmarkGroup, profile_names: &[String]) -> Self {
        Self::new_edit(idx, EditTarget::Group, group, profile_names)
    }

    /// Validate and build a BookmarkGroup (for group mode forms).
    pub fn validate_and_build_group(&mut self, config: &AppConfig) -> Result<BookmarkGroup> {
        let form = self.inner_mut();
        form.validate_and_build_group(config)
    }

    /// Validate and build the appropriate entry type based on sessions.
    pub fn validate_and_build(&mut self, config: &AppConfig) -> Result<UnifiedEntry> {
        let form = self.inner_mut();
        form.validate_and_build(config)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Build the profile options list: ["(none)", "profile-a", "profile-b", ...].
fn build_profile_options(profile_names: &[String]) -> Vec<String> {
    let mut options = Vec::with_capacity(profile_names.len() + 1);
    options.push("(none)".to_string());
    options.extend(profile_names.iter().cloned());
    options
}

/// Convert a trimmed string to Option (None if empty).
fn non_empty_option(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ─── Rendering ───────────────────────────────────────────────────────

/// Render the add/edit form as a centered overlay.
pub fn render_form(
    frame: &mut Frame,
    area: Rect,
    state: &FormState,
    settings: &Settings,
    tc: &ThemeColors,
) {
    match state {
        FormState::Add(f) | FormState::Edit(_, _, f) => {
            render_unified_form(frame, area, f, settings, tc);
        }
    }
}

/// Render the unified form (handles both bookmark and group mode).
fn render_unified_form(
    frame: &mut Frame,
    area: Rect,
    form: &UnifiedForm,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let popup = centered_rect(70, 90, area);
    frame.render_widget(Clear, popup);

    // Title with mode indicator
    let mode_suffix = if form.is_group() {
        let count = form.sessions.len();
        let label = if count == 1 { "session" } else { "sessions" };
        format!(" ({count} {label})")
    } else {
        String::new()
    };
    let title = if form.is_edit {
        format!(" Edit Connection{mode_suffix} ")
    } else {
        format!(" Add Connection{mode_suffix} ")
    };

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.border))
        .style(Style::default().bg(tc.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Build layout constraints
    let mut constraints: Vec<Constraint> = Vec::new();

    // All 12 fields
    for _ in 0..FIELD_COUNT {
        constraints.push(Constraint::Length(2));
    }

    // Sessions section (only when expanded and has sessions)
    if !form.sessions_collapsed && !form.sessions.is_empty() {
        // Sessions header
        constraints.push(Constraint::Length(1));
        // Session lines (2 per session: name + command)
        for _ in 0..form.sessions.len() {
            constraints.push(Constraint::Length(2));
        }
    }

    // Error
    if form.error.is_some() {
        constraints.push(Constraint::Length(1));
    }

    // Spacer + hints
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(1));

    let chunks = Layout::vertical(constraints).split(inner);

    // Field labels (all 12 fields)
    let field_labels = [
        ("Name", FIELD_NAME),
        ("Host", FIELD_HOST),
        ("User", FIELD_USER),
        ("Port", FIELD_PORT),
        ("Env", FIELD_ENV),
        ("Tags", FIELD_TAGS),
        ("Identity File", FIELD_IDENTITY),
        ("Proxy Jump", FIELD_PROXY),
        ("Notes", FIELD_NOTES),
        ("On-Connect", FIELD_ON_CONNECT),
        ("Password", FIELD_PASSWORD),
        ("Profile", FIELD_PROFILE),
    ];

    let mut chunk_idx = 0;

    // Render all fields
    for (label, field_idx) in &field_labels {
        render_field(frame, chunks[chunk_idx], label, *field_idx, form as &dyn FormFields, false, settings, tc);
        chunk_idx += 1;
    }

    // Render sessions section if expanded and has sessions
    if !form.sessions_collapsed && !form.sessions.is_empty() {
        // Sessions header
        let count = form.sessions.len();
        let label = if count == 1 { "Session" } else { "Sessions" };
        let header = Line::from(Span::styled(
            format!(" ── {label} ({count}) ──"),
            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(header), chunks[chunk_idx]);
        chunk_idx += 1;

        // Session lines
        for (i, session) in form.sessions.iter().enumerate() {
            let is_current = i == form.session_cursor;
            let prefix = if is_current { "  > " } else { "    " };
            let cursor = if is_current { "_" } else { "" };

            let name_style = if is_current {
                Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(tc.fg)
            };
            let cmd_style = if is_current {
                Style::default().fg(tc.fg)
            } else {
                Style::default().fg(tc.fg_muted)
            };

            let name_display = if session.name.is_empty() {
                "(unnamed)".to_string()
            } else {
                session.name.clone()
            };

            let on_connect_display = session.on_connect.as_deref().unwrap_or("(no command)");

            // Name line
            let name_line = Line::from(vec![
                Span::raw(format!("{}{}: ", prefix, i + 1)),
                Span::styled(name_display, name_style),
                Span::styled(cursor, name_style),
            ]);
            frame.render_widget(Paragraph::new(name_line), chunks[chunk_idx]);
            chunk_idx += 1;

            // Command line
            let cmd_line = Line::from(vec![
                Span::raw("     "),
                Span::styled(format!("cmd: {}", on_connect_display), cmd_style),
            ]);
            frame.render_widget(Paragraph::new(cmd_line), chunks[chunk_idx]);
            chunk_idx += 1;
        }
    }

    // Error message
    if let Some(ref err) = form.error {
        let color = if err.starts_with("Warning:") {
            tc.warning
        } else {
            tc.error
        };
        let line = Line::from(Span::styled(format!(" {err}"), Style::default().fg(color)));
        frame.render_widget(Paragraph::new(line), chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // Skip spacer
    chunk_idx += 1;

    // Hints line
    let hints = Line::from(vec![
        Span::styled(
            " Tab/\u{2193} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Next  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " S-Tab/\u{2191} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Prev  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Enter ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Save  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Esc ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Cancel  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Ctrl+O ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Add Session  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " - ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Remove", Style::default().fg(tc.fg_dim)),
    ]);
    if chunk_idx < chunks.len() {
        frame.render_widget(Paragraph::new(hints), chunks[chunk_idx]);
    }
}

/// Render a group form with fields + session lines section.
fn render_group_form(
    frame: &mut Frame,
    area: Rect,
    form: &GroupForm,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let popup = centered_rect(70, 90, area);
    frame.render_widget(Clear, popup);

    let title = if form.is_edit {
        " Edit Group "
    } else {
        " Add Group "
    };

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.border))
        .style(Style::default().bg(tc.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Layout: fields (skip password for groups) + sessions header + session lines + error + spacer + hints
    // Fields: 11 fields (skip password) * 2 lines = 22 lines
    // Sessions header: 1 line
    // Session lines: N * 2 lines (name + on_connect per session)
    // Error: 1 line (if any)
    // Spacer: Min(0)
    // Hints: 1 line
    let visible_fields = 11; // All except password
    let session_line_count = form.sessions.len();

    let mut constraints: Vec<Constraint> = Vec::with_capacity(visible_fields + 1 + session_line_count * 2 + 3);
    // Fields
    for _ in 0..visible_fields {
        constraints.push(Constraint::Length(2));
    }
    // Sessions header
    constraints.push(Constraint::Length(1));
    // Session lines
    for _ in 0..session_line_count {
        constraints.push(Constraint::Length(2));
    }
    // Error
    if form.error.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(1));

    let chunks = Layout::vertical(constraints).split(inner);

    // Field labels (skip password)
    let field_labels = [
        ("Name", FIELD_NAME),
        ("Host", FIELD_HOST),
        ("User", FIELD_USER),
        ("Port", FIELD_PORT),
        ("Env", FIELD_ENV),
        ("Tags", FIELD_TAGS),
        ("Identity File", FIELD_IDENTITY),
        ("Proxy Jump", FIELD_PROXY),
        ("Notes", FIELD_NOTES),
        ("On-Connect", FIELD_ON_CONNECT),
        ("Profile", FIELD_PROFILE),
    ];

    let mut chunk_idx = 0;
    for (label, field_idx) in &field_labels {
        render_field(frame, chunks[chunk_idx], label, *field_idx, form as &dyn FormFields, true, settings, tc);
        chunk_idx += 1;
    }

    // Sessions header: " Sessions (N) "
    let count = form.sessions.len();
    let label = if count == 1 {
        "Session".to_string()
    } else {
        "Sessions".to_string()
    };
    let line = Line::from(Span::styled(
        format!(" ── {label} ({count}) ──"),
        Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), chunks[chunk_idx]);
    chunk_idx += 1;

    // Session lines
    for (i, session) in form.sessions.iter().enumerate() {
        let is_current = i == form.session_cursor;
        let prefix = if is_current { "  > " } else { "    " };
        let cursor = if is_current { "_" } else { "" };

        let name_style = if is_current {
            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(tc.fg)
        };
        let cmd_style = if is_current {
            Style::default().fg(tc.fg)
        } else {
            Style::default().fg(tc.fg_muted)
        };

        let name_display = if session.name.is_empty() {
            "(unnamed)".to_string()
        } else {
            session.name.clone()
        };

        let on_connect_display = session.on_connect.as_deref().unwrap_or("(no command)");

        // Name line
        let name_line = Line::from(vec![
            Span::raw(format!("{}{}: ", prefix, i + 1)),
            Span::styled(name_display, name_style),
            Span::styled(cursor, name_style),
        ]);
        frame.render_widget(Paragraph::new(name_line), chunks[chunk_idx]);
        chunk_idx += 1;

        // Command line
        let cmd_line = Line::from(vec![
            Span::raw("     "),
            Span::styled(format!("cmd: {}", on_connect_display), cmd_style),
        ]);
        frame.render_widget(Paragraph::new(cmd_line), chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // Error message
    if let Some(ref err) = form.error {
        let color = if err.starts_with("Warning:") {
            tc.warning
        } else {
            tc.error
        };
        let line = Line::from(Span::styled(format!(" {err}"), Style::default().fg(color)));
        frame.render_widget(Paragraph::new(line), chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // Skip spacer
    chunk_idx += 1;

    // Hints line (with group-specific hints)
    let hints = Line::from(vec![
        Span::styled(
            " Tab/\u{2193} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Next  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " S-Tab/\u{2191} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Prev  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Enter ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Save  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Esc ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Cancel  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Ctrl+O ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Add Session  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " - ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Remove", Style::default().fg(tc.fg_dim)),
    ]);
    if chunk_idx < chunks.len() {
        frame.render_widget(Paragraph::new(hints), chunks[chunk_idx]);
    }
}

/// Render a bookmark-style form (works for both BookmarkForm and GroupForm via trait-like approach).
/// When `is_group` is true, the form is treated as a group form (different title, no password field).
fn render_bookmark_form(
    frame: &mut Frame,
    area: Rect,
    form: &dyn FormFields,
    is_group: bool,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let popup = centered_rect(65, 80, area);
    frame.render_widget(Clear, popup);

    let title = if is_group {
        if form.is_edit() {
            " Edit Group "
        } else {
            " Add Group "
        }
    } else {
        if form.is_edit() {
            " Edit Bookmark "
        } else {
            " Add Bookmark "
        }
    };

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.border))
        .style(Style::default().bg(tc.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Layout: fields + optional error + hint line
    let field_count = FIELD_COUNT as u16;
    let mut constraints: Vec<Constraint> = (0..field_count)
        .map(|_| Constraint::Length(2))
        .collect();
    if form.error().is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(1));

    let chunks = Layout::vertical(constraints).split(inner);

    let field_labels = [
        "Name",
        "Host",
        "User",
        "Port",
        "Env",
        "Tags",
        "Identity File",
        "Proxy Jump",
        "Notes",
        "On-Connect",
        "Password",
        "Profile",
    ];

    for (i, &label) in field_labels.iter().enumerate() {
        render_field(frame, chunks[i], label, i, form, is_group, settings, tc);
    }

    // Error message
    let mut hint_idx = FIELD_COUNT;
    if let Some(ref err) = form.error() {
        let color = if err.starts_with("Warning:") {
            tc.warning
        } else {
            tc.error
        };
        let line = Line::from(Span::styled(format!(" {err}"), Style::default().fg(color)));
        frame.render_widget(Paragraph::new(line), chunks[hint_idx]);
        hint_idx += 1;
    }

    // Skip spacer
    hint_idx += 1;

    // Hints line
    let hints = Line::from(vec![
        Span::styled(
            " Tab/\u{2193} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Next  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " S-Tab/\u{2191} ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Prev  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Enter ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Save  ", Style::default().fg(tc.fg_dim)),
        Span::styled(
            " Esc ",
            Style::default()
                .fg(tc.hint_key_fg)
                .bg(tc.hint_key_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Cancel", Style::default().fg(tc.fg_dim)),
    ]);
    if hint_idx < chunks.len() {
        frame.render_widget(Paragraph::new(hints), chunks[hint_idx]);
    }
}

/// Trait for shared form field access (used by render functions).
trait FormFields {
    fn fields(&self) -> &[String; FIELD_COUNT];
    fn focused(&self) -> usize;
    fn env_index(&self) -> usize;
    fn profile_index(&self) -> usize;
    fn profile_options(&self) -> &[String];
    fn is_edit(&self) -> bool;
    fn error(&self) -> Option<String>;
    fn has_stored_password(&self) -> bool;
    fn password_modified(&self) -> bool;
}

impl FormFields for BookmarkForm {
    fn fields(&self) -> &[String; FIELD_COUNT] { &self.fields }
    fn focused(&self) -> usize { self.focused }
    fn env_index(&self) -> usize { self.env_index }
    fn profile_index(&self) -> usize { self.profile_index }
    fn profile_options(&self) -> &[String] { &self.profile_options }
    fn is_edit(&self) -> bool { self.is_edit }
    fn error(&self) -> Option<String> { self.error.clone() }
    fn has_stored_password(&self) -> bool { self.has_stored_password }
    fn password_modified(&self) -> bool { self.password_modified }
}

impl FormFields for GroupForm {
    fn fields(&self) -> &[String; FIELD_COUNT] { &self.fields }
    fn focused(&self) -> usize { self.focused }
    fn env_index(&self) -> usize { self.env_index }
    fn profile_index(&self) -> usize { self.profile_index }
    fn profile_options(&self) -> &[String] { &self.profile_options }
    fn is_edit(&self) -> bool { self.is_edit }
    fn error(&self) -> Option<String> { self.error.clone() }
    fn has_stored_password(&self) -> bool { false }
    fn password_modified(&self) -> bool { false }
}

impl FormFields for UnifiedForm {
    fn fields(&self) -> &[String; FIELD_COUNT] { &self.fields }
    fn focused(&self) -> usize { self.focused }
    fn env_index(&self) -> usize { self.env_index }
    fn profile_index(&self) -> usize { self.profile_index }
    fn profile_options(&self) -> &[String] { &self.profile_options }
    fn is_edit(&self) -> bool { self.is_edit }
    fn error(&self) -> Option<String> { self.error.clone() }
    fn has_stored_password(&self) -> bool { self.has_stored_password }
    fn password_modified(&self) -> bool { self.password_modified }
}

/// Render a single form field (label + value).
fn render_field(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    field_idx: usize,
    form: &dyn FormFields,
    is_group: bool,
    settings: &Settings,
    tc: &ThemeColors,
) {
    let is_focused = field_idx == form.focused();
    let [label_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Label
    let label_style = if is_focused {
        Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(tc.fg_dim)
    };
    let required = matches!(field_idx, FIELD_NAME | FIELD_HOST);
    let marker = if required { " *" } else { "" };
    let label_line = Line::from(Span::styled(format!("  {label}{marker}"), label_style));
    frame.render_widget(Paragraph::new(label_line), label_area);

    // Skip password field for group forms
    if field_idx == FIELD_PASSWORD && is_group {
        let style = Style::default().fg(tc.fg_dim);
        let line = Line::from(Span::styled("  > (inherited from profile)", style));
        frame.render_widget(Paragraph::new(line), input_area);
        return;
    }

    // Input value — special cases for env, password, profile, and proxy jump fields
    if field_idx == FIELD_ENV {
        render_env_selector(frame, input_area, form, settings, is_focused, tc);
    } else if field_idx == FIELD_PASSWORD {
        render_password_field(frame, input_area, form, is_focused, tc);
    } else if field_idx == FIELD_PROFILE {
        render_profile_selector(frame, input_area, form, is_focused, tc);
    } else if field_idx == FIELD_PROXY {
        render_proxy_jump_field(frame, input_area, form, is_focused, tc);
    } else {
        let cursor = if is_focused { "_" } else { "" };
        let value = &form.fields()[field_idx];
        let input_style = if is_focused {
            Style::default().fg(tc.fg)
        } else {
            Style::default().fg(tc.fg_muted)
        };
        let prefix = if is_focused { "  > " } else { "    " };
        let line = Line::from(Span::styled(
            format!("{prefix}{value}{cursor}"),
            input_style,
        ));
        frame.render_widget(Paragraph::new(line), input_area);
    }
}

/// Render the environment cycle selector as colored badges.
fn render_env_selector(
    frame: &mut Frame,
    area: Rect,
    form: &dyn FormFields,
    settings: &Settings,
    is_focused: bool,
    tc: &ThemeColors,
) {
    let mut spans: Vec<Span> = vec![Span::raw(if is_focused { "  > " } else { "    " })];

    for (i, &env) in ENV_OPTIONS.iter().enumerate() {
        let is_selected = i == form.env_index();

        if env.is_empty() {
            let style = if is_selected {
                Style::default()
                    .fg(tc.fg)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(tc.fg_muted)
            };
            spans.push(Span::styled("(none)", style));
        } else {
            let span = env_badge::env_badge_span(env, settings);
            if is_selected {
                let mut style = span.style;
                style = style.add_modifier(Modifier::UNDERLINED);
                spans.push(Span::styled(span.content.to_string(), style));
            } else if !is_focused {
                spans.push(Span::styled(
                    span.content.to_string(),
                    Style::default().fg(tc.fg_muted),
                ));
            } else {
                spans.push(span);
            }
        }
        spans.push(Span::raw(" "));
    }

    if is_focused {
        spans.push(Span::styled(
            " \u{2190}/\u{2192} to cycle",
            Style::default().fg(tc.fg_muted),
        ));
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

/// Render the profile cycle selector as text labels.
fn render_profile_selector(
    frame: &mut Frame,
    area: Rect,
    form: &dyn FormFields,
    is_focused: bool,
    tc: &ThemeColors,
) {
    let mut spans: Vec<Span> = vec![Span::raw(if is_focused { "  > " } else { "    " })];

    for (i, option) in form.profile_options().iter().enumerate() {
        let is_selected = i == form.profile_index();

        let style = if is_selected {
            Style::default()
                .fg(tc.fg)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(tc.fg_muted)
        };
        spans.push(Span::styled(option.clone(), style));
        spans.push(Span::raw(" "));
    }

    if is_focused {
        spans.push(Span::styled(
            " \u{2190}/\u{2192} to cycle",
            Style::default().fg(tc.fg_muted),
        ));
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

/// Render the Proxy Jump field with placeholder text.
fn render_proxy_jump_field(
    frame: &mut Frame,
    area: Rect,
    form: &dyn FormFields,
    is_focused: bool,
    tc: &ThemeColors,
) {
    let prefix = if is_focused { "  > " } else { "    " };
    let value = &form.fields()[FIELD_PROXY];
    let cursor = if is_focused { "_" } else { "" };

    let line = if value.is_empty() {
        let style = Style::default().fg(tc.fg_dim);
        Line::from(Span::styled(
            format!("{prefix}{PROXY_JUMP_PLACEHOLDER}{cursor}"),
            style,
        ))
    } else {
        let style = if is_focused {
            Style::default().fg(tc.fg)
        } else {
            Style::default().fg(tc.fg_muted)
        };
        Line::from(Span::styled(format!("{prefix}{value}{cursor}"), style))
    };

    frame.render_widget(Paragraph::new(line), area);
}

/// Render the password field with masking.
fn render_password_field(
    frame: &mut Frame,
    area: Rect,
    form: &dyn FormFields,
    is_focused: bool,
    tc: &ThemeColors,
) {
    let prefix = if is_focused { "  > " } else { "    " };
    let value = &form.fields()[FIELD_PASSWORD];

    let line = if !value.is_empty() {
        let dots: String = "●".repeat(value.len());
        let cursor = if is_focused { "_" } else { "" };
        let style = if is_focused {
            Style::default().fg(tc.fg)
        } else {
            Style::default().fg(tc.fg_muted)
        };
        Line::from(Span::styled(format!("{prefix}{dots}{cursor}"), style))
    } else if form.has_stored_password() && !form.password_modified() {
        let style = Style::default().fg(tc.fg_dim);
        let cursor = if is_focused { "_" } else { "" };
        Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled("●●●● ", style),
            Span::styled("(stored in keychain)", style),
            Span::styled(cursor.to_string(), style),
        ])
    } else {
        let style = Style::default().fg(tc.fg_dim);
        let cursor = if is_focused { "_" } else { "" };
        Line::from(Span::styled(format!("{prefix}(not set){cursor}"), style))
    };

    frame.render_widget(Paragraph::new(line), area);
}

/// Create a centered rectangle with given percentage width and height.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1]);

    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bookmark() -> Bookmark {
        Bookmark {
            name: "prod-web-01".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "production".into(),
            tags: vec!["web".into(), "frontend".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
            proxy_jump: Some("bastion".into()),
            notes: Some("Primary web server".into()),
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

    fn sample_config() -> AppConfig {
        AppConfig {
            settings: Settings::default(),
            profiles: vec![],
            bookmarks: vec![sample_bookmark()],
            groups: vec![],
        }
    }

    // ─── UnifiedForm tests ───

    #[test]
    fn test_unified_form_new_add_defaults() {
        let settings = Settings {
            default_user: Some("admin".into()),
            ..Settings::default()
        };
        let form = UnifiedForm::new_add(&settings, &[]);
        assert!(!form.is_edit);
        assert_eq!(form.focused, FIELD_NAME);
        assert_eq!(form.fields[FIELD_PORT], "22");
        assert_eq!(form.fields[FIELD_USER], "admin");
        assert_eq!(form.env_index, 0); // (none)
        assert_eq!(form.profile_index, 0); // (none)
        assert_eq!(form.profile_options, vec!["(none)"]);
        assert!(!form.has_stored_password);
        assert!(!form.password_modified);
        assert!(form.password().is_empty());
        assert!(form.sessions.is_empty());
        assert!(form.sessions_collapsed);
        assert_eq!(form.session_cursor, 0);
    }

    #[test]
    fn test_unified_form_new_edit_bookmark_populates() {
        let bookmark = sample_bookmark();
        let form = UnifiedForm::new_edit_bookmark(&bookmark, &[]);
        assert!(form.is_edit);
        assert_eq!(form.fields[FIELD_NAME], "prod-web-01");
        assert_eq!(form.fields[FIELD_HOST], "10.0.1.5");
        assert_eq!(form.fields[FIELD_USER], "deploy");
        assert_eq!(form.fields[FIELD_PORT], "22");
        assert_eq!(form.fields[FIELD_TAGS], "web, frontend");
        assert_eq!(form.fields[FIELD_IDENTITY], "~/.ssh/id_ed25519");
        assert_eq!(form.fields[FIELD_PROXY], "bastion");
        assert_eq!(form.fields[FIELD_NOTES], "Primary web server");
        assert_eq!(form.selected_env(), "production");
        assert!(form.password().is_empty());
        assert!(!form.password_modified);
        assert!(form.sessions.is_empty());
        assert!(form.sessions_collapsed);
    }

    #[test]
    fn test_unified_form_new_edit_group_populates() {
        let group = BookmarkGroup {
            name: "prod-web".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 2222,
            env: "production".into(),
            tags: vec!["web".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
            proxy_jump: Some("bastion".into()),
            notes: Some("Web servers".into()),
            profile: None,
            on_connect: Some("cd /app".into()),
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
                    on_connect: None,
                    ..Session::default()
                },
            ],
        };
        let form = UnifiedForm::new_edit_group(&group, &[]);
        assert!(form.is_edit);
        assert_eq!(form.fields[FIELD_NAME], "prod-web");
        assert_eq!(form.fields[FIELD_HOST], "10.0.1.5");
        assert_eq!(form.fields[FIELD_PORT], "2222");
        assert_eq!(form.selected_env(), "production");
        assert_eq!(form.sessions.len(), 2);
        assert_eq!(form.sessions[0].name, "session-a");
        assert_eq!(form.sessions[1].name, "session-b");
        assert!(!form.sessions_collapsed);
    }

    #[test]
    fn test_unified_form_new_edit_group_empty_sessions_gets_one() {
        let group = BookmarkGroup {
            name: "empty-group".into(),
            host: "10.0.1.5".into(),
            sessions: vec![],
            ..BookmarkGroup::default()
        };
        let form = UnifiedForm::new_edit_group(&group, &[]);
        assert_eq!(form.sessions.len(), 1); // Gets one empty session
        assert!(!form.sessions_collapsed);
    }

    #[test]
    fn test_form_state_unified_add() {
        let settings = Settings::default();
        let state = FormState::new_add(&settings, &[]);
        if let FormState::Add(f) = state {
            assert!(!f.is_edit);
            assert!(f.sessions.is_empty());
        } else {
            panic!("Expected Add variant");
        }
    }

    #[test]
    fn test_form_state_unified_edit_bookmark() {
        let bookmark = sample_bookmark();
        let state = FormState::new_edit(0, EditTarget::Bookmark, &bookmark, &[]);
        if let FormState::Edit(_, EditTarget::Bookmark, f) = state {
            assert!(f.is_edit);
            assert!(f.sessions.is_empty());
            assert!(f.sessions_collapsed);
        } else {
            panic!("Expected Edit variant with Bookmark target");
        }
    }

    #[test]
    fn test_form_state_unified_edit_group() {
        let group = BookmarkGroup {
            name: "test-group".into(),
            host: "10.0.1.5".into(),
            sessions: vec![Session {
                name: "s1".into(),
                ..Session::default()
            }],
            ..BookmarkGroup::default()
        };
        let state = FormState::new_edit(0, EditTarget::Group, &group, &[]);
        if let FormState::Edit(_, EditTarget::Group, f) = state {
            assert!(f.is_edit);
            assert_eq!(f.sessions.len(), 1);
            assert!(!f.sessions_collapsed);
        } else {
            panic!("Expected Edit variant with Group target");
        }
    }

    #[test]
    fn test_unified_form_sessions_collapsed_default() {
        // new_add starts with sessions collapsed
        let settings = Settings::default();
        let form = UnifiedForm::new_add(&settings, &[]);
        assert!(form.sessions_collapsed);
        assert!(form.sessions.is_empty());
    }

    #[test]
    fn test_unified_form_add_session_expands() {
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        assert!(form.sessions_collapsed);

        form.add_session_line();
        assert!(!form.sessions_collapsed);
        assert_eq!(form.sessions.len(), 1);
    }

    #[test]
    fn test_unified_form_remove_last_session_collapses() {
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        form.add_session_line();
        assert!(!form.sessions_collapsed);

        form.remove_session_line();
        assert!(form.sessions_collapsed);
        assert!(form.sessions.is_empty());
    }

    #[test]
    fn test_unified_form_is_group_transitions() {
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        assert!(form.is_bookmark());
        assert!(!form.is_group());

        form.add_session_line();
        assert!(!form.is_bookmark());
        assert!(form.is_group());

        form.remove_session_line();
        assert!(form.is_bookmark());
        assert!(!form.is_group());
    }

    // ─── UnifiedEntry validation tests ───

    #[test]
    fn test_validate_and_build_returns_bookmark_for_zero_sessions() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let entry = form.validate_and_build(&config).unwrap();
        match entry {
            UnifiedEntry::Bookmark(b) => {
                assert_eq!(b.name, "test-server");
                assert_eq!(b.host, "10.0.1.5");
            }
            UnifiedEntry::Group(_) => panic!("Expected Bookmark"),
        }
    }

    #[test]
    fn test_validate_and_build_returns_group_for_sessions() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "my-group".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.add_session_line();
        form.sessions[0].name = "session-1".into();

        let entry = form.validate_and_build(&config).unwrap();
        match entry {
            UnifiedEntry::Group(g) => {
                assert_eq!(g.name, "my-group");
                assert_eq!(g.host, "10.0.1.5");
                assert_eq!(g.sessions.len(), 1);
            }
            UnifiedEntry::Bookmark(_) => panic!("Expected Group"),
        }
    }

    #[test]
    fn test_cross_type_name_conflict_bookmark_vs_group() {
        let mut config = AppConfig::default();
        config.groups.push(BookmarkGroup {
            name: "existing".into(),
            host: "10.0.0.1".into(),
            ..BookmarkGroup::default()
        });
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "existing".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let result = form.validate_and_build_bookmark(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_cross_type_name_conflict_group_vs_bookmark() {
        let mut config = AppConfig::default();
        config.bookmarks.push(sample_bookmark());
        let settings = Settings::default();
        let mut form = UnifiedForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "prod-web-01".into(); // Same name as sample_bookmark
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.add_session_line();
        form.sessions[0].name = "s1".into();

        let result = form.validate_and_build_group(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_edit_bookmark_preserves_type() {
        let bookmark = sample_bookmark();
        let mut form = UnifiedForm::new_edit_bookmark(&bookmark, &[]);
        let config = AppConfig::default();

        // Should build as bookmark (no sessions)
        let entry = form.validate_and_build(&config).unwrap();
        match entry {
            UnifiedEntry::Bookmark(b) => {
                assert_eq!(b.name, "prod-web-01");
            }
            UnifiedEntry::Group(_) => panic!("Expected Bookmark"),
        }
    }

    #[test]
    fn test_edit_group_preserves_type() {
        let group = BookmarkGroup {
            name: "test-group".into(),
            host: "10.0.1.5".into(),
            user: None,
            port: 22,
            env: String::new(),
            tags: vec![],
            identity_file: None,
            proxy_jump: None,
            notes: None,
            profile: None,
            on_connect: None,
            on_connect_prompt_pattern: None,
            snippets: vec![],
            connect_timeout_secs: None,
            ssh_options: std::collections::BTreeMap::new(),
            sessions: vec![Session {
                name: "s1".into(),
                ..Session::default()
            }],
        };
        let mut form = UnifiedForm::new_edit_group(&group, &[]);
        let config = AppConfig::default();

        // Should build as group (has sessions)
        let entry = form.validate_and_build(&config).unwrap();
        match entry {
            UnifiedEntry::Group(g) => {
                assert_eq!(g.name, "test-group");
                assert_eq!(g.sessions.len(), 1);
            }
            UnifiedEntry::Bookmark(_) => panic!("Expected Group"),
        }
    }

    // ─── BookmarkForm tests ───

    #[test]
    fn test_new_add_form_defaults() {
        let settings = Settings {
            default_user: Some("admin".into()),
            ..Settings::default()
        };
        let form = BookmarkForm::new_add(&settings, &[]);
        assert!(!form.is_edit);
        assert_eq!(form.focused, FIELD_NAME);
        assert_eq!(form.fields[FIELD_PORT], "22");
        assert_eq!(form.fields[FIELD_USER], "admin");
        assert_eq!(form.env_index, 0); // (none)
        assert_eq!(form.profile_index, 0); // (none)
        assert_eq!(form.profile_options, vec!["(none)"]);
        assert!(!form.has_stored_password);
        assert!(!form.password_modified);
        assert!(form.password().is_empty());
    }

    #[test]
    fn test_new_edit_form_populates() {
        let bookmark = sample_bookmark();
        let form = BookmarkForm::new_edit(&bookmark, &[]);
        assert!(form.is_edit);
        assert_eq!(form.fields[FIELD_NAME], "prod-web-01");
        assert_eq!(form.fields[FIELD_HOST], "10.0.1.5");
        assert_eq!(form.fields[FIELD_USER], "deploy");
        assert_eq!(form.fields[FIELD_PORT], "22");
        assert_eq!(form.fields[FIELD_TAGS], "web, frontend");
        assert_eq!(form.fields[FIELD_IDENTITY], "~/.ssh/id_ed25519");
        assert_eq!(form.fields[FIELD_PROXY], "bastion");
        assert_eq!(form.fields[FIELD_NOTES], "Primary web server");
        assert_eq!(form.selected_env(), "production");
        assert!(form.password().is_empty());
        assert!(!form.password_modified);
    }

    #[test]
    fn test_field_navigation() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        assert_eq!(form.focused, 0);

        form.next_field();
        assert_eq!(form.focused, 1);

        form.next_field();
        form.next_field();
        assert_eq!(form.focused, 3);

        form.prev_field();
        assert_eq!(form.focused, 2);

        form.focused = 0;
        form.prev_field();
        assert_eq!(form.focused, 0);

        form.focused = FIELD_COUNT - 1;
        form.next_field();
        assert_eq!(form.focused, FIELD_COUNT - 1);
    }

    #[test]
    fn test_env_cycling() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        assert_eq!(form.env_index, 0);

        form.cycle_env_right();
        assert_eq!(form.selected_env(), "production");

        form.cycle_env_right();
        assert_eq!(form.selected_env(), "staging");

        form.env_index = ENV_OPTIONS.len() - 1;
        form.cycle_env_right();
        assert_eq!(form.env_index, 0);

        form.cycle_env_left();
        assert_eq!(form.env_index, ENV_OPTIONS.len() - 1);
    }

    #[test]
    fn test_char_insert_and_delete() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.focused = FIELD_NAME;

        form.insert_char('a');
        form.insert_char('b');
        assert_eq!(form.fields[FIELD_NAME], "ab");

        form.delete_char();
        assert_eq!(form.fields[FIELD_NAME], "a");
    }

    #[test]
    fn test_env_field_ignores_typing() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.focused = FIELD_ENV;

        form.insert_char('x');
        assert_eq!(form.fields[FIELD_ENV], "");

        form.delete_char();
        assert_eq!(form.fields[FIELD_ENV], "");
    }

    #[test]
    fn test_validate_and_build_success() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PORT] = "2222".into();
        form.fields[FIELD_TAGS] = "web, api".into();
        form.env_index = 3; // development

        let bookmark = form.validate_and_build(&config).unwrap();
        assert_eq!(bookmark.name, "test-server");
        assert_eq!(bookmark.host, "10.0.1.5");
        assert_eq!(bookmark.port, 2222);
        assert_eq!(bookmark.tags, vec!["web", "api"]);
        assert_eq!(bookmark.env, "development");
        assert!(bookmark.user.is_none());
    }

    #[test]
    fn test_validate_empty_name_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_empty_host_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_invalid_port_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PORT] = "abc".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_duplicate_name_fails() {
        let config = sample_config();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "prod-web-01".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_validate_edit_same_name_succeeds() {
        let config = sample_config();
        let bookmark = sample_bookmark();
        let mut form = BookmarkForm::new_edit(&bookmark, &[]);

        let result = form.validate_and_build(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_edit_rename_to_existing_fails() {
        let mut config = sample_config();
        config.bookmarks.push(Bookmark {
            name: "other-server".into(),
            host: "10.0.2.1".into(),
            ..sample_bookmark()
        });

        let bookmark = sample_bookmark();
        let mut form = BookmarkForm::new_edit(&bookmark, &[]);
        form.fields[FIELD_NAME] = "other-server".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_shell_metachar_in_host_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test".into();
        form.fields[FIELD_HOST] = "host;evil".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_auto_detect_env_on_name_input() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.focused = FIELD_NAME;
        for c in "prod-web".chars() {
            form.insert_char(c);
        }
        assert_eq!(form.selected_env(), "production");
    }

    #[test]
    fn test_password_field_masking_and_tracking() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.focused = FIELD_PASSWORD;

        assert!(!form.password_modified);

        form.insert_char('s');
        form.insert_char('e');
        form.insert_char('c');
        assert!(form.password_modified);
        assert_eq!(form.password(), "sec");

        form.delete_char();
        assert!(form.password_modified);
        assert_eq!(form.password(), "se");
    }

    #[test]
    fn test_password_field_clearing_marks_modified() {
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.has_stored_password = true;
        form.focused = FIELD_PASSWORD;

        form.insert_char('x');
        form.delete_char();
        assert!(form.password_modified);
        assert!(form.password().is_empty());
    }

    #[test]
    fn test_validate_port_zero_fails() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PORT] = "0".into();

        let result = form.validate_and_build(&config);
        assert!(result.is_err());
    }

    // ─── Profile selector tests ───

    #[test]
    fn test_profile_cycling() {
        let settings = Settings::default();
        let profiles = vec!["corp-bastion".to_string(), "dev-tunnel".to_string()];
        let mut form = BookmarkForm::new_add(&settings, &profiles);

        assert_eq!(form.profile_index, 0);
        assert!(form.selected_profile().is_none());

        form.cycle_profile_right();
        assert_eq!(form.selected_profile(), Some("corp-bastion"));

        form.cycle_profile_right();
        assert_eq!(form.selected_profile(), Some("dev-tunnel"));

        form.cycle_profile_right();
        assert_eq!(form.profile_index, 0);
        assert!(form.selected_profile().is_none());

        form.cycle_profile_left();
        assert_eq!(form.selected_profile(), Some("dev-tunnel"));
    }

    #[test]
    fn test_profile_field_ignores_typing() {
        let settings = Settings::default();
        let profiles = vec!["ops".to_string()];
        let mut form = BookmarkForm::new_add(&settings, &profiles);
        form.focused = FIELD_PROFILE;

        form.insert_char('x');
        assert_eq!(form.fields[FIELD_PROFILE], "");

        form.delete_char();
        assert_eq!(form.fields[FIELD_PROFILE], "");
    }

    #[test]
    fn test_new_edit_form_populates_profile() {
        let profiles = vec!["ops".to_string(), "dev".to_string()];
        let mut bookmark = sample_bookmark();
        bookmark.profile = Some("dev".to_string());

        let form = BookmarkForm::new_edit(&bookmark, &profiles);
        assert_eq!(form.profile_index, 2);
        assert_eq!(form.selected_profile(), Some("dev"));
    }

    #[test]
    fn test_new_edit_form_deleted_profile_falls_back_to_none() {
        let profiles = vec!["ops".to_string()];
        let mut bookmark = sample_bookmark();
        bookmark.profile = Some("deleted-profile".to_string());

        let form = BookmarkForm::new_edit(&bookmark, &profiles);
        assert_eq!(form.profile_index, 0);
        assert!(form.selected_profile().is_none());
    }

    #[test]
    fn test_validate_and_build_with_profile() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let profiles = vec!["corp-bastion".to_string()];
        let mut form = BookmarkForm::new_add(&settings, &profiles);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.profile_index = 1;

        let bookmark = form.validate_and_build(&config).unwrap();
        assert_eq!(bookmark.profile, Some("corp-bastion".to_string()));
    }

    #[test]
    fn test_validate_and_build_no_profile() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let profiles = vec!["corp-bastion".to_string()];
        let mut form = BookmarkForm::new_add(&settings, &profiles);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let bookmark = form.validate_and_build(&config).unwrap();
        assert!(bookmark.profile.is_none());
    }

    #[test]
    fn test_profile_options_with_no_profiles() {
        let settings = Settings::default();
        let form = BookmarkForm::new_add(&settings, &[]);
        assert_eq!(form.profile_options, vec!["(none)"]);
        assert_eq!(form.profile_index, 0);
        assert!(form.selected_profile().is_none());
    }

    #[test]
    fn test_proxy_jump_field_empty_in_add_form() {
        let settings = Settings::default();
        let form = BookmarkForm::new_add(&settings, &[]);
        assert_eq!(form.fields[FIELD_PROXY], "");
    }

    #[test]
    fn test_proxy_jump_field_populated_in_edit_form() {
        let bookmark = sample_bookmark();
        let form = BookmarkForm::new_edit(&bookmark, &[]);
        assert_eq!(form.fields[FIELD_PROXY], "bastion");
    }

    #[test]
    fn test_validate_and_build_proxy_jump_included() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();
        form.fields[FIELD_PROXY] = "admin@bastion:2222".into();

        let bookmark = form.validate_and_build(&config).unwrap();
        assert_eq!(bookmark.proxy_jump, Some("admin@bastion:2222".to_string()));
    }

    #[test]
    fn test_validate_and_build_empty_proxy_jump_is_none() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut form = BookmarkForm::new_add(&settings, &[]);
        form.fields[FIELD_NAME] = "test-server".into();
        form.fields[FIELD_HOST] = "10.0.1.5".into();

        let bookmark = form.validate_and_build(&config).unwrap();
        assert!(bookmark.proxy_jump.is_none());
    }

    // ─── FormState enum delegation tests ───

    #[test]
    fn test_form_state_new_add_wraps_bookmark_form() {
        let settings = Settings::default();
        let state = FormState::new_add(&settings, &[]);
        assert!(state.is_bookmark_form());
        assert!(!state.is_group_form());
    }

    #[test]
    fn test_form_state_new_group_add_has_one_session() {
        let settings = Settings::default();
        let state = FormState::new_group_add(&settings, &[]);
        assert!(state.is_group_form());
        assert!(!state.is_bookmark_form());

        if let FormState::Add(f) = state {
            assert_eq!(f.sessions.len(), 1);
            assert_eq!(f.session_cursor, 0);
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_form_state_add_session_line() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        state.add_session_line();

        if let FormState::Add(f) = state {
            assert_eq!(f.sessions.len(), 2);
            assert_eq!(f.session_cursor, 1);
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_form_state_remove_session_line() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        // Add two sessions first
        state.add_session_line();
        state.add_session_line();

        if let FormState::Add(ref f) = state {
            assert_eq!(f.sessions.len(), 3);
        }

        // Remove current session
        state.remove_session_line();

        if let FormState::Add(f) = state {
            assert_eq!(f.sessions.len(), 2);
        }
    }

    #[test]
    fn test_form_state_remove_session_line_min_one() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        // In unified form, removing the last session reverts to bookmark mode
        state.remove_session_line();

        if let FormState::Add(f) = state {
            assert_eq!(f.sessions.len(), 0); // Reverted to bookmark mode
            assert!(f.sessions_collapsed);
        }
    }

    #[test]
    fn test_form_state_new_group_edit_populates() {
        let group = BookmarkGroup {
            name: "prod-web".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 2222,
            env: "production".into(),
            tags: vec!["web".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
            proxy_jump: Some("bastion".into()),
            notes: Some("Web servers".into()),
            profile: None,
            on_connect: Some("cd /app".into()),
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
                    on_connect: None,
                    ..Session::default()
                },
            ],
        };
        let state = FormState::new_group_edit(0, &group, &[]);

        if let FormState::Edit(_, EditTarget::Group, f) = state {
            assert!(f.is_edit);
            assert_eq!(f.original_name, Some("prod-web".into()));
            assert_eq!(f.fields[FIELD_NAME], "prod-web");
            assert_eq!(f.fields[FIELD_HOST], "10.0.1.5");
            assert_eq!(f.fields[FIELD_PORT], "2222");
            assert_eq!(f.selected_env(), "production");
            assert_eq!(f.sessions.len(), 2);
            assert_eq!(f.sessions[0].name, "session-a");
            assert_eq!(f.sessions[1].name, "session-b");
        } else {
            panic!("Expected GroupEdit variant");
        }
    }

    #[test]
    fn test_form_state_new_group_edit_empty_sessions_gets_one() {
        let group = BookmarkGroup {
            name: "empty-group".into(),
            host: "10.0.1.5".into(),
            sessions: vec![],
            ..BookmarkGroup::default()
        };
        let state = FormState::new_group_edit(0, &group, &[]);

        if let FormState::Edit(_, EditTarget::Group, f) = state {
            assert_eq!(f.sessions.len(), 1); // Gets one empty session
        } else {
            panic!("Expected GroupEdit variant");
        }
    }

    #[test]
    fn test_form_state_delegation_next_field_add() {
        let settings = Settings::default();
        let mut state = FormState::new_add(&settings, &[]);
        assert_eq!(state.focused(), FIELD_NAME);

        state.next_field();
        assert_eq!(state.focused(), FIELD_HOST);
    }

    #[test]
    fn test_form_state_delegation_next_field_group_add() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);
        assert_eq!(state.focused(), FIELD_NAME);

        state.next_field();
        assert_eq!(state.focused(), FIELD_HOST);
    }

    #[test]
    fn test_form_state_delegation_insert_char() {
        let settings = Settings::default();
        let mut state = FormState::new_add(&settings, &[]);
        state.insert_char('h');
        state.insert_char('i');

        if let FormState::Add(f) = state {
            assert_eq!(f.fields[FIELD_NAME], "hi");
        } else {
            panic!("Expected Add variant");
        }
    }

    #[test]
    fn test_form_state_delegation_insert_char_group() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);
        state.insert_char('g');

        if let FormState::Add(f) = state {
            assert_eq!(f.fields[FIELD_NAME], "g");
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_form_state_validate_and_build_group() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.fields[FIELD_NAME] = "my-group".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.sessions[0].name = "session-1".into();
        }

        let group = state.validate_and_build_group(&config).unwrap();
        assert_eq!(group.name, "my-group");
        assert_eq!(group.host, "10.0.1.5");
        assert_eq!(group.sessions.len(), 1);
        assert_eq!(group.sessions[0].name, "session-1");
    }

    #[test]
    fn test_form_state_validate_and_build_group_duplicate_session_names() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.fields[FIELD_NAME] = "my-group".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.sessions[0].name = "dup".into();
            f.add_session_line();
            f.sessions[1].name = "dup".into();
        }

        let result = state.validate_and_build_group(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Duplicate session name"));
    }

    #[test]
    fn test_form_state_validate_and_build_group_empty_session_name() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.fields[FIELD_NAME] = "my-group".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.sessions[0].name = "".into();
        }

        let result = state.validate_and_build_group(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Session name cannot be empty"));
    }

    #[test]
    fn test_form_state_validate_and_build_group_duplicate_group_name() {
        let mut config = AppConfig::default();
        config.groups.push(BookmarkGroup {
            name: "existing".into(),
            host: "10.0.0.1".into(),
            ..BookmarkGroup::default()
        });
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.fields[FIELD_NAME] = "existing".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.sessions[0].name = "session-1".into();
        }

        let result = state.validate_and_build_group(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_form_state_validate_and_build_group_on_connect_escape_rejected() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.fields[FIELD_NAME] = "my-group".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.sessions[0].name = "session-1".into();
            // Escape sequence in on_connect should be rejected
            f.sessions[0].on_connect = Some("\x1b[31mred\x1b[0m".into());
        }

        let result = state.validate_and_build_group(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("escape"));
    }

    #[test]
    fn test_form_state_validate_and_build_group_on_connect_too_long() {
        let config = AppConfig::default();
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.fields[FIELD_NAME] = "my-group".into();
            f.fields[FIELD_HOST] = "10.0.1.5".into();
            f.sessions[0].name = "session-1".into();
            // on_connect exceeding 1024 bytes should be rejected
            f.sessions[0].on_connect = Some("x".repeat(1025));
        }

        let result = state.validate_and_build_group(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("maximum length"));
    }

    #[test]
    fn test_non_empty_option() {
        assert_eq!(non_empty_option(""), None);
        assert_eq!(non_empty_option("  "), None);
        assert_eq!(non_empty_option("hello"), Some("hello".into()));
        assert_eq!(non_empty_option("  hello  "), Some("hello".into()));
    }

    #[test]
    fn test_build_profile_options() {
        let names = vec!["alpha".to_string(), "beta".to_string()];
        let options = build_profile_options(&names);
        assert_eq!(options, vec!["(none)", "alpha", "beta"]);
    }

    #[test]
    fn test_proxy_jump_placeholder_constant() {
        assert_eq!(PROXY_JUMP_PLACEHOLDER, "(e.g. admin@bastion)");
    }

    // ─── Group form rendering tests ───

    #[test]
    fn test_group_form_empty_session_line_no_panic() {
        // Verify group form with 1 empty session line doesn't panic
        let settings = Settings::default();
        let state = FormState::new_group_add(&settings, &[]);
        if let FormState::Add(f) = state {
            assert_eq!(f.sessions.len(), 1);
            assert!(f.sessions[0].name.is_empty());
            assert!(f.sessions[0].on_connect.is_none());
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_group_form_session_count_display() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        // Add sessions
        state.add_session_line();
        state.add_session_line();

        if let FormState::Add(f) = &state {
            assert_eq!(f.sessions.len(), 3);
            // Session count shown as "Sessions (3)"
            let count = f.sessions.len();
            let label = if count == 1 { "Session" } else { "Sessions" };
            assert_eq!(label, "Sessions");
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_group_form_session_highlighting() {
        let settings = Settings::default();
        let mut state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &mut state {
            f.sessions[0].name = "session-a".into();
            f.add_session_line();
            f.sessions[1].name = "session-b".into();
        }

        if let FormState::Add(f) = &state {
            // session_cursor should be at the new session
            assert_eq!(f.session_cursor, 1);
            assert_eq!(f.sessions[0].name, "session-a");
            assert_eq!(f.sessions[1].name, "session-b");
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_group_form_single_session_label() {
        let settings = Settings::default();
        let state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &state {
            let count = f.sessions.len();
            let label = if count == 1 { "Session" } else { "Sessions" };
            assert_eq!(label, "Session"); // Singular for 1
        } else {
            panic!("Expected GroupAdd variant");
        }
    }

    #[test]
    fn test_group_form_unnamed_session_display() {
        let settings = Settings::default();
        let state = FormState::new_group_add(&settings, &[]);

        if let FormState::Add(f) = &state {
            // Empty name should display as "(unnamed)"
            assert!(f.sessions[0].name.is_empty());
        } else {
            panic!("Expected GroupAdd variant");
        }
    }
}
