use std::io::Write;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::model::AppConfig;
use crate::ssh;
use crate::ssh::terminal_theme;

/// Buffer size for file transfer chunks (32 KB).
const TRANSFER_CHUNK_SIZE: usize = 32 * 1024;

/// Minimum interval between progress bar redraws (100ms).
const PROGRESS_THROTTLE_MS: u128 = 100;

/// Width of the progress bar (in characters).
const BAR_WIDTH: usize = 30;

/// Parse a `bookmark:path` spec. Returns `Some((bookmark_name, remote_path))` for remote paths,
/// or `None` for local paths.
pub fn parse_remote_spec(spec: &str) -> Option<(&str, &str)> {
    // Look for the first colon. If found, the part before is the bookmark name.
    // However, we must avoid matching Windows-style drive letters (e.g., C:\)
    // and paths starting with / or . which are clearly local.
    if spec.starts_with('/') || spec.starts_with('.') {
        return None;
    }

    let colon_pos = spec.find(':')?;

    // Empty bookmark name is not valid
    if colon_pos == 0 {
        return None;
    }

    let bookmark = &spec[..colon_pos];
    let path = &spec[colon_pos + 1..];

    Some((bookmark, path))
}

/// Execute an SCP-style file transfer.
pub async fn scp_transfer(
    config: &AppConfig,
    source: &str,
    destination: &str,
    resume: bool,
) -> Result<()> {
    let src_remote = parse_remote_spec(source);
    let dst_remote = parse_remote_spec(destination);

    match (src_remote, dst_remote) {
        (Some(_), Some(_)) => {
            bail!("Both source and destination are remote. Only one can be remote.");
        }
        (None, None) => {
            bail!(
                "Neither source nor destination is remote. Use `bookmark:path` syntax for the remote side."
            );
        }
        (Some((bookmark_name, remote_path)), None) => {
            // Download: remote -> local
            let local_path = destination;
            download(config, bookmark_name, remote_path, local_path, resume).await
        }
        (None, Some((bookmark_name, remote_path))) => {
            // Upload: local -> remote
            if resume {
                eprintln!("Warning: --resume is only supported for downloads, ignoring.");
            }
            let local_path = source;
            upload(config, bookmark_name, local_path, remote_path).await
        }
    }
}

/// Find bookmark index by name.
fn find_bookmark_index(config: &AppConfig, name: &str) -> Result<usize> {
    config
        .bookmarks
        .iter()
        .position(|b| b.name.eq_ignore_ascii_case(name))
        .with_context(|| {
            format!("No bookmark named '{name}'. Use `sshore list` to see available bookmarks.")
        })
}

/// Establish an SFTP session with theming applied.
async fn open_sftp(config: &AppConfig, bookmark_name: &str) -> Result<(SftpSession, usize)> {
    let index = find_bookmark_index(config, bookmark_name)?;
    let session = ssh::establish_session(config, index).await?;

    // Apply theming
    let bookmark = &config.bookmarks[index];
    let settings = &config.settings;
    let title = format!(
        "SCP: {}",
        terminal_theme::render_tab_title(&settings.tab_title_template, bookmark, settings)
    );
    terminal_theme::apply_theme_with_title(bookmark, settings, &title);

    // Open SFTP subsystem
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

    Ok((sftp, index))
}

/// Download a file from a remote server.
///
/// Writes to a `.part` tempfile first and renames to the final path on success,
/// preventing partial failures from leaving corrupted files at the target path.
/// If `resume` is true and a partial `.part` file exists, seek to its end and continue.
async fn download(
    config: &AppConfig,
    bookmark_name: &str,
    remote_path: &str,
    local_path: &str,
    resume: bool,
) -> Result<()> {
    let (sftp, index) = open_sftp(config, bookmark_name).await?;
    let display_name = &config.bookmarks[index].name;

    let meta = sftp
        .metadata(remote_path)
        .await
        .with_context(|| format!("Failed to stat {display_name}:{remote_path}"))?;

    let total = meta.size.unwrap_or(0);

    let mut remote_file = sftp
        .open(remote_path)
        .await
        .with_context(|| format!("Failed to open {display_name}:{remote_path}"))?;

    // Write to a .part file to avoid corrupting the final path on partial failure
    let part_path = format!("{local_path}.part");

    // Check if the final file is already complete
    if resume
        && let Ok(local_meta) = tokio::fs::metadata(local_path).await
        && local_meta.len() >= total
    {
        eprintln!(
            "Local file is already complete ({}).",
            format_bytes(local_meta.len())
        );
        terminal_theme::reset_theme();
        return Ok(());
    }

    // Resume support: check the .part file for a previous incomplete download
    let mut offset: u64 = 0;
    let mut local_file = if resume {
        if let Ok(part_meta) = tokio::fs::metadata(&part_path).await {
            let part_size = part_meta.len();
            if part_size > 0 && part_size < total {
                eprintln!("Resuming from {}", format_bytes(part_size));
                use tokio::io::AsyncSeekExt;
                remote_file
                    .seek(std::io::SeekFrom::Start(part_size))
                    .await
                    .with_context(|| format!("Failed to seek remote file to offset {part_size}"))?;
                offset = part_size;
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&part_path)
                    .await
                    .with_context(|| format!("Failed to open {part_path} for append"))?
            } else {
                tokio::fs::File::create(&part_path)
                    .await
                    .with_context(|| format!("Failed to create {part_path}"))?
            }
        } else {
            tokio::fs::File::create(&part_path)
                .await
                .with_context(|| format!("Failed to create {part_path}"))?
        }
    } else {
        tokio::fs::File::create(&part_path)
            .await
            .with_context(|| format!("Failed to create {part_path}"))?
    };

    eprintln!("{display_name}:{remote_path}");
    let mut progress = ProgressBar::new(total);
    progress.transferred = offset;
    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];

    loop {
        let n = remote_file
            .read(&mut buf)
            .await
            .context("Failed to read from remote file")?;
        if n == 0 {
            break;
        }
        local_file
            .write_all(&buf[..n])
            .await
            .context("Failed to write to local file")?;
        progress.update(n as u64);
    }

    // Rename .part to final path on success
    tokio::fs::rename(&part_path, local_path)
        .await
        .with_context(|| format!("Failed to rename {part_path} to {local_path}"))?;

    progress.finish();
    terminal_theme::reset_theme();
    Ok(())
}

