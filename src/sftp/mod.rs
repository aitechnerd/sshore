pub mod pipeline;
pub mod shortcuts;

use std::io::{self, BufRead, BufReader, BufWriter, Write};

use anyhow::{Context, Result, bail};
use russh_sftp::client::SftpSession;

use crate::config::model::AppConfig;
use crate::ssh;
use crate::ssh::client::SshoreHandler;
use crate::ssh::terminal_theme;

use self::shortcuts::ProgressBar;

/// Open an interactive SFTP session to a bookmark.
pub async fn open_session(config: &AppConfig, bookmark_index: usize) -> Result<()> {
    let session = ssh::establish_session(config, bookmark_index).await?;

    // Apply terminal theming with SFTP-specific title
    let bookmark = &config.bookmarks[bookmark_index];
    let settings = &config.settings;
    let title = format!(
        "SFTP: {}",
        terminal_theme::render_tab_title(&settings.tab_title_template, bookmark, settings)
    );
    terminal_theme::apply_theme_with_title(bookmark, settings, &title);
    ssh::print_production_banner(bookmark, settings, "SFTP session");
    let is_production = bookmark.env.eq_ignore_ascii_case("production");

    // Open a session channel and request SFTP subsystem
    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open SSH session channel")?;

    channel
        .request_subsystem(true, "sftp")
        .await
        .context("Failed to request SFTP subsystem")?;

    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("Failed to initialize SFTP session")?;

    // Get initial working directory
    let cwd = sftp
        .canonicalize(".")
        .await
        .context("Failed to get remote working directory")?;

    eprintln!("SFTP session opened. Remote directory: {cwd}");
    eprintln!("Type 'help' for available commands.");

    // Run the interactive command loop (pass SSH handle for pipelined transfers)
    let result = run_command_loop(&sftp, &session, cwd, is_production).await;

    // Always reset theme, even on error
    terminal_theme::reset_theme();

    result
}

/// Run the interactive SFTP command loop.
async fn run_command_loop(
    sftp: &SftpSession,
    session: &russh::client::Handle<SshoreHandler>,
    initial_cwd: String,
    is_production: bool,
) -> Result<()> {
    let mut cwd = initial_cwd;
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("sftp> ");
        stdout.flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            // EOF
            break;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (cmd, args) = parse_command(line);

        match cmd {
            "exit" | "quit" => break,
            "help" => print_help(),
            "pwd" => println!("{cwd}"),
            "ls" => {
                let path = if args.is_empty() {
                    cwd.clone()
                } else {
                    resolve_path(&cwd, args)
                };
                if let Err(e) = cmd_ls(sftp, &path).await {
                    eprintln!("ls: {e}");
                }
            }
            "cd" => {
                if args.is_empty() {
                    eprintln!("cd: missing path argument");
                } else {
                    let path = resolve_path(&cwd, args);
                    match sftp.canonicalize(&path).await {
                        Ok(resolved) => cwd = resolved,
                        Err(e) => eprintln!("cd: {e}"),
                    }
                }
            }
            "get" => {
                if args.is_empty() {
                    eprintln!("get: missing remote path argument");
                } else {
                    let (remote, local) = parse_get_put_args(args);
                    let remote = resolve_path(&cwd, remote);
                    let local = local.unwrap_or_else(|| {
                        std::path::Path::new(&remote)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "download".to_string())
                    });
                    if let Err(e) = cmd_get(sftp, session, &remote, &local).await {
                        eprintln!("get: {e}");
                    }
                }
            }
            "put" => {
                if args.is_empty() {
                    eprintln!("put: missing local path argument");
                } else {
                    let (local, remote) = parse_get_put_args(args);
                    let remote = remote.map(|r| resolve_path(&cwd, &r)).unwrap_or_else(|| {
                        let name = std::path::Path::new(local)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "upload".to_string());
                        resolve_path(&cwd, &name)
                    });
                    if let Err(e) = cmd_put(session, local, &remote).await {
                        eprintln!("put: {e}");
                    }
                }
            }
            "mkdir" => {
                if args.is_empty() {
                    eprintln!("mkdir: missing path argument");
                } else {
                    let path = resolve_path(&cwd, args);
                    if let Err(e) = sftp.create_dir(&path).await {
                        eprintln!("mkdir: {e}");
                    }
                }
            }
            "rm" => {
                if args.is_empty() {
                    eprintln!("rm: missing path argument");
                } else {
                    let path = resolve_path(&cwd, args);
                    if is_production && !confirm_production_delete("rm", &path)? {
                        eprintln!("rm: cancelled");
                        continue;
                    }
                    if let Err(e) = sftp.remove_file(&path).await {
                        eprintln!("rm: {e}");
                    }
                }
            }
            "rmdir" => {
                if args.is_empty() {
                    eprintln!("rmdir: missing path argument");
                } else {
                    let path = resolve_path(&cwd, args);
                    if is_production && !confirm_production_delete("rmdir", &path)? {
                        eprintln!("rmdir: cancelled");
                        continue;
                    }
                    if let Err(e) = sftp.remove_dir(&path).await {
                        eprintln!("rmdir: {e}");
                    }
                }
            }
            "chmod" => {
                let parts: Vec<&str> = args.splitn(2, char::is_whitespace).collect();
                if parts.len() < 2 {
                    eprintln!("chmod: usage: chmod <mode> <path>");
                } else if let Err(e) = cmd_chmod(sftp, &cwd, parts[0], parts[1]).await {
                    eprintln!("chmod: {e}");
                }
            }
            "stat" => {
                if args.is_empty() {
                    eprintln!("stat: missing path argument");
                } else {
                    let path = resolve_path(&cwd, args);
                    if let Err(e) = cmd_stat(sftp, &path).await {
                        eprintln!("stat: {e}");
                    }
                }
            }
            other => {
                eprintln!("Unknown command: {other}. Type 'help' for available commands.");
            }
        }
    }

    Ok(())
}

