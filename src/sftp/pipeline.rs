/// Pipelined SFTP transfers.
///
/// SFTP protocol allows multiple in-flight requests on a single channel.
/// OpenSSH's sftp client uses 64 concurrent requests to keep the network pipe
/// full regardless of RTT. This module keeps a bounded set of in-flight
/// requests without spawning a Tokio task per chunk.
///
/// **Architecture**: Fire N concurrent `SSH_FXP_READ` requests via a bounded
/// future queue,
/// buffer out-of-order responses, and write to the local file in sequential
/// order (preserving resume compatibility with `.part` files).
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags};

/// Default max bytes per SFTP read/write request.
/// Matches russh-sftp's internal `MAX_READ_LENGTH` (261,120 bytes ≈ 255 KB).
/// Actual chunk size may be smaller if the server negotiates a lower limit,
/// or larger if the server advertises higher limits via `limits@openssh.com`.
pub(crate) const CHUNK_SIZE: u64 = 261_120;

/// Upper bound on chunk size even when the server allows more.
/// 1 MB balances fewer requests (less overhead) vs memory per in-flight chunk.
const MAX_CHUNK_SIZE: u64 = 1024 * 1024;

/// Maximum concurrent in-flight SFTP requests.
/// OpenSSH uses 64; we use 128 to better saturate high-bandwidth, high-latency
/// links. At 261 KB per request, 128 in-flight covers ~32 MB of data in the pipe,
/// which at 1 Gbps and 100ms RTT keeps the link full (~12 MB in-flight needed).
/// Actual depth is adaptive: min(MAX_PIPELINE_DEPTH, chunks_in_file) so small
/// files don't waste time seeding requests they'll never need.
const MAX_PIPELINE_DEPTH: usize = 128;

/// Result of creating a raw SFTP session, including negotiated limits.
pub struct PipelinedSession {
    pub raw: Arc<RawSftpSession>,
    /// Effective max bytes per read request (from server limits or default).
    pub read_chunk_size: u64,
    /// Effective max bytes per write request (from server limits or default).
    pub write_chunk_size: u64,
}

/// Create an initialized `RawSftpSession` from an SSH channel.
///
/// Negotiates SFTP version and server limits. Sets a generous timeout
/// suitable for file transfers (5 minutes per request).
pub async fn create_raw_session(
    channel: russh::Channel<russh::client::Msg>,
) -> Result<PipelinedSession> {
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

    let mut read_chunk_size = CHUNK_SIZE;
    let mut write_chunk_size = CHUNK_SIZE;

    // Negotiate server limits (max read/write length) if supported.
    // Use the server's advertised limits — both smaller AND larger than our
    // default. Larger chunks reduce per-request overhead (fewer futures,
    // fewer HashMap entries, fewer SSH packets).
    if version.extensions.contains_key("limits@openssh.com")
        && let Ok(limits) = raw.limits().await
    {
        if limits.max_read_len > 0 {
            read_chunk_size = limits.max_read_len.clamp(1, MAX_CHUNK_SIZE);
        }
        if limits.max_write_len > 0 {
            write_chunk_size = limits.max_write_len.clamp(1, MAX_CHUNK_SIZE);
        }
        raw.set_limits(Arc::new(limits.into()));
    }

    Ok(PipelinedSession {
        raw: Arc::new(raw),
        read_chunk_size,
        write_chunk_size,
    })
}

