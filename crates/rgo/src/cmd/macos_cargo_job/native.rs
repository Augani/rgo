//! Exec-visible native attributes which launchd does not inherit from the caller.

#![allow(unsafe_code)] // Bounded fork probes and Darwin kernel attribute calls.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

// Explicit supported Darwin resource classes, rather than an OS-dependent
// RLIM_NLIMITS count or resource indices supplied by the wire request.
const RESOURCES: [i32; 9] = [
    libc::RLIMIT_CPU,
    libc::RLIMIT_FSIZE,
    libc::RLIMIT_DATA,
    libc::RLIMIT_STACK,
    libc::RLIMIT_CORE,
    libc::RLIMIT_AS,
    libc::RLIMIT_MEMLOCK,
    libc::RLIMIT_NPROC,
    libc::RLIMIT_NOFILE,
];

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Limit {
    soft: u64,
    hard: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) struct Attributes {
    mask: u32,
    limits: [Limit; RESOURCES.len()],
    nice: i32,
}

impl Attributes {
    /// Before query threads or lifecycle guards. Never temporarily change the
    /// caller's process-wide mask: a tiny child reads its inherited copy.
    pub(crate) fn capture(inherited: &[i32]) -> Result<Self> {
        for timer in [libc::ITIMER_REAL, libc::ITIMER_VIRTUAL, libc::ITIMER_PROF] {
            let mut value = unsafe { std::mem::zeroed::<libc::itimerval>() };
            ensure!(
                unsafe { libc::getitimer(timer, &mut value) } == 0,
                "cannot inspect Cargo interval timers"
            );
            ensure!(
                value.it_value.tv_sec == 0
                    && value.it_value.tv_usec == 0
                    && value.it_interval.tv_sec == 0
                    && value.it_interval.tv_usec == 0,
                "active Cargo interval timer requires checkout execution"
            );
        }
        let limits = limits()?;
        // A new process resets CPU usage, so a finite CPU budget cannot be
        // reproduced by copying its numeric limit into a fresh Cargo process.
        ensure!(
            limits[libc::RLIMIT_CPU as usize].soft == libc::RLIM_INFINITY
                && limits[libc::RLIMIT_CPU as usize].hard == libc::RLIM_INFINITY,
            "finite Cargo CPU budget requires checkout execution"
        );
        let nice = priority()?;
        let mut files = [0; super::descriptors::MAX_INHERITED + 3];
        files[..3].copy_from_slice(&[0, 1, 2]);
        files[3..3 + inherited.len()].copy_from_slice(inherited);
        let mask = probe(|| unsafe {
            // F_GETLK in the caller would hide its own process-associated
            // locks. The child has no such locks and can detect them without
            // duplicating/closing a caller file or releasing any lock.
            for &fd in &files[..3 + inherited.len()] {
                let mut stat = std::mem::zeroed::<libc::stat>();
                if libc::fstat(fd, &mut stat) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                    continue;
                }
                let mut lock = libc::flock {
                    l_start: 0,
                    l_len: 0,
                    l_pid: 0,
                    l_type: libc::F_WRLCK,
                    l_whence: libc::SEEK_SET as i16,
                };
                if libc::fcntl(fd, libc::F_GETLK, &mut lock) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if lock.l_type != libc::F_UNLCK {
                    return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
                }
            }
            Ok(u32::from(libc::umask(0)))
        })
        .context("capturing Cargo file mask and checking process-associated file locks")?;
        Ok(Self { mask, limits, nice })
    }

    /// Complete before publishing a receipt or accepting a commit. The actual
    /// kernel setters run in an owned disposable child, never in the guardian.
    pub(super) fn validate_for_guardian(&self) -> Result<()> {
        ensure!(self.mask & !0o777 == 0, "invalid Cargo file creation mask");
        ensure!((-20..=20).contains(&self.nice), "invalid Cargo nice value");
        let current = limits()?;
        for (desired, actual) in self.limits.iter().zip(current) {
            ensure!(
                desired.soft <= desired.hard
                    && desired.hard <= libc::RLIM_INFINITY
                    && desired.hard <= actual.hard,
                "Cargo resource limit cannot be restored by its guardian"
            );
        }
        ensure!(
            self.limits[libc::RLIMIT_CPU as usize].soft == libc::RLIM_INFINITY
                && self.limits[libc::RLIMIT_CPU as usize].hard == libc::RLIM_INFINITY,
            "finite Cargo CPU budget requires checkout execution"
        );
        ensure!(
            self.nice >= priority()?,
            "Cargo scheduling priority cannot be restored by its guardian"
        );
        probe(|| unsafe { self.restore_in_child().map(|()| 0) })
            .context("preflighting Cargo native attributes")?;
        Ok(())
    }

    /// No allocation or lock-taking userspace work after fork. Darwin's
    /// setrlimit and PRIO_PROCESS setpriority wrappers only enter the kernel.
    pub(super) unsafe fn restore_in_child(&self) -> io::Result<()> {
        for (resource, limit) in RESOURCES.into_iter().zip(&self.limits) {
            let value = libc::rlimit {
                rlim_cur: limit.soft,
                rlim_max: limit.hard,
            };
            let mut actual = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if unsafe { libc::getrlimit(resource, &mut actual) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if actual.rlim_cur == value.rlim_cur && actual.rlim_max == value.rlim_max {
                continue;
            }
            if unsafe { libc::setrlimit(resource, &value) } != 0
                || unsafe { libc::getrlimit(resource, &mut actual) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if actual.rlim_cur != value.rlim_cur || actual.rlim_max != value.rlim_max {
                return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
            }
        }
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, self.nice) } != 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::umask(self.mask as libc::mode_t) };
        Ok(())
    }
}

