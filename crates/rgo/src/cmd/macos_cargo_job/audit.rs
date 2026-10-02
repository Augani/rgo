//! Debug-only deterministic handshake audit. Never compiled into release bins.

use super::{SIGNALS, cancelled_signal};
use anyhow::{Result, ensure};
use rgo_core::paths::RgoPaths;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn directory(paths: &RgoPaths, enabled: bool) -> Result<Option<std::path::PathBuf>> {
    if !enabled {
        return Ok(None);
    }
    let directory = paths.state_dir().join("macos-supervisor-audit");
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "supervisor audit requires a private owned directory"
    );
    Ok(Some(directory))
}

fn publish(directory: &std::path::Path, name: &str) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(name))?
        .write_all(b"observed")?;
    Ok(())
}

pub(super) fn prepared(paths: &RgoPaths) -> Result<()> {
    let enabled = std::env::var_os("RGO_MACOS_SUPERVISOR_AUDIT").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let Some(directory) = directory(paths, enabled)? else {
        return Ok(());
    };
    publish(&directory, "prepared")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = false;
    let mut pending_observed = false;
    loop {
        if !observed && cancelled_signal().is_some() {
            publish(&directory, "cancel-observed")?;
            observed = true;
        }
        // A blocked notification may still be in the kernel, or already
        // captured by the relay. Observe either without consuming it.
        let mut pending = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::sigpending(&mut pending) } == 0,
            "cannot observe audit pending signals"
        );
        if !pending_observed
            && (unsafe { libc::sigismember(&pending, libc::SIGTERM) } == 1
                || SIGNALS.load(Ordering::Acquire) & (1 << libc::SIGTERM) != 0)
        {
            publish(&directory, "signal-observed")?;
            pending_observed = true;
        }
        match std::fs::read(directory.join("release")) {
            Ok(action) => {
                ensure!(
                    action == b"resume",
                    "injected preparation completion failure"
                );
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "supervisor audit was not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn committed(paths: &RgoPaths, enabled: bool) -> Result<()> {
    if let Some(directory) = directory(paths, enabled)? {
        publish(&directory, "committed")?;
    }
    Ok(())
}

pub(super) fn caller_committed(paths: &RgoPaths) -> Result<()> {
    let enabled = std::env::var_os("RGO_MACOS_SUPERVISOR_COMMIT_AUDIT").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let Some(directory) = directory(paths, enabled)? else {
        return Ok(());
    };
    publish(&directory, "caller-committed")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::fs::read(directory.join("delivery-release")) {
            Ok(action) => {
                ensure!(action == b"resume", "invalid commit audit release");
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "supervisor commit audit was not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn commit_prefix(
    paths: &RgoPaths,
    stream: &mut UnixStream,
    commit: &[u8; 5],
) -> Result<usize> {
    let enabled = std::env::var_os("RGO_MACOS_SUPERVISOR_COMMIT_CUT_AUDIT").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let Some(directory) = directory(paths, enabled)? else {
        return Ok(0);
    };
    stream.write_all(&commit[..3])?;
    publish(&directory, "commit-cut")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = false;
    loop {
        if !observed && cancelled_signal().is_some() {
            publish(&directory, "commit-cancel-observed")?;
            observed = true;
        }
        match std::fs::read(directory.join("cut-release")) {
            Ok(action) => {
                ensure!(action == b"resume", "invalid commit cut release");
                stream.shutdown(std::net::Shutdown::Write)?;
                return Ok(3);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "supervisor commit cut was not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