/// Pipelined SFTP download.
///
/// Fires `MAX_PIPELINE_DEPTH` concurrent `SSH_FXP_READ` requests and writes
/// responses to the local file in sequential order. Out-of-order responses
/// are buffered in memory (bounded by `MAX_PIPELINE_DEPTH` × chunk_size).
///
/// `chunk_size` should come from `PipelinedSession::read_chunk_size` to
/// match the server's negotiated limits and avoid short reads.
///
/// `on_bytes_written` is called after each contiguous chunk is flushed to disk,
/// with the number of bytes just written. This enables progress reporting.
#[allow(clippy::too_many_arguments)]
pub async fn download<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    remote_path: &str,
    local_file: &mut (impl Write + Send),
    total_size: u64,
    start_offset: u64,
    chunk_size: u64,
    mut on_bytes_written: F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    if total_size == 0 || start_offset >= total_size {
        return Ok(());
    }

    // Open remote file handle via raw SFTP protocol.
    let open_start = std::time::Instant::now();
    let handle = raw
        .open(remote_path, OpenFlags::READ, FileAttributes::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open remote file: {e}"))?;
    let handle_str: Arc<str> = handle.handle.into();
    let open_ms = open_start.elapsed().as_millis();
    if open_ms > 50 {
        tracing::debug!("pipeline::download open took {open_ms}ms: {remote_path}");
    }

    let t0 = std::time::Instant::now();

    let result = download_inner(
        raw,
        Arc::clone(&handle_str),
        local_file,
        total_size,
        start_offset,
        chunk_size,
        &mut on_bytes_written,
        cancel,
    )
    .await;

    let dt = t0.elapsed().as_secs_f64();
    let bytes = total_size - start_offset;

    // Always close the remote handle, even on error.
    let close_start = std::time::Instant::now();
    let _ = raw.close(handle_str.as_ref()).await;
    let close_ms = close_start.elapsed().as_millis() as u64;

    if dt > 0.1 {
        let mbps = bytes as f64 / dt / 1_048_576.0;
        let total_ms = open_ms as u64 + (dt * 1000.0) as u64 + close_ms;
        let overhead_pct = (open_ms as u64 + close_ms) as f64 / total_ms as f64 * 100.0;
        tracing::debug!(
            "pipeline::download {remote_path}: {:.1} MB in {dt:.1}s = {mbps:.1} MB/s \
             (open={open_ms}ms close={close_ms}ms overhead={overhead_pct:.0}% chunk={chunk_size})",
            bytes as f64 / 1_048_576.0,
        );
    }

    result
}

/// Inner download loop. Separated so the handle is always closed in `download()`.
#[allow(clippy::too_many_arguments)]
async fn download_inner<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: Arc<str>,
    local_file: &mut (impl Write + Send),
    total_size: u64,
    start_offset: u64,
    chunk_size: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut next_request_offset = start_offset;
    let mut next_write_offset = start_offset;
    let mut inflight = FuturesUnordered::new();

    // Adaptive pipeline depth: no point having more in-flight requests than chunks in the file.
    let remaining = total_size - start_offset;
    let total_chunks = remaining.div_ceil(chunk_size) as usize;
    let depth = total_chunks.min(MAX_PIPELINE_DEPTH);

    // Pre-allocate with expected capacity to avoid rehashing.
    let mut buffer: HashMap<u64, Vec<u8>> = HashMap::with_capacity(depth);

    // Seed the pipeline with concurrent read requests.
    while inflight.len() < depth && next_request_offset < total_size {
        let len = std::cmp::min(chunk_size, total_size - next_request_offset);
        queue_read(
            &mut inflight,
            raw,
            Arc::clone(&handle_str),
            next_request_offset,
            len,
        );
        next_request_offset += len;
    }

    // Process responses and refill pipeline.
    while let Some((offset, expected_len, sftp_result)) = inflight.next().await {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            anyhow::bail!("Transfer cancelled");
        }

        let data = sftp_result.map_err(|e| anyhow::anyhow!("SFTP read failed: {e}"))?;
        let received_len = data.data.len() as u64;

        // Report progress as soon as data arrives (not after in-order write),
        // so the UI speed display reflects actual network throughput.
        on_bytes_written(received_len);

        // If the server returned less data than requested (short read),
        // re-request the remaining range to fill the gap.
        if received_len < expected_len && offset + received_len < total_size {
            let gap_offset = offset + received_len;
            let gap_len = expected_len - received_len;
            queue_read(
                &mut inflight,
                raw,
                Arc::clone(&handle_str),
                gap_offset,
                gap_len,
            );
        }

        // Fast path: if this chunk is the next expected offset, write directly
        // to disk without touching the HashMap. This is the common case when
        // responses arrive roughly in order.
        if offset == next_write_offset {
            local_file
                .write_all(&data.data)
                .context("Failed to write to local file")?;
            next_write_offset += received_len;

            // Drain any buffered chunks that are now contiguous.
            while let Some(chunk) = buffer.remove(&next_write_offset) {
                let chunk_len = chunk.len() as u64;
                local_file
                    .write_all(&chunk)
                    .context("Failed to write to local file")?;
                next_write_offset += chunk_len;
            }
        } else {
            // Out-of-order response: buffer it.
            buffer.insert(offset, data.data);
        }

        // Refill pipeline with new chunks beyond what's already requested.
        if next_request_offset < total_size {
            let len = std::cmp::min(chunk_size, total_size - next_request_offset);
            queue_read(
                &mut inflight,
                raw,
                Arc::clone(&handle_str),
                next_request_offset,
                len,
            );
            next_request_offset += len;
        }
    }

    // Flush any remaining buffered chunks (shouldn't happen if pipeline drains cleanly).
    while let Some(chunk) = buffer.remove(&next_write_offset) {
        local_file
            .write_all(&chunk)
            .context("Failed to write to local file")?;
        next_write_offset += chunk.len() as u64;
    }

    local_file.flush().context("Failed to flush local file")?;
    Ok(())
}