fn limits() -> Result<[Limit; RESOURCES.len()]> {
    let mut values = [Limit { soft: 0, hard: 0 }; RESOURCES.len()];
    for (resource, limit) in RESOURCES.into_iter().zip(&mut values) {
        let mut value = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        ensure!(
            unsafe { libc::getrlimit(resource, &mut value) } == 0,
            "cannot inspect Cargo resource limits"
        );
        *limit = Limit {
            soft: value.rlim_cur,
            hard: value.rlim_max,
        };
    }
    Ok(values)
}

fn priority() -> Result<i32> {
    unsafe {
        *libc::__error() = 0; // -1 is also a valid priority.
        let value = libc::getpriority(libc::PRIO_PROCESS, 0);
        ensure!(
            *libc::__error() == 0,
            "cannot inspect Cargo scheduling priority"
        );
        Ok(value)
    }
}

/// The child only uses raw syscall-backed operations and _exit. Refuse custom
/// SIGCHLD/reaping policy before fork; otherwise waitpid owns this unreaped PID
/// until completion, so timeout cleanup cannot signal a reused process ID.
fn probe(operation: impl FnOnce() -> io::Result<u32>) -> Result<u32> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    ensure!(
        unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } == 0
            && unsafe { libc::sigprocmask(0, std::ptr::null(), &mut mask) } == 0,
        "cannot inspect Cargo child reaping policy"
    );
    ensure!(
        action.sa_sigaction == libc::SIG_DFL
            && action.sa_flags & libc::SA_NOCLDWAIT == 0
            && unsafe { libc::sigismember(&mask, libc::SIGCHLD) } == 0,
        "Cargo child reaping policy requires checkout execution"
    );
    let mut descriptors = [-1; 2];
    ensure!(
        unsafe { libc::pipe(descriptors.as_mut_ptr()) } == 0,
        "cannot create native attribute probe pipe"
    );
    let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    for fd in [&reader, &writer] {
        ensure!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "cannot isolate native attribute probe pipe"
        );
    }
    let pid = unsafe { libc::fork() };
    ensure!(pid >= 0, "cannot fork native attribute probe");
    if pid == 0 {
        let packet = match operation() {
            Ok(value) => [0_u32, value],
            Err(error) => [error.raw_os_error().unwrap_or(libc::EIO) as u32, 0],
        };
        loop {
            let written = unsafe { libc::write(writer.as_raw_fd(), packet.as_ptr().cast(), 8) };
            if written == 8 {
                unsafe { libc::_exit(0) };
            }
            if written < 0 && unsafe { *libc::__error() } == libc::EINTR {
                continue;
            }
            unsafe { libc::_exit(1) };
        }
    }
    drop(writer);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            break;
        }
        if waited < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Err(io::Error::last_os_error()).context("waiting for native attribute probe");
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            while unsafe { libc::waitpid(pid, &mut status, 0) } < 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
            {}
            anyhow::bail!("native attribute probe timed out");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    ensure!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "native attribute probe did not complete"
    );
    let mut packet = [0_u8; 8];
    std::fs::File::from(reader).read_exact(&mut packet)?;
    let error = u32::from_ne_bytes(packet[..4].try_into()?);
    ensure!(
        error == 0,
        "native attribute probe: {}",
        io::Error::from_raw_os_error(error as i32)
    );
    Ok(u32::from_ne_bytes(packet[4..].try_into()?))
}