/// Ask for explicit confirmation before destructive actions on production hosts.
fn confirm_production_delete(action: &str, path: &str) -> Result<bool> {
    eprint!("\x1b[1;37;41m PROD \x1b[0m Confirm {action} {path}? Type 'yes' to proceed: ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("yes"))
}

/// Parse a command line into (command, args).
fn parse_command(line: &str) -> (&str, &str) {
    let trimmed = line.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    }
}

/// Resolve a path relative to the current working directory.
/// Absolute paths (starting with `/`) are returned as-is.
fn resolve_path(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if cwd == "/" {
        format!("/{path}")
    } else {
        format!("{cwd}/{path}")
    }
}

/// Parse "first [second]" arguments for get/put commands.
fn parse_get_put_args(args: &str) -> (&str, Option<String>) {
    match args.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, Some(rest.trim().to_string())),
        None => (args, None),
    }
}

/// List directory contents.
async fn cmd_ls(sftp: &SftpSession, path: &str) -> Result<()> {
    let entries = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("Failed to list {path}"))?;

    let mut items: Vec<_> = entries.collect();
    items.sort_by_key(|a| a.file_name());

    for entry in &items {
        let meta = entry.metadata();
        let perms = meta.permissions();
        let size = meta.size.unwrap_or(0);
        let type_char = match entry.file_type() {
            russh_sftp::protocol::FileType::Dir => 'd',
            russh_sftp::protocol::FileType::Symlink => 'l',
            _ => '-',
        };
        let name = entry.file_name();
        println!("{type_char}{perms} {size:>10}  {name}");
    }

    if items.is_empty() {
        println!("(empty directory)");
    }

    Ok(())
}

/// Download a remote file via pipelined SFTP on a dedicated channel.
async fn cmd_get(
    sftp: &SftpSession,
    session: &russh::client::Handle<SshoreHandler>,
    remote: &str,
    local: &str,
) -> Result<()> {
    // Use the existing SftpSession for metadata.
    let meta = sftp
        .metadata(remote)
        .await
        .with_context(|| format!("Failed to stat {remote}"))?;
    let total = meta.size.unwrap_or(0);

    // Open a dedicated channel for the pipelined transfer.
    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open transfer channel")?;
    let session = pipeline::create_raw_session(channel).await?;

    let local_file = std::fs::File::create(local)
        .with_context(|| format!("Failed to create local file {local}"))?;
    let mut local_file = BufWriter::with_capacity((pipeline::CHUNK_SIZE * 2) as usize, local_file);

    let mut progress = ProgressBar::new(total);
    pipeline::download(
        &session.raw,
        remote,
        &mut local_file,
        total,
        0,
        session.read_chunk_size,
        |b| progress.update(b),
        None,
    )
    .await?;

    progress.finish();
    Ok(())
}

/// Upload a local file via pipelined SFTP on a dedicated channel.
async fn cmd_put(
    session: &russh::client::Handle<SshoreHandler>,
    local: &str,
    remote: &str,
) -> Result<()> {
    let local_meta =
        std::fs::metadata(local).with_context(|| format!("Failed to stat local file {local}"))?;

    if !local_meta.is_file() {
        bail!("{local} is not a regular file");
    }

    let total = local_meta.len();

    // Open a dedicated channel for the pipelined transfer.
    let channel = session
        .channel_open_session()
        .await
        .context("Failed to open transfer channel")?;
    let session = pipeline::create_raw_session(channel).await?;

    let local_file =
        std::fs::File::open(local).with_context(|| format!("Failed to open local file {local}"))?;
    let mut local_file = BufReader::with_capacity((pipeline::CHUNK_SIZE * 2) as usize, local_file);

    let mut progress = ProgressBar::new(total);
    pipeline::upload(
        &session.raw,
        remote,
        &mut local_file,
        total,
        |b| progress.update(b),
        None,
    )
    .await?;

    progress.finish();
    Ok(())
}

