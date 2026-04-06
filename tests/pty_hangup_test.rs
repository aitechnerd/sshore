#[cfg(unix)]
mod unix_only {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::RawFd;
    use std::thread;
    use std::time::{Duration, Instant};

    fn spawn_probe() -> io::Result<(libc::pid_t, RawFd)> {
        let mut master_fd = 0;
        let pid = unsafe {
            libc::forkpty(
                &mut master_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };

        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            let bin = CString::new(env!("CARGO_BIN_EXE_sshore")).expect("valid binary path");
            let arg0 = CString::new("sshore").expect("valid argv[0]");
            let arg1 = CString::new("_test-pty-hangup").expect("valid test subcommand");
            unsafe {
                libc::execl(
                    bin.as_ptr(),
                    arg0.as_ptr(),
                    arg1.as_ptr(),
                    std::ptr::null::<libc::c_char>(),
                );
                libc::_exit(127);
            }
        }

        Ok((pid, master_fd))
    }

    fn wait_for_ready(master_fd: RawFd, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 256];
        let mut output = Vec::new();

        while Instant::now() < deadline {
            let mut poll_fd = libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut poll_fd, 1, 100) };
            if rc == -1 {
                return Err(io::Error::last_os_error());
            }
            if rc == 0 {
                continue;
            }

            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n as usize]);
            if output
                .windows(b"READY".len())
                .any(|window| window == b"READY")
            {
                return Ok(());
            }
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "probe process did not become ready before timeout",
        ))
    }

    fn wait_for_exit(pid: libc::pid_t, timeout: Duration) -> io::Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut status = 0;
            let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if rc == -1 {
                return Err(io::Error::last_os_error());
            }
            if rc == pid {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    return Ok(std::process::ExitStatus::from_raw(status));
                }
            }
            thread::sleep(Duration::from_millis(25));
        }

        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "probe process did not exit after PTY hangup",
        ))
    }

    #[test]
    fn test_process_exits_when_pty_is_closed() -> Result<(), Box<dyn std::error::Error>> {
        let (pid, master_fd) = spawn_probe()?;
        wait_for_ready(master_fd, Duration::from_secs(5))?;

        unsafe {
            libc::close(master_fd);
        }

        let status = wait_for_exit(pid, Duration::from_secs(5))?;
        assert!(
            status.success(),
            "probe exited unsuccessfully after PTY hangup: {status}"
        );
        Ok(())
    }
}
