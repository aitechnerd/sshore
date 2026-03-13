use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::Error;

pub fn unix(time: SystemTime) -> u32 {
    DateTime::<Utc>::from(time).timestamp() as u32
}

/// A buffer backed by `mmap`/`munmap` so that freed pages are returned to
/// the OS immediately — avoiding the RSS inflation that occurs when jemalloc
/// uses `madvise(MADV_FREE)` on macOS (which merely marks pages reusable
/// without reducing RSS).
///
/// Used as the backing store for SFTP packet `Bytes` via `Bytes::from_owner`.
#[cfg(unix)]
struct MmapBuf {
    ptr: *mut u8,
    len: usize,
}

#[cfg(unix)]
unsafe impl Send for MmapBuf {}
#[cfg(unix)]
unsafe impl Sync for MmapBuf {}

#[cfg(unix)]
impl MmapBuf {
    fn alloc(len: usize) -> Result<Self, Error> {
        if len == 0 {
            return Ok(Self {
                ptr: std::ptr::NonNull::dangling().as_ptr(),
                len: 0,
            });
        }
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(Error::BadMessage("mmap failed".to_owned()));
            }
            Ok(Self {
                ptr: ptr as *mut u8,
                len,
            })
        }
    }
}

#[cfg(unix)]
impl AsRef<[u8]> for MmapBuf {
    fn as_ref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[cfg(unix)]
impl Drop for MmapBuf {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
            }
        }
    }
}

pub async fn read_packet<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<Bytes, Error> {
    let length = stream.read_u32().await?;
    let len = length as usize;

    #[cfg(unix)]
    {
        let buf = MmapBuf::alloc(len)?;
        // SAFETY: mmap returns zeroed memory, and read_exact will fill
        // exactly `len` bytes or return an error.
        unsafe {
            let slice = std::slice::from_raw_parts_mut(buf.ptr, buf.len);
            stream.read_exact(slice).await?;
        }
        Ok(Bytes::from_owner(buf))
    }

    #[cfg(not(unix))]
    {
        let mut buf = Vec::with_capacity(len);
        unsafe { buf.set_len(len) };
        stream.read_exact(&mut buf).await?;
        Ok(Bytes::from(buf))
    }
}
