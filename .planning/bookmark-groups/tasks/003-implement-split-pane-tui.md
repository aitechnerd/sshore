---
feature: bookmark-groups
status: pending
---

# 003: Implement split-pane TUI for groups and sessions

## Goal
Replace the single-pane bookmark list with a split-pane layout showing groups/sessions on the left and the active terminal on the right.

## Scope
**Does:** Split the list view into two panes (left ~30% for group/session tree, right ~70% for terminal). Render groups as collapsible headers with sessions nested underneath. Handle keyboard navigation (up/down through sessions, enter to connect, toggle group collapse). Show session display name as "group-name / session-name" when needed.
**Does not:** SSH connection logic (Task 004), model definitions (Task 001)

## Acceptance Criteria
- [ ] TUI renders two panes: left (group/session list) and right (terminal area)
- [ ] Groups are displayed as collapsible headers
- [ ] Sessions are displayed indented under their parent group
- [ ] Keyboard navigation moves through sessions (skipping group headers)
- [ ] Pressing Enter on a session triggers connection (placeholder for now, wired in Task 004)
- [ ] Group collapse/expand toggles visibility of child sessions
- [ ] Empty state: when no groups exist, existing bookmarks still render in the left pane
- [ ] Session display shows env badge and label (inherited from group)

## Spec Reference
From `SPEC.md`:
- User stories: 4, 6
- Acceptance criteria: TUI split-pane, group/session tree, selection handling
- Implementation decisions: split-pane via ratatui Layout

## Notes
Use ratatui's `Layout` direction `Horizontal` with constrained ratios. The existing `list.rs` view is the primary file to modify. Consider keeping the existing bookmark list as a fallback when no groups are defined.
