# Feature: Bookmark Groups
Status: ready
Created: 2026-06-18
Slug: bookmark-groups

---

## Problem

When working with multiple projects on the same server, users must repeat the same SSH connection details (host, user, key, environment) for every bookmark. Each project needs its own `on_connect` command (e.g. `tmux attach -t project-a`), but defining host, user, identity file, and env separately for each bookmark is verbose and error-prone. The existing Profile system shares auth settings but cannot share host or environment tier, leaving the most repetitive fields duplicated.

## Solution

Introduce **Bookmark Groups** as a first-class config concept. A Group defines the shared server connection (host, port, user, identity file, proxy jump, environment). Within a Group, multiple **Sessions** each define a unique `on_connect` command. Sessions inherit all connection settings from their parent Group and can optionally override individual fields. Sessions also inherit from any Profile referenced by the Group, maintaining the existing five-layer resolution chain.

In the TUI, the list view splits into two panes: the left pane shows Groups (collapsible) with their Sessions nested underneath; the right pane shows the active SSH terminal for the selected session.

## User Stories

1. As a developer with multiple projects on one server, I want to define the server connection once and add sessions for each project, so that I don't repeat host, user, and key for every bookmark.
2. As a developer, I want each session to run its own `on_connect` command (e.g. `tmux attach -t project-a`), so that connecting to a session drops me into the right workspace.
3. As a developer, I want sessions to inherit from a Profile for auth settings, so that I can combine server-level defaults (Group) with auth-level defaults (Profile).
4. As a developer, I want to see my sessions grouped by server in the TUI list, so that I can quickly find the right project without scrolling through a flat list.
5. As a developer, I want to override a session's `on_connect` without affecting other sessions in the same group, so that each project has independent behaviour.
6. As a developer, I want sessions with the same name in different groups to coexist, so that I can name sessions consistently across servers (e.g. "frontend" on both staging and production).
7. As a developer, I want existing bookmarks (without groups) to continue working, so that I can adopt groups incrementally.
8. As a developer, I want a warning if a Group references a Profile that doesn't exist, so that I catch misconfiguration early.
9. As a developer, I want the config to reject duplicate session names within the same Group, so that I don't accidentally create ambiguous entries.
10. As a developer, I want the config to reject duplicate Group names, so that there's no ambiguity about which server a session belongs to.

## Acceptance Criteria

- [ ] A `[[groups]]` TOML section accepts: `name`, `host`, `port`, `user`, `identity_file`, `proxy_jump`, `env`, `tags`, `notes`, `profile`, `connect_timeout_secs`, `ssh_options`, `on_connect`, `on_connect_prompt_pattern`, `snippets`, and a nested `[[groups.sessions]]` list.
- [ ] A `[[groups.sessions]]` TOML section accepts: `name`, `on_connect`, `on_connect_prompt_pattern`, `snippets`, `connect_timeout_secs`, `ssh_options`, and optional overrides for `user`, `identity_file`, `proxy_jump`.
- [ ] Field resolution follows the chain: Session → Group → Profile → Settings default → Hardcoded default.
- [ ] TOML serialization/deserialization round-trips losslessly for configs containing groups.
- [ ] Config load rejects duplicate Group names (hard error).
- [ ] Config load rejects duplicate Session names within the same Group (hard error).
- [ ] Config load allows Session names to be duplicated across different Groups.
- [ ] Config load warns (soft) if a Group references a non-existent Profile.
- [ ] `on_connect` validation (no escape sequences, max 1024 bytes) applies to both Group-level and Session-level values.
- [ ] Existing configs without any `[[groups]]` sections load without errors.
- [ ] Existing `[[bookmarks]]` entries continue to work alongside `[[groups]]`.
- [ ] The TUI list view renders Groups as collapsible headers with Sessions nested underneath.
- [ ] Selecting a Session in the TUI initiates an SSH connection using the resolved effective settings.
- [ ] The TUI right pane shows the active SSH terminal for the connected Session.
- [ ] Connection failure for one Session does not prevent connecting to another Session in the same Group.
- [ ] Session display in the TUI uses the format "group-name / session-name" when there's a naming collision across groups.

## Implementation Decisions

- **Group as a new top-level config key**: `AppConfig` gains a `groups: Vec<BookmarkGroup>` field alongside the existing `profiles` and `bookmarks`. This keeps backward compatibility — existing configs with no `groups` field deserialize fine via serde defaults.
- **Session inherits from Group, Group inherits from Profile**: The resolution chain is five layers (Session → Group → Profile → Settings → hardcoded). This mirrors the existing Bookmark → Profile → Settings pattern and keeps the mental model consistent.
- **Session is not a Bookmark**: Sessions live inside Groups and are not stored in the `bookmarks` array. They are resolved at connect time into effective connection parameters. This avoids duplication and keeps the config DRY.
- **Split-pane TUI via ratatui Layout**: The existing single-pane list is replaced with a two-column layout. Left column (30-40% width) shows the group/session tree. Right column shows the active terminal. The split is added to the existing `list.rs` view.
- **Group `on_connect` is optional base command**: If a Group defines `on_connect`, it serves as the default for all Sessions that don't override it. This supports the case where all sessions share a common prefix (e.g. `cd /projects &&`).
- **No automatic tmux management**: sshore runs whatever `on_connect` command is specified. It does not inspect, create, or destroy tmux sessions. The command is the user's responsibility.

## Testing Decisions

- **Model tests** in `config/model.rs`: serde roundtrip for `BookmarkGroup` and `Session`, resolution chain for every inheritable field (Session overrides Group, Group falls through to Profile, etc.), validation (duplicate names, on_connect length/escape sequences).
- **Config load tests** in `config/mod.rs`: load TOML with groups, verify validation errors (duplicate group name, duplicate session name within group, dangling profile ref), verify soft warnings, verify backward compatibility (no groups = no error).
- **TUI tests**: verify split-pane rendering, group collapse/expand state, session selection triggers correct connection params. Use existing ratatui test infrastructure.
- **NOT tested**: SSH connection lifecycle (covered by existing tests), tmux session behaviour (outside sshore's control), keychain integration (unchanged).

## Out of Scope

- Nested groups (groups within groups)
- Automatic tmux session creation/destruction
- Per-session auth credentials (auth comes from Group or Profile)
- Import/export of groups (existing import/export covers bookmarks; groups can be added later)
- Session-to-session communication or shared state
- Mobile or non-TUI interfaces

## Open Decisions

- Should Groups support `proxy_jump` at the group level, or is that always a Profile concern? (Currently included in Group fields for flexibility.)
- Should the TUI left pane width be configurable? (Default ~30%, no config option for v1.)

## Modules Affected

- **`src/config/model.rs`**: New `BookmarkGroup` and `Session` structs; resolution methods on `Session` mirroring `Bookmark::effective_*` methods; validation helpers
- **`src/config/mod.rs`**: Load-time validation for groups (unique names, on_connect validation, dangling profile warnings); `locked_modify` compatibility
- **`src/config/writer.rs`**: Serialize `groups` array in `AppConfig` (automatic via serde, but verify atomic write handles it)
- **`src/tui/views/list.rs`**: Split-pane layout; group/session tree rendering; selection handling
- **`src/tui/mod.rs`**: State for active session; session lifecycle management (connect, disconnect, switch)
- **`src/ssh/client.rs`**: Support for managing the SSH connection for a selected session (existing code may already support this; verify)
