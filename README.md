<p align="center">
  <img src="assets/logo.png" alt="sshore logo" width="160" />
</p>

<h1 align="center">sshore</h1>

<p align="center">
  <strong>SSH connections, file transfers, tunnels, and team config — all in one place. Your data stays on your machine.</strong>
</p>

<p align="center">
  <a href="https://github.com/aitechnerd/sshore/actions"><img src="https://img.shields.io/github/actions/workflow/status/aitechnerd/sshore/ci.yml?branch=master&style=flat-square&logo=github&label=CI" alt="CI"></a>
  <a href="https://github.com/aitechnerd/sshore/releases/latest"><img src="https://img.shields.io/github/v/release/aitechnerd/sshore?style=flat-square&color=%23f97316" alt="Release"></a>
  <a href="https://github.com/aitechnerd/sshore/blob/main/LICENSE"><img src="https://img.shields.io/github/license/aitechnerd/sshore?style=flat-square" alt="License: MIT"></a>
  <a href="https://crates.io/crates/sshore"><img src="https://img.shields.io/crates/v/sshore?style=flat-square&logo=rust" alt="Crates.io"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-blue?style=flat-square" alt="Platforms">
</p>

<p align="center">
  <a href="#-quick-start">Quick Start</a> •
  <a href="#-installation">Installation</a> •
  <a href="#-why-sshore">Why sshore?</a> •
  <a href="#-features">Features</a> •
  <a href="#%EF%B8%8F-configuration">Configuration</a> •
  <a href="#-security">Security</a> •
  <a href="#-contributing">Contributing</a>
</p>

---

<!--
<p align="center">
  <img src="assets/demo.gif" alt="sshore demo" width="720" />
</p>
-->

sshore is a terminal-native SSH connection manager built in Rust. Every bookmark has an environment tier — production, staging, development — that drives color-coded safety cues through the entire experience: list badges, terminal tab colors, delete confirmations, transfer progress theming. When you connect to a production server, your terminal tab turns red. When you try to delete a production bookmark, you type "yes" — not just Enter.

It does native SSH (no shell-out to `ssh`), built-in SFTP with a dual-pane file browser, persistent tunnels with auto-reconnect, sudo password assist from your OS keychain, per-host command snippets, and config export for team sharing. All in a compact Rust binary that starts instantly, works offline, and never phones home.

## 🚀 Quick Start

```bash
# Install (macOS)
brew tap aitechnerd/sshore && brew install sshore

# Import your existing SSH config
sshore import

# Launch the TUI — fuzzy search, connect, done
sshore
```

That's it. Your `~/.ssh/config` hosts are imported with auto-detected environments, color-coded and ready. No accounts, no cloud, no configuration required.

```bash
# Or connect directly
sshore prod-web-01

# Transfer files
sshore scp prod-web-01:/var/log/app.log ~/Downloads/

# Browse remote files (dual-pane, mc-style)
sshore browse prod-web-01

# Start a persistent tunnel
sshore tunnel start prod-db -L 5432:localhost:5432

# Run a command across all staging servers
sshore exec --env staging -- "df -h"
```

## 📦 Installation

### macOS

```bash
# Homebrew (recommended)
brew tap aitechnerd/sshore
brew install sshore

# Or download the binary
# Apple Silicon:
curl -L https://github.com/aitechnerd/sshore/releases/latest/download/sshore-aarch64-apple-darwin.tar.gz | tar xz
sudo mv sshore-aarch64-apple-darwin/sshore /usr/local/bin/

# Intel:
curl -L https://github.com/aitechnerd/sshore/releases/latest/download/sshore-x86_64-apple-darwin.tar.gz | tar xz
sudo mv sshore-x86_64-apple-darwin/sshore /usr/local/bin/
```

### Linux

```bash
# Debian / Ubuntu
curl -LO https://github.com/aitechnerd/sshore/releases/latest/download/sshore-x86_64-unknown-linux-gnu.tar.gz
tar xzf sshore-x86_64-unknown-linux-gnu.tar.gz
sudo mv sshore-x86_64-unknown-linux-gnu/sshore /usr/local/bin/

# ARM64 (Raspberry Pi, AWS Graviton)
curl -LO https://github.com/aitechnerd/sshore/releases/latest/download/sshore-aarch64-unknown-linux-gnu.tar.gz
tar xzf sshore-aarch64-unknown-linux-gnu.tar.gz
sudo mv sshore-aarch64-unknown-linux-gnu/sshore /usr/local/bin/

# Arch Linux (AUR)
yay -S sshore-bin
```

### From Source (any platform)

