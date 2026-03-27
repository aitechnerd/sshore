use std::collections::HashSet;
use std::io::Write;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{Event, KeyCode};

use crate::config::model::{Bookmark, Snippet};
use crate::ssh::SessionInfo;

/// Minimum inter-byte interval (ms) for trigger detection.
/// If consecutive trigger bytes arrive faster than this threshold,
/// treat the sequence as paste/program output and flush instead of triggering.
/// Single-character triggers bypass this guard since timing is the only
/// possible signal and single-char triggers were always inadvisable for
/// contexts where paste is common.
const PASTE_THRESHOLD_MS: u64 = 20;

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
///
/// Includes a paste-flood guard: if consecutive trigger bytes arrive faster
/// than [`PASTE_THRESHOLD_MS`] apart, the sequence is treated as pasted text
/// and flushed rather than triggering the snippet picker.
pub struct EscapeDetector {
    state: EscapeState,
    trigger: Vec<u8>,
    buffered: Vec<u8>,
    /// Timestamp of the last byte that advanced the match state.
    /// Used to detect paste floods (bytes arriving faster than human typing).
    last_byte_time: Option<Instant>,
}

impl EscapeDetector {
    /// Create a new detector with the given trigger string.
    pub fn new(trigger: &str) -> Self {
        Self {
            state: EscapeState::Normal,
            trigger: trigger.as_bytes().to_vec(),
            buffered: Vec::new(),
            last_byte_time: None,
        }
    }

    /// Feed a single byte from stdin. Returns what to do with it.
    /// Uses `Instant::now()` for paste-flood detection timing.
    ///
    /// In the current codebase, `SessionEscapeHandler` is the sole production
    /// caller and routes through `feed_with_time` directly. This method is
    /// kept as the primary public API for direct `EscapeDetector` consumers.
    #[allow(dead_code)]
    pub fn feed(&mut self, byte: u8) -> EscapeAction {
        self.feed_with_time(byte, Instant::now())
    }

    /// Feed a byte without paste-flood timing checks.
    /// Used when bytes are forwarded between chained detectors within a
    /// single keystroke event — timing was already validated upstream.
    fn feed_untimed(&mut self, byte: u8) -> EscapeAction {
        // Temporarily disable timing by clearing last_byte_time,
        // then feed with a dummy instant. The timing check only fires
        // in Matching state when last_byte_time is Some, so clearing
        // it effectively bypasses the guard for this byte.
        self.last_byte_time = None;
        // Use a dummy instant; since last_byte_time is None, the
        // timing check will be skipped.
        self.feed_with_time(byte, Instant::now())
    }

