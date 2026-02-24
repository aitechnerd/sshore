use clap::{Parser, Subcommand};

/// Terminal-native SSH connection manager with environment-aware safety.
#[derive(Parser, Debug)]
#[command(name = "sshore", version, about)]
pub struct Cli {
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
    /// Import bookmarks from ~/.ssh/config.
    Import {
        /// Path to ssh config file (default: ~/.ssh/config).
        #[arg(short, long)]
        file: Option<String>,

        /// Overwrite existing bookmarks with same name.
        #[arg(long)]
        overwrite: bool,
    },

    /// Manage stored passwords in OS keychain.
    Password {
        #[command(subcommand)]
        action: PasswordAction,
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
    },

    /// Stop a tunnel.
    Stop {
        /// Bookmark name.
        bookmark: String,
    },

    /// Show active tunnels.
    Status,
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
                file: None,
                overwrite: false
            })
        ));
    }

    #[test]
    fn test_parse_import_with_file() {
        let cli = Cli::try_parse_from(["sshore", "import", "--file", "/path/to/config"]).unwrap();
        match cli.command {
            Some(Commands::Import { file, overwrite }) => {
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
            Some(Commands::Import { overwrite, .. }) => {
                assert!(overwrite);
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
            }) => {
                assert_eq!(source, "myhost:/tmp/file");
                assert_eq!(destination, "/local/path");
            }
            _ => panic!("Expected Scp command"),
        }
    }
}
