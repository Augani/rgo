//! One process-lifetime wake pipe for replies and async signal handlers. It is
//! close-on-exec, and stays open through handler teardown to prevent FD reuse.

use anyhow::{Context, Result, ensure};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

pub(super) struct Wake {
    read: OwnedFd,
    write: OwnedFd,
}
static WAKE: OnceLock<Wake> = OnceLock::new();

impl Wake {
    pub(super) fn get() -> Result<&'static Self> {
        if WAKE.get().is_none() {
            let mut descriptors = [-1; 2];
            ensure!(
                unsafe { libc::pipe(descriptors.as_mut_ptr()) } == 0,
                "cannot create Cargo wake pipe"
            );
            let candidate = Self {
                read: unsafe { OwnedFd::from_raw_fd(descriptors[0]) },
                write: unsafe { OwnedFd::from_raw_fd(descriptors[1]) },
            };
            for descriptor in [&candidate.read, &candidate.write] {
                let fd = descriptor.as_raw_fd();
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                ensure!(
                    flags >= 0
                        && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0
                        && unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                    "cannot establish Cargo wake descriptor ownership"
                );
            }
            let _ = WAKE.set(candidate);
        }
        WAKE.get().context("Cargo wake pipe is missing")
    }
    pub(super) fn reader(&self) -> i32 {
        self.read.as_raw_fd()
    }
    pub(super) fn writer(&self) -> i32 {
        self.write.as_raw_fd()
    }
    pub(super) fn notify(&self) {
        // A full pipe is already readable; notification never blocks.
        loop {
            if unsafe { libc::write(self.writer(), b"W".as_ptr().cast(), 1) } >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
    pub(super) fn drain(&self) -> Result<()> {
        let mut buffer = [0_u8; 512];
        for _ in 0..8 {
            let count =
                unsafe { libc::read(self.reader(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if count > 0 {
                continue;
            }
            if count == 0 {
                anyhow::bail!("Cargo wake pipe disconnected");
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::WouldBlock => return Ok(()),
                std::io::ErrorKind::Interrupted => continue,
                _ => return Err(error.into()),
            }
        }
        Ok(())
    }
    pub(super) fn wait(&self) -> Result<()> {
        let mut descriptor = libc::pollfd {
            fd: self.reader(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, -1) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}
