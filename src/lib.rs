use std::sync::atomic::{AtomicBool, Ordering};

pub mod config;
pub mod keychain;
pub mod sftp;
pub mod ssh;
pub mod storage;
pub mod tui;

/// Global flag set by SIGHUP/SIGTERM handlers to signal the process should exit.
/// Checked by TUI event loops to break out of polling when the terminal is gone
/// (e.g., the terminal tab was closed), preventing a 100% CPU spin loop.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Install process-level signal handlers for SIGHUP and SIGTERM.
///
/// When the terminal emulator closes the tab, it destroys the PTY and sends
/// SIGHUP to the process group. Without this handler, the TUI event loop would
/// spin on the dead fd at 100% CPU because crossterm's `event::poll()` returns
/// immediately (POLLHUP) and `event::read()` returns ignorable events.
///
/// Must be called before entering the TUI event loop, and re-called after SSH
/// sessions (which install their own tokio signal handlers that override ours).
#[cfg(unix)]
pub fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGHUP,
            signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            signal_handler as *const () as libc::sighandler_t,
        );
    }
}

/// Async-signal-safe handler: just sets an atomic flag.
#[cfg(unix)]
extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

#[cfg(not(unix))]
pub fn install_signal_handlers() {
    // On Windows, terminal close is handled differently (no SIGHUP).
}