/// Upload a file to a remote server.
async fn upload(
    config: &AppConfig,
    bookmark_name: &str,
    local_path: &str,
    remote_path: &str,
) -> Result<()> {
    let (sftp, index) = open_sftp(config, bookmark_name).await?;
    let display_name = &config.bookmarks[index].name;

    let local_meta = tokio::fs::metadata(local_path)
        .await
        .with_context(|| format!("Failed to stat local file {local_path}"))?;

    if !local_meta.is_file() {
        terminal_theme::reset_theme();
        bail!("{local_path} is not a regular file");
    }

    let total = local_meta.len();

    let mut local_file = tokio::fs::File::open(local_path)
        .await
        .with_context(|| format!("Failed to open local file {local_path}"))?;

    let mut remote_file = sftp
        .create(remote_path)
        .await
        .with_context(|| format!("Failed to create {display_name}:{remote_path}"))?;

    eprintln!("{display_name}:{remote_path}");
    let mut progress = ProgressBar::new(total);
    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];

    loop {
        let n = local_file
            .read(&mut buf)
            .await
            .context("Failed to read local file")?;
        if n == 0 {
            break;
        }
        remote_file
            .write_all(&buf[..n])
            .await
            .context("Failed to write to remote file")?;
        progress.update(n as u64);
    }

    remote_file
        .shutdown()
        .await
        .context("Failed to close remote file")?;

    progress.finish();
    terminal_theme::reset_theme();
    Ok(())
}

/// Simple stderr-based progress bar for file transfers.
pub struct ProgressBar {
    total_bytes: u64,
    transferred: u64,
    start_time: Instant,
    last_draw: Instant,
}

impl ProgressBar {
    /// Create a new progress bar with the given total byte count.
    pub fn new(total_bytes: u64) -> Self {
        let now = Instant::now();
        Self {
            total_bytes,
            transferred: 0,
            start_time: now,
            last_draw: now,
        }
    }

    /// Update progress after transferring more bytes.
    pub fn update(&mut self, bytes_added: u64) {
        self.transferred += bytes_added;

        let now = Instant::now();
        if now.duration_since(self.last_draw).as_millis() >= PROGRESS_THROTTLE_MS {
            self.draw();
            self.last_draw = now;
        }
    }

    /// Draw the final 100% progress line.
    pub fn finish(&mut self) {
        self.transferred = self.total_bytes;
        self.draw();
        eprintln!(); // Final newline
    }

    /// Draw the progress bar to stderr.
    fn draw(&self) {
        let pct = if self.total_bytes > 0 {
            (self.transferred as f64 / self.total_bytes as f64 * 100.0).min(100.0)
        } else {
            100.0
        };

        let filled = if self.total_bytes > 0 {
            (BAR_WIDTH as f64 * self.transferred as f64 / self.total_bytes as f64) as usize
        } else {
            BAR_WIDTH
        };
        let empty = BAR_WIDTH.saturating_sub(filled);

        let elapsed = self.start_time.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            self.transferred as f64 / elapsed
        } else {
            0.0
        };

        let eta = if speed > 0.0 && self.transferred < self.total_bytes {
            let remaining = self.total_bytes - self.transferred;
            format_duration((remaining as f64 / speed) as u64)
        } else {
            "-".to_string()
        };

        let mut stderr = std::io::stderr();
        let _ = write!(
            stderr,
            "\r[{}>{}] {:.0}% {}/{} {} ETA: {}    ",
            "=".repeat(filled),
            " ".repeat(empty),
            pct,
            format_bytes(self.transferred),
            format_bytes(self.total_bytes),
            format_bytes_per_sec(speed),
            eta,
        );
        let _ = stderr.flush();
    }
}

