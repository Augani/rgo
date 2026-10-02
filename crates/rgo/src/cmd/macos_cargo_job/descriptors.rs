//! Preserve the caller's exec-visible descriptors without parsing jobserver flags.

#![allow(unsafe_code)] // Darwin descriptor inventory and fork-safe duplication.

use anyhow::{Context, Result, ensure};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub(super) const MAX_INHERITED: usize = 32;
pub(super) const MAX_TRANSFER: usize = MAX_INHERITED + 4;

pub(crate) struct Inherited {
    pub(crate) targets: Vec<i32>,
}

impl Inherited {
    /// Capture before rgo opens its lifecycle guards or starts query threads.
    /// Keep raw numbers: duplicating and closing a caller's regular file could
    /// release its process-associated record locks even on checkout fallback.
    pub(crate) fn capture() -> Result<Self> {
        let pid = std::process::id().try_into()?;
        let bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        ensure!(bytes > 0, "cannot inventory inherited Cargo descriptors");
        let width = std::mem::size_of::<libc::proc_fdinfo>();
        let capacity = usize::try_from(bytes)? / width;
        ensure!(
            capacity <= 65_536 && usize::try_from(bytes)? % width == 0,
            "Cargo descriptor inventory is too large or invalid"
        );
        let mut entries: Vec<libc::proc_fdinfo> = (0..capacity)
            .map(|_| libc::proc_fdinfo {
                proc_fd: -1,
                proc_fdtype: 0,
            })
            .collect();
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                bytes,
            )
        };
        // The size query includes spare slots. A completely filled snapshot
        // cannot prove that no later entries were truncated.
        ensure!(
            read > 0 && read < bytes && usize::try_from(read)? % width == 0,
            "Cargo descriptor inventory changed or could not be read"
        );
        let mut targets = Vec::new();
        for entry in &entries[..usize::try_from(read)? / width] {
            if entry.proc_fd < 3 {
                continue;
            }
            let flags = unsafe { libc::fcntl(entry.proc_fd, libc::F_GETFD) };
            ensure!(
                flags >= 0,
                "inherited Cargo descriptor changed during capture"
            );
            if flags & libc::FD_CLOEXEC == 0 {
                targets.push(entry.proc_fd);
            }
        }
        validate(&targets)?;
        Ok(Self { targets })
    }
}

pub(super) fn validate(targets: &[i32]) -> Result<()> {
    ensure!(
        targets.len() <= MAX_INHERITED,
        "too many inherited Cargo descriptors"
    );
    for (index, &target) in targets.iter().enumerate() {
        ensure!(
            target >= 3 && !targets[..index].contains(&target),
            "invalid or repeated inherited Cargo descriptor"
        );
    }
    Ok(())
}

pub(super) struct Restored {
    mappings: Vec<(i32, OwnedFd)>,
    _reservations: Vec<OwnedFd>,
}

impl Restored {
    /// Stage before admission. Sources stay above every target, so aliases and
    /// cycles cannot overwrite a later source. Occupy otherwise empty targets
    /// until spawn allocates its exec-error pipe, preventing it from colliding
    /// with a target overwritten in the child.
    pub(super) fn prepare(targets: &[i32], files: Vec<OwnedFd>) -> Result<Self> {
        validate(targets)?;
        ensure!(
            targets.len() == files.len(),
            "inherited Cargo descriptors are missing"
        );
        let minimum = targets
            .iter()
            .max()
            .copied()
            .unwrap_or(2)
            .checked_add(1)
            .context("inherited Cargo descriptor number is too large")?;
        let mut mappings = Vec::new();
        for (&target, file) in targets.iter().zip(files) {
            let source = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, minimum) };
            ensure!(
                source >= 0,
                "cannot stage inherited Cargo descriptor: {}",
                std::io::Error::last_os_error()
            );
            mappings.push((target, unsafe { OwnedFd::from_raw_fd(source) }));
        }
        let mut reservations = Vec::new();
        for &(target, ref source) in &mappings {
            if unsafe { libc::fcntl(target, libc::F_GETFD) } >= 0 {
                continue;
            }
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
                "cannot inspect Cargo descriptor destination"
            );
            ensure!(
                unsafe { libc::dup2(source.as_raw_fd(), target) } == target,
                "cannot reserve Cargo descriptor destination"
            );
            let reservation = unsafe { OwnedFd::from_raw_fd(target) };
            ensure!(
                unsafe { libc::fcntl(target, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "cannot close Cargo descriptor reservation on exec"
            );
            reservations.push(reservation);
        }
        Ok(Self {
            mappings,
            _reservations: reservations,
        })
    }

    /// Only async-signal-safe calls between fork and exec. dup2 clears CLOEXEC
    /// on the destination, matching the caller's original exec-visible set.
    pub(super) unsafe fn restore_in_child(&self) -> std::io::Result<()> {
        for (target, source) in &self.mappings {
            if unsafe { libc::dup2(source.as_raw_fd(), *target) } != *target {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