/// Pipelined SFTP upload.
///
/// Reads the local file sequentially and fires `MAX_PIPELINE_DEPTH` concurrent
/// `SSH_FXP_WRITE` requests. Progress is reported as write ACKs arrive.
pub async fn upload<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    remote_path: &str,
    local_file: &mut (impl Read + Send),
    total_size: u64,
    chunk_size: u64,
    mut on_bytes_written: F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    // Open/create remote file.
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = raw
        .open(remote_path, flags, FileAttributes::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create remote file: {e}"))?;
    let handle_str: Arc<str> = handle.handle.into();

    let t0 = std::time::Instant::now();

    let result = upload_inner(
        raw,
        Arc::clone(&handle_str),
        local_file,
        total_size,
        chunk_size,
        &mut on_bytes_written,
        cancel,
    )
    .await;

    let dt = t0.elapsed().as_secs_f64();
    if dt > 0.1 {
        let mbps = total_size as f64 / dt / 1_048_576.0;
        tracing::debug!(
            "pipeline::upload {remote_path}: {:.1} MB in {dt:.1}s = {mbps:.1} MB/s",
            total_size as f64 / 1_048_576.0,
        );
    }

    // Always close (flushes data on server side).
    let _ = raw.close(handle_str.as_ref()).await;

    result
}

/// Inner upload loop.
async fn upload_inner<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: Arc<str>,
    local_file: &mut (impl Read + Send),
    total_size: u64,
    chunk_size: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut offset = 0u64;
    let mut inflight = FuturesUnordered::new();

    // Adaptive pipeline depth for small files.
    let total_chunks = total_size.div_ceil(chunk_size) as usize;
    let depth = total_chunks.min(MAX_PIPELINE_DEPTH);

    // Pre-allocate a reusable read buffer to avoid per-chunk allocation.
    let mut read_buf = vec![0u8; chunk_size as usize];

    // Seed the pipeline.
    while inflight.len() < depth && offset < total_size {
        let len = std::cmp::min(chunk_size, total_size - offset) as usize;
        let n = read_chunk_into(local_file, &mut read_buf, len)?;
        if n == 0 {
            break;
        }
        let data = read_buf[..n].to_vec();
        queue_write(&mut inflight, raw, Arc::clone(&handle_str), offset, data);
        offset += n as u64;
    }

    // Process ACKs and refill.
    while let Some(write_result) = inflight.next().await {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            anyhow::bail!("Transfer cancelled");
        }

        let bytes_written = write_result.map_err(|e| anyhow::anyhow!("SFTP write failed: {e}"))?;
        on_bytes_written(bytes_written as u64);

        // Refill.
        if offset < total_size {
            let len = std::cmp::min(chunk_size, total_size - offset) as usize;
            let n = read_chunk_into(local_file, &mut read_buf, len)?;
            if n > 0 {
                let data = read_buf[..n].to_vec();
                queue_write(&mut inflight, raw, Arc::clone(&handle_str), offset, data);
                offset += n as u64;
            }
        }
    }

    Ok(())
}

