/// Delimiter characters that form word boundaries in hostnames and bookmark names.
const WORD_DELIMITERS: &[char] = &['-', '_', '.', ' ', ':'];

const PRODUCTION_PATTERNS: &[&str] = &["prod", "production", "prd"];
const STAGING_PATTERNS: &[&str] = &["stag", "staging", "stg"];
const DEVELOPMENT_PATTERNS: &[&str] = &["dev", "development", "develop"];
const LOCAL_PATTERNS: &[&str] = &["local", "localhost", "127.0.0.1", "::1"];
const TESTING_PATTERNS: &[&str] = &["test", "testing", "qa", "uat"];

/// Detect environment tier from bookmark name and hostname.
///
/// Combines name and host, then checks pattern groups in priority order.
/// Returns the environment string or empty string if no match.
pub fn detect_env(name: &str, host: &str) -> String {
    let combined = format!("{} {}", name.to_lowercase(), host.to_lowercase());

    if contains_word(&combined, PRODUCTION_PATTERNS) {
        "production".into()
    } else if contains_word(&combined, STAGING_PATTERNS) {
        "staging".into()
    } else if contains_word(&combined, DEVELOPMENT_PATTERNS) {
        "development".into()
    } else if contains_word(&combined, LOCAL_PATTERNS) {
        "local".into()
    } else if contains_word(&combined, TESTING_PATTERNS) {
        "testing".into()
    } else {
        String::new()
    }
}

/// Check if any pattern appears as a whole word in the text.
///
/// A "word" is bounded by start/end of string or a delimiter character.
/// This prevents "prod" from matching inside "reproduce".
fn contains_word(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| {
        let mut start = 0;
        while let Some(pos) = text[start..].find(pattern) {
            let abs_pos = start + pos;
            let end_pos = abs_pos + pattern.len();

            let left_ok =
                abs_pos == 0 || WORD_DELIMITERS.contains(&(text.as_bytes()[abs_pos - 1] as char));
            let right_ok = end_pos == text.len()
                || WORD_DELIMITERS.contains(&(text.as_bytes()[end_pos] as char));

            if left_ok && right_ok {
                return true;
            }

            start = abs_pos + 1;
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Positive matches ---

    #[test]
    fn test_detect_env_production_keyword() {
        assert_eq!(detect_env("prod-web-01", "10.0.1.5"), "production");
    }

    #[test]
    fn test_detect_env_production_full_word() {
        assert_eq!(
            detect_env("production-api", "api.example.com"),
            "production"
        );
    }

    #[test]
    fn test_detect_env_production_prd() {
        assert_eq!(detect_env("prd-db-01", "db.example.com"), "production");
    }

    #[test]
    fn test_detect_env_staging() {
        assert_eq!(detect_env("staging-api", "api.stg.example.com"), "staging");
    }

    #[test]
    fn test_detect_env_staging_stg() {
        assert_eq!(detect_env("stg-worker", "worker.example.com"), "staging");
    }

    #[test]
    fn test_detect_env_staging_stag() {
        assert_eq!(detect_env("stag-app", "app.example.com"), "staging");
    }

    #[test]
    fn test_detect_env_development() {
        assert_eq!(
            detect_env("dev-worker", "worker.dev.example.com"),
            "development"
        );
    }

    #[test]
    fn test_detect_env_development_full() {
        assert_eq!(
            detect_env("development-api", "api.example.com"),
            "development"
        );
    }

    #[test]
    fn test_detect_env_development_develop() {
        assert_eq!(
            detect_env("develop-branch", "ci.example.com"),
            "development"
        );
    }

    #[test]
    fn test_detect_env_local_keyword() {
        assert_eq!(detect_env("local-vm", "192.168.1.1"), "local");
    }

    #[test]
    fn test_detect_env_localhost() {
        assert_eq!(detect_env("myapp", "localhost"), "local");
    }

    #[test]
    fn test_detect_env_loopback_ipv4() {
        assert_eq!(detect_env("myapp", "127.0.0.1"), "local");
    }

    #[test]
    fn test_detect_env_loopback_ipv6() {
        assert_eq!(detect_env("myapp", "::1"), "local");
    }

    #[test]
    fn test_detect_env_testing() {
        assert_eq!(detect_env("test-runner", "test.example.com"), "testing");
    }

    #[test]
    fn test_detect_env_qa() {
        assert_eq!(detect_env("qa-server", "qa.example.com"), "testing");
    }

    #[test]
    fn test_detect_env_uat() {
        assert_eq!(detect_env("uat-env", "uat.example.com"), "testing");
    }

    // --- No match ---

    #[test]
    fn test_detect_env_no_match() {
        assert_eq!(detect_env("bastion", "jump.example.com"), "");
    }

    #[test]
    fn test_detect_env_generic_server() {
        assert_eq!(detect_env("webserver-01", "web.example.com"), "");
    }

    // --- False positive prevention ---

    #[test]
    fn test_detect_env_reproduce_not_production() {
        assert_eq!(detect_env("reproduce-bug", "debug.example.com"), "");
    }

    #[test]
    fn test_detect_env_devious_not_development() {
        assert_eq!(detect_env("devious-server", "evil.example.com"), "");
    }

    #[test]
    fn test_detect_env_contest_not_testing() {
        assert_eq!(detect_env("contest-app", "contest.example.com"), "");
    }

    #[test]
    fn test_detect_env_protest_not_testing() {
        assert_eq!(detect_env("protest", "protest.example.com"), "");
    }

    #[test]
    fn test_detect_env_attestation_not_testing() {
        assert_eq!(detect_env("attestation", "attest.example.com"), "");
    }

    #[test]
    fn test_detect_env_unstaged_not_staging() {
        assert_eq!(detect_env("unstaged-changes", "ci.example.com"), "");
    }

    // --- Host-based detection ---

    #[test]
    fn test_detect_env_host_based_production() {
        assert_eq!(detect_env("webserver", "prod.example.com"), "production");
    }

    #[test]
    fn test_detect_env_host_based_staging() {
        assert_eq!(detect_env("webserver", "stg.example.com"), "staging");
    }

    // --- Case insensitivity ---

    #[test]
    fn test_detect_env_case_insensitive_name() {
        assert_eq!(detect_env("PROD-WEB-01", "10.0.1.5"), "production");
    }

    #[test]
    fn test_detect_env_case_insensitive_host() {
        assert_eq!(detect_env("webserver", "PROD.Example.COM"), "production");
    }

    // --- Priority ordering ---

    #[test]
    fn test_detect_env_production_wins_over_staging() {
        // If both "prod" and "stg" appear, production wins (checked first)
        assert_eq!(detect_env("prod-stg-mirror", "example.com"), "production");
    }

    #[test]
    fn test_detect_env_staging_wins_over_development() {
        assert_eq!(detect_env("stg-dev-sync", "example.com"), "staging");
    }

    // --- Edge cases ---

    #[test]
    fn test_detect_env_pattern_at_end() {
        assert_eq!(detect_env("web-prod", "example.com"), "production");
    }

    #[test]
    fn test_detect_env_pattern_alone() {
        assert_eq!(detect_env("prod", "example.com"), "production");
    }

    #[test]
    fn test_detect_env_empty_inputs() {
        assert_eq!(detect_env("", ""), "");
    }

    #[test]
    fn test_detect_env_dot_delimiter() {
        assert_eq!(detect_env("app", "prod.example.com"), "production");
    }
}