    /// Feed a single byte with an explicit timestamp for deterministic testing.
    /// If consecutive trigger bytes arrive faster than [`PASTE_THRESHOLD_MS`],
    /// the sequence is treated as paste and flushed instead of triggering.
    pub fn feed_with_time(&mut self, byte: u8, now: Instant) -> EscapeAction {
        // Empty trigger means detection is disabled
        if self.trigger.is_empty() {
            return EscapeAction::Forward(vec![byte]);
        }

        match self.state {
            EscapeState::Normal => {
                if byte == self.trigger[0] {
                    self.state = EscapeState::Matching(1);
                    self.buffered.push(byte);
                    self.last_byte_time = Some(now);
                    if self.trigger.len() == 1 {
                        // Single-char trigger — immediate match.
                        // Timing guard is skipped for single-char triggers since
                        // there is no inter-byte interval to measure.
                        self.state = EscapeState::Normal;
                        self.buffered.clear();
                        self.last_byte_time = None;
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
                    // Paste-flood guard: if this byte arrived too fast after
                    // the previous match byte, treat the whole sequence as
                    // paste/program output and flush.
                    if let Some(prev) = self.last_byte_time {
                        let elapsed = now.duration_since(prev);
                        if elapsed.as_millis() < PASTE_THRESHOLD_MS as u128 {
                            // Too fast — flush buffered bytes + this byte
                            let mut flushed = std::mem::take(&mut self.buffered);
                            flushed.push(byte);
                            self.state = EscapeState::Normal;
                            self.last_byte_time = None;
                            return EscapeAction::Forward(flushed);
                        }
                    }

                    self.buffered.push(byte);
                    self.last_byte_time = Some(now);
                    if n + 1 == self.trigger.len() {
                        // Full match
                        self.state = EscapeState::Normal;
                        self.buffered.clear();
                        self.last_byte_time = None;
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
                    self.last_byte_time = None;
                    EscapeAction::Forward(flushed)
                }
            }
        }
    }
}

/// Wrap a cursor index with bounds, implementing Up/Down wrapping.
///
/// Returns the new cursor position after moving by `delta` within `count` items.
/// Wraps around: moving up from 0 goes to `count - 1`, and vice versa.
fn wrap_cursor(current: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    ((current as isize + delta).rem_euclid(count as isize)) as usize
}

/// Draw the snippet picker at the current cursor position.
/// `cursor` is the highlighted item index.
fn draw_snippet_picker(
    stdout: &mut std::io::Stdout,
    snippets: &[&Snippet],
    cursor: usize,
) -> Result<()> {
    write!(stdout, "\r\n\x1b[1m── Snippets ──\x1b[0m\r\n")?;
    for (i, snippet) in snippets.iter().enumerate() {
        if i == cursor {
            // Reverse video for highlighted item
            write!(
                stdout,
                "  \x1b[7m\x1b[33m{}\x1b[0m\x1b[7m. {}\x1b[0m\r\n",
                i + 1,
                snippet.name
            )?;
        } else {
            write!(stdout, "  \x1b[33m{}\x1b[0m. {}\r\n", i + 1, snippet.name)?;
        }
    }
    let max = snippets.len();
    write!(
        stdout,
        "\x1b[2m(Up/Down move, Enter select, 1-{max} jump, Esc cancel)\x1b[0m\r\n"
    )?;
    stdout.flush()?;
    Ok(())
}

/// Show the snippet picker inline during an SSH session.
/// Returns the command string to inject, or None if cancelled.
///
/// Navigation: Up/Down arrows move cursor with wrapping. Enter selects.
/// Esc cancels. Number keys 1-9 are immediate shortcuts for backwards compat.
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

    let max = all_snippets.len();
    let mut cursor: usize = 0;
    let lines_to_clear = max + 3; // header + items + footer

    // Initial draw
    draw_snippet_picker(stdout, &all_snippets, cursor)?;

    // Read loop: raw mode is already active during SSH session
    let selection = loop {
        if let Event::Key(key) = crossterm::event::read()? {
            match key.code {
                KeyCode::Esc => {
                    break None;
                }
                KeyCode::Enter => {
                    break Some(cursor);
                }
                KeyCode::Up => {
                    cursor = wrap_cursor(cursor, -1, max);
                    clear_lines(stdout, lines_to_clear)?;
                    draw_snippet_picker(stdout, &all_snippets, cursor)?;
                }
                KeyCode::Down => {
                    cursor = wrap_cursor(cursor, 1, max);
                    clear_lines(stdout, lines_to_clear)?;
                    draw_snippet_picker(stdout, &all_snippets, cursor)?;
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let n = c.to_digit(10).unwrap() as usize;
                    if n >= 1 && n <= max {
                        break Some(n - 1);
                    }
                }
                _ => {}
            }
        }
    };

    // Clear the picker lines
    clear_lines(stdout, lines_to_clear)?;

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
    /// Show the in-session SFTP file browser.
    ShowBrowser,
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

/// Combined escape handler for snippet, bookmark, and browser triggers.
/// Handles three independent escape sequences during an SSH session.
/// Tracks terminal escape sequences (CSI, SS3) to avoid intercepting
/// function key bytes like the `~` terminator in `\x1b[21~` (F10).
pub struct SessionEscapeHandler {
    snippet_detector: EscapeDetector,
    bookmark_detector: EscapeDetector,
    browser_detector: EscapeDetector,
    term_seq: TermSeqState,
}

impl SessionEscapeHandler {
    /// Create a new handler with the given trigger strings.
    pub fn new(snippet_trigger: &str, bookmark_trigger: &str, browser_trigger: &str) -> Self {
        Self {
            snippet_detector: EscapeDetector::new(snippet_trigger),
            bookmark_detector: EscapeDetector::new(bookmark_trigger),
            browser_detector: EscapeDetector::new(browser_trigger),
            term_seq: TermSeqState::Normal,
        }
    }

    /// Feed a single byte from stdin. Returns what action to take.
    ///
    /// Terminal escape sequences (CSI, SS3) are forwarded directly without
    /// trigger detection. For normal bytes, the snippet detector gets first
    /// pass; when it forwards, those bytes are checked by the bookmark detector.
    pub fn feed(&mut self, byte: u8) -> SessionAction {
        self.feed_with_time(byte, Instant::now())
    }

    /// Feed a single byte with an explicit timestamp, propagated to all
    /// underlying detectors for deterministic paste-flood testing.
    pub fn feed_with_time(&mut self, byte: u8, now: Instant) -> SessionAction {
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
        match self.snippet_detector.feed_with_time(byte, now) {
            EscapeAction::Trigger => SessionAction::ShowSnippets,
            EscapeAction::Buffer => {
                // Snippet detector is buffering — don't forward to bookmark detector yet.
                // If the snippet match fails later, the flushed bytes will go through
                // the bookmark detector at that point.
                SessionAction::Buffer
            }
            EscapeAction::Forward(bytes) => {
                // Snippet detector forwarded these bytes — now check bookmark trigger.
                // Use feed_untimed because timing was already validated at the
                // snippet detector level; these are cascaded within a single event.
                let mut after_bookmark = Vec::new();
                for &b in &bytes {
                    match self.bookmark_detector.feed_untimed(b) {
                        EscapeAction::Trigger => return SessionAction::ShowSaveBookmark,
                        EscapeAction::Buffer => {} // absorbed by bookmark detector
                        EscapeAction::Forward(fwd) => after_bookmark.extend(fwd),
                    }
                }
                // Bookmark detector forwarded these bytes — now check browser trigger
                let mut final_forward = Vec::new();
                for &b in &after_bookmark {
                    match self.browser_detector.feed_untimed(b) {
                        EscapeAction::Trigger => return SessionAction::ShowBrowser,
                        EscapeAction::Buffer => {} // absorbed by browser detector
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
                        on_connect_prompt_pattern: None,
                        snippets: vec![],
                        connect_timeout_secs: None,
                        ssh_options: std::collections::BTreeMap::new(),
                        profile: None,
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
    use std::time::Duration;

    /// Helper: create an Instant and a closure that returns instants spaced
    /// by a given interval, simulating human typing speed.
    fn typing_clock(interval_ms: u64) -> impl FnMut() -> Instant {
        let mut now = Instant::now();
        let interval = Duration::from_millis(interval_ms);
        move || {
            let t = now;
            now += interval;
            t
        }
    }

    #[test]
    fn test_escape_detector_trigger() {
        let mut detector = EscapeDetector::new("~~");
        let mut clock = typing_clock(30); // 30ms apart — deliberate typing

        // First tilde — should buffer
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        // Second tilde — should trigger (30ms > 20ms threshold)
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Trigger
        ));
    }

    #[test]
    fn test_escape_detector_single_tilde_then_other() {
        let mut detector = EscapeDetector::new("~~");
        let mut clock = typing_clock(30);

        // First tilde — buffer
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        // 'a' — not a match, flush "~a"
        match detector.feed_with_time(b'a', clock()) {
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
        let mut clock = typing_clock(30);

        assert!(matches!(
            detector.feed_with_time(b'!', clock()),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'!', clock()),
            EscapeAction::Trigger
        ));
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
        let mut clock = typing_clock(30);

        // Trigger
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Trigger
        ));

        // After trigger, back to normal
        match detector.feed_with_time(b'x', clock()) {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'x']);
            }
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_escape_detector_partial_then_trigger() {
        let mut detector = EscapeDetector::new("~~");
        let mut clock = typing_clock(30);

        // Partial match that fails
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        match detector.feed_with_time(b'a', clock()) {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'~', b'a']);
            }
            _ => panic!("Expected Forward"),
        }

