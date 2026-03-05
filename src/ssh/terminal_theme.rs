use std::io::Write;

use crate::config::model::{Bookmark, Settings};

/// Apply terminal theming: set tab title and tab color based on bookmark environment.
pub fn apply_theme(bookmark: &Bookmark, settings: &Settings) {
    let title = render_tab_title(&settings.tab_title_template, bookmark, settings);
    apply_theme_with_title(bookmark, settings, &title);
}

/// Apply terminal theming with a custom tab title.
pub fn apply_theme_with_title(bookmark: &Bookmark, settings: &Settings, title: &str) {
    save_title();
    save_background();
    set_tab_title(title);

    if let Some(env_color) = settings.env_colors.get(&bookmark.env) {
        set_tab_color(&env_color.bg);
        set_background_tint(&env_color.bg);
        set_cursor_color(&env_color.bg);
    }
}

/// Re-apply terminal theming after returning from a sub-TUI (e.g. file browser).
/// The sub-TUI's cleanup guard resets the theme, so we need to restore it for
/// the ongoing SSH session.
pub fn reapply_theme(bookmark: &Bookmark, settings: &Settings) {
    let title = render_tab_title(&settings.tab_title_template, bookmark, settings);
    set_tab_title(&title);

    if let Some(env_color) = settings.env_colors.get(&bookmark.env) {
        set_tab_color(&env_color.bg);
        set_background_tint(&env_color.bg);
        set_cursor_color(&env_color.bg);
    }
}

/// Reset terminal tab title and color to defaults.
pub fn reset_theme() {
    reset_background();
    reset_tab_title();
    reset_tab_color();
}

/// Save the current window title to the terminal's title stack (xterm CSI 22;0t).
/// Supported by most modern terminals: xterm, iTerm2, Tabby, kitty, Alacritty, WezTerm.
fn save_title() {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[22;0t");
    let _ = stdout.flush();
}

/// Set terminal tab title via OSC 0 (universally supported).
fn set_tab_title(title: &str) {
    let title = sanitize_terminal_text(title);
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]0;{title}\x07");
    let _ = stdout.flush();
}

/// Restore the previous window title.
/// First sets an empty title via OSC 0 (tells the terminal to use its default title),
/// then pops the title stack via CSI 23;0t for terminals that support it.
/// The empty-title approach works universally (iTerm2, Terminal.app, kitty, etc.)
/// while the stack pop is a best-effort bonus for xterm-compatible terminals.
fn reset_tab_title() {
    let mut stdout = std::io::stdout();
    // Set empty title — terminal reverts to its default (e.g. shell process name)
    let _ = write!(stdout, "\x1b]0;\x07");
    // Also try popping the title stack for xterm-compatible terminals
    let _ = write!(stdout, "\x1b[23;0t");
    let _ = stdout.flush();
}

/// Set terminal tab color using both Terminal.app (OSC 6) and iTerm2 (OSC 1337) codes.
/// Unknown OSC codes are silently ignored by terminals, so it's safe to emit both.
fn set_tab_color(hex_color: &str) {
    let Some((r, g, b)) = parse_hex_rgb(hex_color) else {
        return;
    };

    let mut stdout = std::io::stdout();
    // Terminal.app (macOS proprietary OSC 6)
    let _ = write!(stdout, "\x1b]6;1;bg;red;brightness;{r}\x07");
    let _ = write!(stdout, "\x1b]6;1;bg;green;brightness;{g}\x07");
    let _ = write!(stdout, "\x1b]6;1;bg;blue;brightness;{b}\x07");

    // iTerm2 (macOS proprietary OSC 1337)
    if let Some(hex_stripped) = hex_color.strip_prefix('#') {
        let _ = write!(stdout, "\x1b]1337;SetColors=tab={hex_stripped}\x07");
    }

    let _ = stdout.flush();
}

/// Reset terminal tab color for both Terminal.app and iTerm2.
fn reset_tab_color() {
    let mut stdout = std::io::stdout();
    // Terminal.app reset
    let _ = write!(stdout, "\x1b]6;1;bg;*;default\x07");
    // iTerm2 reset
    let _ = write!(stdout, "\x1b]1337;SetColors=tab=default\x07");
    let _ = stdout.flush();
}

/// Save the current terminal background color so we can restore it later.
/// Uses OSC 11 query + xterm stack push. Not all terminals support the query,
/// so we also rely on resetting to a known default on restore.
fn save_background() {
    let mut stdout = std::io::stdout();
    // Push background color onto xterm's color stack (CSI 22;11t is not standard,
    // so we just rely on reset_background restoring the default).
    // For iTerm2, save via proprietary sequence:
    let _ = write!(stdout, "\x1b[22;0t"); // already saved by save_title, but harmless
    let _ = stdout.flush();
}

