# v0.3.0 — Bookmark Groups, Mux Mode, Persistent Sessions

## New Features

### Bookmark Groups
- Organize bookmarks into groups with shared host, user, env, and profile settings
- Sessions within groups inherit settings from the group with per-session overrides (5-layer chain)
- Split-pane TUI for groups: left pane shows sessions, right pane shows details
- Group form for adding/editing groups with session lines (Ctrl+O to add session)

### Group Session Mux
- Enter mux mode from a group (Enter on group in list)
- Navigate sessions with Up/Down, connect with Enter
- Right pane shows session info and terminal output

### Persistent Mux Sessions
- Keep one persistent SSH connection per group in mux mode
- Switch sessions without re-authenticating — sends new command over existing connection
- Terminal output shown in right pane
- Non-interactive auth (keychain-only) for seamless switching
- Interactive auth fallback: if keychain has no password, pressing Enter again prompts for password

### Unified Form
- Single form for both bookmarks and groups
- Cross-type editing: edit bookmark → add sessions → becomes group automatically
- Session `on_connect` command editing in form
- Name conflict detection (bookmark vs group)

### SFTP Improvements
- Background transfers at 500ms poll rate (was 100ms) — ~23% → ~5% CPU when not watching popup
- Per-file progress tracking with batched updates
- Feed loop cancellable with timeout to prevent deadlock
- Create parent directories on-demand (mkdir -p style)
- Immediate source file deletion during move operations

### Config & Security
- Auto-detect debug builds by binary path — uses separate config dir to protect production data
- `SSHORE_CONFIG_DIR` env var to override config path
- `SSHORE_NO_POLL_CHECK` env var to disable rapid-poll detection
- Advisory file locking for config modifications (prevents concurrent instance conflicts)
- Allow `/`, spaces, and parentheses in bookmark names

## Bug Fixes
- SFTP transfer hang when session dies mid-transfer
- Rapid-poll false positives (raised limit from 10 to 100, fixed poll ordering)
- Ctrl+O handled as raw control character in form
- Form session add key changed from Ctrl+Enter to Alt+Enter (then Ctrl+O)

## CI/Dev
- Pre-push hook: runs fmt, clippy, and tests before pushing
- `.planning/` added to gitignore
