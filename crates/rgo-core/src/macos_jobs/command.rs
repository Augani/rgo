//! Wait for short-lived launchctl helpers without a polling delay or an
//! unbounded wait. A helper timeout never authorizes retiring a live receipt.

use anyhow::{Context, Result, bail, ensure};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, ExitStatus};
use std::time::Instant;

pub(super) fn run(command: &mut Command, deadline: Instant) -> Result<ExitStatus> {
    ensure!(Instant::now() < deadline, "launchctl deadline expired");
    let descriptor = unsafe { libc::kqueue() };
    ensure!(descriptor >= 0, "cannot observe launchctl completion");
    let queue = unsafe { OwnedFd::from_raw_fd(descriptor) };
    ensure!(
        unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "cannot close launchctl observer on exec"
    );
    let mut child = command.spawn().context("starting launchctl")?;
    let result = (|| -> Result<ExitStatus> {
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = child.id() as usize;
        change.filter = libc::EVFILT_PROC;
        change.flags = libc::EV_ADD | libc::EV_ONESHOT;
        change.fflags = libc::NOTE_EXIT;
        let poll = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            if unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    &poll,
                )
            } == 0
            {
                break;
            }
            let error = std::io::Error::last_os_error();
            // An immediate exit can win the observer registration race. Only
            // waitpid confirms completion; ESRCH alone is not an exit status.
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("registering launchctl observer");
            }
            ensure!(Instant::now() < deadline, "launchctl timed out");
        }
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "launchctl timed out");
            let timeout = libc::timespec {
                tv_sec: remaining.as_secs().try_into()?,
                tv_nsec: remaining.subsec_nanos().into(),
            };
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let ready = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("waiting for launchctl completion");
                }
            } else if ready > 0 && event.flags & libc::EV_ERROR != 0 {
                bail!("launchctl observer reported error {}", { event.data });
            }
        }
    })();
    if result.is_err() {
        // launchctl itself is a direct child; do not leave a blocked helper or
        // zombie behind. Job ownership/receipt cleanup is a separate decision.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    #[test]
    fn helper_wait_preserves_status_and_reaps_on_timeout() {
        let status = run(
            Command::new("/bin/sh").args(["-c", "exit 23"]),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(status.code(), Some(23));

        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("helper.pid");
        let started = Instant::now();
        let error = run(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 30",
                    "helper",
                ])
                .arg(&pid_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            started + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
