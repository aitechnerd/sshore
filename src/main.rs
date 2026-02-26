mod cli;
mod config;
mod keychain;
mod sftp;
mod ssh;
mod tui;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};

use cli::{Cli, Commands, PasswordAction};
use config::model::Bookmark;
use config::ssh_import::{merge_imports, parse_ssh_config};

/// Default SSH config path (~/.ssh/config).
fn default_ssh_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("config")
}

/// Format a bookmark row for the list table.
fn format_bookmark_row(b: &Bookmark, settings: &config::model::Settings) -> String {
    let user = b.effective_user(settings);
    let env_display = if b.env.is_empty() { "-" } else { &b.env };
    format!(
        "  {:<20} {:<30} {:<12} {:<6} {}",
        b.name, b.host, user, b.port, env_display
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Import { file, overwrite }) => {
            cmd_import(file, overwrite)?;
        }
        Some(Commands::List { env, format: _ }) => {
            cmd_list(env)?;
        }
        Some(Commands::Completions { shell }) => {
            cmd_completions(shell);
        }
        Some(Commands::Password { action }) => {
            cmd_password(action)?;
        }
        Some(Commands::Sftp { bookmark }) => {
            cmd_sftp(&bookmark).await?;
        }
        Some(Commands::Scp {
            source,
            destination,
        }) => {
            cmd_scp(&source, &destination).await?;
        }
        Some(Commands::Tunnel { .. }) => {
            eprintln!("Not yet implemented (Phase 7)");
        }
        None => {
            if let Some(name) = cli.connect {
                cmd_connect(&name).await?;
            } else {
                let mut app_config = config::load().context("Failed to load config")?;
                tui::run(&mut app_config).await?;
            }
        }
    }

    Ok(())
}

/// Import bookmarks from an SSH config file.
fn cmd_import(file: Option<String>, overwrite: bool) -> Result<()> {
    let ssh_config_path = file
        .map(|f| shellexpand::tilde(&f).to_string().into())
        .unwrap_or_else(default_ssh_config_path);

    if !ssh_config_path.exists() {
        anyhow::bail!(
            "SSH config not found: {}\nSpecify a path with: sshore import --file <path>",
            ssh_config_path.display()
        );
    }

    let mut app_config = config::load().context("Failed to load sshore config")?;

    let imported = parse_ssh_config(&ssh_config_path)
        .with_context(|| format!("Failed to parse {}", ssh_config_path.display()))?;

    let total_parsed = imported.len();
    let result = merge_imports(&mut app_config.bookmarks, imported, overwrite);

    config::save(&app_config).context("Failed to save config")?;

    println!(
        "Imported {} bookmarks from {} ({} parsed, {} already existed)",
        result.imported.len(),
        ssh_config_path.display(),
        total_parsed,
        result.already_existed
    );

    if !result.imported.is_empty() {
        println!("\nNew bookmarks:");
        for b in &result.imported {
            let env_tag = if b.env.is_empty() {
                String::new()
            } else {
                format!(" [{}]", b.env)
            };
            println!(
                "  {} → {}@{}:{}{}",
                b.name,
                b.user.as_deref().unwrap_or("?"),
                b.host,
                b.port,
                env_tag
            );
        }
    }

    Ok(())
}

/// List bookmarks in a table format.
fn cmd_list(env_filter: Option<String>) -> Result<()> {
    let app_config = config::load().context("Failed to load config")?;

    let bookmarks: Vec<&Bookmark> = app_config
        .bookmarks
        .iter()
        .filter(|b| {
            env_filter
                .as_ref()
                .is_none_or(|env| b.env.eq_ignore_ascii_case(env))
        })
        .collect();

    if bookmarks.is_empty() {
        if env_filter.is_some() {
            println!("No bookmarks matching environment filter.");
        } else {
            println!("No bookmarks yet. Import from SSH config:");
            println!("  sshore import");
        }
        return Ok(());
    }

    // Table header
    println!(
        "  {:<20} {:<30} {:<12} {:<6} ENV",
        "NAME", "HOST", "USER", "PORT"
    );
    println!("  {}", "-".repeat(76));

    for b in &bookmarks {
        println!("{}", format_bookmark_row(b, &app_config.settings));
    }

    println!("\n  {} bookmark(s)", bookmarks.len());

    Ok(())
}

