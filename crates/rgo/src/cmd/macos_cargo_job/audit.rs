//! Debug-only deterministic handshake audit. Never compiled into release bins.

use super::SIGNALS;
use anyhow::{Result, ensure};
use rgo_core::paths::RgoPaths;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
    let terminating = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .fold(0_u32, |mask, signal| mask | (1 << signal));
    let mut observed = false;
    loop {
        if !observed && SIGNALS.load(Ordering::Acquire) & terminating != 0 {
            publish(&directory, "cancel-observed")?;
            observed = true;
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
