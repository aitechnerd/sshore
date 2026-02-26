use clap::{Parser, Subcommand, ValueEnum};

/// Terminal-native SSH connection manager with environment-aware safety.
#[derive(Parser, Debug)]
#[command(name = "sshore", version, about)]
pub struct Cli {
    /// Path to config file (default: ~/.config/sshore/config.toml).
    /// Also settable via SSHORE_CONFIG env var.
    #[arg(long, global = true, env = "SSHORE_CONFIG")]
    pub config: Option<String>,

    /// Connect directly to a bookmark by name (skip TUI).
    ///
    /// Note: bookmark names that collide with subcommand names (e.g., "import")
    /// will be parsed as subcommands.
    #[arg(value_name = "BOOKMARK")]
    pub connect: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Import bookmarks from various sources.
    Import {
        /// Import source format.
        /// If not specified, defaults to ssh_config (or sshore TOML auto-detect).
        #[arg(long, value_enum)]
        from: Option<ImportSource>,

        /// Path to the source file.
        /// For ssh_config: defaults to ~/.ssh/config if not specified.
        /// For all other sources: required.
        #[arg(short, long, value_name = "FILE")]
        file: Option<String>,

        /// Overwrite existing bookmarks with same name.
        #[arg(long)]
        overwrite: bool,

        /// Override environment for all imported bookmarks.
        #[arg(long)]
        env: Option<String>,

        /// Add tag(s) to all imported bookmarks.
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,

        /// Show what would be imported without writing config.
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage stored passwords in OS keychain.
    Password {
        #[command(subcommand)]
        action: PasswordAction,
    },

    /// Connect to a host directly (without a bookmark).
    Connect {
        /// Connection string: [user@]host[:port]
        target: String,
    },

    /// Open SFTP session to a bookmark.
    Sftp {
        /// Bookmark name.
        bookmark: String,
    },

    /// Copy files to/from a bookmark (SCP-style).
    Scp {
        /// Source path (bookmark:path or local path).
        source: String,
        /// Destination path (bookmark:path or local path).
        destination: String,
        /// Resume a partially downloaded file instead of starting over.
        #[arg(long)]
        resume: bool,
    },

    /// Open dual-pane file browser to a bookmark.
    Browse {
        /// Bookmark name, optionally with remote path (e.g. "prod-web-01:/var/log").
        target: String,

        /// Local starting directory (default: current directory).
        #[arg(short, long)]
        local: Option<String>,

        /// Show hidden files.
        #[arg(short = 'a', long)]
        show_hidden: bool,
    },

    /// Manage persistent SSH tunnels.
    Tunnel {
        #[command(subcommand)]
        action: TunnelAction,
    },

    /// List all bookmarks (non-interactive).
    List {
        /// Filter by environment.
        #[arg(short, long)]
        env: Option<String>,

        /// Output format.
        #[arg(short, long, default_value = "table")]
        format: String,
    },

    /// Generate shell completions.
    Completions {
        /// Shell to generate for.
        shell: clap_complete::Shell,
    },

    /// Execute a command on one or more bookmarks without interactive session.
    Exec {
        /// Bookmark name (for single-host exec).
        #[arg(value_name = "BOOKMARK")]
        bookmark: Option<String>,

        /// Command to execute on the remote host(s).
        #[arg(last = true)]
        command: Vec<String>,

        /// Filter by tag (can be specified multiple times, AND logic).
        #[arg(short, long)]
        tag: Vec<String>,

        /// Filter by environment.
        #[arg(short, long)]
        env: Option<String>,

        /// Maximum concurrent SSH connections for multi-host exec.
        #[arg(long, default_value = "10")]
        concurrency: usize,
    },

    /// Export bookmarks to a portable TOML file.
    Export {
        /// Filter by environment.
        #[arg(short, long)]
        env: Option<String>,

        /// Filter by tag (can be specified multiple times, AND logic).
        #[arg(short, long)]
        tag: Vec<String>,

        /// Filter by name pattern (glob-style: "prod-*").
        #[arg(short, long)]
        name: Option<String>,

        /// Output file path (default: stdout).
        #[arg(short, long)]
        output: Option<String>,

        /// Include settings (env_colors, global snippets) in export.
        #[arg(long)]
        include_settings: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PasswordAction {
    /// Store a password for a bookmark.
    Set {
        /// Bookmark name.
        bookmark: String,
    },
    /// Remove a stored password.
    Remove {
        /// Bookmark name.
        bookmark: String,
    },
    /// List bookmarks with stored passwords.
    List,
}

#[derive(Subcommand, Debug)]
pub enum TunnelAction {
    /// Create a persistent tunnel.
    Start {
        /// Bookmark name.
        bookmark: String,

        /// Local port forwarding spec (local:remote_host:remote_port).
        #[arg(short = 'L')]
        local_forward: Vec<String>,

        /// Remote port forwarding spec (remote:local_host:local_port).
        #[arg(short = 'R')]
        remote_forward: Vec<String>,

        /// Keep tunnel alive across disconnects.
        #[arg(long)]
        persist: bool,

        /// Internal: run as daemon process (used by --persist re-exec).
        #[arg(long, hide = true)]
        daemon: bool,
    },

    /// Stop a tunnel.
    Stop {
        /// Bookmark name.
        bookmark: String,
    },

    /// Show active tunnels.
    Status,
}

/// Source format for import.
#[derive(Clone, Debug, ValueEnum)]
pub enum ImportSource {
    /// OpenSSH config (~/.ssh/config)
    SshConfig,
    /// PuTTY registry export (.reg file)
    Putty,
    /// MobaXterm session export (.mxtsessions file)
    Mobaxterm,
    /// Tabby terminal config (config.yaml)
    Tabby,
    /// SecureCRT XML export
    Securecrt,
    /// CSV file (name,host,user,port,env columns)
    Csv,
    /// JSON file (array of bookmark objects)
    Json,
    /// sshore TOML config (from another machine / team share)
    Sshore,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn test_parse_no_args() {
        let cli = Cli::try_parse_from(["sshore"]).unwrap();
        assert!(cli.connect.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_parse_connect_arg() {
        let cli = Cli::try_parse_from(["sshore", "prod-web-01"]).unwrap();
        assert_eq!(cli.connect, Some("prod-web-01".into()));
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_parse_import_subcommand() {
        let cli = Cli::try_parse_from(["sshore", "import"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Import {
                from: None,
                file: None,
                overwrite: false,
                ..
            })
        ));
    }

    #[test]
    fn test_parse_import_with_file() {
        let cli = Cli::try_parse_from(["sshore", "import", "--file", "/path/to/config"]).unwrap();
        match cli.command {
            Some(Commands::Import {
                file, overwrite, ..
            }) => {
                assert_eq!(file, Some("/path/to/config".into()));
                assert!(!overwrite);
            }
            _ => panic!("Expected Import command"),
        }
    }

    #[test]
    fn test_parse_import_with_overwrite() {
        let cli = Cli::try_parse_from(["sshore", "import", "--overwrite"]).unwrap();
        match cli.command {
            Some(Commands::Import {
                overwrite,
                env,
                dry_run,
                ..
            }) => {
                assert!(overwrite);
                assert!(env.is_none());
                assert!(!dry_run);
            }
            _ => panic!("Expected Import command"),
        }
    }

    #[test]
    fn test_parse_import_from_putty() {
        let cli = Cli::try_parse_from([
            "sshore",
            "import",
            "--from",
            "putty",
            "--file",
            "sessions.reg",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Import { from, file, .. }) => {
                assert!(matches!(from, Some(ImportSource::Putty)));
                assert_eq!(file, Some("sessions.reg".into()));
            }
            _ => panic!("Expected Import command"),
        }
    }

    #[test]
    fn test_parse_import_from_csv_with_env_and_tag() {
        let cli = Cli::try_parse_from([
            "sshore",
            "import",
            "--from",
            "csv",
            "--file",
            "hosts.csv",
            "--env",
            "staging",
            "--tag",
            "web,legacy",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Import {
                from,
                file,
                env,
                tag,
                ..
            }) => {
                assert!(matches!(from, Some(ImportSource::Csv)));
                assert_eq!(file, Some("hosts.csv".into()));
                assert_eq!(env, Some("staging".into()));
                assert_eq!(tag, vec!["web", "legacy"]);
            }
            _ => panic!("Expected Import command"),
        }
    }

    #[test]
    fn test_parse_import_backward_compat_file_only() {
        // --file without --from should still work (backward compat)
        let cli = Cli::try_parse_from(["sshore", "import", "--file", "~/.ssh/config"]).unwrap();
        match cli.command {
            Some(Commands::Import { from, file, .. }) => {
                assert!(from.is_none());
                assert_eq!(file, Some("~/.ssh/config".into()));
            }
            _ => panic!("Expected Import command"),
        }
    }

    #[test]
    fn test_parse_list_subcommand() {
        let cli = Cli::try_parse_from(["sshore", "list"]).unwrap();
        match cli.command {
            Some(Commands::List { env, format }) => {
                assert!(env.is_none());
                assert_eq!(format, "table");
            }
            _ => panic!("Expected List command"),
        }
    }

    #[test]
    fn test_parse_list_with_env_filter() {
        let cli = Cli::try_parse_from(["sshore", "list", "--env", "production"]).unwrap();
        match cli.command {
            Some(Commands::List { env, .. }) => {
                assert_eq!(env, Some("production".into()));
            }
            _ => panic!("Expected List command"),
        }
    }

    #[test]
    fn test_parse_completions() {
        let cli = Cli::try_parse_from(["sshore", "completions", "bash"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Completions { .. })));
    }

    #[test]
    fn test_parse_sftp() {
        let cli = Cli::try_parse_from(["sshore", "sftp", "myhost"]).unwrap();
        match cli.command {
            Some(Commands::Sftp { bookmark }) => {
                assert_eq!(bookmark, "myhost");
            }
            _ => panic!("Expected Sftp command"),
        }
    }

    #[test]
    fn test_parse_scp() {
        let cli =
            Cli::try_parse_from(["sshore", "scp", "myhost:/tmp/file", "/local/path"]).unwrap();
        match cli.command {
            Some(Commands::Scp {
                source,
                destination,
                resume,
            }) => {
                assert_eq!(source, "myhost:/tmp/file");
                assert_eq!(destination, "/local/path");
                assert!(!resume);
            }
            _ => panic!("Expected Scp command"),
        }
    }

    #[test]
    fn test_parse_scp_with_resume() {
        let cli = Cli::try_parse_from([
            "sshore",
            "scp",
            "myhost:/tmp/file",
            "/local/path",
            "--resume",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Scp { resume, .. }) => {
                assert!(resume);
            }
            _ => panic!("Expected Scp command"),
        }
    }

    #[test]
    fn test_parse_connect() {
        let cli = Cli::try_parse_from(["sshore", "connect", "user@host:2222"]).unwrap();
        match cli.command {
            Some(Commands::Connect { target }) => {
                assert_eq!(target, "user@host:2222");
            }
            _ => panic!("Expected Connect command"),
        }
    }

    #[test]
    fn test_parse_tunnel_start() {
        let cli = Cli::try_parse_from([
            "sshore",
            "tunnel",
            "start",
            "myhost",
            "-L",
            "5432:localhost:5432",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Tunnel {
                action:
                    TunnelAction::Start {
                        bookmark,
                        local_forward,
                        remote_forward,
                        persist,
                        daemon,
                    },
            }) => {
                assert_eq!(bookmark, "myhost");
                assert_eq!(local_forward, vec!["5432:localhost:5432"]);
                assert!(remote_forward.is_empty());
                assert!(!persist);
                assert!(!daemon);
            }
            _ => panic!("Expected Tunnel Start command"),
        }
    }

    #[test]
    fn test_parse_tunnel_start_persist() {
        let cli = Cli::try_parse_from([
            "sshore",
            "tunnel",
            "start",
            "myhost",
            "-L",
            "5432:localhost:5432",
            "--persist",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Tunnel {
                action:
                    TunnelAction::Start {
                        persist, daemon, ..
                    },
            }) => {
                assert!(persist);
                assert!(!daemon);
            }
            _ => panic!("Expected Tunnel Start command"),
        }
    }

    #[test]
    fn test_parse_tunnel_start_multiple_forwards() {
        let cli = Cli::try_parse_from([
            "sshore",
            "tunnel",
            "start",
            "myhost",
            "-L",
            "5432:localhost:5432",
            "-L",
            "8080:localhost:80",
            "-R",
            "3000:localhost:3000",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Tunnel {
                action:
                    TunnelAction::Start {
                        local_forward,
                        remote_forward,
                        ..
                    },
            }) => {
                assert_eq!(local_forward.len(), 2);
                assert_eq!(remote_forward.len(), 1);
            }
            _ => panic!("Expected Tunnel Start command"),
        }
    }

    #[test]
    fn test_parse_tunnel_stop() {
        let cli = Cli::try_parse_from(["sshore", "tunnel", "stop", "myhost"]).unwrap();
        match cli.command {
            Some(Commands::Tunnel {
                action: TunnelAction::Stop { bookmark },
            }) => {
                assert_eq!(bookmark, "myhost");
            }
            _ => panic!("Expected Tunnel Stop command"),
        }
    }

    #[test]
    fn test_parse_tunnel_status() {
        let cli = Cli::try_parse_from(["sshore", "tunnel", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Tunnel {
                action: TunnelAction::Status
            })
        ));
    }

    #[test]
    fn test_parse_exec_single_host() {
        let cli = Cli::try_parse_from(["sshore", "exec", "myhost", "--", "uptime"]).unwrap();
        match cli.command {
            Some(Commands::Exec {
                bookmark,
                command,
                tag,
                env,
                concurrency,
            }) => {
                assert_eq!(bookmark, Some("myhost".into()));
                assert_eq!(command, vec!["uptime"]);
                assert!(tag.is_empty());
                assert!(env.is_none());
                assert_eq!(concurrency, 10);
            }
            _ => panic!("Expected Exec command"),
        }
    }

    #[test]
    fn test_parse_exec_multi_host() {
        let cli = Cli::try_parse_from([
            "sshore",
            "exec",
            "--env",
            "production",
            "--tag",
            "web",
            "--concurrency",
            "5",
            "--",
            "df",
            "-h",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Exec {
                bookmark,
                command,
                tag,
                env,
                concurrency,
            }) => {
                assert!(bookmark.is_none());
                assert_eq!(command, vec!["df", "-h"]);
                assert_eq!(tag, vec!["web"]);
                assert_eq!(env, Some("production".into()));
                assert_eq!(concurrency, 5);
            }
            _ => panic!("Expected Exec command"),
        }
    }

    #[test]
    fn test_parse_export() {
        let cli = Cli::try_parse_from([
            "sshore",
            "export",
            "--env",
            "production",
            "--output",
            "servers.toml",
            "--include-settings",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Export {
                env,
                tag,
                name,
                output,
                include_settings,
            }) => {
                assert_eq!(env, Some("production".into()));
                assert!(tag.is_empty());
                assert!(name.is_none());
                assert_eq!(output, Some("servers.toml".into()));
                assert!(include_settings);
            }
            _ => panic!("Expected Export command"),
        }
    }

    #[test]
    fn test_parse_import_with_env_override() {
        let cli = Cli::try_parse_from([
            "sshore",
            "import",
            "--file",
            "servers.toml",
            "--env",
            "staging",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Import {
                file,
                overwrite,
                env,
                dry_run,
                ..
            }) => {
                assert_eq!(file, Some("servers.toml".into()));
                assert!(!overwrite);
                assert_eq!(env, Some("staging".into()));
                assert!(dry_run);
            }
            _ => panic!("Expected Import command"),
        }
    }
}
