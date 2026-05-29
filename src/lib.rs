use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

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

/// Shared signal channel: a single tokio task watches SIGHUP/SIGTERM and
/// broadcasts to all subscribers (TUI event loop, SSH proxy loop, etc.).
///
/// This replaces the old `libc::signal()` + per-session `tokio::signal` approach
/// that caused a race: `tokio::signal::unix::signal()` calls `sigaction()` and
/// replaces any `libc::signal()` handler, so the two approaches fought each other
/// and signals could be lost in the gap between handler replacement and re-registration.
#[cfg(unix)]
static SIGNAL_TX: std::sync::LazyLock<watch::Sender<bool>> =
    std::sync::LazyLock::new(|| watch::channel(false).0);

#[cfg(not(unix))]
static SIGNAL_TX: std::sync::LazyLock<watch::Sender<bool>> =
    std::sync::LazyLock::new(|| watch::channel(false).0);

/// Subscribe to the global signal channel. Returns a receiver that gets notified
/// when SIGHUP or SIGTERM is received.
pub fn subscribe_shutdown() -> watch::Receiver<bool> {
    SIGNAL_TX.subscribe()
}

/// Install the global signal watcher task (call once at startup).
/// Spawns a background tokio task that listens for SIGHUP and SIGTERM,
/// sets the `SHUTDOWN_REQUESTED` flag, and broadcasts to all subscribers.
#[cfg(unix)]
pub fn install_signal_handlers() {
    use tokio::signal::unix::{SignalKind, signal};

    let tx = SIGNAL_TX.clone();
    tokio::spawn(async move {
        let mut sighup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        loop {
            tokio::select! {
                _ = sighup.recv() => {
                    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                    let _ = tx.send(true);
                }
                _ = sigterm.recv() => {
                    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                    let _ = tx.send(true);
                }
            }
        }
    });
}

#[cfg(not(unix))]
pub fn install_signal_handlers() {
    // On Windows, terminal close is handled differently (no SIGHUP).
}
