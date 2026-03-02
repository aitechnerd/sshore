mod cli;
mod config;
mod keychain;
mod sftp;
mod ssh;
mod storage;
mod tui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};

use cli::{Cli, Commands, ImportSource, PasswordAction, TunnelAction};
use config::ImportSourceKind;
use config::model::Bookmark;
use config::ssh_import::merge_imports;

/// Terminate a process by PID. Returns `true` if the signal was sent successfully.
#[cfg(unix)]
fn terminate_process(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Terminate a process by PID (Windows variant). Returns `true` if the process was killed.
#[cfg(windows)]
fn terminate_process(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

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

/// Install a panic hook that restores terminal state before printing the panic.
/// This catches panics in any thread, preventing raw mode from persisting.
fn setup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore terminal FIRST, before printing the panic
        let _ = io::stdout().flush();
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::cursor::Show,
            crossterm::style::ResetColor,
            crossterm::terminal::LeaveAlternateScreen,
        );
        ssh::terminal_theme::reset_theme();
        let _ = io::stdout().flush();

        // Now print the panic message to a clean terminal
        default_hook(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_panic_hook();
    let cli = Cli::parse();
    let cfg_override = cli.config.as_deref();

    // Warn if a subcommand name collides with a bookmark name
    if let Some(ref cmd) = cli.command {
        check_bookmark_subcommand_collision(cmd, cfg_override);
    }

    match cli.command {
        Some(Commands::Import {
            from,
            file,
            overwrite,
            env,
            tag,
            dry_run,
        }) => {
            cmd_import(from, file, overwrite, env, tag, dry_run, cfg_override)?;
        }
        Some(Commands::List { env, format: _ }) => {
            cmd_list(env, cfg_override)?;
        }
        Some(Commands::Completions { shell }) => {
            cmd_completions(shell);
        }
        Some(Commands::Connect { target }) => {
            cmd_connect_adhoc(&target, cfg_override).await?;
        }
        Some(Commands::Password { action }) => {
            cmd_password(action, cfg_override)?;
        }
        Some(Commands::Sftp { bookmark }) => {
            cmd_sftp(&bookmark, cfg_override).await?;
        }
        Some(Commands::Scp {
            source,
            destination,
            resume,
        }) => {
            cmd_scp(&source, &destination, resume, cfg_override).await?;
        }
        Some(Commands::Browse {
            target,
            local,
            show_hidden,
        }) => {
            cmd_browse(&target, local.as_deref(), show_hidden, cfg_override).await?;
        }
        Some(Commands::Tunnel { action }) => {
            cmd_tunnel(action, cfg_override).await?;
        }
        Some(Commands::Exec {
            bookmark,
            command,
            tag,
            env,
            concurrency,
        }) => {
            cmd_exec(bookmark, command, tag, env, concurrency, cfg_override).await?;
        }
        Some(Commands::Export {
            env,
            tag,
            name,
            output,
            include_settings,
        }) => {
            cmd_export(env, tag, name, output, include_settings, cfg_override)?;
        }
        None => {
            if let Some(name) = cli.connect {
                cmd_connect(&name, cfg_override).await?;
            } else {
                let mut app_config =
                    config::load_with_override(cfg_override).context("Failed to load config")?;
                tui::run(&mut app_config, cfg_override).await?;
            }
        }
    }

    Ok(())
}

/// Check if the invoked subcommand name collides with an existing bookmark name.
/// If so, print a hint suggesting `sshore connect <name>` instead.
fn check_bookmark_subcommand_collision(cmd: &Commands, cfg_override: Option<&str>) {
    let cmd_name = match cmd {
        Commands::Import { .. } => "import",
        Commands::List { .. } => "list",
        Commands::Connect { .. } => return, // `connect` is the intended escape hatch
        Commands::Sftp { .. } => "sftp",
        Commands::Scp { .. } => "scp",
        Commands::Browse { .. } => "browse",
        Commands::Tunnel { .. } => "tunnel",
        Commands::Exec { .. } => "exec",
        Commands::Export { .. } => "export",
        Commands::Password { .. } => "password",
        Commands::Completions { .. } => "completions",
    };

    if let Ok(app_config) = config::load_with_override(cfg_override)
        && app_config
            .bookmarks
            .iter()
            .any(|b| b.name.eq_ignore_ascii_case(cmd_name))
    {
        eprintln!(
            "Hint: you have a bookmark named '{cmd_name}'. \
             To connect to it, use: sshore connect {cmd_name}"
        );
    }
}

/// Import bookmarks from various sources.
fn cmd_import(
    from: Option<ImportSource>,
    file: Option<String>,
    overwrite: bool,
    env_override: Option<String>,
    extra_tags: Vec<String>,
    dry_run: bool,
    cfg_override: Option<&str>,
) -> Result<()> {
    // Validate env_override against known tiers (warn only)
    if let Some(ref env) = env_override {
        config::env::warn_unknown_env(env);
    }

    // AC-8: --from/--file flags bypass wizard entirely (backward compat)
    // AC-9: other flags (--overwrite, --env) without --from/--file open wizard with flags applied
    if from.is_none() && file.is_none() {
        let app_config =
            config::load_with_override(cfg_override).context("Failed to load sshore config")?;
        let result = match tui::views::import_wizard::run_wizard(
            &app_config,
            overwrite,
            env_override.as_deref(),
            &extra_tags,
            false,
        )? {
            Some(r) => r,
            None => return Ok(()), // User cancelled
        };
        return finish_import(
            result.bookmarks,
            &result.source_label,
            &result.file_path,
            overwrite,
            dry_run,
            cfg_override,
        );
    }

    // Resolve the import file path
    let import_path: PathBuf = if let Some(ref f) = file {
        shellexpand::tilde(f).to_string().into()
    } else if matches!(from, Some(ImportSource::SshConfig)) {
        default_ssh_config_path()
    } else {
        anyhow::bail!(
            "File path is required for --from {}. Usage: sshore import --from {} --file <path>",
            from.as_ref().map(|s| format!("{s:?}")).unwrap_or_default(),
            from.as_ref().map(|s| format!("{s:?}")).unwrap_or_default(),
        );
    };

    if !import_path.exists() {
        anyhow::bail!(
            "Import file not found: {}\nSpecify a path with: sshore import --file <path>",
            import_path.display()
        );
    }

    // Convert CLI ImportSource to config ImportSourceKind
    let source_kind = match &from {
        None => ImportSourceKind::Auto,
        Some(ImportSource::SshConfig) => ImportSourceKind::SshConfig,
        Some(ImportSource::Putty) => ImportSourceKind::Putty,
        Some(ImportSource::Mobaxterm) => ImportSourceKind::Mobaxterm,
        Some(ImportSource::Tabby) => ImportSourceKind::Tabby,
        Some(ImportSource::Securecrt) => ImportSourceKind::Securecrt,
        Some(ImportSource::Csv) => ImportSourceKind::Csv,
        Some(ImportSource::Json) => ImportSourceKind::Json,
        Some(ImportSource::Sshore) => ImportSourceKind::Sshore,
    };

    // Determine the source label for display
    let source_label = match &source_kind {
        ImportSourceKind::Auto => "auto-detect",
        ImportSourceKind::SshConfig => "SSH config",
        ImportSourceKind::Putty => "PuTTY",
        ImportSourceKind::Mobaxterm => "MobaXterm",
        ImportSourceKind::Tabby => "Tabby",
        ImportSourceKind::Securecrt => "SecureCRT",
        ImportSourceKind::Csv => "CSV",
        ImportSourceKind::Json => "JSON",
        ImportSourceKind::Sshore => "sshore TOML",
    };

    // Use the multi-source dispatcher
    let is_passthrough = matches!(
        source_kind,
        ImportSourceKind::Auto | ImportSourceKind::SshConfig | ImportSourceKind::Sshore
    );
    let mut imported = config::import_from_source(
        &import_path,
        source_kind,
        env_override.as_deref(),
        &extra_tags,
    )
    .with_context(|| {
        format!(
            "Failed to parse {} from {}",
            source_label,
            import_path.display()
        )
    })?;

    // Apply environment override for sources that don't handle it internally
    // (ssh_config, auto, sshore TOML don't pass env_override to the parser)
    if let Some(ref env) = env_override
        && is_passthrough
    {
        for bookmark in &mut imported {
            bookmark.env = env.clone();
        }
    }

    // Apply extra tags for sources that don't handle them internally
    if !extra_tags.is_empty() && is_passthrough {
        for bookmark in &mut imported {
            for tag in &extra_tags {
                if !bookmark.tags.contains(tag) {
                    bookmark.tags.push(tag.clone());
                }
            }
        }
    }

    finish_import(
        imported,
        source_label,
        &import_path,
        overwrite,
        dry_run,
        cfg_override,
    )
}

/// Complete an import: dry-run output or merge + save + summary.
fn finish_import(
    imported: Vec<Bookmark>,
    source_label: &str,
    import_path: &Path,
    overwrite: bool,
    dry_run: bool,
    cfg_override: Option<&str>,
) -> Result<()> {
    let mut app_config =
        config::load_with_override(cfg_override).context("Failed to load sshore config")?;

    let total_parsed = imported.len();

    if dry_run {
        println!("Dry run — no changes will be written.\n");
        println!("Source: {} ({})\n", source_label, import_path.display());

        let mut added = 0;
        let mut skipped = 0;
        let mut overwritten = 0;

        let existing_names: std::collections::HashSet<String> = app_config
            .bookmarks
            .iter()
            .map(|b| b.name.clone())
            .collect();

        for bookmark in &imported {
            if existing_names.contains(&bookmark.name) {
                if overwrite {
                    println!("  Would overwrite: {}", bookmark.name);
                    overwritten += 1;
                } else {
                    println!("  Skipping (already exists): {}", bookmark.name);
                    skipped += 1;
                }
            } else {
                let env_tag = if bookmark.env.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", bookmark.env)
                };
                println!("  Would import: {}{}", bookmark.name, env_tag);
                added += 1;
            }
        }

        println!(
            "\n{} would be added, {} would be overwritten, {} skipped",
            added, overwritten, skipped
        );
        return Ok(());
    }

    let result = merge_imports(&mut app_config.bookmarks, imported, overwrite);

    config::save_with_override(&app_config, cfg_override).context("Failed to save config")?;

    // Post-import summary
    println!(
        "Import complete from {} ({})\n",
        source_label,
        import_path.display()
    );
    println!("  {} bookmarks imported", result.imported.len());
    if result.already_existed > 0 {
        if overwrite {
            println!("  {} overwritten", result.already_existed);
        } else {
            println!("  {} skipped (already exist)", result.already_existed);
        }
    }
    println!("  {} total parsed from source", total_parsed);

    // Environment breakdown
    if !result.imported.is_empty() {
        let mut env_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for b in &result.imported {
            let env_key = if b.env.is_empty() {
                "(unset)".to_string()
            } else {
                b.env.clone()
            };
            *env_counts.entry(env_key).or_insert(0) += 1;
        }

        println!("\n  Environment breakdown:");
        for (env, count) in &env_counts {
            let noun = if *count == 1 { "bookmark" } else { "bookmarks" };
            println!("     {:<14} {} {}", env, count, noun);
        }

        println!("\n  Tip: Run 'sshore list' to see your imported bookmarks.");
    }

    Ok(())
}

