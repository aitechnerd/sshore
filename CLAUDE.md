# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

sshore — Terminal-native SSH connection manager with environment-aware safety, built in Rust. Manages SSH bookmarks with color-coded environment tiers (production/staging/development/local/testing), interactive TUI, native SSH via `russh`, sudo password assist, SFTP/SCP shortcuts with resumable transfers, dual-pane TUI file browser (mc-style), persistent tunnels, per-bookmark snippets, quick remote exec, config export, known_hosts verification, ad-hoc connect with save-as-bookmark, cross-machine config sync, and a `StorageBackend` trait for future S3/cloud support.

See `CLAUDE.local.md` for the implementation plan (phase files, project overview, Cargo.toml spec). Follow phases in order.

## Build & Run

```bash
# Build
cargo build
cargo build --release

# Run CLI
cargo run -- --help
cargo run -- list                          # list bookmarks (non-interactive)
cargo run -- import                        # import from ~/.ssh/config
cargo run                                  # launch TUI (default, no subcommand)
cargo run -- prod-web-01                   # direct connect by bookmark name
cargo run -- exec prod-web-01 "uptime"     # quick remote exec
cargo run -- export --env production       # export bookmarks to TOML
cargo run -- connect user@10.0.1.50        # ad-hoc connect without bookmark
cargo run -- browse prod-web-01            # dual-pane file browser
cargo run -- browse prod-web-01:/var/log   # browse starting at specific path
cargo run -- --config ~/sync/sshore.toml list  # use custom config path

# Install locally
cargo install --path .
sshore --help
```

## Test Commands

```bash
cargo test                                 # all tests
cargo test config_roundtrip                # single test by name
cargo test --test ssh_import_test          # single integration test file
cargo test env_detection                   # pattern match
cargo test -- --ignored                    # run ignored tests (integration, need OS keychain)
cargo test -- --nocapture                  # show println! output
```

## Lint & Format

```bash
cargo fmt                                  # format all code
cargo fmt -- --check                       # check formatting (CI)
cargo clippy -- -D warnings                # lint with all warnings as errors
cargo clippy --fix                         # auto-fix
```

## Architecture

**Core flow:** Config → TUI → SSH Connect → Terminal Proxy (with sudo assist, theming)

**Native SSH client (not shell-out):** Uses `russh` crate for native async SSH. This enables sudo prompt detection, terminal theming injection, SFTP integration, and persistent tunnels — all impossible with shell-out to `ssh`.

**Data flow during SSH session:**
```
[keyboard] → sshore → [russh SSH channel] → [remote PTY]
[screen]   ← sshore ← [russh SSH channel] ← [remote PTY]
                ↑
         watches for sudo prompts,
         injects terminal theming,
         handles reconnects
```

```
src/
├── main.rs                      # Entry point: clap CLI parsing, dispatch to TUI or subcommands
├── cli.rs                       # Clap derive structs for all CLI args and subcommands
├── config/
│   ├── mod.rs                   # Public API: load(), save(), config_path()
│   ├── model.rs                 # Serde structs: AppConfig, Bookmark, EnvColor, Settings
│   ├── ssh_import.rs            # Parse ~/.ssh/config → Vec<Bookmark>, handle Include directives
│   ├── env.rs                   # Environment auto-detection heuristic from hostname/name
│   └── writer.rs                # Atomic config writes: serialize → tempfile → rename
├── ssh/
│   ├── mod.rs                   # Public API: connect(), exec_command(), SSH session lifecycle
│   ├── client.rs                # russh client::Handler implementation
│   ├── terminal_theme.rs        # OSC escape codes: tab title, tab color
│   ├── password.rs              # Sudo prompt detection + password injection
│   ├── snippet.rs               # Escape sequence detector + inline snippet picker
│   ├── known_hosts.rs           # Host key verification against ~/.ssh/known_hosts
│   └── tunnel.rs                # Persistent tunnel management with auto-reconnect
├── sftp/
│   ├── mod.rs                   # Public API: sftp_session(), transfer()
│   └── shortcuts.rs             # CLI shortcuts: sshore sftp/scp <bookmark>
├── tui/
│   ├── mod.rs                   # App state machine, main event loop, screen routing
│   ├── views/
│   │   ├── list.rs              # Main bookmark list table with env badge column
│   │   ├── form.rs              # Add/Edit bookmark form with field validation
│   │   ├── confirm.rs           # Delete confirmation (production requires typing "yes")
│   │   └── help.rs              # Keybinding help overlay
│   ├── widgets/
│   │   ├── search_bar.rs        # Fuzzy search input with real-time filtering
│   │   ├── env_badge.rs         # Colored environment badge widget
│   │   └── status_bar.rs        # Bottom bar: context-aware keybinding hints
│   └── theme.rs                 # Environment color palette resolution
├── keychain.rs                  # OS keychain wrapper via `keyring` crate
```

**Config system:** TOML-based (`~/.config/sshore/config.toml`, XDG-compliant). Serde models in `config/model.rs`. Atomic writes via tempfile-then-rename. File permissions 0600 on Unix.

