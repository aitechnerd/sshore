use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use russh::keys::PublicKey;
use sha2::{Digest, Sha256};

/// Result of checking a server's host key against known_hosts.
#[derive(Debug)]
pub enum HostKeyStatus {
    /// Key matches a known entry — safe to proceed.
    Known,
    /// Host not in known_hosts — prompt user to accept.
    Unknown {
        fingerprint: String,
        key_type: String,
    },
    /// Host IS in known_hosts but the key has CHANGED — potential MITM attack.
    Changed {
        fingerprint_new: String,
        known_hosts_line: usize,
    },
}

/// Path to the user's known_hosts file.
fn known_hosts_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("known_hosts")
}

/// Check a server's host key against ~/.ssh/known_hosts.
pub fn check_host_key(hostname: &str, port: u16, server_key: &PublicKey) -> Result<HostKeyStatus> {
    let known_hosts_file = known_hosts_path();

    if !known_hosts_file.exists() {
        let fingerprint = format_fingerprint(server_key);
        let key_type = format_key_type(server_key);
        return Ok(HostKeyStatus::Unknown {
            fingerprint,
            key_type,
        });
    }

    let content =
        std::fs::read_to_string(&known_hosts_file).context("Failed to read known_hosts")?;

    // Build the host pattern to match (hostname, or [hostname]:port for non-22)
    let host_pattern = if port == 22 {
        hostname.to_string()
    } else {
        format!("[{}]:{}", hostname, port)
    };

    let server_key_data = server_key.to_bytes().unwrap_or_default();

    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(entry) = parse_known_hosts_line(line)
            && entry.matches_host(&host_pattern)
        {
            if entry.key_matches(&server_key_data) {
                return Ok(HostKeyStatus::Known);
            } else {
                // KEY CHANGED — potential MITM
                return Ok(HostKeyStatus::Changed {
                    fingerprint_new: format_fingerprint(server_key),
                    known_hosts_line: line_num + 1,
                });
            }
        }
    }

    // Not found in known_hosts
    let fingerprint = format_fingerprint(server_key);
    let key_type = format_key_type(server_key);
    Ok(HostKeyStatus::Unknown {
        fingerprint,
        key_type,
    })
}

/// Append a new host key entry to ~/.ssh/known_hosts.
pub fn add_host_key(hostname: &str, port: u16, server_key: &PublicKey) -> Result<()> {
    let known_hosts_file = known_hosts_path();

    // Ensure ~/.ssh directory exists
    if let Some(parent) = known_hosts_file.parent() {
        std::fs::create_dir_all(parent).context("Failed to create ~/.ssh directory")?;
    }

    let host_pattern = if port == 22 {
        hostname.to_string()
    } else {
        format!("[{}]:{}", hostname, port)
    };

    let entry = format_known_hosts_entry(&host_pattern, server_key);

    // Append to file
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&known_hosts_file)
        .context("Failed to open known_hosts for writing")?;

    writeln!(file, "{}", entry)?;

    // Ensure 0600 permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&known_hosts_file, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// A parsed known_hosts entry.
struct KnownHostEntry {
    /// Host patterns (comma-separated in the file).
    host_patterns: Vec<String>,
    /// Key type (e.g., "ssh-ed25519", "ssh-rsa").
    _key_type: String,
    /// Base64-encoded public key data.
    key_base64: String,
}

impl KnownHostEntry {
    /// Check if this entry matches the given host pattern.
    /// `host_pattern` is "hostname" for port 22, or "[hostname]:port" for other ports.
    fn matches_host(&self, host_pattern: &str) -> bool {
        for pattern in &self.host_patterns {
            if pattern == host_pattern {
                return true;
            }
            // Hashed hostnames: |1|base64(salt)|base64(HMAC-SHA1(salt, hostname))
            if check_hashed_host(pattern, host_pattern) == Some(true) {
                return true;
            }
        }
        false
    }