/// Change permissions on a remote file.
async fn cmd_chmod(sftp: &SftpSession, cwd: &str, mode_str: &str, path: &str) -> Result<()> {
    let mode =
        u32::from_str_radix(mode_str, 8).with_context(|| format!("Invalid mode: {mode_str}"))?;

    let path = resolve_path(cwd, path);
    let mut meta = sftp
        .metadata(&path)
        .await
        .with_context(|| format!("Failed to stat {path}"))?;

    meta.permissions = Some(mode);
    sftp.set_metadata(&path, meta)
        .await
        .with_context(|| format!("Failed to set permissions on {path}"))?;

    Ok(())
}

/// Display file metadata.
async fn cmd_stat(sftp: &SftpSession, path: &str) -> Result<()> {
    let meta = sftp
        .metadata(path)
        .await
        .with_context(|| format!("Failed to stat {path}"))?;

    println!("  Path: {path}");
    println!("  Type: {}", format_file_type(meta.file_type()));
    println!(
        "  Size: {}",
        shortcuts::format_bytes(meta.size.unwrap_or(0))
    );
    println!("  Permissions: {}", meta.permissions());
    if let Some(uid) = meta.uid {
        let user_str = meta.user.as_deref().unwrap_or("?");
        println!("  Owner: {user_str} ({uid})");
    }
    if let Some(gid) = meta.gid {
        let group_str = meta.group.as_deref().unwrap_or("?");
        println!("  Group: {group_str} ({gid})");
    }
    if let Some(mtime) = meta.mtime {
        println!("  Modified: {}", format_timestamp(mtime));
    }
    if let Some(atime) = meta.atime {
        println!("  Accessed: {}", format_timestamp(atime));
    }

    Ok(())
}

/// Format a file type for display.
fn format_file_type(ft: russh_sftp::protocol::FileType) -> &'static str {
    match ft {
        russh_sftp::protocol::FileType::Dir => "directory",
        russh_sftp::protocol::FileType::File => "regular file",
        russh_sftp::protocol::FileType::Symlink => "symbolic link",
        russh_sftp::protocol::FileType::Other => "other",
    }
}

/// Format a Unix timestamp for display.
fn format_timestamp(epoch_secs: u32) -> String {
    chrono::DateTime::from_timestamp(i64::from(epoch_secs), 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{epoch_secs}"))
}

/// Print available SFTP commands.
fn print_help() {
    println!(
        "\
Commands:
  ls [path]            List directory contents
  cd <path>            Change remote directory
  pwd                  Print current remote directory
  get <remote> [local] Download a file
  put <local> [remote] Upload a file
  mkdir <path>         Create a directory
  rm <path>            Remove a file
  rmdir <path>         Remove a directory
  chmod <mode> <path>  Change file permissions (octal, e.g. 755)
  stat <path>          Show file metadata
  help                 Show this help
  exit / quit          Close SFTP session"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_command_simple() {
        assert_eq!(parse_command("ls"), ("ls", ""));
        assert_eq!(parse_command("exit"), ("exit", ""));
    }

    #[test]
    fn test_parse_command_with_args() {
        assert_eq!(parse_command("ls /tmp"), ("ls", "/tmp"));
        assert_eq!(parse_command("cd /home/user"), ("cd", "/home/user"));
        assert_eq!(
            parse_command("chmod 755 /tmp/file"),
            ("chmod", "755 /tmp/file")
        );
    }

    #[test]
    fn test_parse_command_extra_whitespace() {
        assert_eq!(parse_command("  ls  /tmp  "), ("ls", "/tmp"));
        assert_eq!(parse_command("  pwd  "), ("pwd", ""));
    }

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/home/user", "/tmp/file"), "/tmp/file");
        assert_eq!(resolve_path("/", "/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(
            resolve_path("/home/user", "file.txt"),
            "/home/user/file.txt"
        );
        assert_eq!(resolve_path("/home/user", "sub/dir"), "/home/user/sub/dir");
    }

    #[test]
    fn test_resolve_path_root_cwd() {
        assert_eq!(resolve_path("/", "file.txt"), "/file.txt");
        assert_eq!(resolve_path("/", "sub/dir"), "/sub/dir");
    }

    #[test]
    fn test_parse_get_put_args_single() {
        let (first, second) = parse_get_put_args("remote.txt");
        assert_eq!(first, "remote.txt");
        assert!(second.is_none());
    }

    #[test]
    fn test_parse_get_put_args_two() {
        let (first, second) = parse_get_put_args("remote.txt local.txt");
        assert_eq!(first, "remote.txt");
        assert_eq!(second.as_deref(), Some("local.txt"));
    }

    #[test]
    fn test_parse_get_put_args_extra_spaces() {
        let (first, second) = parse_get_put_args("remote.txt   local.txt");
        assert_eq!(first, "remote.txt");
        assert_eq!(second.as_deref(), Some("local.txt"));
    }

    #[test]
    fn test_format_timestamp() {
        // 2024-01-01 00:00:00 UTC
        assert_eq!(format_timestamp(1704067200), "2024-01-01 00:00:00 UTC");
    }

    #[test]
    fn test_format_timestamp_zero() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
    }
}
