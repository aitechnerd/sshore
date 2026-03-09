/// Pipelined SFTP transfers.
///
/// SFTP protocol allows multiple in-flight requests on a single channel.
/// OpenSSH's sftp client uses 64 concurrent requests to keep the network pipe
/// full regardless of RTT. This module implements the same approach using
/// `RawSftpSession`, achieving near-zero application-level CPU overhead by
/// reducing per-byte async wake-ups from ~3,000/sec to ~50/sec.
///
/// **Architecture**: Fire N concurrent `SSH_FXP_READ` requests via `JoinSet`,
/// buffer out-of-order responses, and write to the local file in sequential
/// order (preserving resume compatibility with `.part` files).
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::task::JoinSet;

/// Max bytes per SFTP read/write request.
/// Matches russh-sftp's internal `MAX_READ_LENGTH` (261,120 bytes ≈ 255 KB).
const CHUNK_SIZE: u64 = 261_120;

/// Concurrent in-flight SFTP requests.
/// OpenSSH uses 64. With 32 requests × 255 KB = ~8 MB in flight, which
/// saturates links up to ~640 Mbit/s at 100ms RTT.
const PIPELINE_DEPTH: usize = 32;

/// Create an initialized `RawSftpSession` from an SSH channel.
///
/// Negotiates SFTP version and server limits. Sets a generous timeout
/// suitable for file transfers (5 minutes per request).
pub async fn create_raw_session(
    channel: russh::Channel<russh::client::Msg>,
) -> Result<Arc<RawSftpSession>> {
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("Failed to request SFTP subsystem")?;

    let mut raw = RawSftpSession::new(channel.into_stream());
    raw.set_timeout(300).await;

    let version = raw
        .init()
        .await
        .map_err(|e| anyhow::anyhow!("SFTP init failed: {e}"))?;

    // Negotiate server limits (max read/write length) if supported.
    if version.extensions.contains_key("limits@openssh.com")
        && let Ok(limits) = raw.limits().await
    {
        raw.set_limits(Arc::new(limits.into()));
    }

    Ok(Arc::new(raw))
}

/// Pipelined SFTP download.
///
/// Fires `PIPELINE_DEPTH` concurrent `SSH_FXP_READ` requests and writes
/// responses to the local file in sequential order. Out-of-order responses
/// are buffered in memory (bounded by `PIPELINE_DEPTH` × `CHUNK_SIZE` ≈ 8 MB).
///
/// `on_bytes_written` is called after each contiguous chunk is flushed to disk,
/// with the number of bytes just written. This enables progress reporting.
pub async fn download<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    remote_path: &str,
    local_file: &mut (impl Write + Send),
    total_size: u64,
    start_offset: u64,
    mut on_bytes_written: F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    if total_size == 0 || start_offset >= total_size {
        return Ok(());
    }

    // Open remote file handle via raw SFTP protocol.
    let handle = raw
        .open(remote_path, OpenFlags::READ, FileAttributes::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open remote file: {e}"))?;
    let handle_str = handle.handle;

    let result = download_inner(
        raw,
        &handle_str,
        local_file,
        total_size,
        start_offset,
        &mut on_bytes_written,
        cancel,
    )
    .await;

    // Always close the remote handle, even on error.
    let _ = raw.close(&handle_str).await;

    result
}

/// Inner download loop. Separated so the handle is always closed in `download()`.
async fn download_inner<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: &str,
    local_file: &mut (impl Write + Send),
    total_size: u64,
    start_offset: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut next_request_offset = start_offset;
    let mut next_write_offset = start_offset;
    let mut set = JoinSet::new();
    let mut buffer: HashMap<u64, Vec<u8>> = HashMap::new();

    // Seed the pipeline with concurrent read requests.
    while set.len() < PIPELINE_DEPTH && next_request_offset < total_size {
        spawn_read(&mut set, raw, handle_str, next_request_offset, total_size);
        next_request_offset += chunk_len(next_request_offset, total_size);
    }

    // Process responses and refill pipeline.
    while let Some(join_result) = set.join_next().await {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            set.abort_all();
            anyhow::bail!("Transfer cancelled");
        }

        let (offset, sftp_result) = join_result.context("SFTP read task panicked")?;
        let data = sftp_result.map_err(|e| anyhow::anyhow!("SFTP read failed: {e}"))?;

        // Buffer the response (may arrive out of order).
        buffer.insert(offset, data.data);

        // Flush contiguous chunks to disk in order.
        while let Some(chunk) = buffer.remove(&next_write_offset) {
            let chunk_len = chunk.len() as u64;
            local_file
                .write_all(&chunk)
                .context("Failed to write to local file")?;
            next_write_offset += chunk_len;
            on_bytes_written(chunk_len);
        }

        // Refill pipeline.
        if next_request_offset < total_size {
            spawn_read(&mut set, raw, handle_str, next_request_offset, total_size);
            next_request_offset += chunk_len(next_request_offset, total_size);
        }
    }

    // Flush any remaining buffered chunks (shouldn't happen if pipeline drains cleanly).
    while let Some(chunk) = buffer.remove(&next_write_offset) {
        let chunk_len = chunk.len() as u64;
        local_file
            .write_all(&chunk)
            .context("Failed to write to local file")?;
        next_write_offset += chunk_len;
        on_bytes_written(chunk_len);
    }

    local_file.flush().context("Failed to flush local file")?;
    Ok(())
}

