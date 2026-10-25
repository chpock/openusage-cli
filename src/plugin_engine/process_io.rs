use std::io;
use std::process::{Command, Output};
use std::time::Duration;

/// Runs a finite helper job, including its inherited pipes, under one deadline.
/// Background work in the helper's process group is killed even after success.
#[cfg(target_os = "linux")]
pub(super) fn run_command(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<Output> {
    linux::run_command(command, input, timeout)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn run_command(
    _command: &mut Command,
    _input: Option<&[u8]>,
    _timeout: Duration,
) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "deadline-bound helper execution requires Linux",
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    struct OwnedChild {
        child: Child,
        group_cleaned: bool,
        reaped: bool,
    }

    impl OwnedChild {
        fn cleanup(&mut self) -> io::Result<()> {
            // Do not reap the leader before signaling: its unreaped PID reserves the PGID.
            let group_result = if self.group_cleaned {
                Ok(())
            } else if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } == 0 {
                self.group_cleaned = true;
                Ok(())
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    self.group_cleaned = true;
                    Ok(())
                } else {
                    // Still attempt direct-child cleanup if group signaling failed.
                    let _ = self.child.kill();
                    Err(error)
                }
            };
            let wait_result = self.child.wait();
            self.reaped = wait_result.is_ok();
            group_result.and(wait_result.map(|_| ()))
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.cleanup();
            }
        }
    }

    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn read_once(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> io::Result<bool> {
        let mut buffer = [0; 16384];
        match pipe.read(&mut buffer) {
            Ok(0) => Ok(true),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                Ok(false)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn exited(child: &Child) -> io::Result<bool> {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    pub(super) fn run_command(
        command: &mut Command,
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<Output> {
        let start = Instant::now();
        command
            .process_group(0)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut owned = OwnedChild {
            child: command.spawn()?,
            group_cleaned: false,
            reaped: false,
        };
        let result = (|| {
            let mut stdin = owned.child.stdin.take();
            let mut stdout = owned.child.stdout.take();
            let mut stderr = owned.child.stderr.take();
            if let Some(pipe) = &stdin {
                nonblocking(pipe)?;
            }
            nonblocking(stdout.as_ref().expect("piped stdout"))?;
            nonblocking(stderr.as_ref().expect("piped stderr"))?;
            let mut remaining_input = input.unwrap_or_default();
            let mut out = Vec::new();
            let mut err = Vec::new();
            loop {
                if start.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "helper execution timed out",
                    ));
                }
                // Each stream gets one bounded operation so busy pipes cannot starve the deadline.
                if remaining_input.is_empty() {
                    stdin.take();
                }
                if let Some(pipe) = &mut stdin {
                    match pipe.write(&remaining_input[..remaining_input.len().min(16384)]) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "helper stdin write returned zero",
                            ));
                        }
                        Ok(count) => remaining_input = &remaining_input[count..],
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => return Err(error),
                    }
                    if remaining_input.is_empty() {
                        stdin.take();
                    }
                }
                if let Some(pipe) = &mut stdout
                    && read_once(pipe, &mut out)?
                {
                    stdout.take();
                }
                if let Some(pipe) = &mut stderr
                    && read_once(pipe, &mut err)?
                {
                    stderr.take();
                }
                if exited(&owned.child)? && stdin.is_none() && stdout.is_none() && stderr.is_none()
                {
                    return Ok((out, err));
                }
                let mut polls = Vec::with_capacity(3);
                if let Some(pipe) = &stdin {
                    polls.push(libc::pollfd {
                        fd: pipe.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    });
                }
                if let Some(pipe) = &stdout {
                    polls.push(libc::pollfd {
                        fd: pipe.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                }
                if let Some(pipe) = &stderr {
                    polls.push(libc::pollfd {
                        fd: pipe.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                }
                let remaining = timeout.saturating_sub(start.elapsed());
                let milliseconds = remaining.as_millis().min(10) as i32;
                let ready = unsafe {
                    libc::poll(
                        polls.as_mut_ptr(),
                        polls.len() as libc::nfds_t,
                        milliseconds,
                    )
                };
                if ready == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        })();
        // Pipe handles have been dropped before cleanup; no readers or writers outlive this call.
        let cleanup = owned.cleanup();
        match (result, cleanup) {
            (Ok((stdout, stderr)), Ok(())) => Ok(Output {
                status: owned.child.wait()?,
                stdout,
                stderr,
            }),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(io::Error::new(
                cleanup.kind(),
                format!("helper cleanup failed: {cleanup}; execution failed: {error}"),
            )),
        }
    }
}
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::time::Instant;

    fn shell(root: &Path, script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .current_dir(root)
            .env_clear()
            .env("HOME", root);
        command
    }

    #[test]
    fn run_command_preserves_large_binary_streams_and_nonzero_status() {
        let root = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();
        fs::write(root.path().join("stderr"), &bytes).unwrap();
        let output = run_command(
            &mut shell(root.path(), "/bin/cat stderr >&2; /bin/cat; exit 23"),
            Some(&bytes),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stdout, bytes);
        assert_eq!(output.stderr, bytes);
    }

    #[test]
    fn run_command_closes_absent_stdin_and_preserves_success() {
        let root = tempfile::tempdir().unwrap();
        let output = run_command(
            &mut shell(root.path(), "/bin/cat; printf done"),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"done");
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn run_command_reports_spawn_failure() {
        let root = tempfile::tempdir().unwrap();
        let error = run_command(
            &mut Command::new(root.path().join("absent")),
            None,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    // A pidfd pins the fixture identity, so panic cleanup never signals a recycled PID.
    struct Descendant {
        pid: i32,
        fd: OwnedFd,
    }

    impl Descendant {
        fn open(root: &Path) -> Self {
            let record = fs::read_to_string(root.join("descendant")).unwrap();
            let mut fields = record.split_whitespace();
            let pid: i32 = fields.next().unwrap().parse().unwrap();
            let start = fields.next().unwrap();
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            assert!(fd >= 0, "fixture pidfd: {}", io::Error::last_os_error());
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let descendant = Self { pid, fd: owned };
            assert_eq!(
                stat.rsplit_once(") ")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .nth(19)
                    .unwrap(),
                start
            );
            fs::write(root.join("observed"), "").unwrap();
            descendant
        }

        fn assert_exited(&self) {
            let mut poll = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 1000) };
            assert_eq!(
                ready, 1,
                "owned descendant {} survived helper return",
                self.pid
            );
            assert_ne!(poll.revents & libc::POLLIN, 0);
        }
    }

    impl Drop for Descendant {
        fn drop(&mut self) {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }

    fn fixture(root: &Path, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "plugin_engine::process_io::tests::process_fixture",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", root)
            .env("PROCESS_IO_FIXTURE", mode)
            .current_dir(root);
        command
    }

    #[test]
    // Deliberately orphaned to test group cleanup; the observing test owns a pidfd kill guard.
    #[allow(clippy::zombie_processes)]
    fn process_fixture() {
        let Ok(mode) = std::env::var("PROCESS_IO_FIXTURE") else {
            return;
        };
        let mut sleep = Command::new("/bin/sleep");
        sleep.arg("10").stdin(std::process::Stdio::null());
        if matches!(mode.as_str(), "close-pipes" | "wait-without-pipes") {
            sleep
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let child = sleep.spawn().unwrap();
        let pid = child.id();
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let start = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        fs::write("descendant", format!("{pid} {start}")).unwrap();
        fs::write(
            "group",
            format!(
                "{} {} {}",
                std::process::id(),
                unsafe { libc::getpgrp() },
                unsafe { libc::getpgid(pid as i32) }
            ),
        )
        .unwrap();
        while !Path::new("observed").exists() {
            std::thread::sleep(Duration::from_millis(1));
        }
        match mode.as_str() {
            "exit-with-pipe" => std::process::exit(0),
            "close-pipes" => {
                unsafe {
                    libc::close(1);
                    libc::close(2);
                }
                std::process::exit(0);
            }
            "wait-without-pipes" => {
                unsafe {
                    libc::close(1);
                    libc::close(2);
                }
                std::thread::sleep(Duration::from_secs(10));
            }
            "unread-input" => std::thread::sleep(Duration::from_secs(10)),
            "output" => {
                use std::io::Write;
                let mut stdout = io::stdout().lock();
                loop {
                    if stdout.write_all(&[b'x'; 16384]).is_err() {
                        break;
                    }
                }
            }
            "broken-input" => {
                unsafe {
                    libc::close(0);
                }
                std::thread::sleep(Duration::from_secs(10));
            }
            _ => panic!("unknown fixture mode"),
        }
    }

    fn observe(root: &Path) -> std::thread::JoinHandle<Descendant> {
        let root = root.to_owned();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !root.join("group").exists() {
                assert!(
                    Instant::now() < deadline,
                    "fixture did not publish process identities"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Descendant::open(&root)
        })
    }

    struct UnrelatedChild(std::process::Child);

    impl Drop for UnrelatedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn timeout_fixture(mode: &str, input: Option<&[u8]>) {
        let root = tempfile::tempdir().unwrap();
        let mut unrelated = UnrelatedChild(
            Command::new("/bin/sleep")
                .arg("10")
                .current_dir(root.path())
                .env_clear()
                .env("HOME", root.path())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let observer = observe(root.path());
        let start = Instant::now();
        let result = run_command(
            &mut fixture(root.path(), mode),
            input,
            Duration::from_millis(400),
        );
        let descendant = observer.join().unwrap();
        descendant.assert_exited();
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "unrelated owned control process was killed"
        );
        let group = fs::read_to_string(root.path().join("group")).unwrap();
        let ids: Vec<i32> = group
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[1], ids[2]);
        assert_ne!(ids[1], unsafe { libc::getpgrp() });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn run_command_deadline_includes_child_wait_after_pipe_eof() {
        timeout_fixture("wait-without-pipes", None);
    }

    #[test]
    fn run_command_deadline_includes_descendant_pipe_eof() {
        timeout_fixture("exit-with-pipe", None);
    }

    #[test]
    fn run_command_deadline_includes_unread_large_stdin() {
        timeout_fixture("unread-input", Some(&vec![b's'; 2 * 1024 * 1024]));
    }

    #[test]
    fn run_command_deadline_is_not_starved_by_sustained_output() {
        timeout_fixture("output", None);
    }

    #[test]
    fn run_command_success_cleans_background_descendants() {
        let root = tempfile::tempdir().unwrap();
        let observer = observe(root.path());
        let result = run_command(
            &mut fixture(root.path(), "close-pipes"),
            None,
            Duration::from_secs(5),
        );
        let descendant = observer.join().unwrap();
        descendant.assert_exited();
        assert!(result.unwrap().status.success());
    }

    #[test]
    fn run_command_io_failure_cleans_owned_group() {
        let root = tempfile::tempdir().unwrap();
        let observer = observe(root.path());
        let result = run_command(
            &mut fixture(root.path(), "broken-input"),
            Some(&vec![0; 2 * 1024 * 1024]),
            Duration::from_secs(5),
        );
        let descendant = observer.join().unwrap();
        descendant.assert_exited();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }
}
