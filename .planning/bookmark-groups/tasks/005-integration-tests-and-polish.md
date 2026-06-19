---
feature: bookmark-groups
status: pending
---

# 005: Integration tests and polish

## Goal
End-to-end tests for the full group → session → connection flow, plus config save/load roundtrip.

## Scope
**Does:** Integration test for loading a config with groups, validating it, resolving session effective values, and verifying the resolution chain end-to-end. Test atomic config save with groups included. Test backward compatibility (config with no groups loads fine). Test mixed config (bookmarks + groups coexist).
**Does not:** New model structs (Task 001), new validation logic (Task 002), TUI changes (Task 003-004)

## Acceptance Criteria
- [ ] Full resolution chain test: session overrides group, group falls through to profile, profile falls through to settings
- [ ] Config save/load roundtrip preserves groups with all fields
- [ ] Config with no groups loads without errors (backward compatibility)
- [ ] Config with both bookmarks and groups loads without errors
- [ ] Dangling profile reference from group produces a warning (not an error)
- [ ] Duplicate session names across groups is allowed; within same group is rejected
- [ ] on_connect validation catches escape sequences in both group and session levels

## Spec Reference
From `SPEC.md`:
- User stories: 7, 8, 9, 10
- Acceptance criteria: all validation and compatibility criteria
- Testing decisions: model tests, config load tests

## Notes
This task ties together all previous tasks. Run the full test suite to ensure no regressions in existing bookmark/profile functionality.