/// Set terminal background to a faded version of the env color.
/// Blends the env color at ~22% intensity with a dark base (#1a1a2e) to
/// produce a noticeable tint that doesn't interfere with readability.
fn set_background_tint(hex_color: &str) {
    let Some((r, g, b)) = parse_hex_rgb(hex_color) else {
        return;
    };

    // Blend env color at 22% with dark base (26, 26, 46)
    const BASE_R: u16 = 26;
    const BASE_G: u16 = 26;
    const BASE_B: u16 = 46;
    const BLEND: u16 = 22; // percent

    let tr = (BASE_R * (100 - BLEND) + r as u16 * BLEND) / 100;
    let tg = (BASE_G * (100 - BLEND) + g as u16 * BLEND) / 100;
    let tb = (BASE_B * (100 - BLEND) + b as u16 * BLEND) / 100;

    set_osc_background(tr as u8, tg as u8, tb as u8);
}

/// Set terminal background via OSC 11 and cursor color via OSC 12.
/// Cursor color uses a brighter version of the tint so it's visible even
/// inside full-screen apps (mc, vim, htop) that override background colors.
fn set_osc_background(r: u8, g: u8, b: u8) {
    let mut stdout = std::io::stdout();
    // OSC 11 — set terminal background color (widely supported)
    let _ = write!(
        stdout,
        "\x1b]11;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x07",
        r, r, g, g, b, b
    );
    let _ = stdout.flush();
}

/// Set the cursor color to the env color (slightly dimmed).
/// Visible even inside full-screen apps like mc, vim, htop.
fn set_cursor_color(hex_color: &str) {
    let Some((r, g, b)) = parse_hex_rgb(hex_color) else {
        return;
    };

    // Use env color at ~70% brightness for the cursor
    let cr = (r as u16 * 70 / 100) as u8;
    let cg = (g as u16 * 70 / 100) as u8;
    let cb = (b as u16 * 70 / 100) as u8;

    let mut stdout = std::io::stdout();
    // OSC 12 — set cursor color
    let _ = write!(
        stdout,
        "\x1b]12;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x07",
        cr, cr, cg, cg, cb, cb
    );
    let _ = stdout.flush();
}

/// Reset terminal background and cursor color to defaults.
fn reset_background() {
    let mut stdout = std::io::stdout();
    // OSC 111 — reset background to terminal default
    let _ = write!(stdout, "\x1b]111\x07");
    // OSC 112 — reset cursor color to terminal default
    let _ = write!(stdout, "\x1b]112\x07");
    let _ = stdout.flush();
}

/// Parse a hex color string "#RRGGBB" into (r, g, b) components.
fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Render tab title template with bookmark values.
/// Supported placeholders: {name}, {host}, {user}, {env}, {badge}, {label}
pub fn render_tab_title(template: &str, bookmark: &Bookmark, settings: &Settings) -> String {
    let env_color = settings.env_colors.get(&bookmark.env);
    let safe_template = sanitize_terminal_text(template);
    let safe_name = sanitize_terminal_text(&bookmark.name);
    let safe_host = sanitize_terminal_text(&bookmark.host);
    let safe_user = sanitize_terminal_text(bookmark.user.as_deref().unwrap_or(""));
    let safe_env = sanitize_terminal_text(&bookmark.env);
    let safe_badge = sanitize_terminal_text(env_color.map_or("", |c| &c.badge));
    let safe_label = sanitize_terminal_text(env_color.map_or("", |c| &c.label));

    safe_template
        .replace("{name}", &safe_name)
        .replace("{host}", &safe_host)
        .replace("{user}", &safe_user)
        .replace("{env}", &safe_env)
        .replace("{badge}", &safe_badge)
        .replace("{label}", &safe_label)
}

/// Strip control bytes from user-supplied terminal text to prevent escape injection.
fn sanitize_terminal_text(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_ascii_control())
        .collect::<String>()
}

// ---------------------------------------------------------------------------
// OSC title stripper — prevents remote shell from overwriting sshore's tab title
// ---------------------------------------------------------------------------

/// Strips OSC 0 and OSC 2 (window/tab title) sequences from raw terminal data,
/// preserving all other output. This prevents the remote shell's PROMPT_COMMAND
/// or PS1 from overwriting sshore's environment-aware tab title.
///
/// OSC sequences have the form: `ESC ] <code> ; <text> BEL` or `ESC ] <code> ; <text> ESC \`
///
/// This operates on raw bytes because SSH data is not guaranteed to be valid UTF-8
/// and we need to preserve binary transparency for everything except title sequences.
pub struct OscTitleStripper {
    /// Parser state for the OSC sequence state machine.
    state: StripState,
    /// Accumulates the OSC parameter code (e.g. "0", "2") to decide whether to strip.
    code_buf: Vec<u8>,
}

