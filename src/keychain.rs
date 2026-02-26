use anyhow::{Context, Result};

use crate::config::model::Bookmark;

/// Service name used for OS keychain entries.
const SERVICE_NAME: &str = "sshore";

/// Store a password in the OS keychain for a bookmark.
pub fn set_password(bookmark_name: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, bookmark_name)
        .context("Failed to create keychain entry")?;
    entry
        .set_password(password)
        .context("Failed to store password in keychain")?;
    Ok(())
}

/// Retrieve a stored password from the OS keychain.
/// Returns `Ok(None)` if no password is stored for this bookmark.
pub fn get_password(bookmark_name: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE_NAME, bookmark_name)
        .context("Failed to create keychain entry")?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Failed to read from keychain: {e}")),
    }
}

/// Delete a stored password from the OS keychain.
/// Returns `Ok(false)` if no password was stored for this bookmark.
pub fn delete_password(bookmark_name: &str) -> Result<bool> {
    let entry = keyring::Entry::new(SERVICE_NAME, bookmark_name)
        .context("Failed to create keychain entry")?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("Failed to delete from keychain: {e}")),
    }
}

/// List bookmark names that have stored passwords.
/// Iterates all bookmarks and checks the keychain for each (keyring has no list API).
pub fn list_passwords(bookmarks: &[Bookmark]) -> Vec<String> {
    bookmarks
        .iter()
        .filter(|b| matches!(get_password(&b.name), Ok(Some(_))))
        .map(|b| b.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test: requires OS keychain access.
    /// Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn test_set_get_delete_cycle() {
        let name = "sshore-test-keychain-cycle";
        let password = "test-password-123";

        // Clean up any leftover from previous runs
        let _ = delete_password(name);

        // Initially no password
        assert_eq!(get_password(name).unwrap(), None);

        // Set and retrieve
        set_password(name, password).unwrap();
        assert_eq!(get_password(name).unwrap(), Some(password.to_string()));

        // Delete and verify gone
        assert!(delete_password(name).unwrap());
        assert_eq!(get_password(name).unwrap(), None);
    }

    /// Deleting a nonexistent entry returns Ok(false).
    #[test]
    #[ignore]
    fn test_delete_nonexistent_returns_false() {
        let name = "sshore-test-nonexistent-bookmark";
        let _ = delete_password(name); // ensure clean
        assert!(!delete_password(name).unwrap());
    }

    /// list_passwords only returns bookmarks with stored passwords.
    #[test]
    #[ignore]
    fn test_list_filters_correctly() {
        let stored_name = "sshore-test-list-stored";
        let unstored_name = "sshore-test-list-unstored";

        // Clean up
        let _ = delete_password(stored_name);
        let _ = delete_password(unstored_name);

        set_password(stored_name, "secret").unwrap();

        let bookmarks = vec![
            Bookmark {
                name: stored_name.into(),
                host: "host1".into(),
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
            Bookmark {
                name: unstored_name.into(),
                host: "host2".into(),
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

        let result = list_passwords(&bookmarks);
        assert_eq!(result, vec![stored_name.to_string()]);

        // Clean up
        let _ = delete_password(stored_name);
    }
}
