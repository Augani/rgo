use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rgo_core::ipc;
use rgo_core::paths::RgoPaths;
use rgo_protocol::{Request, Response};

use super::env;

pub fn run(foreground: bool) -> Result<()> {
    let e = env()?;
    if !foreground {
        let exe = std::env::current_exe().context("locating rgo executable")?;
        Command::new(exe)
            .args(["daemon", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting background daemon")?;
        return Ok(());
    }
    rgo_core::daemon::run(e.paths, e.cfg)
}

/// Best-effort daemon startup used by commands that must coordinate state. A failed start is
/// deliberately reported as `false`: builds can continue without coordination, while GC and
/// other destructive operations refuse to run.
pub fn ensure_running(paths: &RgoPaths) -> bool {
    if daemon_responds(paths) {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if Command::new(exe)
        .args(["daemon", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_err()
    {
        return false;
    }
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(25));
        if daemon_responds(paths) {
            return true;
        }
    }
    false
}

fn daemon_responds(paths: &RgoPaths) -> bool {
    match ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(2),
    ) {
        Ok(Response::Status(_)) => true,
        Ok(other) => {
            tracing::debug!(response = ?other, "daemon health check returned unexpected response");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "daemon health check failed");
            false
        }
    }
}