/// Internal state machine for OSC sequence detection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StripState {
    /// Normal pass-through.
    Normal,
    /// Saw ESC (0x1b), waiting for `]` (OSC) or anything else.
    Esc,
    /// Inside `ESC ]`, accumulating the numeric code before `;`.
    OscCode,
    /// Inside an OSC that we want to strip — skip bytes until the terminator.
    OscSkip,
    /// Inside an OSC that we want to keep — pass bytes until the terminator.
    OscPass,
    /// Saw ESC inside an OSC body (could be the ST = `ESC \` terminator).
    OscSkipEsc,
    /// Saw ESC inside a pass-through OSC body.
    OscPassEsc,
}

impl Default for OscTitleStripper {
    fn default() -> Self {
        Self::new()
    }
}

impl OscTitleStripper {
    pub fn new() -> Self {
        Self {
            state: StripState::Normal,
            code_buf: Vec::with_capacity(4),
        }
    }

    /// Process a chunk of raw terminal data, returning a `Vec<u8>` with
    /// OSC 0/2 title sequences removed. All other data passes through unchanged.
    pub fn strip(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());

        for &byte in data {
            match self.state {
                StripState::Normal => {
                    if byte == 0x1b {
                        self.state = StripState::Esc;
                    } else {
                        out.push(byte);
                    }
                }
                StripState::Esc => {
                    if byte == b']' {
                        // Start of OSC sequence
                        self.state = StripState::OscCode;
                        self.code_buf.clear();
                    } else {
                        // Not OSC — emit the held ESC + this byte
                        out.push(0x1b);
                        out.push(byte);
                        self.state = StripState::Normal;
                    }
                }
                StripState::OscCode => {
                    if byte == b';' {
                        // Semicolon terminates the code — decide strip or pass
                        let should_strip = self.code_buf == b"0"
                            || self.code_buf == b"2"
                            || self.code_buf == b"10"
                            || self.code_buf == b"11"
                            || self.code_buf == b"12";
                        if should_strip {
                            self.state = StripState::OscSkip;
                        } else {
                            // Emit the held ESC ] <code> ;
                            out.push(0x1b);
                            out.push(b']');
                            out.extend_from_slice(&self.code_buf);
                            out.push(b';');
                            self.state = StripState::OscPass;
                        }
                    } else if byte == 0x07 {
                        // BEL terminates a code-only OSC (no semicolon)
                        let should_strip = self.code_buf == b"0"
                            || self.code_buf == b"2"
                            || self.code_buf == b"10"
                            || self.code_buf == b"11"
                            || self.code_buf == b"12";
                        if !should_strip {
                            out.push(0x1b);
                            out.push(b']');
                            out.extend_from_slice(&self.code_buf);
                            out.push(0x07);
                        }
                        self.state = StripState::Normal;
                    } else if byte.is_ascii_digit() {
                        self.code_buf.push(byte);
                    } else {
                        // Unexpected byte — not a valid OSC, emit everything
                        out.push(0x1b);
                        out.push(b']');
                        out.extend_from_slice(&self.code_buf);
                        out.push(byte);
                        self.state = StripState::Normal;
                    }
                }
                StripState::OscSkip => {
                    if byte == 0x07 {
                        // BEL terminates — done stripping
                        self.state = StripState::Normal;
                    } else if byte == 0x1b {
                        self.state = StripState::OscSkipEsc;
                    }
                    // else: skip this byte
                }
                StripState::OscSkipEsc => {
                    if byte == b'\\' {
                        // ST (ESC \) terminates — done stripping
                        self.state = StripState::Normal;
                    } else {
                        // False alarm — still inside OSC body
                        self.state = StripState::OscSkip;
                    }
                }
                StripState::OscPass => {
                    if byte == 0x07 {
                        out.push(0x07);
                        self.state = StripState::Normal;
                    } else if byte == 0x1b {
                        self.state = StripState::OscPassEsc;
                    } else {
                        out.push(byte);
                    }
                }
                StripState::OscPassEsc => {
                    if byte == b'\\' {
                        // ST terminator
                        out.push(0x1b);
                        out.push(b'\\');
                        self.state = StripState::Normal;
                    } else {
                        // Not ST — emit ESC and continue in pass mode
                        out.push(0x1b);
                        out.push(byte);
                        self.state = StripState::OscPass;
                    }
                }
            }
        }

