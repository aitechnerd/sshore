use std::ffi::c_void;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::errno::Errno;
use nix::sys::mman::{MapFlags, MmapAdvise, ProtFlags};

use super::MemoryLockError;

/// Unlock memory on drop for Unix-based systems.
#[allow(dead_code)]
pub fn munlock(ptr: *const u8, len: usize) -> Result<(), MemoryLockError> {
    unsafe {
        Errno::clear();
        let ptr = NonNull::new_unchecked(ptr as *mut c_void);
        nix::sys::mman::munlock(ptr, len).map_err(|e| {
            MemoryLockError::new(format!("munlock: {} (0x{:x})", e.desc(), e as i32))
        })?;
    }
    Ok(())
}

pub fn mlock(ptr: *const u8, len: usize) -> Result<(), MemoryLockError> {
    unsafe {
        Errno::clear();
        let ptr = NonNull::new_unchecked(ptr as *mut c_void);
        nix::sys::mman::mlock(ptr, len)
            .map_err(|e| MemoryLockError::new(format!("mlock: {} (0x{:x})", e.desc(), e as i32)))?;
    }
    Ok(())
}

/// Allocate `len` bytes via `mmap` (bypassing the global allocator).
///
/// Returns a page-aligned, zeroed, mlocked allocation. Pages are returned
/// to the OS immediately on `mmap_dealloc` via `munmap`, avoiding the
/// RSS inflation that occurs when freed pages linger in jemalloc's caches.
pub fn mmap_alloc(len: usize) -> Result<NonNull<u8>, MemoryLockError> {
    let len = NonZeroUsize::new(len).ok_or_else(|| MemoryLockError::new("mmap_alloc: zero length".into()))?;
    unsafe {
        let ptr = nix::sys::mman::mmap_anonymous(
            None,
            len,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE,
        )
        .map_err(|e| MemoryLockError::new(format!("mmap: {} (0x{:x})", e.desc(), e as i32)))?;

        // mmap returns zeroed memory, now mlock it.
        let raw = ptr.as_ptr() as *mut u8;
        let nn = NonNull::new_unchecked(raw);
        nix::sys::mman::mlock(ptr, len.into()).map_err(|e| {
            // Clean up on mlock failure.
            let _ = nix::sys::mman::munmap(ptr, len.into());
            MemoryLockError::new(format!("mlock after mmap: {} (0x{:x})", e.desc(), e as i32))
        })?;
        Ok(nn)
    }
}

/// Free an `mmap_alloc`'d region. Zeroes, munlocks, then munmaps.
///
/// # Safety
/// `ptr` must have been returned by `mmap_alloc` with the same `len`.
pub unsafe fn mmap_dealloc(ptr: NonNull<u8>, len: usize) {
    if len == 0 {
        return;
    }
    unsafe {
        // Zeroize before releasing.
        std::ptr::write_bytes(ptr.as_ptr(), 0, len);
        // Optimization barrier so the compiler doesn't elide the zeroing.
        core::arch::asm!("# {}", in(reg) ptr.as_ptr(), options(readonly, preserves_flags, nostack));

        let void_ptr = NonNull::new_unchecked(ptr.as_ptr() as *mut c_void);
        let _ = nix::sys::mman::munlock(void_ptr, len);

        // On macOS, use MADV_FREE before munmap to hint the kernel.
        #[cfg(target_os = "macos")]
        {
            let _ = nix::sys::mman::madvise(void_ptr, len, MmapAdvise::MADV_FREE);
        }

        let _ = nix::sys::mman::munmap(void_ptr, len);
    }
}
