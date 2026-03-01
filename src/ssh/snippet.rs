use std::collections::HashSet;
use std::io::Write;

use anyhow::Result;
use crossterm::event::{Event, KeyCode};

use crate::config::model::{Bookmark, Snippet};
use crate::ssh::SessionInfo;

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
            let snippet = all_snippets
                .get(idx)
                .ok_or_else(|| anyhow::anyhow!("Snippet index {idx} out of bounds"))?;
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

/// Action returned by SessionEscapeHandler.
pub enum SessionAction {
    /// Forward these bytes to the SSH channel unchanged.
    Forward(Vec<u8>),
    /// Hold — we're in the middle of a potential match.
    Buffer,
    /// Show the snippet picker.
    ShowSnippets,
    /// Show the save-as-bookmark form.
    ShowSaveBookmark,
}

/// Tracks terminal escape sequence state so the trigger detectors
/// don't intercept bytes that are part of CSI/SS3 sequences
/// (e.g. F10 = `\x1b[21~` — the trailing `~` must not start trigger matching).
enum TermSeqState {
    Normal,
    /// Saw `\x1b`, waiting for type indicator.
    AfterEsc,
    /// Inside a CSI sequence (`\x1b[`), waiting for final byte (0x40..=0x7E).
    InCsi,
    /// Inside an SS3 sequence (`\x1b O`), next byte completes it.
    InSs3,
}

/// Combined escape handler for snippet trigger and bookmark trigger.
/// Handles two independent escape sequences during an SSH session.
/// Tracks terminal escape sequences (CSI, SS3) to avoid intercepting
/// function key bytes like the `~` terminator in `\x1b[21~` (F10).
pub struct SessionEscapeHandler {
    snippet_detector: EscapeDetector,
    bookmark_detector: EscapeDetector,
    term_seq: TermSeqState,
}

impl SessionEscapeHandler {
    /// Create a new handler with the given trigger strings.
    pub fn new(snippet_trigger: &str, bookmark_trigger: &str) -> Self {
        Self {
            snippet_detector: EscapeDetector::new(snippet_trigger),
            bookmark_detector: EscapeDetector::new(bookmark_trigger),
            term_seq: TermSeqState::Normal,
        }
    }

    /// Feed a single byte from stdin. Returns what action to take.
    ///
    /// Terminal escape sequences (CSI, SS3) are forwarded directly without
    /// trigger detection. For normal bytes, the snippet detector gets first
    /// pass; when it forwards, those bytes are checked by the bookmark detector.
    pub fn feed(&mut self, byte: u8) -> SessionAction {
        // Track terminal escape sequence state.
        // Bytes inside escape sequences bypass trigger detection entirely.
        match self.term_seq {
            TermSeqState::Normal => {
                if byte == 0x1b {
                    self.term_seq = TermSeqState::AfterEsc;
                    return SessionAction::Forward(vec![byte]);
                }
                // Not inside an escape sequence — run trigger detection below
            }
            TermSeqState::AfterEsc => {
                self.term_seq = match byte {
                    b'[' => TermSeqState::InCsi,
                    b'O' => TermSeqState::InSs3,
                    // Two-byte sequence (\x1b + char, e.g. Alt+key) — done
                    _ => TermSeqState::Normal,
                };
                return SessionAction::Forward(vec![byte]);
            }
            TermSeqState::InCsi => {
                // Final byte range 0x40..=0x7E ends the CSI sequence
                // (includes `~`, `A`-`Z`, `a`-`z`, etc.)
                if (0x40..=0x7E).contains(&byte) {
                    self.term_seq = TermSeqState::Normal;
                }
                return SessionAction::Forward(vec![byte]);
            }
            TermSeqState::InSs3 => {
                // SS3 sequences are exactly one byte after \x1bO
                self.term_seq = TermSeqState::Normal;
                return SessionAction::Forward(vec![byte]);
            }
        }

        // Normal byte — run through trigger detectors
        match self.snippet_detector.feed(byte) {
            EscapeAction::Trigger => SessionAction::ShowSnippets,
            EscapeAction::Buffer => {
                // Snippet detector is buffering — don't forward to bookmark detector yet.
                // If the snippet match fails later, the flushed bytes will go through
                // the bookmark detector at that point.
                SessionAction::Buffer
            }
            EscapeAction::Forward(bytes) => {
                // Snippet detector forwarded these bytes — now check bookmark trigger
                let mut final_forward = Vec::new();
                for &b in &bytes {
                    match self.bookmark_detector.feed(b) {
                        EscapeAction::Trigger => return SessionAction::ShowSaveBookmark,
                        EscapeAction::Buffer => {} // absorbed by bookmark detector
                        EscapeAction::Forward(fwd) => final_forward.extend(fwd),
                    }
                }
                if final_forward.is_empty() {
                    SessionAction::Buffer
                } else {
                    SessionAction::Forward(final_forward)
                }
            }
        }
    }
}

