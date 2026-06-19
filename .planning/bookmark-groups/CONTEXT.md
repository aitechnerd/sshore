# Domain Terms — Bookmark Groups

**Bookmark Group**: A named collection that holds shared SSH connection settings (host, port, user, identity file, proxy jump, environment tier) plus an ordered list of Sessions. One group represents one server.

**Session**: A named entry within a Bookmark Group that defines a unique `on_connect` command (e.g. `tmux attach -t project-a`). Sessions inherit all connection settings from their parent Group, and can optionally override individual fields. Each session appears as a distinct entry in the TUI.

**Resolution chain**: Session field → Group field → Profile field → Settings default → Hardcoded default. Five layers of inheritance.

**Split-pane TUI**: The main list view is split into two panes. Left pane shows Groups (collapsible) and their Sessions. Right pane shows the active SSH terminal for the selected session.