    /// Check if the stored key matches the server's key data.
    fn key_matches(&self, server_key_data: &[u8]) -> bool {
        if let Ok(stored_bytes) = base64::engine::general_purpose::STANDARD.decode(&self.key_base64)
        {
            stored_bytes == server_key_data
        } else {
            false
        }
    }
}

/// Check if a hashed known_hosts pattern matches the given hostname.
///
/// OpenSSH hashed format: `|1|base64(salt)|base64(HMAC-SHA1(salt, hostname))`
/// Returns `Some(true)` if matched, `Some(false)` if valid hash but no match,
/// `None` if the pattern is not a hashed entry.
fn check_hashed_host(stored_pattern: &str, host_to_check: &str) -> Option<bool> {
    // Format: |1|salt_b64|hash_b64 → split by '|' gives ["", "1", "salt_b64", "hash_b64"]
    let parts: Vec<&str> = stored_pattern.split('|').collect();
    if parts.len() != 4 || !parts[0].is_empty() || parts[1] != "1" {
        return None;
    }

    let salt = base64::engine::general_purpose::STANDARD
        .decode(parts[2])
        .ok()?;
    let stored_hash = base64::engine::general_purpose::STANDARD
        .decode(parts[3])
        .ok()?;

    type HmacSha1 = Hmac<sha1::Sha1>;
    let mut mac = HmacSha1::new_from_slice(&salt).ok()?;
    mac.update(host_to_check.as_bytes());
    let computed = mac.finalize().into_bytes();

    Some(computed.as_slice() == stored_hash.as_slice())
}

/// Parse a single known_hosts line into its components.
fn parse_known_hosts_line(line: &str) -> Option<KnownHostEntry> {
    // Format: host_patterns key_type base64_key [comment]
    let mut parts = line.splitn(3, char::is_whitespace);
    let hosts_str = parts.next()?.trim();
    let key_type = parts.next()?.trim().to_string();
    let rest = parts.next()?.trim();

    // The base64 key is the next whitespace-delimited token (ignore trailing comment)
    let key_base64 = rest.split_whitespace().next().unwrap_or("").to_string();

    if key_base64.is_empty() {
        return None;
    }

    let host_patterns: Vec<String> = hosts_str.split(',').map(|s| s.trim().to_string()).collect();

    Some(KnownHostEntry {
        host_patterns,
        _key_type: key_type,
        key_base64,
    })
}

/// Format a public key's fingerprint as SHA256 hash.
fn format_fingerprint(key: &PublicKey) -> String {
    let key_bytes = key.to_bytes().unwrap_or_default();
    let hash = Sha256::digest(&key_bytes);
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash);
    format!("SHA256:{b64}")
}

/// Format the key type as a human-readable string.
fn format_key_type(key: &PublicKey) -> String {
    key.algorithm().to_string()
}