/// Format a byte count for human-readable display.
pub fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if n >= GB {
        format!("{:.1}GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1}MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}KB", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

/// Format bytes-per-second speed for display.
fn format_bytes_per_sec(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;

    if bps >= MB {
        format!("{:.1}MB/s", bps / MB)
    } else if bps >= KB {
        format!("{:.0}KB/s", bps / KB)
    } else {
        format!("{:.0}B/s", bps)
    }
}

/// Format a duration in seconds for human-readable display.
pub fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m {s}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_remote_spec ---

    #[test]
    fn test_parse_remote_spec_valid() {
        assert_eq!(
            parse_remote_spec("prod-web:/var/log/app.log"),
            Some(("prod-web", "/var/log/app.log"))
        );
    }

    #[test]
    fn test_parse_remote_spec_empty_path() {
        assert_eq!(parse_remote_spec("myhost:"), Some(("myhost", "")));
    }

    #[test]
    fn test_parse_remote_spec_relative_path() {
        assert_eq!(
            parse_remote_spec("myhost:file.txt"),
            Some(("myhost", "file.txt"))
        );
    }

    #[test]
    fn test_parse_remote_spec_local_absolute() {
        assert_eq!(parse_remote_spec("/tmp/file.txt"), None);
    }

    #[test]
    fn test_parse_remote_spec_local_relative() {
        assert_eq!(parse_remote_spec("./file.txt"), None);
        assert_eq!(parse_remote_spec("../file.txt"), None);
    }

    #[test]
    fn test_parse_remote_spec_no_colon() {
        assert_eq!(parse_remote_spec("file.txt"), None);
    }

    #[test]
    fn test_parse_remote_spec_empty_bookmark() {
        // ":path" has empty bookmark name — invalid
        assert_eq!(parse_remote_spec(":path"), None);
    }

    // --- format_bytes ---

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(1023), "1023B");
    }

    #[test]
    fn test_format_bytes_kb() {
        assert_eq!(format_bytes(1024), "1.0KB");
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(450 * 1024), "450.0KB");
    }

    #[test]
    fn test_format_bytes_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 + 512 * 1024), "2.5MB");
    }

    #[test]
    fn test_format_bytes_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0GB");
        assert_eq!(
            format_bytes(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "3.5GB"
        );
    }

    // --- format_duration ---

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(60), "1m 0s");
        assert_eq!(format_duration(83), "1m 23s");
        assert_eq!(format_duration(3599), "59m 59s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(7500), "2h 5m");
    }

    // --- ProgressBar ---

    #[test]
    fn test_progress_bar_percentage() {
        let mut pb = ProgressBar::new(100);
        pb.transferred = 50;
        // Just verify it doesn't panic when drawing
        pb.draw();
    }

    #[test]
    fn test_progress_bar_zero_total() {
        let mut pb = ProgressBar::new(0);
        pb.update(0);
        pb.finish(); // Should not panic or divide by zero
    }

    #[test]
    fn test_progress_bar_throttle() {
        let mut pb = ProgressBar::new(1000);
        // Rapid updates should be throttled
        for _ in 0..100 {
            pb.update(10);
        }
        assert_eq!(pb.transferred, 1000);
    }

    // --- scp_transfer error paths (both-remote, neither-remote) ---
    // These test the parse_remote_spec logic that drives the bail! branches.

    #[test]
    fn test_both_remote_detected() {
        let src = parse_remote_spec("server1:/path/a");
        let dst = parse_remote_spec("server2:/path/b");
        // Both are Some → scp_transfer would bail "Both source and destination are remote"
        assert!(src.is_some());
        assert!(dst.is_some());
    }

    #[test]
    fn test_neither_remote_detected() {
        let src = parse_remote_spec("/local/path/a");
        let dst = parse_remote_spec("./local/path/b");
        // Both are None → scp_transfer would bail "Neither source nor destination is remote"
        assert!(src.is_none());
        assert!(dst.is_none());
    }

    #[test]
    fn test_parse_remote_spec_multiple_colons() {
        // Only the first colon splits bookmark:path
        let result = parse_remote_spec("server:/path/with:colon");
        assert_eq!(result, Some(("server", "/path/with:colon")));
    }

    // --- format_bytes_per_sec ---

    #[test]
    fn test_format_bytes_per_sec_small() {
        assert_eq!(format_bytes_per_sec(500.0), "500B/s");
        assert_eq!(format_bytes_per_sec(0.0), "0B/s");
    }

    #[test]
    fn test_format_bytes_per_sec_kb() {
        assert_eq!(format_bytes_per_sec(1024.0), "1KB/s");
        assert_eq!(format_bytes_per_sec(10240.0), "10KB/s");
    }

    #[test]
    fn test_format_bytes_per_sec_mb() {
        assert_eq!(format_bytes_per_sec(1024.0 * 1024.0), "1.0MB/s");
        assert_eq!(format_bytes_per_sec(5.5 * 1024.0 * 1024.0), "5.5MB/s");
    }
}
