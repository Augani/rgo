//! Wake the guardian for primary state changes without a polling sleep.

#![allow(unsafe_code)] // Async-signal-safe write and poll, confined to the job.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

static CHILD_WAKE: AtomicI32 = AtomicI32::new(-1);

// Exec preserves ignored dispositions and the caller's signal mask. The
// guardian's own SIGCHLD/SIGHUP handlers must not leak into real Cargo.
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
pub(super) struct NativeSignals {
    ignored: u32,
    blocked: u32,
}

impl NativeSignals {
    pub(super) fn capture() -> Result<Self> {
        let mut mask = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) } == 0,
            "reading caller signal mask failed"
        );
        let mut value = Self::default();
        for signal in 1..32 {
            if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                continue;
            }
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            ensure!(
                unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } == 0,
                "reading caller signal disposition failed"
            );
            if action.sa_sigaction == libc::SIG_IGN && signal != libc::SIGPIPE {
                value.ignored |= 1 << signal;
            }
            if unsafe { libc::sigismember(&mask, signal) } == 1 {
                value.blocked |= 1 << signal;
            }
        }
        Ok(value)
    }

    // Only async-signal-safe syscalls: this runs between fork and exec.
    pub(super) unsafe fn restore_in_child(self) -> std::io::Result<()> {
        unsafe {
            let mut mask = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            for signal in 1..32 {
                if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = if self.ignored & (1 << signal) != 0 {
                    libc::SIG_IGN
                } else {
                    libc::SIG_DFL
                };
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if self.blocked & (1 << signal) != 0 {
                    libc::sigaddset(&mut mask, signal);
                }
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

extern "C" fn child_changed(_: i32) {
    unsafe {
        let errno = libc::__error();
        let saved = *errno;
        let fd = CHILD_WAKE.load(Ordering::Relaxed);
        if fd >= 0 {
            // The socket is nonblocking. A full buffer already provides a wake.
            libc::write(fd, b"C".as_ptr().cast(), 1);
        }
        *errno = saved;
    }
}

pub(super) struct ChildEvents {
    reader: UnixStream,
    _writer: UnixStream,
    previous: libc::sigaction,
}

impl ChildEvents {
    pub(super) fn new() -> Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = child_changed as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        let mut previous = unsafe { std::mem::zeroed() };
        CHILD_WAKE.store(writer.as_raw_fd(), Ordering::Relaxed);
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        ensure!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, &mut previous) } == 0,
            "cannot observe Cargo child state"
        );
        Ok(Self {
            reader,
            _writer: writer,
            previous,
        })
    }

    pub(super) fn wait(
        &mut self,
        stream: &UnixStream,
        disconnected: bool,
        timeout: i32,
        extra: &[libc::pollfd],
    ) -> Result<()> {
        let mut descriptors = vec![
            libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if disconnected { -1 } else { stream.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        descriptors.extend_from_slice(extra);
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout,
            )
        };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut bytes = [0_u8; 64];
        loop {
            match self.reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Drop for ChildEvents {
    fn drop(&mut self) {
        // This guardian is single-threaded. Block delivery while restoring the
        // handler so it cannot use a closed/reused descriptor during teardown.
        unsafe {
            let mut blocked = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGCHLD);
            let mut previous_mask = std::mem::zeroed();
            if libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous_mask) == 0 {
                libc::sigaction(libc::SIGCHLD, &self.previous, std::ptr::null_mut());
                CHILD_WAKE.store(-1, Ordering::Relaxed);
                libc::sigprocmask(libc::SIG_SETMASK, &previous_mask, std::ptr::null_mut());
            }
        }
    }
}