/// Save-as-bookmark fields that can be edited inline.
enum BookmarkField {
    Name,
    Tags,
    Notes,
}

const BOOKMARK_FIELDS: [BookmarkField; 3] = [
    BookmarkField::Name,
    BookmarkField::Tags,
    BookmarkField::Notes,
];

/// Show the save-as-bookmark inline form during an SSH session.
/// Returns Some(Bookmark) if saved, None if cancelled.
pub fn show_save_bookmark_form(
    stdout: &mut std::io::Stdout,
    session_info: &SessionInfo,
) -> Result<Option<Bookmark>> {
    let is_existing = session_info.bookmark_name.is_some();

    // Pre-fill fields
    let mut name = session_info
        .bookmark_name
        .clone()
        .unwrap_or_else(|| crate::ssh::infer_bookmark_name(&session_info.host));
    let env = crate::config::env::detect_env(&name, &session_info.host);
    let mut tags = String::new();
    let mut notes = String::new();
    let mut field_idx: usize = 0;

    // Draw the form
    let draw = |stdout: &mut std::io::Stdout,
                name: &str,
                tags: &str,
                notes: &str,
                env: &str,
                field_idx: usize,
                is_existing: bool,
                info: &SessionInfo|
     -> Result<()> {
        let title = if is_existing {
            format!("Update Bookmark: {}", name)
        } else {
            "Save as Bookmark".to_string()
        };
        write!(stdout, "\r\n\x1b[1m── {} ──\x1b[0m\r\n", title)?;
        write!(
            stdout,
            "  Host: \x1b[36m{}@{}:{}\x1b[0m\r\n",
            info.user, info.host, info.port
        )?;
        if !env.is_empty() {
            write!(stdout, "  Env:  \x1b[33m{}\x1b[0m (auto-detected)\r\n", env)?;
        }

        let name_cursor = if field_idx == 0 { "\x1b[7m" } else { "" };
        let tags_cursor = if field_idx == 1 { "\x1b[7m" } else { "" };
        let notes_cursor = if field_idx == 2 { "\x1b[7m" } else { "" };
        let reset = "\x1b[0m";

        if !is_existing {
            write!(
                stdout,
                "  Name:  {}{}{}\r\n",
                name_cursor,
                if name.is_empty() { " " } else { name },
                reset
            )?;
        }
        write!(
            stdout,
            "  Tags:  {}{}{}\r\n",
            tags_cursor,
            if tags.is_empty() { " " } else { tags },
            reset
        )?;
        write!(
            stdout,
            "  Notes: {}{}{}\r\n",
            notes_cursor,
            if notes.is_empty() { " " } else { notes },
            reset
        )?;
        write!(
            stdout,
            "\x1b[2m  Enter=Save  Tab=Next  Esc=Cancel\x1b[0m\r\n"
        )?;
        stdout.flush()?;
        Ok(())
    };

    // For existing bookmarks, skip the Name field
    if is_existing {
        field_idx = 1; // start at Tags
    }

    draw(
        stdout,
        &name,
        &tags,
        &notes,
        &env,
        field_idx,
        is_existing,
        session_info,
    )?;

    let total_lines = if is_existing { 5 } else { 6 }; // lines drawn by the form

    // Event loop
    loop {
        if let Event::Key(key) = crossterm::event::read()? {
            match key.code {
                KeyCode::Esc => {
                    clear_lines(stdout, total_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_lines(stdout, total_lines)?;

                    if name.is_empty() {
                        write!(stdout, "\x1b[31mBookmark name cannot be empty\x1b[0m\r\n")?;
                        stdout.flush()?;
                        return Ok(None);
                    }

                    let bookmark = Bookmark {
                        name: name.clone(),
                        host: session_info.host.clone(),
                        user: if session_info.user.is_empty() {
                            None
                        } else {
                            Some(session_info.user.clone())
                        },
                        port: session_info.port,
                        env: env.clone(),
                        tags: tags
                            .split(',')
                            .map(|t| t.trim().to_string())
                            .filter(|t| !t.is_empty())
                            .collect(),
                        identity_file: session_info.identity_file.clone(),
                        proxy_jump: session_info.proxy_jump.clone(),
                        notes: if notes.is_empty() { None } else { Some(notes) },
                        last_connected: Some(chrono::Utc::now()),
                        connect_count: 1,
                        on_connect: None,
                        snippets: vec![],
                        connect_timeout_secs: None,
                        ssh_options: std::collections::HashMap::new(),
                    };
                    return Ok(Some(bookmark));
                }
                KeyCode::Tab => {
                    let start = if is_existing { 1 } else { 0 };
                    let count = BOOKMARK_FIELDS.len() - start;
                    field_idx = start + ((field_idx - start + 1) % count);
                    clear_lines(stdout, total_lines)?;
                    draw(
                        stdout,
                        &name,
                        &tags,
                        &notes,
                        &env,
                        field_idx,
                        is_existing,
                        session_info,
                    )?;
                }
                KeyCode::Char(c) => {
                    match BOOKMARK_FIELDS[field_idx] {
                        BookmarkField::Name => name.push(c),
                        BookmarkField::Tags => tags.push(c),
                        BookmarkField::Notes => notes.push(c),
                    }
                    clear_lines(stdout, total_lines)?;
                    draw(
                        stdout,
                        &name,
                        &tags,
                        &notes,
                        &env,
                        field_idx,
                        is_existing,
                        session_info,
                    )?;
                }
                KeyCode::Backspace => {
                    match BOOKMARK_FIELDS[field_idx] {
                        BookmarkField::Name => {
                            name.pop();
                        }
                        BookmarkField::Tags => {
                            tags.pop();
                        }
                        BookmarkField::Notes => {
                            notes.pop();
                        }
                    }
                    clear_lines(stdout, total_lines)?;
                    draw(
                        stdout,
                        &name,
                        &tags,
                        &notes,
                        &env,
                        field_idx,
                        is_existing,
                        session_info,
                    )?;
                }
                _ => {}
            }
        }
    }
}

/// Clear `n` lines above the cursor (used to redraw inline forms).
fn clear_lines(stdout: &mut std::io::Stdout, n: usize) -> Result<()> {
    for _ in 0..n {
        write!(stdout, "\x1b[A\x1b[2K")?;
    }
    stdout.flush()?;
    Ok(())
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

    // --- SessionEscapeHandler ---

    #[test]
    fn test_session_handler_snippet_trigger() {
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        assert!(matches!(handler.feed(b'~'), SessionAction::Buffer));
        assert!(matches!(handler.feed(b'~'), SessionAction::ShowSnippets));
    }

    #[test]
    fn test_session_handler_bookmark_trigger() {
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        assert!(matches!(handler.feed(b'~'), SessionAction::Buffer));
        // After snippet detector flushes '~' + 'b', bookmark detector should catch it
        match handler.feed(b'b') {
            SessionAction::ShowSaveBookmark => {} // expected
            other => panic!(
                "Expected ShowSaveBookmark, got {:?}",
                match other {
                    SessionAction::Forward(ref f) => format!("Forward({:?})", f),
                    SessionAction::Buffer => "Buffer".to_string(),
                    SessionAction::ShowSnippets => "ShowSnippets".to_string(),
                    SessionAction::ShowSaveBookmark => "ShowSaveBookmark".to_string(),
                }
            ),
        }
    }

    #[test]
    fn test_session_handler_normal_text() {
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        match handler.feed(b'h') {
            SessionAction::Forward(bytes) => assert_eq!(bytes, vec![b'h']),
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_session_handler_both_disabled() {
        let mut handler = SessionEscapeHandler::new("", "");

        match handler.feed(b'~') {
            SessionAction::Forward(bytes) => assert_eq!(bytes, vec![b'~']),
            _ => panic!("Expected Forward for disabled triggers"),
        }
    }

    // --- Terminal escape sequence passthrough ---

    #[test]
    fn test_session_handler_f10_csi_passthrough() {
        // F10 = \x1b[21~ — the trailing ~ must NOT start trigger matching
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        let f10 = b"\x1b[21~";
        let mut forwarded = Vec::new();
        for &byte in f10 {
            match handler.feed(byte) {
                SessionAction::Forward(fwd) => forwarded.extend(fwd),
                SessionAction::Buffer => panic!("CSI bytes should not be buffered by trigger"),
                _ => panic!("CSI sequence should not trigger actions"),
            }
        }
        assert_eq!(forwarded, f10.to_vec());
    }

    #[test]
    fn test_session_handler_f5_csi_passthrough() {
        // F5 = \x1b[15~
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        let f5 = b"\x1b[15~";
        let mut forwarded = Vec::new();
        for &byte in f5 {
            match handler.feed(byte) {
                SessionAction::Forward(fwd) => forwarded.extend(fwd),
                _ => panic!("F5 CSI sequence should pass through"),
            }
        }
        assert_eq!(forwarded, f5.to_vec());
    }

    #[test]
    fn test_session_handler_arrow_key_csi_passthrough() {
        // Up arrow = \x1b[A
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        let up = b"\x1b[A";
        let mut forwarded = Vec::new();
        for &byte in up {
            match handler.feed(byte) {
                SessionAction::Forward(fwd) => forwarded.extend(fwd),
                _ => panic!("Arrow key CSI should pass through"),
            }
        }
        assert_eq!(forwarded, up.to_vec());
    }

    #[test]
    fn test_session_handler_ss3_passthrough() {
        // Some terminals send SS3 for function keys: \x1bOP (F1)
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        let f1 = b"\x1bOP";
        let mut forwarded = Vec::new();
        for &byte in f1 {
            match handler.feed(byte) {
                SessionAction::Forward(fwd) => forwarded.extend(fwd),
                _ => panic!("SS3 sequence should pass through"),
            }
        }
        assert_eq!(forwarded, f1.to_vec());
    }

    #[test]
    fn test_session_handler_csi_then_trigger_works() {
        // After a CSI sequence completes, trigger detection should still work
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        // Send F10
        for &byte in b"\x1b[21~" {
            handler.feed(byte);
        }

        // Now trigger ~~ should still work
        assert!(matches!(handler.feed(b'~'), SessionAction::Buffer));
        assert!(matches!(handler.feed(b'~'), SessionAction::ShowSnippets));
    }

    #[test]
    fn test_session_handler_alt_key_passthrough() {
        // Alt+x = \x1b x (two bytes)
        let mut handler = SessionEscapeHandler::new("~~", "~b");

        let alt_x = b"\x1bx";
        let mut forwarded = Vec::new();
        for &byte in alt_x {
            match handler.feed(byte) {
                SessionAction::Forward(fwd) => forwarded.extend(fwd),
                _ => panic!("Alt+key should pass through"),
            }
        }
        assert_eq!(forwarded, alt_x.to_vec());
    }
}
