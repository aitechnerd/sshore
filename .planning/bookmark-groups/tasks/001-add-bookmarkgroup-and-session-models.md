---
feature: bookmark-groups
status: pending
---

# 001: Add BookmarkGroup and Session model structs

## Goal
Define the `BookmarkGroup` and `Session` structs in `config/model.rs` with all inheritable fields and resolution methods.

## Scope
**Does:** Add `BookmarkGroup` struct (host, port, user, identity_file, proxy_jump, env, tags, notes, profile, connect_timeout_secs, ssh_options, on_connect, on_connect_prompt_pattern, snippets, sessions list). Add `Session` struct (name, on_connect, on_connect_prompt_pattern, snippets, connect_timeout_secs, ssh_options, optional overrides for user/identity_file/proxy_jump). Add `Session::effective_*` methods mirroring the five-layer resolution chain (Session → Group → Profile → Settings → hardcoded). Add `AppConfig.groups: Vec<BookmarkGroup>` field.
**Does not:** TUI changes, SSH connection logic, import/export

## Acceptance Criteria
- [ ] `BookmarkGroup` and `Session` structs compile with serde Serialize/Deserialize
- [ ] `Session::effective_user()` resolves: session → group → profile → settings → OS user
- [ ] `Session::effective_identity_file()` resolves: session → group → profile
- [ ] `Session::effective_proxy_jump()` resolves: session → group → profile
- [ ] `Session::effective_on_connect()` resolves: session → group → profile
- [ ] `Session::effective_connect_timeout()` resolves: session → group → profile → settings
- [ ] `Session::effective_ssh_options()` merges: profile → group → session (session wins)
- [ ] `AppConfig` has `groups: Vec<BookmarkGroup>` with serde default
- [ ] Serde roundtrip test passes for a config with groups

## Spec Reference
From `SPEC.md`:
- User stories: 1, 2, 3, 5
- Acceptance criteria: TOML schema, resolution chain
- Implementation decisions: Group as new top-level config key, five-layer resolution

## Notes
Follow the existing `Bookmark::effective_*` patterns exactly. The `profile_field` helper method can be extended or reused.
