/// Cancellable stdin reader that uses an OS thread with `poll()`.
///
/// Unlike `tokio::io::stdin()` which uses `spawn_blocking` (leaving an
/// unkillable thread on abort), this reader uses a self-pipe to wake a
/// `poll()` call, guaranteeing the thread exits when `stop()` is called.
#[cfg(unix)]
use std::os::unix::io::RawFd;

/// Read buffer size for stdin data.
const READ_BUF_SIZE: usize = 1024;

/// A cancellable stdin reader backed by an OS thread.
///
/// On Unix, uses `libc::poll()` on both stdin (fd 0) and a wake pipe.
/// Writing to the wake pipe unblocks `poll()`, allowing the thread to
/// exit cleanly. The `Drop` impl calls `stop()` as a safety net.
#[cfg(unix)]
pub struct StdinReader {
    /// Write end of the wake pipe. A single byte wakes the poll loop.
    wake_fd: RawFd,
    /// Read end of the wake pipe (kept open so the fd stays valid).
    _wake_read_fd: RawFd,
    /// Join handle for the reader thread.
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl StdinReader {
    /// Spawn a background thread that reads stdin and sends data to `tx`.
    ///
    /// The thread `poll()`s on stdin and a wake pipe. When stdin has data,
    /// it reads and sends chunks to the channel. Call `stop()` to wake the
    /// pipe and join the thread.
    pub fn spawn(tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        // Create the self-pipe used to signal shutdown
        let mut fds = [0 as libc::c_int; 2];
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert!(ret == 0, "pipe() failed");

        let wake_read_fd = fds[0];
        let wake_write_fd = fds[1];

        // Set close-on-exec for both ends so child processes don't inherit them
        unsafe {
            libc::fcntl(wake_read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(wake_write_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let thread = std::thread::Builder::new()
            .name("stdin-reader".into())
            .spawn(move || {
                Self::reader_loop(wake_read_fd, tx);
                // Close read end when thread exits
                unsafe { libc::close(wake_read_fd) };
            })
            .expect("failed to spawn stdin-reader thread");

        Self {
            wake_fd: wake_write_fd,
            _wake_read_fd: wake_read_fd,
            thread: Some(thread),
        }
    }

    /// Signal the reader thread to stop and wait for it to exit.
    ///
    /// Writes a byte to the wake pipe, which unblocks `poll()` in the
    /// reader thread. Then joins the thread, guaranteeing it is dead
    /// before this method returns.
    pub fn stop(&mut self) {
        if self.wake_fd >= 0 {
            // Write a single byte to unblock poll()
            unsafe { libc::write(self.wake_fd, [1u8].as_ptr().cast(), 1) };
            // Close write end — reader will see POLLHUP if write didn't wake it
            unsafe { libc::close(self.wake_fd) };
            self.wake_fd = -1;
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    /// The poll loop running on the background thread.
    fn reader_loop(wake_fd: RawFd, tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
        let stdin_fd: RawFd = 0;
        let mut buf = [0u8; READ_BUF_SIZE];

        loop {
            let mut poll_fds = [
                libc::pollfd {
                    fd: stdin_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            // Block until stdin or wake pipe is readable (no timeout)
            let ret = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, -1) };
            if ret < 0 {
                // EINTR — signal interrupted poll, retry
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }

            // Wake pipe signalled — time to exit
            if poll_fds[1].revents != 0 {
                break;
            }

            // Stdin is readable
            if poll_fds[0].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break; // EOF or error
                }
                let data = buf[..n as usize].to_vec();
                if tx.blocking_send(data).is_err() {
                    break; // Receiver dropped
                }
            }

            // POLLHUP/POLLERR on stdin — EOF
            if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                break;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for StdinReader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Non-Unix fallback: uses `tokio::io::stdin()` (Windows stdin can't be polled
/// with Unix pipes, so we accept the spawn_blocking limitation there).
#[cfg(not(unix))]
pub struct StdinReader {
    handle: tokio::task::JoinHandle<()>,
}

#[cfg(not(unix))]
impl StdinReader {
    pub fn spawn(tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        let handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; READ_BUF_SIZE];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { handle }
    }

    pub fn stop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(not(unix))]
impl Drop for StdinReader {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_stop_is_idempotent() {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut reader = StdinReader::spawn(tx);
        // Calling stop twice should not panic
        reader.stop();
        reader.stop();
    }
}