/// Read up to `len` bytes from a file into `buf`. Returns bytes read.
/// Reuses the caller's buffer to avoid per-chunk allocation overhead.
fn read_chunk_into(file: &mut impl Read, buf: &mut [u8], len: usize) -> Result<usize> {
    let target = len.min(buf.len());
    let mut filled = 0;
    while filled < target {
        let n = file
            .read(&mut buf[filled..target])
            .context("Failed to read local file")?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// (offset, expected_len, result) — expected_len lets us detect short reads.
type ReadRequest = std::pin::Pin<
    Box<dyn Future<Output = (u64, u64, Result<russh_sftp::protocol::Data, SftpError>)> + Send>,
>;
type WriteRequest = std::pin::Pin<Box<dyn Future<Output = Result<usize, SftpError>> + Send>>;

/// Queue a pipelined SFTP read request without spawning a task.
fn queue_read(
    inflight: &mut FuturesUnordered<ReadRequest>,
    raw: &Arc<RawSftpSession>,
    handle_str: Arc<str>,
    offset: u64,
    len: u64,
) {
    let r = Arc::clone(raw);
    inflight.push(Box::pin(async move {
        (
            offset,
            len,
            r.read(handle_str.as_ref(), offset, len as u32).await,
        )
    }));
}

/// Queue a pipelined SFTP write request without spawning a task.
fn queue_write(
    inflight: &mut FuturesUnordered<WriteRequest>,
    raw: &Arc<RawSftpSession>,
    handle_str: Arc<str>,
    offset: u64,
    data: Vec<u8>,
) {
    let r = Arc::clone(raw);
    let len = data.len();
    inflight.push(Box::pin(async move {
        r.write(handle_str.as_ref(), offset, data).await?;
        Ok(len)
    }));
}

// === Handle-based API for prefetching open/close across files ===

/// Open a remote file for reading. Returns the SFTP handle string.
pub async fn open_read(raw: &RawSftpSession, path: &str) -> Result<Arc<str>> {
    let handle = raw
        .open(path, OpenFlags::READ, FileAttributes::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open remote file: {e}"))?;
    Ok(handle.handle.into())
}

/// Open a remote file for writing (create/truncate). Returns the SFTP handle string.
pub async fn open_write(raw: &RawSftpSession, path: &str) -> Result<Arc<str>> {
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = raw
        .open(path, flags, FileAttributes::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create remote file: {e}"))?;
    Ok(handle.handle.into())
}

/// Close an SFTP file handle.
pub async fn close_handle(raw: &RawSftpSession, handle: &str) {
    let _ = raw.close(handle).await;
}

/// Pipelined download using a pre-opened handle. Does NOT open or close the handle.
#[allow(clippy::too_many_arguments)]
pub async fn download_from_handle<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: &Arc<str>,
    local_file: &mut (impl Write + Send),
    total_size: u64,
    start_offset: u64,
    chunk_size: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    if total_size == 0 || start_offset >= total_size {
        return Ok(());
    }
    download_inner(
        raw,
        Arc::clone(handle_str),
        local_file,
        total_size,
        start_offset,
        chunk_size,
        on_bytes_written,
        cancel,
    )
    .await
}

/// Pipelined upload using a pre-opened handle. Does NOT open or close the handle.
pub async fn upload_from_handle<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: &Arc<str>,
    local_file: &mut (impl Read + Send),
    total_size: u64,
    chunk_size: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    upload_inner(
        raw,
        Arc::clone(handle_str),
        local_file,
        total_size,
        chunk_size,
        on_bytes_written,
        cancel,
    )
    .await
}