/// Find a bookmark by name and return its index.
fn find_bookmark_index(config: &config::model::AppConfig, name: &str) -> Result<usize> {
    config
        .bookmarks
        .iter()
        .position(|b| b.name.eq_ignore_ascii_case(name))
        .with_context(|| {
            format!("No bookmark named '{name}'. Use `sshore list` to see available bookmarks.")
        })
}

/// Connect to a bookmark by name directly (no TUI).
async fn cmd_connect(name: &str) -> Result<()> {
    let mut app_config = config::load().context("Failed to load config")?;
    let index = find_bookmark_index(&app_config, name)?;
    ssh::connect(&mut app_config, index).await
}

/// Open an interactive SFTP session to a bookmark.
async fn cmd_sftp(name: &str) -> Result<()> {
    let config = config::load().context("Failed to load config")?;
    let index = find_bookmark_index(&config, name)?;
    sftp::open_session(&config, index).await
}

/// Copy files to/from a remote server (SCP-style).
async fn cmd_scp(source: &str, destination: &str) -> Result<()> {
    let config = config::load().context("Failed to load config")?;
    sftp::shortcuts::scp_transfer(&config, source, destination).await
}

/// Manage stored passwords in OS keychain.
fn cmd_password(action: PasswordAction) -> Result<()> {
    match action {
        PasswordAction::Set { bookmark } => cmd_password_set(&bookmark),
        PasswordAction::Remove { bookmark } => cmd_password_remove(&bookmark),
        PasswordAction::List => cmd_password_list(),
    }
}

/// Store a password for a bookmark in the OS keychain.
fn cmd_password_set(bookmark_name: &str) -> Result<()> {
    let app_config = config::load().context("Failed to load config")?;

    let bookmark = app_config
        .bookmarks
        .iter()
        .find(|b| b.name.eq_ignore_ascii_case(bookmark_name))
        .with_context(|| {
            format!(
                "No bookmark named '{bookmark_name}'. Use `sshore list` to see available bookmarks."
            )
        })?;

    // Warn for production environments
    if bookmark.env.eq_ignore_ascii_case("production") {
        eprint!("Warning: storing a password for a PRODUCTION bookmark. Continue? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let password = read_password_from_tty("Password: ")?;
    keychain::set_password(&bookmark.name, &password)?;
    println!("Password stored for '{}'.", bookmark.name);

    Ok(())
}

/// Remove a stored password for a bookmark from the OS keychain.
fn cmd_password_remove(bookmark_name: &str) -> Result<()> {
    let deleted = keychain::delete_password(bookmark_name)?;
    if deleted {
        println!("Password removed for '{bookmark_name}'.");
    } else {
        println!("No stored password for '{bookmark_name}'.");
    }
    Ok(())
}

/// List bookmarks that have stored passwords.
fn cmd_password_list() -> Result<()> {
    let app_config = config::load().context("Failed to load config")?;
    let names = keychain::list_passwords(&app_config.bookmarks);

    if names.is_empty() {
        println!("No stored passwords.");
        return Ok(());
    }

    println!("  {:<20} ENV", "BOOKMARK");
    println!("  {}", "-".repeat(32));

    for name in &names {
        let env = app_config
            .bookmarks
            .iter()
            .find(|b| b.name == *name)
            .map(|b| b.env.as_str())
            .unwrap_or("-");
        let env_display = if env.is_empty() { "-" } else { env };
        println!("  {:<20} {}", name, env_display);
    }

    println!("\n  {} password(s)", names.len());

    Ok(())
}

/// Read a password from the terminal without echoing characters.
fn read_password_from_tty(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;

    crossterm::terminal::enable_raw_mode()?;
    let mut password = String::new();
    loop {
        if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
            match key.code {
                crossterm::event::KeyCode::Enter => break,
                crossterm::event::KeyCode::Char(c) => password.push(c),
                crossterm::event::KeyCode::Backspace => {
                    password.pop();
                }
                crossterm::event::KeyCode::Esc => {
                    crossterm::terminal::disable_raw_mode()?;
                    eprintln!();
                    bail!("Cancelled");
                }
                _ => {}
            }
        }
    }
    crossterm::terminal::disable_raw_mode()?;
    eprintln!(); // Newline after password entry

    Ok(password)
}

/// Generate shell completions to stdout.
fn cmd_completions(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "sshore", &mut io::stdout());
}