        // Now a real trigger
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Trigger
        ));
    }

    #[test]
    fn test_escape_detector_three_char_trigger() {
        let mut detector = EscapeDetector::new("abc");
        let mut clock = typing_clock(30);

        assert!(matches!(
            detector.feed_with_time(b'a', clock()),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'b', clock()),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'c', clock()),
            EscapeAction::Trigger
        ));
    }

    // --- Paste-flood guard ---

    #[test]
    fn test_escape_detector_paste_flood_no_trigger() {
        // AC-9: Bytes arriving within 5ms should NOT trigger — treated as paste
        let mut detector = EscapeDetector::new("~~");
        let mut clock = typing_clock(5); // 5ms apart — paste speed

        // First tilde — buffer (starts the match)
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        // Second tilde — too fast, should flush both bytes as Forward
        match detector.feed_with_time(b'~', clock()) {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'~', b'~']);
            }
            _ => panic!("Expected Forward for paste-speed bytes"),
        }
    }

    #[test]
    fn test_escape_detector_deliberate_typing_triggers() {
        // AC-10: Bytes arriving > 20ms apart should trigger normally
        let mut detector = EscapeDetector::new("~~");
        let mut clock = typing_clock(25); // 25ms apart — deliberate typing

        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'~', clock()),
            EscapeAction::Trigger
        ));
    }

    #[test]
    fn test_escape_detector_paste_exactly_at_threshold() {
        // At exactly 20ms, should trigger (threshold is < 20ms)
        let mut detector = EscapeDetector::new("~~");
        let base = Instant::now();
        let t0 = base;
        let t1 = base + Duration::from_millis(20);

        assert!(matches!(
            detector.feed_with_time(b'~', t0),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'~', t1),
            EscapeAction::Trigger
        ));
    }

    #[test]
    fn test_escape_detector_paste_flood_three_char_trigger() {
        // Three-char trigger with paste speed: second byte triggers flush
        let mut detector = EscapeDetector::new("abc");
        let mut clock = typing_clock(5); // paste speed

        assert!(matches!(
            detector.feed_with_time(b'a', clock()),
            EscapeAction::Buffer
        ));
        // 'b' arrives too fast — flush "ab"
        match detector.feed_with_time(b'b', clock()) {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'a', b'b']);
            }
            _ => panic!("Expected Forward for paste-speed bytes"),
        }
    }

    #[test]
    fn test_escape_detector_single_char_trigger_no_timing() {
        // Single-char triggers bypass the timing guard entirely since
        // there is no inter-byte interval to measure.
        let mut detector = EscapeDetector::new("~");

        // Even with rapid calls (Instant::now()), single-char should trigger
        assert!(matches!(detector.feed(b'~'), EscapeAction::Trigger));
    }

    #[test]
    fn test_escape_detector_paste_then_deliberate_trigger() {
        // After a paste flush, a deliberate trigger should still work
        let mut detector = EscapeDetector::new("~~");
        let base = Instant::now();

        // Paste-speed attempt (flushed)
        assert!(matches!(
            detector.feed_with_time(b'~', base),
            EscapeAction::Buffer
        ));
        match detector.feed_with_time(b'~', base + Duration::from_millis(5)) {
            EscapeAction::Forward(bytes) => {
                assert_eq!(bytes, vec![b'~', b'~']);
            }
            _ => panic!("Expected Forward for paste-speed bytes"),
        }

        // Now deliberate typing (should trigger)
        assert!(matches!(
            detector.feed_with_time(b'~', base + Duration::from_millis(100)),
            EscapeAction::Buffer
        ));
        assert!(matches!(
            detector.feed_with_time(b'~', base + Duration::from_millis(130)),
            EscapeAction::Trigger
        ));
    }

    // --- Cursor wrapping ---

    #[test]
    fn test_wrap_cursor_down() {
        assert_eq!(wrap_cursor(0, 1, 5), 1);
        assert_eq!(wrap_cursor(3, 1, 5), 4);
    }

    #[test]
    fn test_wrap_cursor_up() {
        assert_eq!(wrap_cursor(1, -1, 5), 0);
        assert_eq!(wrap_cursor(3, -1, 5), 2);
    }

    #[test]
    fn test_wrap_cursor_down_wraps() {
        // At last item, Down wraps to 0
        assert_eq!(wrap_cursor(4, 1, 5), 0);
    }

    #[test]
    fn test_wrap_cursor_up_wraps() {
        // At first item, Up wraps to last
        assert_eq!(wrap_cursor(0, -1, 5), 4);
    }

    #[test]
    fn test_wrap_cursor_single_item() {
        // With one item, always stays at 0
        assert_eq!(wrap_cursor(0, 1, 1), 0);
        assert_eq!(wrap_cursor(0, -1, 1), 0);
    }

    #[test]
    fn test_wrap_cursor_empty() {
        assert_eq!(wrap_cursor(0, 1, 0), 0);
    }

    // --- Empty snippet list ---

    #[test]
    fn test_show_snippet_picker_empty_returns_none() {
        // AC-5: When both bookmark and global snippet lists are empty,
        // the picker returns None without any IO side effects.
        // We cannot call show_snippet_picker in a test (requires stdout + crossterm),
        // but we verify the merge logic produces an empty list.
        let bookmark_snippets: Vec<Snippet> = vec![];
        let global_snippets: Vec<Snippet> = vec![];

        // Reproduce the merge logic from show_snippet_picker
        let mut all_snippets: Vec<&Snippet> = bookmark_snippets.iter().collect();
        let bookmark_names: HashSet<&str> =
            bookmark_snippets.iter().map(|s| s.name.as_str()).collect();
        for gs in &global_snippets {
            if !bookmark_names.contains(gs.name.as_str()) {
                all_snippets.push(gs);
            }
        }
        assert!(all_snippets.is_empty());
    }

    // --- SessionEscapeHandler ---

    #[test]
    fn test_session_handler_snippet_trigger() {
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");
        let mut clock = typing_clock(30);

        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::ShowSnippets
        ));
    }

    #[test]
    fn test_session_handler_bookmark_trigger() {
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");
        let mut clock = typing_clock(30);

        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        // After snippet detector flushes '~' + 'b', bookmark detector should catch it
        match handler.feed_with_time(b'b', clock()) {
            SessionAction::ShowSaveBookmark => {} // expected
            other => panic!(
                "Expected ShowSaveBookmark, got {:?}",
                match other {
                    SessionAction::Forward(ref f) => format!("Forward({:?})", f),
                    SessionAction::Buffer => "Buffer".to_string(),
                    SessionAction::ShowSnippets => "ShowSnippets".to_string(),
                    SessionAction::ShowSaveBookmark => "ShowSaveBookmark".to_string(),
                    SessionAction::ShowBrowser => "ShowBrowser".to_string(),
                }
            ),
        }
    }

    #[test]
    fn test_session_handler_normal_text() {
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");

        match handler.feed(b'h') {
            SessionAction::Forward(bytes) => assert_eq!(bytes, vec![b'h']),
            _ => panic!("Expected Forward"),
        }
    }

    #[test]
    fn test_session_handler_both_disabled() {
        let mut handler = SessionEscapeHandler::new("", "", "");

        match handler.feed(b'~') {
            SessionAction::Forward(bytes) => assert_eq!(bytes, vec![b'~']),
            _ => panic!("Expected Forward for disabled triggers"),
        }
    }

    // --- Terminal escape sequence passthrough ---

    #[test]
    fn test_session_handler_f10_csi_passthrough() {
        // F10 = \x1b[21~ — the trailing ~ must NOT start trigger matching
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");

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
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");

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
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");

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
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");

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
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");
        let mut clock = typing_clock(30);

        // Send F10
        for &byte in b"\x1b[21~" {
            handler.feed_with_time(byte, clock());
        }

        // Now trigger ~~ should still work
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::ShowSnippets
        ));
    }

    #[test]
    fn test_session_handler_alt_key_passthrough() {
        // Alt+x = \x1b x (two bytes)
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");

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

    #[test]
    fn test_session_handler_browser_trigger() {
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");
        let mut clock = typing_clock(30);

        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        assert!(matches!(
            handler.feed_with_time(b'f', clock()),
            SessionAction::ShowBrowser
        ));
    }

    #[test]
    fn test_session_handler_browser_trigger_disabled() {
        let mut handler = SessionEscapeHandler::new("~~", "~b", "");
        let mut clock = typing_clock(30);

        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        // With browser trigger disabled, ~f should forward
        match handler.feed_with_time(b'f', clock()) {
            SessionAction::Forward(bytes) => assert_eq!(bytes, vec![b'~', b'f']),
            _ => panic!("Expected Forward when browser trigger disabled"),
        }
    }

    #[test]
    fn test_session_handler_all_triggers_independent() {
        // Verify all three triggers work in sequence
        let mut handler = SessionEscapeHandler::new("~~", "~b", "~f");
        let mut clock = typing_clock(30);

        // Snippet trigger
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::ShowSnippets
        ));

        // Bookmark trigger
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        assert!(matches!(
            handler.feed_with_time(b'b', clock()),
            SessionAction::ShowSaveBookmark
        ));

        // Browser trigger
        assert!(matches!(
            handler.feed_with_time(b'~', clock()),
            SessionAction::Buffer
        ));
        assert!(matches!(
            handler.feed_with_time(b'f', clock()),
            SessionAction::ShowBrowser
        ));
    }
}
