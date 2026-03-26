/// Pipelined SFTP transfers.
///
/// SFTP protocol allows multiple in-flight requests on a single channel.
/// Like OpenSSH's sftp client, this module keeps a bounded set of in-flight
/// requests without spawning a Tokio task per chunk.
///
/// **Architecture**: Fire N concurrent `SSH_FXP_READ` requests via a bounded
/// future queue. Responses may arrive out of order, so a reorder buffer
/// (`BTreeMap<offset, data>`) ensures chunks are written sequentially from
/// `start_offset`. This guarantees the local file always contains contiguous
/// data from byte 0, making `.part` file size a safe resume point on cancel.
/// Peak memory is bounded by pipeline depth × chunk_size (~2 MB).
use std::collections::BTreeMap;
use std::future::Future;
use std::io::{Read, Seek, Write};
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

/// Maximum concurrent in-flight SFTP requests (hard cap).
const MAX_PIPELINE_DEPTH: usize = 64;

/// Target upper bound on in-flight bytes across all concurrent requests.
/// Matches SSH_WINDOW_SIZE (2 MB) — the SSH channel backpressure limit.
/// The reorder buffer holds at most this many bytes before they are written
/// sequentially. 2 MB in-flight saturates most links (at 100 ms RTT,
/// covers ~160 Mbps — typical SSH single-connection throughput).
const MAX_INFLIGHT_BYTES: u64 = 2 * 1024 * 1024;

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
/// Fires up to `MAX_PIPELINE_DEPTH` concurrent `SSH_FXP_READ` requests.
/// Responses may arrive out of order; a reorder buffer ensures chunks are
/// written to the local file sequentially from `start_offset`. This makes
/// the file always contain contiguous data, so its size is a safe resume
/// point on cancel.
///
/// `chunk_size` should come from `PipelinedSession::read_chunk_size` to
/// match the server's negotiated limits and avoid short reads.
///
/// `on_bytes_written` is called as soon as each chunk arrives (for progress
/// display), regardless of whether it's written immediately or buffered.
#[allow(clippy::too_many_arguments)]
pub async fn download<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    remote_path: &str,
    local_file: &mut (impl Write + Seek + Send),
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
///
/// Uses a reorder buffer (`BTreeMap<offset, data>`) to ensure chunks are
/// written sequentially. When a response arrives:
/// - If offset == write_cursor: write directly and drain any contiguous
///   buffered chunks.
/// - If offset > write_cursor: buffer for later.
///
/// This guarantees the file always contains contiguous data from byte 0,
/// making file size a safe resume point. Peak memory is bounded by
/// pipeline depth × chunk_size (~2 MB with 8 × 256 KB).
#[allow(clippy::too_many_arguments)]
async fn download_inner<F: FnMut(u64)>(
    raw: &Arc<RawSftpSession>,
    handle_str: Arc<str>,
    local_file: &mut (impl Write + Seek + Send),
    total_size: u64,
    start_offset: u64,
    chunk_size: u64,
    on_bytes_written: &mut F,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut next_request_offset = start_offset;
    let mut inflight = FuturesUnordered::new();

    // Adaptive pipeline depth: cap by in-flight bytes so larger chunks use fewer
    // concurrent requests, keeping memory bounded regardless of server limits.
    let remaining = total_size - start_offset;
    let total_chunks = remaining.div_ceil(chunk_size) as usize;
    let by_bytes = (MAX_INFLIGHT_BYTES / chunk_size).max(8) as usize;
    let depth = total_chunks.min(MAX_PIPELINE_DEPTH).min(by_bytes);

    // Reorder buffer: holds out-of-order chunks until they can be written
    // sequentially. Keyed by absolute file offset.
    let mut reorder_buf: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    // Next sequential offset to write. All bytes before this are on disk.
    let mut write_cursor = start_offset;

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
            // Drain this chunk + all already-resolved futures so the .part
            // file preserves as much contiguous data as possible.
            // Helper: buffer or write a chunk, drain contiguous.
            let mut flush_chunk = |off: u64, bytes: &[u8]| {
                if off == write_cursor {
                    let _ = local_file.write_all(bytes);
                    write_cursor += bytes.len() as u64;
                    while let Some(entry) = reorder_buf.first_entry() {
                        if *entry.key() != write_cursor {
                            break;
                        }
                        let buf = entry.remove();
                        let _ = local_file.write_all(&buf);
                        write_cursor += buf.len() as u64;
                    }
                } else if off > write_cursor {
                    reorder_buf.insert(off, bytes.to_vec());
                }
            };
            // Process the current chunk.
            if let Ok(data) = sftp_result {
                flush_chunk(offset, &data.data);
            }
            // Drain all remaining already-resolved futures (no await).
            use futures::FutureExt;
            while let Some(Some((off, _expected, result))) = inflight.next().now_or_never() {
                if let Ok(data) = result {
                    flush_chunk(off, &data.data);
                }
            }
            // Final drain of contiguous reorder buffer.
            while let Some(entry) = reorder_buf.first_entry() {
                if *entry.key() != write_cursor {
                    break;
                }
                let buf = entry.remove();
                let _ = local_file.write_all(&buf);
                write_cursor += buf.len() as u64;
            }
            let _ = local_file.flush();
            anyhow::bail!("Transfer cancelled");
        }

        let data = sftp_result.map_err(|e| anyhow::anyhow!("SFTP read failed: {e}"))?;
        let received_len = data.data.len() as u64;

        // Report progress as soon as data arrives, so the UI speed display
        // reflects actual network throughput.
        on_bytes_written(received_len);

        // If the server returned less data than requested (short read),
        // re-request the remaining range to fill the gap.
        let is_gap_fill = received_len < expected_len && offset + received_len < total_size;
        if is_gap_fill {
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

        // Buffer or write the chunk depending on whether it's the next sequential one.
        if offset == write_cursor {
            // This chunk is next in sequence — write directly.
            local_file
                .write_all(&data.data)
                .context("Failed to write to local file")?;
            write_cursor += received_len;

            // Drain any buffered chunks that are now contiguous.
            while let Some(entry) = reorder_buf.first_entry() {
                if *entry.key() != write_cursor {
                    break;
                }
                let buffered = entry.remove();
                local_file
                    .write_all(&buffered)
                    .context("Failed to write to local file")?;
                write_cursor += buffered.len() as u64;
            }
        } else {
            // Out of order — buffer for later.
            reorder_buf.insert(offset, data.data.to_vec());
        }

        // Periodic debug: track write_cursor vs reorder buffer divergence.
        // Refill pipeline with new chunks beyond what's already requested.
        // Skip refill for gap-fill responses — they're re-requests for ranges
        // already counted in next_request_offset. Without this guard, each short
        // read produces 2 requests (gap + new), causing inflight to grow unbounded.
        if !is_gap_fill && next_request_offset < total_size {
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

    // Adaptive pipeline depth: cap by in-flight bytes (same as download).
    let total_chunks = total_size.div_ceil(chunk_size) as usize;
    let by_bytes = (MAX_INFLIGHT_BYTES / chunk_size).max(8) as usize;
    let depth = total_chunks.min(MAX_PIPELINE_DEPTH).min(by_bytes);

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
    local_file: &mut (impl Write + Seek + Send),
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- read_chunk_into tests ---

    #[test]
    fn test_read_chunk_into_full_read() {
        let data = b"hello world";
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let mut buf = [0u8; 64];
        let n = read_chunk_into(&mut cursor, &mut buf, 11).unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf[..n], b"hello world");
    }

    #[test]
    fn test_read_chunk_into_eof_before_len() {
        // File has 5 bytes but we ask for 100 — should return 5, not error.
        let data = b"short";
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let mut buf = [0u8; 128];
        let n = read_chunk_into(&mut cursor, &mut buf, 100).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"short");
    }

    #[test]
    fn test_read_chunk_into_zero_len() {
        let data = b"data";
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let mut buf = [0u8; 64];
        let n = read_chunk_into(&mut cursor, &mut buf, 0).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_read_chunk_into_len_exceeds_buf() {
        // len > buf.len() — should cap at buf size.
        let data = vec![0xABu8; 200];
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let mut buf = [0u8; 64];
        let n = read_chunk_into(&mut cursor, &mut buf, 200).unwrap();
        assert_eq!(n, 64);
    }

    /// A reader that returns an I/O error on the first read.
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "simulated read failure",
            ))
        }
    }

    #[test]
    fn test_read_chunk_into_io_error() {
        let mut reader = FailingReader;
        let mut buf = [0u8; 64];
        let result = read_chunk_into(&mut reader, &mut buf, 64);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("Failed to read local file"));
    }

    /// A reader that succeeds for N bytes then errors.
    struct PartialThenFailReader {
        good_bytes: usize,
        delivered: usize,
    }

    impl Read for PartialThenFailReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.delivered >= self.good_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection lost mid-read",
                ));
            }
            let n = buf.len().min(self.good_bytes - self.delivered);
            buf[..n].fill(0x42);
            self.delivered += n;
            Ok(n)
        }
    }

    #[test]
    fn test_read_chunk_into_error_after_partial() {
        // Delivers 10 bytes successfully, then errors.
        let mut reader = PartialThenFailReader {
            good_bytes: 10,
            delivered: 0,
        };
        let mut buf = [0u8; 64];
        let result = read_chunk_into(&mut reader, &mut buf, 64);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("Failed to read local file"));
    }

    /// A reader that returns 1 byte at a time (simulates slow/fragmented reads).
    struct SlowReader {
        data: Vec<u8>,
        pos: usize,
    }

    impl Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    #[test]
    fn test_read_chunk_into_byte_at_a_time() {
        let mut reader = SlowReader {
            data: b"abcdef".to_vec(),
            pos: 0,
        };
        let mut buf = [0u8; 64];
        let n = read_chunk_into(&mut reader, &mut buf, 6).unwrap();
        assert_eq!(n, 6);
        assert_eq!(&buf[..6], b"abcdef");
    }

    // --- Reorder buffer logic tests ---
    //
    // These test the reorder buffer pattern used in download_inner by
    // simulating it directly (since download_inner requires a real SFTP
    // session). The logic is: maintain a BTreeMap of out-of-order chunks
    // and a write_cursor, writing sequentially.

    /// Simulate the reorder buffer logic from download_inner.
    /// Takes (offset, data) pairs in arrival order and returns the
    /// final file contents.
    fn simulate_reorder_download(chunks: Vec<(u64, Vec<u8>)>, start_offset: u64) -> Vec<u8> {
        let mut output = Vec::new();
        let mut reorder_buf: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut write_cursor = start_offset;

        for (offset, data) in chunks {
            if offset == write_cursor {
                output.extend_from_slice(&data);
                write_cursor += data.len() as u64;

                while let Some(entry) = reorder_buf.first_entry() {
                    if *entry.key() != write_cursor {
                        break;
                    }
                    let buffered = entry.remove();
                    output.extend_from_slice(&buffered);
                    write_cursor += buffered.len() as u64;
                }
            } else {
                reorder_buf.insert(offset, data);
            }
        }

        output
    }

    #[test]
    fn test_reorder_buffer_in_order() {
        // Chunks arrive in order — should be written directly with no buffering.
        let chunks = vec![
            (0, b"AAAA".to_vec()),
            (4, b"BBBB".to_vec()),
            (8, b"CCCC".to_vec()),
        ];
        let result = simulate_reorder_download(chunks, 0);
        assert_eq!(result, b"AAAABBBBCCCC");
    }

    #[test]
    fn test_reorder_buffer_reversed() {
        // Chunks arrive in reverse order — all buffered until the first one arrives.
        let chunks = vec![
            (8, b"CCCC".to_vec()),
            (4, b"BBBB".to_vec()),
            (0, b"AAAA".to_vec()),
        ];
        let result = simulate_reorder_download(chunks, 0);
        assert_eq!(result, b"AAAABBBBCCCC");
    }

    #[test]
    fn test_reorder_buffer_interleaved() {
        // Middle chunk arrives first, then first, then last.
        let chunks = vec![
            (10, b"BB".to_vec()),
            (0, b"AAAAAAAAAA".to_vec()), // 10 bytes — triggers drain of offset 10
            (12, b"CC".to_vec()),
        ];
        let result = simulate_reorder_download(chunks, 0);
        assert_eq!(result, b"AAAAAAAAAABBCC");
    }

    #[test]
    fn test_reorder_buffer_with_start_offset() {
        // Resume scenario: start_offset = 100. Chunks keyed by absolute offset.
        let chunks = vec![
            (110, b"BB".to_vec()),         // out of order
            (100, b"AAAAAAAAAA".to_vec()), // triggers write + drain
            (112, b"CC".to_vec()),
        ];
        let result = simulate_reorder_download(chunks, 100);
        assert_eq!(result, b"AAAAAAAAAABBCC");
    }

    #[test]
    fn test_reorder_buffer_single_chunk() {
        let chunks = vec![(0, b"ONLY".to_vec())];
        let result = simulate_reorder_download(chunks, 0);
        assert_eq!(result, b"ONLY");
    }

    #[test]
    fn test_reorder_buffer_many_out_of_order() {
        // 8 chunks of 4 bytes each, arriving in a scrambled order.
        let chunks = vec![
            (16, b"EEEE".to_vec()),
            (28, b"HHHH".to_vec()),
            (4, b"BBBB".to_vec()),
            (20, b"FFFF".to_vec()),
            (0, b"AAAA".to_vec()), // triggers drain of 4
            (12, b"DDDD".to_vec()),
            (8, b"CCCC".to_vec()),  // triggers drain of 8, 12, 16, 20
            (24, b"GGGG".to_vec()), // triggers drain of 24, 28
        ];
        let result = simulate_reorder_download(chunks, 0);
        assert_eq!(result, b"AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH");
    }
}