/// List bookmarks in a table format.
fn cmd_list(env_filter: Option<String>, cfg_override: Option<&str>) -> Result<()> {
    let app_config = config::load_with_override(cfg_override).context("Failed to load config")?;

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
            println!(
                "No bookmarks yet. Run `sshore import` to import from SSH config, PuTTY, Tabby, CSV, and more."
            );
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

/// Connect to an ad-hoc host (not a bookmark).
async fn cmd_connect_adhoc(target: &str, cfg_override: Option<&str>) -> Result<()> {
    let (user, host, port) = ssh::parse_connection_string(target)?;
    let mut app_config =
        config::load_with_override(cfg_override).context("Failed to load config")?;
    ssh::connect_adhoc(&mut app_config, user, host, port, cfg_override).await
}

/// Connect to a bookmark by name directly (no TUI).
async fn cmd_connect(name: &str, cfg_override: Option<&str>) -> Result<()> {
    let mut app_config =
        config::load_with_override(cfg_override).context("Failed to load config")?;
    let index = find_bookmark_index(&app_config, name)?;
    ssh::connect(&mut app_config, index, cfg_override).await
}

/// Open an interactive SFTP session to a bookmark.
async fn cmd_sftp(name: &str, cfg_override: Option<&str>) -> Result<()> {
    let config = config::load_with_override(cfg_override).context("Failed to load config")?;
    let index = find_bookmark_index(&config, name)?;
    sftp::open_session(&config, index).await
}

/// Copy files to/from a remote server (SCP-style).
async fn cmd_scp(
    source: &str,
    destination: &str,
    resume: bool,
    cfg_override: Option<&str>,
) -> Result<()> {
    let config = config::load_with_override(cfg_override).context("Failed to load config")?;
    sftp::shortcuts::scp_transfer(&config, source, destination, resume).await
}

/// Open the dual-pane file browser.
async fn cmd_browse(
    target: &str,
    local_start: Option<&str>,
    show_hidden: bool,
    cfg_override: Option<&str>,
) -> Result<()> {
    let config = config::load_with_override(cfg_override).context("Failed to load config")?;

    // Parse target: "prod-web-01" or "prod-web-01:/var/log"
    let (bookmark_name, remote_start_path) = if target.contains(':') {
        let parts: Vec<&str> = target.splitn(2, ':').collect();
        (parts[0], Some(parts[1]))
    } else {
        (target, None)
    };

    let index = find_bookmark_index(&config, bookmark_name)?;
    let bookmark = &config.bookmarks[index];
    ssh::print_production_banner(bookmark, &config.settings, "SFTP browser");

    // Apply terminal theming
    ssh::terminal_theme::apply_theme(bookmark, &config.settings);

    // Create backends
    let remote_sftp = if let Some(path) = remote_start_path {
        storage::sftp_backend::SftpBackend::with_path(&config, index, path).await?
    } else {
        storage::sftp_backend::SftpBackend::new(&config, index).await?
    };
    let mut remote_backend = storage::Backend::Sftp(remote_sftp);

    let local_dir = local_start.unwrap_or(".");
    let local_fs = storage::local_backend::LocalBackend::new(local_dir)
        .context("Failed to open local directory")?;
    let mut local_backend = storage::Backend::Local(local_fs);

    // Launch browser TUI
    tui::views::browser::run(
        &mut local_backend,
        &mut remote_backend,
        &bookmark.name,
        &bookmark.env,
        show_hidden,
    )
    .await?;

    // Reset theming on exit
    ssh::terminal_theme::reset_theme();
    Ok(())
}

/// Manage stored passwords in OS keychain.
fn cmd_password(action: PasswordAction, cfg_override: Option<&str>) -> Result<()> {
    match action {
        PasswordAction::Set { bookmark } => cmd_password_set(&bookmark, cfg_override),
        PasswordAction::Remove { bookmark } => cmd_password_remove(&bookmark),
        PasswordAction::List => cmd_password_list(cfg_override),
    }
}

/// Store a password for a bookmark in the OS keychain.
fn cmd_password_set(bookmark_name: &str, cfg_override: Option<&str>) -> Result<()> {
    let app_config = config::load_with_override(cfg_override).context("Failed to load config")?;

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
fn cmd_password_list(cfg_override: Option<&str>) -> Result<()> {
    let app_config = config::load_with_override(cfg_override).context("Failed to load config")?;
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

/// Execute a command on one or more bookmarks.
async fn cmd_exec(
    bookmark: Option<String>,
    command: Vec<String>,
    tag: Vec<String>,
    env: Option<String>,
    concurrency: usize,
    cfg_override: Option<&str>,
) -> Result<()> {
    let config = config::load_with_override(cfg_override).context("Failed to load config")?;
    let command_str = command.join(" ");

    if command_str.is_empty() {
        bail!("No command specified. Usage: sshore exec <bookmark> -- <command>");
    }

    if let Some(name) = bookmark {
        // Single-host exec
        let index = find_bookmark_index(&config, &name)?;
        let result = ssh::exec_command(&config, index, &command_str).await?;
        std::process::exit(result.exit_code as i32);
    } else if !tag.is_empty() || env.is_some() {
        // Multi-host exec
        let matches: Vec<usize> = config
            .bookmarks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                if let Some(ref e) = env
                    && !b.env.eq_ignore_ascii_case(e)
                {
                    return false;
                }
                for t in &tag {
                    if !b.tags.contains(t) {
                        return false;
                    }
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        if matches.is_empty() {
            bail!("No bookmarks match the given filters");
        }

        eprintln!(
            "Running on {} bookmark(s) (concurrency: {})...",
            matches.len(),
            concurrency
        );
        ssh::exec_multi(&config, &matches, &command_str, concurrency).await?;
    } else {
        bail!(
            "Specify a bookmark name or use --tag/--env filters.\n\
             Usage: sshore exec <bookmark> -- <command>\n\
             Usage: sshore exec --env production -- <command>"
        );
    }

    Ok(())
}

/// Export bookmarks to a portable TOML file.
fn cmd_export(
    env: Option<String>,
    tag: Vec<String>,
    name: Option<String>,
    output: Option<String>,
    include_settings: bool,
    cfg_override: Option<&str>,
) -> Result<()> {
    let app_config = config::load_with_override(cfg_override).context("Failed to load config")?;
    let toml_output = config::export_bookmarks(
        &app_config,
        env.as_deref(),
        &tag,
        name.as_deref(),
        include_settings,
    )?;

    if let Some(path) = output {
        std::fs::write(&path, &toml_output)
            .with_context(|| format!("Failed to write export to {path}"))?;
        eprintln!("Exported to {path}");
    } else {
        print!("{toml_output}");
    }

    Ok(())
}

/// Dispatch tunnel subcommands.
async fn cmd_tunnel(action: TunnelAction, cfg_override: Option<&str>) -> Result<()> {
    match action {
        TunnelAction::Start {
            bookmark,
            local_forward,
            remote_forward,
            persist,
            daemon,
        } => {
            cmd_tunnel_start(
                &bookmark,
                &local_forward,
                &remote_forward,
                persist,
                daemon,
                cfg_override,
            )
            .await
        }
        TunnelAction::Stop { bookmark } => cmd_tunnel_stop(&bookmark),
        TunnelAction::Status => cmd_tunnel_status(),
    }
}

/// Start a tunnel to a bookmark.
async fn cmd_tunnel_start(
    bookmark_name: &str,
    local_specs: &[String],
    remote_specs: &[String],
    persist: bool,
    daemon: bool,
    cfg_override: Option<&str>,
) -> Result<()> {
    use ssh::tunnel::{ForwardDirection, ForwardSpec, parse_forward_spec};

    if local_specs.is_empty() && remote_specs.is_empty() {
        bail!("No forward specs provided. Use -L or -R to specify port forwards.");
    }

    let config = config::load_with_override(cfg_override).context("Failed to load config")?;
    let index = find_bookmark_index(&config, bookmark_name)?;

    // Parse all forward specs
    let mut forwards: Vec<ForwardSpec> = Vec::new();
    for spec in local_specs {
        forwards.push(parse_forward_spec(spec, ForwardDirection::Local)?);
    }
    for spec in remote_specs {
        forwards.push(parse_forward_spec(spec, ForwardDirection::Remote)?);
    }

    if persist && !daemon {
        // Re-exec as daemon: detach from terminal and run in background
        let exe = std::env::current_exe().context("Failed to get current executable path")?;

        let mut args = Vec::new();
        if let Some(cfg) = cfg_override {
            args.push("--config".to_string());
            args.push(cfg.to_string());
        }
        args.extend([
            "tunnel".to_string(),
            "start".to_string(),
            bookmark_name.to_string(),
            "--persist".to_string(),
            "--daemon".to_string(),
        ]);
        for spec in local_specs {
            args.push("-L".to_string());
            args.push(spec.clone());
        }
        for spec in remote_specs {
            args.push("-R".to_string());
            args.push(spec.clone());
        }

        let child = std::process::Command::new(exe)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn daemon process")?;

        println!(
            "Persistent tunnel started for '{}' (PID {})",
            bookmark_name,
            child.id()
        );
        return Ok(());
    }

    if daemon {
        // Running as daemon process
        ssh::tunnel::run_daemon_loop(&config, index, &forwards).await
    } else {
        // Foreground mode
        ssh::tunnel::run_foreground(&config, index, &forwards).await
    }
}

/// Stop a tunnel for a bookmark.
fn cmd_tunnel_stop(bookmark_name: &str) -> Result<()> {
    use ssh::tunnel::{cleanup_stale_tunnels, load_tunnel_state, save_tunnel_state};

    let mut state = load_tunnel_state().context("Failed to load tunnel state")?;
    cleanup_stale_tunnels(&mut state);

    let entry = state
        .tunnels
        .iter()
        .find(|t| t.bookmark.eq_ignore_ascii_case(bookmark_name));

    let Some(entry) = entry else {
        println!("No active tunnel for '{bookmark_name}'.");
        return Ok(());
    };

    let pid = entry.pid;

    // Terminate the tunnel process
    if terminate_process(pid) {
        println!("Stopped tunnel for '{bookmark_name}' (PID {pid}).");
    } else {
        eprintln!("Warning: failed to send signal to PID {pid}, removing stale entry.");
    }

    // Remove from state file
    state
        .tunnels
        .retain(|t| !t.bookmark.eq_ignore_ascii_case(bookmark_name));
    save_tunnel_state(&state).context("Failed to update tunnel state")?;

    Ok(())
}

/// Show status of all active tunnels.
fn cmd_tunnel_status() -> Result<()> {
    use ssh::tunnel::{TunnelStatus, cleanup_stale_tunnels, load_tunnel_state, save_tunnel_state};

    let mut state = load_tunnel_state().context("Failed to load tunnel state")?;
    cleanup_stale_tunnels(&mut state);
    save_tunnel_state(&state).context("Failed to update tunnel state")?;

    if state.tunnels.is_empty() {
        println!("No active tunnels.");
        return Ok(());
    }

    println!(
        "  {:<20} {:<30} {:<14} {:<10} RECONNECTS",
        "BOOKMARK", "FORWARDS", "STATUS", "UPTIME"
    );
    println!("  {}", "-".repeat(86));

    for entry in &state.tunnels {
        let forwards_str: Vec<String> = entry.forwards.iter().map(|f| f.to_string()).collect();
        let forwards_display = forwards_str.join(", ");

        let status_display = match entry.status {
            TunnelStatus::Connected => "connected",
            TunnelStatus::Reconnecting => "reconnecting",
            TunnelStatus::Stopped => "stopped",
        };

        let uptime = chrono::Utc::now()
            .signed_duration_since(entry.started_at)
            .num_seconds();
        let uptime_display = format_uptime(uptime);

        println!(
            "  {:<20} {:<30} {:<14} {:<10} {}",
            entry.bookmark, forwards_display, status_display, uptime_display, entry.reconnect_count
        );
    }

    println!("\n  {} tunnel(s)", state.tunnels.len());

    Ok(())
}

/// Format seconds into a human-readable uptime string (e.g., "2h 15m", "3d 1h").
fn format_uptime(total_secs: i64) -> String {
    if total_secs < 0 {
        return "0s".to_string();
    }

    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Generate shell completions to stdout.
fn cmd_completions(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "sshore", &mut io::stdout());
}
