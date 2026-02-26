use std::collections::HashSet;
use std::io::Write;

use anyhow::Result;
use crossterm::event::{Event, KeyCode};

use crate::config::model::Snippet;

/// Result of feeding a byte to the escape detector.
pub enum EscapeAction {
    /// Forward these bytes to the SSH channel unchanged.
    Forward(Vec<u8>),
    /// Hold — we're in the middle of a potential match, don't forward yet.
    Buffer,
    /// Full escape sequence matched — trigger the snippet picker.
    Trigger,
}

/// Internal state of the escape sequence detector.
enum EscapeState {
    Normal,
    /// Matched trigger[..n] bytes so far.
    Matching(usize),
}

/// Detects the snippet trigger escape sequence in stdin bytes.
/// Follows the same feed-and-act pattern as PasswordDetector.
pub struct EscapeDetector {
    state: EscapeState,
    trigger: Vec<u8>,
    buffered: Vec<u8>,
}

impl EscapeDetector {
    /// Create a new detector with the given trigger string.
    pub fn new(trigger: &str) -> Self {
        Self {
            state: EscapeState::Normal,
            trigger: trigger.as_bytes().to_vec(),
            buffered: Vec::new(),
        }
    }

    /// Feed a single byte from stdin. Returns what to do with it.
    pub fn feed(&mut self, byte: u8) -> EscapeAction {
        // Empty trigger means detection is disabled
        if self.trigger.is_empty() {
            return EscapeAction::Forward(vec![byte]);
        }

        match self.state {
            EscapeState::Normal => {
                if byte == self.trigger[0] {
                    self.state = EscapeState::Matching(1);
                    self.buffered.push(byte);
                    if self.trigger.len() == 1 {
                        // Single-char trigger — immediate match
                        self.state = EscapeState::Normal;
                        self.buffered.clear();
                        EscapeAction::Trigger
                    } else {
                        EscapeAction::Buffer
                    }
                } else {
                    EscapeAction::Forward(vec![byte])
                }
            }
            EscapeState::Matching(n) => {
                if byte == self.trigger[n] {
                    self.buffered.push(byte);
                    if n + 1 == self.trigger.len() {
                        // Full match
                        self.state = EscapeState::Normal;
                        self.buffered.clear();
                        EscapeAction::Trigger
                    } else {
                        self.state = EscapeState::Matching(n + 1);
                        EscapeAction::Buffer
                    }
                } else {
                    // Not a match — flush buffered bytes + this byte
                    let mut flushed = std::mem::take(&mut self.buffered);
                    flushed.push(byte);
                    self.state = EscapeState::Normal;
                    EscapeAction::Forward(flushed)
                }
            }
        }
    }
}

/// Show the snippet picker inline during an SSH session.
/// Returns the command string to inject, or None if cancelled.
pub fn show_snippet_picker(
    stdout: &mut std::io::Stdout,
    bookmark_snippets: &[Snippet],
    global_snippets: &[Snippet],
) -> Result<Option<String>> {
    // Merge: bookmark-specific first, then global (skip if name collides)
    let mut all_snippets: Vec<&Snippet> = bookmark_snippets.iter().collect();
    let bookmark_names: HashSet<&str> = bookmark_snippets.iter().map(|s| s.name.as_str()).collect();
    for gs in global_snippets {
        if !bookmark_names.contains(gs.name.as_str()) {
            all_snippets.push(gs);
        }
    }

    if all_snippets.is_empty() {
        return Ok(None);
    }

    // Print the picker below the current cursor position
    write!(stdout, "\r\n\x1b[1m── Snippets ──\x1b[0m\r\n")?;
    for (i, snippet) in all_snippets.iter().enumerate() {
        write!(stdout, "  \x1b[33m{}\x1b[0m. {}\r\n", i + 1, snippet.name)?;
    }
    let max = all_snippets.len();
    write!(stdout, "\x1b[2m(1-{max} select, Esc cancel)\x1b[0m\r\n")?;
    stdout.flush()?;

    // Read user selection (raw mode is already active)
    let selection = read_snippet_selection(max)?;

    // Clear the picker lines
    let lines_to_clear = max + 3; // header + items + footer
    for _ in 0..lines_to_clear {
        write!(stdout, "\x1b[A\x1b[2K")?; // move up, clear line
    }
    stdout.flush()?;

    match selection {
        Some(idx) => {
            let snippet = all_snippets[idx];
            let command = if snippet.auto_execute {
                format!("{}\n", snippet.command)
            } else {
                snippet.command.clone()
            };
            Ok(Some(command))
        }
        None => Ok(None),
    }
}

/// Read a single keypress for snippet selection.
/// Returns Some(index) for a valid number, None for Esc.
fn read_snippet_selection(max: usize) -> Result<Option<usize>> {
    loop {
        if let Event::Key(key) = crossterm::event::read()? {
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let n = c.to_digit(10).unwrap() as usize;
                    if n >= 1 && n <= max {
                        return Ok(Some(n - 1));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_detector_trigger() {
        let mut detector = EscapeDetector::new("~~");

        // First tilde — should buffer
        assert!(matches!(detector.feed(b'~'), EscapeAction::Buffer));
        // Second tilde — should trigger
        assert!(matches!(detector.feed(b'~'), EscapeAction::Trigger));
    }

    #[test]
    fn test_escape_detector_single_tilde_then_other() {
        let mut detector = EscapeDetector::new("~~");

        // First tilde — buffer
        assert!(matches!(detector.feed(b'~'), EscapeAction::Buffer));
        // 'a' — not a match, flush "~a"
        match detector.feed(b'a') {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'~', b'a']);
            }
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_escape_detector_normal_text() {
        let mut detector = EscapeDetector::new("~~");

        match detector.feed(b'h') {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'h']);
            }
            _ => panic!("Expected Forward"),
        }

        match detector.feed(b'i') {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'i']);
            }
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_escape_detector_custom_trigger() {
        let mut detector = EscapeDetector::new("!!");

        assert!(matches!(detector.feed(b'!'), EscapeAction::Buffer));
        assert!(matches!(detector.feed(b'!'), EscapeAction::Trigger));
    }

    #[test]
    fn test_escape_detector_empty_trigger() {
        let mut detector = EscapeDetector::new("");

        // Empty trigger means no detection — everything forwards
        match detector.feed(b'~') {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'~']);
            }
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_escape_detector_trigger_then_normal() {
        let mut detector = EscapeDetector::new("~~");

        // Trigger
        assert!(matches!(detector.feed(b'~'), EscapeAction::Buffer));
        assert!(matches!(detector.feed(b'~'), EscapeAction::Trigger));

        // After trigger, back to normal
        match detector.feed(b'x') {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'x']);
            }
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_escape_detector_partial_then_trigger() {
        let mut detector = EscapeDetector::new("~~");

        // Partial match that fails
        assert!(matches!(detector.feed(b'~'), EscapeAction::Buffer));
        match detector.feed(b'a') {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'~', b'a']);
            }
            _ => panic!("Expected Forward"),
        }

        // Now a real trigger
        assert!(matches!(detector.feed(b'~'), EscapeAction::Buffer));
        assert!(matches!(detector.feed(b'~'), EscapeAction::Trigger));
    }

    #[test]
    fn test_escape_detector_three_char_trigger() {
        let mut detector = EscapeDetector::new("abc");

        assert!(matches!(detector.feed(b'a'), EscapeAction::Buffer));
        assert!(matches!(detector.feed(b'b'), EscapeAction::Buffer));
        assert!(matches!(detector.feed(b'c'), EscapeAction::Trigger));
    }
}