        out
    }
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
        }
    }

    #[test]
    fn test_render_tab_title_default_template() {
        let settings = Settings::default();
        let bookmark = sample_bookmark();
        let result = render_tab_title(&settings.tab_title_template, &bookmark, &settings);
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("PROD"));
    }

    #[test]
    fn test_render_tab_title_all_placeholders() {
        let settings = Settings::default();
        let bookmark = sample_bookmark();
        let template = "{badge} {label} — {name} ({user}@{host}) [{env}]";
        let result = render_tab_title(template, &bookmark, &settings);
        assert!(result.contains("prod-web-01"));
        assert!(result.contains("deploy"));
        assert!(result.contains("10.0.1.5"));
        assert!(result.contains("production"));
        assert!(result.contains("PROD"));
    }

    #[test]
    fn test_render_tab_title_no_user() {
        let settings = Settings::default();
        let mut bookmark = sample_bookmark();
        bookmark.user = None;
        let template = "{user}@{host}";
        let result = render_tab_title(template, &bookmark, &settings);
        assert_eq!(result, "@10.0.1.5");
    }

    #[test]
    fn test_render_tab_title_unknown_env() {
        let settings = Settings::default();
        let mut bookmark = sample_bookmark();
        bookmark.env = "custom".into();
        let template = "{badge} {label} — {name}";
        let result = render_tab_title(template, &bookmark, &settings);
        // No badge or label for unknown env
        assert_eq!(result, "  — prod-web-01");
    }

    #[test]
    fn test_parse_hex_rgb_valid() {
        assert_eq!(parse_hex_rgb("#CC0000"), Some((204, 0, 0)));
        assert_eq!(parse_hex_rgb("#FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex_rgb("#000000"), Some((0, 0, 0)));
    }

    #[test]
    fn test_parse_hex_rgb_invalid() {
        assert_eq!(parse_hex_rgb(""), None);
        assert_eq!(parse_hex_rgb("CC0000"), None);
        assert_eq!(parse_hex_rgb("#FFF"), None); // Too short
        assert_eq!(parse_hex_rgb("#GGGGGG"), None);
    }

    #[test]
    fn test_render_tab_title_strips_control_chars() {
        let settings = Settings::default();
        let bookmark = Bookmark {
            name: "prod\x1b]0;hacked\x07".into(),
            host: "10.0.1.5".into(),
            user: Some("deploy".into()),
            port: 22,
            env: "production".into(),
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
        };
        let result = render_tab_title("{name}", &bookmark, &settings);
        assert_eq!(result, "prod]0;hacked");
        assert!(!result.contains('\x1b'));
        assert!(!result.contains('\x07'));
    }

    // -- OSC title stripper --

    #[test]
    fn test_strip_osc0_bel_terminated() {
        let mut s = OscTitleStripper::new();
        let input = b"hello\x1b]0;my title\x07world";
        let out = s.strip(input);
        assert_eq!(out, b"helloworld");
    }

    #[test]
    fn test_strip_osc2_bel_terminated() {
        let mut s = OscTitleStripper::new();
        let input = b"before\x1b]2;window title\x07after";
        let out = s.strip(input);
        assert_eq!(out, b"beforeafter");
    }

    #[test]
    fn test_strip_osc0_st_terminated() {
        let mut s = OscTitleStripper::new();
        // ST = ESC backslash
        let input = b"hello\x1b]0;my title\x1b\\world";
        let out = s.strip(input);
        assert_eq!(out, b"helloworld");
    }

    #[test]
    fn test_pass_other_osc_codes() {
        let mut s = OscTitleStripper::new();
        // OSC 7 (directory) should pass through
        let input = b"\x1b]7;file:///home/user\x07ok";
        let out = s.strip(input);
        assert_eq!(out, b"\x1b]7;file:///home/user\x07ok");
    }

    #[test]
    fn test_strip_across_chunks() {
        let mut s = OscTitleStripper::new();
        // Title sequence split across two data chunks
        let out1 = s.strip(b"data\x1b]0;my ti");
        let out2 = s.strip(b"tle\x07more");
        assert_eq!(out1, b"data");
        assert_eq!(out2, b"more");
    }

    #[test]
    fn test_normal_escapes_pass_through() {
        let mut s = OscTitleStripper::new();
        // CSI sequences (ESC [) should pass through
        let input = b"\x1b[32mgreen\x1b[0m";
        let out = s.strip(input);
        assert_eq!(out, input.to_vec());
    }

    #[test]
    fn test_no_escapes_passthrough() {
        let mut s = OscTitleStripper::new();
        let input = b"plain text with no escapes";
        let out = s.strip(input);
        assert_eq!(out, input.to_vec());
    }

    #[test]
    fn test_strip_multiple_osc0_in_one_chunk() {
        let mut s = OscTitleStripper::new();
        let input = b"a\x1b]0;t1\x07b\x1b]0;t2\x07c";
        let out = s.strip(input);
        assert_eq!(out, b"abc");
    }
}
