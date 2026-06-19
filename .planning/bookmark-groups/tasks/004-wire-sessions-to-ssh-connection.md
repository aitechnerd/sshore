---
feature: bookmark-groups
status: pending
---

# 004: Wire sessions to SSH connection

## Goal
Connect a selected session to an SSH terminal using the resolved effective settings.

## Scope
**Does:** When user selects a session in the TUI, resolve all effective connection parameters (host, user, port, identity_file, proxy_jump, on_connect, etc.) and initiate an SSH connection via the existing `ssh/client.rs` infrastructure. Render the terminal output in the right pane. Handle connection failures per-session (error in right pane, red indicator in left pane). Support switching between sessions (disconnect current, connect new).
**Does not:** Model definitions (Task 001), validation (Task 002), TUI layout (Task 003)

## Acceptance Criteria
- [ ] Selecting a session initiates SSH connection using resolved effective settings
- [ ] Terminal output renders in the right pane
- [ ] Connection failure shows error in right pane without blocking other sessions
- [ ] Switching sessions disconnects the current session and connects the new one
- [ ] `on_connect` command is executed after connection (with existing delay/prompt logic)
- [ ] Session inherits env tier from group for visual safety indicators (badge, colors)

## Spec Reference
From `SPEC.md`:
- User stories: 2, 4
- Acceptance criteria: SSH connection, terminal rendering, per-session failure, switching
- Implementation decisions: session resolves at connect time, five-layer chain

## Notes
Reuse the existing `ssh/client.rs` connection logic. The key change is that the connection params come from a Session's resolved effective values rather than a Bookmark. May need to refactor the connection initiator to accept a generic "connection params" struct rather than a Bookmark directly.
