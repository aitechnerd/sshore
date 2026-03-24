# Project Context for Codex

You are acting as an independent reviewer for this project.
Your reviews are consumed by Claude Code's AI Dev Team pipeline.

## Output Format
- Be concise — your output is parsed by another AI, not a human
- Use numbered lists for issues/suggestions
- Prefix severity: [critical], [major], [minor], [suggestion]
- Focus on gaps, edge cases, and things the primary agent might miss

## Product
- **Name:** sshore
- **Purpose:** Terminal-native SSH connection manager with environment-aware visual safety
- **Users:** DevOps engineers, sysadmins, and developers who SSH into multiple servers across environments daily
- **Domain:** Developer tools / infrastructure management

## Stack
- Rust (edition 2024, cargo 1.93.1, rustc 1.93.1)
- Single binary CLI/TUI
- Key crates: russh (native SSH), ratatui + crossterm (TUI), tokio (async), clap (CLI), serde + toml (config)
- No database — TOML config file (~/.config/sshore/config.toml)

## Conventions
- Native SSH via russh (no shell-out to ssh binary)
- Environment tier (production/staging/dev/local/testing) drives all UX decisions
- Atomic config writes (tempfile then rename)
- Passwords in OS keychain only, never in config files
- Offline-first: no network calls until user-initiated action
- Error handling: anyhow::Result with .context() on all fallible ops
- Tests alongside code; unit tests in-file, integration tests in tests/
- All logic changes ship with tests
- Commit style: action-oriented, scoped (e.g., "config: Add export filtering")
