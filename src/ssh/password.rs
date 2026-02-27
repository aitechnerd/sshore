use std::sync::LazyLock;

use regex::Regex;

/// Maximum rolling buffer size in bytes.
const BUFFER_CAP: usize = 256;

/// Compiled regex patterns for common sudo/password prompts.
static PASSWORD_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\[sudo\] password for \S+:\s*$",
        r"Password:\s*$",
        r"Enter passphrase for key '.+':\s*$",
        r"\S+'s password:\s*$",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("password pattern must compile"))
    .collect()
});

/// Extract only valid UTF-8 from a byte slice, skipping invalid bytes.
///
/// Unlike `String::from_utf8_lossy`, this discards invalid bytes entirely
/// rather than replacing them with U+FFFD, preventing binary data from
/// producing false positive regex matches.
fn extract_valid_utf8(data: &[u8]) -> String {
    let mut result = String::new();
    let mut remaining = data;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(s) => {
                result.push_str(s);
                break;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if valid_up_to > 0 {
                    // Safety: from_utf8 guarantees data[..valid_up_to] is valid UTF-8
                    if let Ok(valid) = std::str::from_utf8(&remaining[..valid_up_to]) {
                        result.push_str(valid);
                    }
                }
                // Skip past the invalid byte(s)
                let skip = match e.error_len() {
                    Some(len) => valid_up_to + len,
                    None => remaining.len(),
                };
                remaining = &remaining[skip..];
            }
        }
    }
    result
}

/// Detects password prompts in an SSH output stream using a rolling buffer.
pub struct PasswordDetector {
    buffer: String,
    has_password: bool,
}

impl PasswordDetector {
    /// Create a new detector.
    /// If `has_password` is false, `feed()` always returns false (skips scanning).
    pub fn new(has_password: bool) -> Self {
        Self {
            buffer: String::new(),
            has_password,
        }
    }

    /// Feed data from the SSH output stream into the detector.
    /// Returns `true` if a password prompt is detected.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        if !self.has_password {
            return false;
        }

        // Only process valid UTF-8 portions, skipping invalid bytes to avoid
        // false positives from replacement characters in binary data.
        let text = extract_valid_utf8(data);
        if text.is_empty() {
            return false;
        }
        self.buffer.push_str(&text);

        // Cap buffer size — keep only the tail
        if self.buffer.len() > BUFFER_CAP {
            let start = self.buffer.len() - BUFFER_CAP;
            // Find a char boundary at or after `start` to avoid splitting multi-byte chars
            let boundary = self.buffer[start..]
                .char_indices()
                .next()
                .map(|(i, _)| start + i)
                .unwrap_or(self.buffer.len());
            self.buffer = self.buffer[boundary..].to_string();
        }

        PASSWORD_PATTERNS.iter().any(|p| p.is_match(&self.buffer))
    }

    /// Reset the buffer after password injection or when skipping.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_sudo_prompt() {
        let mut d = PasswordDetector::new(true);
        assert!(d.feed(b"[sudo] password for deploy: "));
    }

    #[test]
    fn test_detect_generic_password_prompt() {
        let mut d = PasswordDetector::new(true);
        assert!(d.feed(b"Password: "));
    }

    #[test]
    fn test_detect_password_prompt_no_trailing_space() {
        let mut d = PasswordDetector::new(true);
        assert!(d.feed(b"Password:"));
    }

    #[test]
    fn test_detect_passphrase_prompt() {
        let mut d = PasswordDetector::new(true);
        assert!(d.feed(b"Enter passphrase for key '/home/user/.ssh/id_rsa': "));
    }

    #[test]
    fn test_detect_user_password_prompt() {
        let mut d = PasswordDetector::new(true);
        assert!(d.feed(b"deploy's password: "));
    }

    #[test]
    fn test_no_false_positive_on_similar_text() {
        let mut d = PasswordDetector::new(true);
        assert!(!d.feed(b"The password was reset successfully."));
    }

    #[test]
    fn test_no_false_positive_on_partial_prompt() {
        let mut d = PasswordDetector::new(true);
        assert!(!d.feed(b"[sudo] password for"));
    }

    #[test]
    fn test_has_password_false_skips_detection() {
        let mut d = PasswordDetector::new(false);
        assert!(!d.feed(b"[sudo] password for deploy: "));
    }

    #[test]
    fn test_clear_resets_buffer() {
        let mut d = PasswordDetector::new(true);
        d.feed(b"[sudo] password for ");
        d.clear();
        // After clear, partial prompt from before is gone — this new data alone doesn't match
        assert!(!d.feed(b"deploy: "));
    }

    #[test]
    fn test_rolling_buffer_caps_at_limit() {
        let mut d = PasswordDetector::new(true);
        // Feed a large chunk that exceeds BUFFER_CAP
        let filler = "x".repeat(300);
        d.feed(filler.as_bytes());
        // Buffer should be capped
        assert!(d.buffer.len() <= BUFFER_CAP);
    }

    #[test]
    fn test_prompt_split_across_feeds() {
        let mut d = PasswordDetector::new(true);
        assert!(!d.feed(b"[sudo] password "));
        assert!(d.feed(b"for deploy: "));
    }

    #[test]
    fn test_prompt_after_other_output() {
        let mut d = PasswordDetector::new(true);
        assert!(!d.feed(b"Last login: Mon Feb 24 10:00:00 2026\n"));
        assert!(d.feed(b"[sudo] password for admin: "));
    }

    #[test]
    fn test_non_utf8_data_handled() {
        let mut d = PasswordDetector::new(true);
        // Feed some invalid UTF-8 followed by a valid prompt
        let mut data = vec![0xFF, 0xFE];
        data.extend_from_slice(b"Password: ");
        assert!(d.feed(&data));
    }

    #[test]
    fn test_pure_binary_data_no_false_positive() {
        let mut d = PasswordDetector::new(true);
        // Pure binary data should not trigger any match
        let binary = vec![0xFF, 0xFE, 0x80, 0x81, 0x90, 0xA0, 0xB0, 0xC0];
        assert!(!d.feed(&binary));
    }

    #[test]
    fn test_extract_valid_utf8() {
        // All valid
        assert_eq!(extract_valid_utf8(b"hello"), "hello");
        // Invalid prefix, valid suffix
        assert_eq!(extract_valid_utf8(&[0xFF, b'h', b'i']), "hi");
        // All invalid
        assert_eq!(extract_valid_utf8(&[0xFF, 0xFE]), "");
        // Mixed: valid, invalid, valid
        let mut mixed = b"abc".to_vec();
        mixed.push(0xFF);
        mixed.extend_from_slice(b"def");
        assert_eq!(extract_valid_utf8(&mixed), "abcdef");
    }
}
