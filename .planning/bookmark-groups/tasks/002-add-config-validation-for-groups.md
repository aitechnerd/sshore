---
feature: bookmark-groups
status: pending
---

# 002: Add config validation for groups

## Goal
Add load-time validation for Bookmark Groups in `config/mod.rs`.

## Scope
**Does:** Validate unique group names (hard error), validate unique session names within each group (hard error), validate on_connect fields (escape sequences, max length) for both groups and sessions, warn on dangling profile references from groups, allow session name collisions across different groups.
**Does not:** Model struct definitions (Task 001), TUI changes

## Acceptance Criteria
- [ ] Config load rejects duplicate group names with a clear error message
- [ ] Config load rejects duplicate session names within the same group with a clear error message
- [ ] Config load allows session names to be duplicated across different groups
- [ ] Config load warns (to stderr) if a group references a non-existent profile
- [ ] `on_connect` validation applies to both group-level and session-level values (escape sequences, max 1024 bytes)
- [ ] Config load succeeds when no groups are present (backward compatibility)
- [ ] Existing bookmarks continue to load alongside groups

## Spec Reference
From `SPEC.md`:
- User stories: 7, 8, 9, 10
- Acceptance criteria: duplicate name rejection, soft warnings, backward compatibility
- Implementation decisions: validation mirrors existing profile/bookmark patterns

## Notes
Follow the existing `validate_profiles()` and `validate_on_connect_fields()` patterns. Add a new `validate_groups()` function called from `load_from()`.