/// Format a known_hosts entry line.
fn format_known_hosts_entry(host_pattern: &str, key: &PublicKey) -> String {
    let key_type = format_key_type(key);
    let key_bytes = key.to_bytes().unwrap_or_default();
    let key_base64 = base64::engine::general_purpose::STANDARD.encode(&key_bytes);
    format!("{host_pattern} {key_type} {key_base64}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_pattern_standard_port() {
        let entry = KnownHostEntry {
            host_patterns: vec!["example.com".into()],
            _key_type: "ssh-ed25519".into(),
            key_base64: "AAAA".into(),
        };
        assert!(entry.matches_host("example.com"));
        assert!(!entry.matches_host("[example.com]:2222"));
    }

    #[test]
    fn test_host_pattern_non_standard_port() {
        let entry = KnownHostEntry {
            host_patterns: vec!["[example.com]:2222".into()],
            _key_type: "ssh-ed25519".into(),
            key_base64: "AAAA".into(),
        };
        assert!(entry.matches_host("[example.com]:2222"));
        assert!(!entry.matches_host("example.com"));
    }

    #[test]
    fn test_host_pattern_multiple_hosts() {
        let entry = KnownHostEntry {
            host_patterns: vec!["host1.example.com".into(), "host2.example.com".into()],
            _key_type: "ssh-rsa".into(),
            key_base64: "AAAA".into(),
        };
        assert!(entry.matches_host("host1.example.com"));
        assert!(entry.matches_host("host2.example.com"));
        assert!(!entry.matches_host("host3.example.com"));
    }

    #[test]
    fn test_parse_known_hosts_line_basic() {
        let line = "example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest";
        let entry = parse_known_hosts_line(line).unwrap();
        assert_eq!(entry.host_patterns, vec!["example.com"]);
        assert_eq!(entry._key_type, "ssh-ed25519");
        assert_eq!(entry.key_base64, "AAAAC3NzaC1lZDI1NTE5AAAAItest");
    }

    #[test]
    fn test_parse_known_hosts_line_with_comment() {
        let line = "example.com ssh-rsa AAAAB3NzaC1yc2EAAAA user@host";
        let entry = parse_known_hosts_line(line).unwrap();
        assert_eq!(entry._key_type, "ssh-rsa");
        assert_eq!(entry.key_base64, "AAAAB3NzaC1yc2EAAAA");
    }

    #[test]
    fn test_parse_known_hosts_line_comma_hosts() {
        let line = "host1,host2,host3 ssh-ed25519 AAAA";
        let entry = parse_known_hosts_line(line).unwrap();
        assert_eq!(entry.host_patterns.len(), 3);
        assert_eq!(entry.host_patterns[0], "host1");
        assert_eq!(entry.host_patterns[2], "host3");
    }

    #[test]
    fn test_parse_known_hosts_line_bracketed_port() {
        let line = "[example.com]:2222 ssh-ed25519 AAAA";
        let entry = parse_known_hosts_line(line).unwrap();
        assert_eq!(entry.host_patterns, vec!["[example.com]:2222"]);
    }

    #[test]
    fn test_known_hosts_path() {
        let path = known_hosts_path();
        assert!(path.ends_with(".ssh/known_hosts"));
    }

    #[test]
    fn test_hashed_hostname_matching() {
        // Generate a known HMAC-SHA1 hash for "example.com"
        type HmacSha1 = Hmac<sha1::Sha1>;
        let salt = b"test_salt_20_bytes!!";
        let mut mac = HmacSha1::new_from_slice(salt).unwrap();
        mac.update(b"example.com");
        let hash = mac.finalize().into_bytes();

        let salt_b64 = base64::engine::general_purpose::STANDARD.encode(salt);
        let hash_b64 = base64::engine::general_purpose::STANDARD.encode(&hash);
        let hashed_pattern = format!("|1|{salt_b64}|{hash_b64}");

        let entry = KnownHostEntry {
            host_patterns: vec![hashed_pattern],
            _key_type: "ssh-ed25519".into(),
            key_base64: "AAAA".into(),
        };

        assert!(entry.matches_host("example.com"));
        assert!(!entry.matches_host("other.com"));
    }

    #[test]
    fn test_hashed_hostname_non_standard_port() {
        type HmacSha1 = Hmac<sha1::Sha1>;
        let salt = b"another_salt_bytes!!";
        let hostname = "[myhost.io]:2222";
        let mut mac = HmacSha1::new_from_slice(salt).unwrap();
        mac.update(hostname.as_bytes());
        let hash = mac.finalize().into_bytes();

        let salt_b64 = base64::engine::general_purpose::STANDARD.encode(salt);
        let hash_b64 = base64::engine::general_purpose::STANDARD.encode(&hash);
        let hashed_pattern = format!("|1|{salt_b64}|{hash_b64}");

        let entry = KnownHostEntry {
            host_patterns: vec![hashed_pattern],
            _key_type: "ssh-rsa".into(),
            key_base64: "AAAA".into(),
        };

        assert!(entry.matches_host("[myhost.io]:2222"));
        assert!(!entry.matches_host("myhost.io"));
    }

    #[test]
    fn test_hashed_pattern_not_confused_with_plain() {
        // A non-hashed pattern that starts with | shouldn't be treated as hashed
        let entry = KnownHostEntry {
            host_patterns: vec!["|2|abc|def".into()], // Wrong version
            _key_type: "ssh-ed25519".into(),
            key_base64: "AAAA".into(),
        };
        // Should not match anything via hash check (version != 1)
        assert!(!entry.matches_host("example.com"));
    }

    #[test]
    fn test_parse_hashed_known_hosts_line() {
        // Full line with hashed hostname
        let line = "|1|c2FsdA==|aGFzaA== ssh-ed25519 AAAA";
        let entry = parse_known_hosts_line(line).unwrap();
        assert_eq!(entry.host_patterns.len(), 1);
        assert!(entry.host_patterns[0].starts_with("|1|"));
    }

    #[test]
    fn test_key_matches_base64() {
        let data = vec![1, 2, 3, 4, 5];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let entry = KnownHostEntry {
            host_patterns: vec!["test".into()],
            _key_type: "ssh-rsa".into(),
            key_base64: b64,
        };
        assert!(entry.key_matches(&data));
        assert!(!entry.key_matches(&[1, 2, 3, 4, 6]));
    }

    #[test]
    fn test_key_matches_invalid_base64() {
        let entry = KnownHostEntry {
            host_patterns: vec!["test".into()],
            _key_type: "ssh-rsa".into(),
            key_base64: "not-valid-base64!!!".into(),
        };
        assert!(!entry.key_matches(&[1, 2, 3]));
    }

    #[test]
    fn test_parse_known_hosts_line_empty_returns_none() {
        assert!(parse_known_hosts_line("").is_none());
    }

    #[test]
    fn test_parse_known_hosts_line_comment_is_skipped_by_caller() {
        // Comments are skipped by check_host_key's loop, not by parse_known_hosts_line.
        // But parse should handle a line with only a host and key_type (no key) gracefully.
        assert!(parse_known_hosts_line("host ssh-rsa").is_none());
    }

    #[test]
    fn test_parse_known_hosts_line_only_host_returns_none() {
        assert!(parse_known_hosts_line("example.com").is_none());
    }

    #[test]
    fn test_check_hashed_host_invalid_salt() {
        // Malformed base64 in salt should return None (not panic)
        assert!(check_hashed_host("|1|!!!invalid|aGFzaA==", "example.com").is_none());
    }

    #[test]
    fn test_check_hashed_host_invalid_hash() {
        // Valid salt but malformed base64 in hash should return None
        assert!(check_hashed_host("|1|dGVzdA==|!!!invalid", "example.com").is_none());
    }

    #[test]
    fn test_check_hashed_host_too_few_parts() {
        assert!(check_hashed_host("|1|onlytwosections", "host").is_none());
    }

    #[test]
    fn test_check_hashed_host_empty_string() {
        assert!(check_hashed_host("", "host").is_none());
    }

    #[test]
    fn test_host_pattern_empty_entry_no_match() {
        let entry = KnownHostEntry {
            host_patterns: vec![],
            _key_type: "ssh-ed25519".into(),
            key_base64: "AAAA".into(),
        };
        assert!(!entry.matches_host("example.com"));
    }

    #[test]
    fn test_key_matches_empty_key() {
        let entry = KnownHostEntry {
            host_patterns: vec!["test".into()],
            _key_type: "ssh-rsa".into(),
            key_base64: String::new(),
        };
        // Empty base64 decodes to empty vec, which shouldn't match non-empty data
        assert!(!entry.key_matches(&[1, 2, 3]));
    }
}
