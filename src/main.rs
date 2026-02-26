mod cli;
mod config;
mod ssh;
mod tui;

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};

use cli::{Cli, Commands};
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
        Some(Commands::Password { .. }) => {
            eprintln!("Not yet implemented (Phase 5)");
        }
        Some(Commands::Sftp { .. }) => {
            eprintln!("Not yet implemented (Phase 6)");
        }
        Some(Commands::Scp { .. }) => {
            eprintln!("Not yet implemented (Phase 6)");
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

/// Connect to a bookmark by name directly (no TUI).
async fn cmd_connect(name: &str) -> Result<()> {
    let mut app_config = config::load().context("Failed to load config")?;

    let bookmark_index = app_config
        .bookmarks
        .iter()
        .position(|b| b.name.eq_ignore_ascii_case(name))
        .with_context(|| {
            format!("No bookmark named '{name}'. Use `sshore list` to see available bookmarks.")
        })?;

    ssh::connect(&mut app_config, bookmark_index).await
}

/// Generate shell completions to stdout.
fn cmd_completions(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "sshore", &mut io::stdout());
}