**Async runtime:** Tokio. Required by `russh`. All SSH, SFTP, and tunnel operations are async. TUI event loop uses `crossterm`'s async event stream.

**Implementation phases:** Foundation (config/CLI) → TUI list → Bookmark CRUD → SSH connect → Sudo assist → SFTP/SCP → Tunnels → Polish/release → Snippets/exec/export → Hardening/reliability.

## Engineering Principles

- **DRY** — flag and eliminate repetition aggressively.
- **Explicit > clever** — readable code wins over terse one-liners. Avoid macro-heavy patterns when plain functions suffice.
- **Handle edge cases** — err on the side of handling more, not fewer.
- **Engineered enough** — not under-engineered (fragile, hacky) and not over-engineered (premature abstraction, unnecessary complexity).
- **Well-tested code is non-negotiable** — every logic change ships with tests.
- **Offline-first** — sshore never makes network calls at startup or during TUI operation. All network activity is user-initiated: SSH connect, SFTP transfer, tunnel start. No update checks, no telemetry, no DNS resolution until the user presses Enter on a bookmark.

## Code Quality & Style

- **Error handling**: Use `anyhow::Result` for application code. Add `.context("descriptive message")?` to all fallible operations. Never `unwrap()` on user-facing paths — always handle gracefully. TUI should never panic; catch errors and display in the status bar.
- **Comments**: Explain *why*, not *what*. Doc comments (`///`) on all public functions and types.
- **Constants over magic numbers**: Name thresholds, timeouts, buffer sizes, and retry limits.
- **Logging**: Not yet needed — use `eprintln!` for user-facing warnings/errors. Add structured logging (`tracing` crate) if/when complexity warrants it.
- **Imports**: Group as `std` / third-party crates / local `crate::` modules, separated by blank lines.
- **Naming**: Follow Rust conventions — `snake_case` for functions/variables, `PascalCase` for types/enums, `SCREAMING_SNAKE_CASE` for constants.
- **Derive macros**: Use `#[derive(Debug, Clone, Serialize, Deserialize)]` on all config/model structs. Add `PartialEq` when tests need equality comparison.
- **`#[cfg]` gating**: Use `#[cfg(unix)]` for file permissions, `#[cfg(target_os = "macos")]` only when truly macOS-specific. Most code should be platform-independent — `crossterm` and `dirs` handle platform differences.

## Security

- **Config file permissions**: Create with mode 0600 on Unix. Warn to stderr if permissions are wider than 0600.
- **Input validation**: Reject hostnames with shell metacharacters (`;`, `|`, `&`, `$`, `` ` ``, `(`, `)`, `{`, `}`, `<`, `>`, `\n`, `\r`). Bookmark names: alphanumeric + `-_.` only. Identity file paths must resolve within the user's home directory.
- **Passwords**: Stored exclusively in OS keychain (via `keyring` crate). Never written to config files, log output, or stdout. Password injection always requires explicit user confirmation (Enter press).
- **Atomic writes**: All config modifications use tempfile-then-rename to prevent corruption on crash.
- **SFTP channel isolation**: SFTP sessions MUST use a separate SSH channel from interactive shell sessions. Transfer errors must never corrupt or close an interactive SSH session.
- **Never log credentials**: No SSH keys, passwords, or keychain contents in any output.

## Testing

- **Framework**: `cargo test` with standard Rust test harness.
- **Conventions**:
  - Unit tests: `#[cfg(test)] mod tests` at the bottom of each source file.
  - Integration tests: `tests/<module_name>.rs` files.
  - Test naming: `test_<function>_<scenario>` (e.g., `test_detect_env_production_keyword`).
  - Helper fixtures: Create builder functions (e.g., `sample_bookmark()`, `sample_config()`) in a `tests/common/mod.rs` or test-local helpers.
  - Mocking: No network calls in tests. Use trait objects or dependency injection to mock SSH/keychain. Use `tempfile` crate for config file tests.
  - Tests must be deterministic and fast.
  - `#[ignore]` for tests needing OS keychain access or real SSH connections.

## Regression Prevention

- **Config forward compatibility**: Use `#[serde(default)]` on all optional fields. Unknown TOML keys should be silently ignored (serde's default behavior) so older configs work with newer versions.
- **Terminal cleanup**: Always restore terminal state (raw mode off, cursor visible, theme reset) even on panic or Ctrl+C. Use drop guards.

## File Size Guidelines

- **Target**: ~500 LOC per file. This is a guideline, not a hard limit. Split when it improves clarity or testability.

## Commit Guidelines

- **Never commit automatically** — only commit when the user explicitly asks. Do not create commits on your own.
- **Message style**: Concise, action-oriented. Start with a verb: Add / Fix / Refactor / Update / Remove.
- **Scope prefix** when helpful: e.g. `config: ...`, `ssh: ...`, `tui: ...`, `cli: ...`
- **One logical change per commit** — don't bundle unrelated refactors.
- **Never commit secrets**: SSH keys, keychain passwords, config files with real hostnames.