Requires [Rust 1.75+](https://rustup.rs):

```bash
cargo install --git https://github.com/aitechnerd/sshore
```

### Shell Completions

```bash
# Generate and install completions
sshore completions bash > ~/.local/share/bash-completion/completions/sshore
sshore completions zsh > ~/.zfunc/_sshore
sshore completions fish > ~/.config/fish/completions/sshore.fish
```

## 💡 Why sshore?

**Environment safety is built in, not bolted on.** Every bookmark has an environment tier. Every screen shows it. Production is red, always. You can't accidentally `rm -rf` on prod without seeing a wall of red and typing "yes" to confirm. This isn't a feature — it's a philosophy.

**Five seconds to organized.** `sshore import` reads your `~/.ssh/config` — or your PuTTY sessions, MobaXterm bookmarks, Tabby profiles, SecureCRT exports, even a CSV — and auto-classifies hosts by environment based on hostname patterns. From install to fully organized, color-coded bookmarks in a single command.

**A single, compact binary.** Native Rust with `russh` for SSH, `ratatui` for TUI, and zero runtime dependencies. Starts in milliseconds, uses negligible memory, runs on macOS, Linux, and Windows.

**Your data is yours, in an open format.** Your config is a single TOML file — `cat` it, `git` it, `diff` it, share it. Export your bookmarks anytime with `sshore export`. No proprietary formats, no lock-in, no account required to access your own data. Passwords live in your OS keychain, never in the config file.

**Fully offline.** sshore never makes a network call unless you explicitly connect, transfer, or tunnel. No update checks, no telemetry, no DNS resolution at startup. Your infrastructure stays yours.

## ✨ Features

### SSH Connection Manager

- **Interactive TUI** with fuzzy search, tag filtering, and environment grouping — handles hundreds of bookmarks
- **Bookmark CRUD** with inline validation, auto-detected environments, and multi-source import (SSH config, PuTTY, MobaXterm, Tabby, SecureCRT, CSV)
- **Direct connect** by bookmark name: `sshore prod-web-01` (no TUI needed)
- **Ad-hoc connect** to any host: `sshore connect user@10.0.1.50` — save as bookmark later with `~b`
- **ProxyJump / bastion host** support, including chained jumps
- **Shell completions** for bash, zsh, and fish

### Environment-Aware Safety

| Tier | Badge | Terminal Tab | Delete | 
|------|-------|-------------|--------|
| Production | 🔴 PROD | Red | Type "yes" |
| Staging | 🟡 STG | Yellow | Enter |
| Development | 🟢 DEV | Green | Enter |
| Local | 🔵 LOCAL | Blue | Enter |
| Testing | 🟣 TEST | Purple | Enter |

- Terminal tab title and color change on connect (iTerm2, WezTerm, others)
- Environment badges visible everywhere: list, connect banner, SFTP, file browser
- Custom tiers with your own colors, badges, and labels

### File Transfers & Browsing

- **`sshore scp`** — upload/download with progress bars and environment theming
- **`sshore browse`** — dual-pane TUI file browser (local ↔ remote), inspired by Midnight Commander
  - Glob filtering (`/` key → `*.log`), recursive search (`f` key)
  - Edit remote files in `$EDITOR` — download, edit, auto-upload on save
  - Multi-select and batch copy/move/delete
  - Production delete safety carries through to file operations
- **Resumable downloads** — `sshore scp --resume` picks up where a failed transfer left off
- **Isolated SFTP channels** — file transfer errors never kill your SSH session

### Tunnels

- **`sshore tunnel start`** — local and remote port forwarding
- **`sshore tunnel start --persist`** — daemonized tunnels with auto-reconnect on disconnect
- **`sshore tunnel status`** / **`sshore tunnel stop`** — manage running tunnels

### Sudo Password Assist

When you `sudo` on a remote server, sshore detects the password prompt and offers to inject your password from the OS keychain. No clipboard, no typing, no plaintext exposure.

### Snippets & Quick Exec

- **Per-bookmark snippets** — frequently-used commands stored with each host, triggered by `~~` during a session
- **`sshore exec`** — run a command on one or many hosts without an interactive session
  ```bash
  sshore exec --env production -- uptime
  sshore exec --tag web -- "systemctl status nginx"
  ```

### Config Export & Team Sharing

```bash
# Export production bookmarks (passwords are NEVER included)
sshore export --env production -o prod-servers.toml

# Import on another machine
sshore import --file prod-servers.toml

# Use a synced config location (Git, Dropbox, iCloud)
export SSHORE_CONFIG=~/dotfiles/sshore/config.toml
```

### Bring Your Bookmarks

Switching tools should take seconds, not hours. sshore imports from wherever your bookmarks live today:

```bash
sshore import                                        # Interactive wizard — pick your source
sshore import --from putty sessions.reg              # PuTTY registry export
sshore import --from mobaxterm sessions.mxtsessions  # MobaXterm sessions
sshore import --from tabby config.yaml               # Tabby profiles
sshore import --from securecrt export.xml            # SecureCRT XML export
sshore import --from csv hosts.csv                   # CSV (bulk provisioning, Ansible inventories)
sshore import --from json hosts.json                 # JSON
```

Every import auto-detects environment tiers from hostname patterns and folder names, shows a preview of what will be created, and lets you resolve conflicts before committing. Add `--dry-run` to preview without saving.

The `~/.ssh/config` importer handles `Include` directives, `ProxyJump` chains, `IdentityFile` with `~` and `$VAR` expansion, and 16 directives total. The PuTTY importer reads `.reg` registry exports. The MobaXterm importer reads `.mxtsessions` INI files. Tabby's jump host UUIDs are resolved to real hostnames automatically.

sshore never scans your filesystem or registry looking for other tools — you choose what to import and provide the file.

### Host Key Verification

- Checks `~/.ssh/known_hosts` on every connection (OpenSSH-compatible)
- Unknown hosts prompt for fingerprint confirmation before connecting
- Changed host keys trigger a clear `WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!` banner

## ⚙️ Configuration

Config lives at `~/.config/sshore/config.toml` (XDG-compliant on Linux/macOS). Created automatically on first run. Override with `--config <path>` or `SSHORE_CONFIG` env var.

<details>
<summary><strong>Example config</strong></summary>

```toml
[settings]
default_user = "deploy"
sort_by_name = true
show_env_column = true
tab_title_template = "{badge} {label} — {name}"
theme = "tokyo-night"
snippet_trigger = "~~"
connect_timeout_secs = 15

[settings.env_colors.production]
fg = "#FFFFFF"
bg = "#CC0000"
badge = "🔴"
label = "PROD"

[settings.env_colors.staging]
fg = "#000000"
bg = "#CCCC00"
badge = "🟡"
label = "STG"

[[bookmarks]]
name = "prod-web-01"
host = "10.0.1.5"
user = "deploy"
port = 22
env = "production"
tags = ["web", "frontend"]
identity_file = "~/.ssh/id_ed25519"
proxy_jump = "bastion"
notes = "Primary web server — Nginx reverse proxy"
on_connect = "cd /var/www/app && exec $SHELL"

[[bookmarks.snippets]]
name = "Tail app logs"
command = "tail -f /var/log/app/current.log"

[[bookmarks.snippets]]
name = "Disk usage"
command = "df -h && du -sh /var/www/app/*"
```

</details>

<details>
<summary><strong>Settings reference</strong></summary>

| Field | Default | Description |
|-------|---------|-------------|
| `default_user` | OS username | Fallback SSH user when not set per-bookmark |
| `sort_by_name` | `true` | Sort bookmarks alphabetically |
| `show_env_column` | `true` | Show environment column in TUI |
| `tab_title_template` | `"{badge} {label} — {name}"` | Terminal tab title. Placeholders: `{name}`, `{host}`, `{user}`, `{env}`, `{badge}`, `{label}` |
| `theme` | `"tokyo-night"` | TUI theme. Options: `tokyo-night`, `catppuccin-mocha`, `dracula`, `default` |
| `snippet_trigger` | `"~~"` | Escape sequence to open snippet picker during SSH |
| `connect_timeout_secs` | `15` | Connection timeout in seconds |
| `host_key_checking` | `"strict"` | Host key policy: `strict`, `accept-new`, `off` |
| `env_colors` | 5 built-in tiers | Custom environment definitions |

</details>

<details>
<summary><strong>Bookmark reference</strong></summary>

| Field | Default | Description |
|-------|---------|-------------|
| `name` | (required) | Unique display name |
| `host` | (required) | Hostname or IP address |
| `user` | settings default | SSH username |
| `port` | `22` | SSH port |
| `env` | auto-detected | Environment tier |
| `tags` | `[]` | Searchable tags |
| `identity_file` | — | SSH private key path (`~` and `$VAR` expansion) |
| `proxy_jump` | — | Bastion host for ProxyJump |
| `notes` | — | Free-form notes |
| `on_connect` | — | Command to run after SSH shell is ready |
| `snippets` | `[]` | Per-host command snippets |
| `connect_timeout_secs` | — | Per-host timeout override |

</details>

### Terminal Compatibility

Tab title and tab color theming uses OSC escape sequences:

| Terminal | Tab Title | Tab Color |
|----------|-----------|-----------|
| iTerm2 | ✅ | ✅ |
| WezTerm | ✅ | ✅ |
| Terminal.app | ✅ | — |
| Kitty | ✅ | — |
| Alacritty | ✅ | — |
| Windows Terminal | ✅ | — |
| GNOME Terminal | ✅ | — |

Unsupported escape codes are silently ignored — theming degrades gracefully.

## 🔒 Security

sshore is designed with the assumption that your SSH infrastructure is critical.

**Passwords never touch disk.** Passwords are stored exclusively in your OS keychain (macOS Keychain, GNOME Keyring / KWallet, Windows Credential Manager) via the [`keyring`](https://crates.io/crates/keyring) crate. They are never written to the config file, log files, command history, or stdout. The `sshore export` command will never include passwords — not even hashed or encrypted.

**Host keys are verified.** Every connection checks `~/.ssh/known_hosts`. Changed host keys produce the same unmistakable warning banner you know from OpenSSH. Configurable per-host (`strict`, `accept-new`, `off`).

**No network calls unless you say so.** sshore never contacts any server at startup, during config load, or during TUI rendering. No update checks, no telemetry, no analytics. Every network call is a direct result of you connecting, transferring, or tunneling.

**Config file integrity.** All writes use atomic tempfile-then-rename. A crash or power loss mid-write cannot corrupt your config. File permissions are set to `0600` (owner read/write only) on Unix.

**Input validation.** Hostnames are validated to reject shell metacharacters (`;`, `|`, `&`, `$`, `` ` ``). Bookmark names accept only alphanumeric characters, hyphens, underscores, and dots.

**Transparent and auditable.** sshore is MIT-licensed with a single `cargo install` build path. No bundled installers, no proprietary blobs, no download wrappers. You can read every line of code that touches your SSH keys and credentials.

### Reporting Vulnerabilities

If you discover a security vulnerability, please email **security@aitechnerd.com** (or [open a private security advisory](https://github.com/aitechnerd/sshore/security/advisories/new) on GitHub). Do not open a public issue for security vulnerabilities. We aim to acknowledge reports within 48 hours and provide a fix within 7 days for critical issues.

## 🤝 Contributing

Contributions are welcome — whether it's a bug report, feature request, documentation improvement, or code.

### Getting Started

```bash
git clone https://github.com/aitechnerd/sshore
cd sshore
cargo build
cargo test
```

### Before Submitting a PR

```bash
cargo fmt -- --check          # Code formatting
cargo clippy -- -D warnings   # Linting
cargo test                     # All tests pass
```

### Ways to Contribute

- **Bug reports** — [Open an issue](https://github.com/aitechnerd/sshore/issues/new?template=bug_report.md) with steps to reproduce, expected behavior, and your OS/terminal
- **Feature requests** — [Open an issue](https://github.com/aitechnerd/sshore/issues/new?template=feature_request.md) describing the use case and your current workaround
- **Pull requests** — Fork, create a feature branch, make your changes, and open a PR. Include tests where applicable.
- **Documentation** — README improvements, usage examples, terminal compatibility reports
- **Feedback** — Star the repo if sshore is useful to you. It helps others discover the project.

### Architecture

sshore is structured around a clear phase-based architecture documented in [`CLAUDE.md`](CLAUDE.md). Key modules:

| Module | Purpose |
|--------|---------|
| `src/config/` | TOML config, ssh_config import, bookmark model |
| `src/tui/` | ratatui-based terminal UI (list, forms, file browser) |
| `src/ssh/` | russh connection, proxy loop, escape handling, known_hosts |
| `src/sftp/` | SFTP session, transfers, resume logic |
| `src/tunnel/` | Port forwarding, daemon mode, auto-reconnect |
| `src/storage/` | StorageBackend trait (SFTP, local — future: S3, cloud) |

### Roadmap

sshore is under active development. Current priorities:

- [ ] Team sharing — export a filtered set of bookmarks for teammates (by environment, tag, or glob)
- [ ] S3 / cloud storage browsing via `StorageBackend` trait + OpenDAL
- [ ] Interactive import wizard with preview and conflict resolution TUI
- [ ] Daily testing and stability hardening
- [ ] Demo GIF and terminal recordings
- [x] Homebrew tap and binary releases for all platforms

See the full plan in [CLAUDE.md](CLAUDE.md) and [CLAUDE.local.md](CLAUDE.local.md).

## 📄 License

MIT — see [LICENSE](LICENSE) for details.

---

<p align="center">
  <sub>Built with 🦀 Rust · Native SSH via <a href="https://github.com/Eugeny/russh">russh</a> · TUI via <a href="https://github.com/ratatui/ratatui">ratatui</a></sub>
</p>