/// Pipelined SFTP upload.
///
/// Reads the local file sequentially and fires `PIPELINE_DEPTH` concurrent
/// `SSH_FXP_WRITE` requests. Progress is reported as write ACKs arrive.
pub async fn upload<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    remote_path: &str,
    local_file: &mut (impl Read + Send),
    total_size: u64,
    mut on_bytes_written: F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    // Open/create remote file.
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = raw
        .open(remote_path, flags, FileAttributes::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create remote file: {e}"))?;
    let handle_str = handle.handle;

    let result = upload_inner(
        raw,
        &handle_str,
        local_file,
        total_size,
        &mut on_bytes_written,
        cancel,
    )
    .await;

    // Always close (flushes data on server side).
    let _ = raw.close(&handle_str).await;

    result
}

/// Inner upload loop.
async fn upload_inner<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: &str,
    local_file: &mut (impl Read + Send),
    total_size: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut offset = 0u64;
    let mut set = JoinSet::new();

    // Seed the pipeline.
    while set.len() < PIPELINE_DEPTH && offset < total_size {
        let len = chunk_len(offset, total_size) as usize;
        let n = read_chunk(local_file, len)?;
        if n.is_empty() {
            break;
        }
        spawn_write(&mut set, raw, handle_str, offset, n);
        offset += len as u64;
    }

    // Process ACKs and refill.
    while let Some(join_result) = set.join_next().await {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            set.abort_all();
            anyhow::bail!("Transfer cancelled");
        }

        let bytes_written = join_result.context("SFTP write task panicked")?;
        let bytes_written = bytes_written.map_err(|e| anyhow::anyhow!("SFTP write failed: {e}"))?;
        on_bytes_written(bytes_written as u64);

        // Refill.
        if offset < total_size {
            let len = chunk_len(offset, total_size) as usize;
            let n = read_chunk(local_file, len)?;
            if !n.is_empty() {
                spawn_write(&mut set, raw, handle_str, offset, n);
                offset += len as u64;
            }
        }
    }

    Ok(())
}

/// Read a chunk from a local file. Returns the data (may be shorter than `len` at EOF).
fn read_chunk(file: &mut impl Read, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = file
            .read(&mut buf[filled..])
            .context("Failed to read local file")?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Spawn a pipelined SFTP read request.
fn spawn_read(
    set: &mut JoinSet<(
        u64,
        Result<russh_sftp::protocol::Data, russh_sftp::client::error::Error>,
    )>,
    raw: &Arc<RawSftpSession>,
    handle_str: &str,
    offset: u64,
    total_size: u64,
) {
    let len = chunk_len(offset, total_size) as u32;
    let r = Arc::clone(raw);
    let h = handle_str.to_string();
    set.spawn(async move { (offset, r.read(h, offset, len).await) });
}

/// Spawn a pipelined SFTP write request.
fn spawn_write(
    set: &mut JoinSet<Result<usize, russh_sftp::client::error::Error>>,
    raw: &Arc<RawSftpSession>,
    handle_str: &str,
    offset: u64,
    data: Vec<u8>,
) {
    let r = Arc::clone(raw);
    let h = handle_str.to_string();
    let len = data.len();
    set.spawn(async move {
        r.write(h, offset, data).await?;
        Ok(len)
    });
}

/// Calculate chunk length, clamping to file boundary.
fn chunk_len(offset: u64, total_size: u64) -> u64 {
    std::cmp::min(CHUNK_SIZE, total_size - offset)
}
