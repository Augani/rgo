//! `rgo <anything>` → `cargo <anything>` with the same stdio, signals and exit code.
//! Before exec'ing, record the workspace → build-dir attribution so GC can detect orphans
//! even when the rustc wrapper is not installed.

use std::ffi::OsString;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rgo_core::ipc;
use rgo_protocol::{BYPASS_ENV, DEFAULT_HEARTBEAT_SECS, DEFAULT_LEASE_TTL_SECS, LEASE_ENV,
    LeaseScope, Request, Response};

use super::{daemon, env};

pub fn run(args: Vec<OsString>) -> Result<()> {
    let cargo = std::env::var_os("CARGO")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "cargo".into());

    let mut lease = None;
    let mut manifest = None;
    if std::env::var_os(BYPASS_ENV).is_none() {
        if let Ok(e) = env() {
            manifest = attribute(&cargo, &args).ok().flatten();
            if let Some(manifest_path) = &manifest {
                if daemon::ensure_running(&e.paths) {
                    let workspace_root = std::path::Path::new(manifest_path)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned());
                    if let Some(workspace_root) = workspace_root {
                        if let Ok(Response::Lease { lease_id, .. }) = ipc::request(
                            &e.paths.socket_path(),
                            Request::AcquireLease {
                                scope: LeaseScope::Workspace { workspace_root },
                                pid: std::process::id(),
                                ttl_secs: DEFAULT_LEASE_TTL_SECS,
                            },
                        ) {
                            lease = Some(LeaseGuard::new(e.paths.socket_path(), lease_id));
                        }
                    }
                }
            }
        }
    }

    let mut command = Command::new(&cargo);
    command.args(&args);
    if let Some(manifest) = manifest {
        command.env("RGO_MANIFEST_PATH", manifest);
    }
    if let Some(lease) = &lease {
        command.env(LEASE_ENV, lease.id.to_string());
    }
    let status = command
        .status()
        .with_context(|| format!("running {}", cargo.to_string_lossy()))?;
    drop(lease);
    std::process::exit(exit_code(status));
}

/// Ask Cargo for the workspace root; the build-dir itself is only learned when a compile
/// happens (via the wrapper), so here we just make sure a `cargo locate-project` succeeds
/// and stash the result for the wrapper / future daemon.
fn attribute(cargo: &OsString, args: &[OsString]) -> Result<Option<String>> {
    let mut locate = Command::new(cargo);
    locate.args(["locate-project", "--workspace", "--message-format=plain"]);
    if let Some(manifest_path) = argument_value(args, "--manifest-path") {
        locate.args(["--manifest-path".into(), manifest_path]);
    }
    let out = locate.output()?;
    if !out.status.success() {
        return Ok(None); // not inside a cargo project; nothing to attribute
    }
    let manifest = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok(Some(manifest))
}

fn argument_value(args: &[OsString], flag: &str) -> Option<OsString> {
    let mut iter = args.iter();
    while let Some(argument) = iter.next() {
        if argument == flag {
            return iter.next().cloned();
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|value| value.strip_prefix(flag))
            .and_then(|value| value.strip_prefix('='))
        {
            return Some(value.into());
        }
    }
    None
}

struct LeaseGuard {
    socket: std::path::PathBuf,
    id: u64,
    stop: Option<Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LeaseGuard {
    fn new(socket: std::path::PathBuf, id: u64) -> Self {
        let (stop, stop_thread) = mpsc::channel();
        let socket_thread = socket.clone();
        let thread = thread::spawn(move || {
            loop {
                match stop_thread.recv_timeout(Duration::from_secs(u64::from(DEFAULT_HEARTBEAT_SECS))) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        let _ = ipc::request(&socket_thread, Request::Heartbeat { lease_id: id });
                    }
                }
            }
        });
        Self { socket, id, stop: Some(stop), thread: Some(thread) }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = ipc::request(&self.socket, Request::ReleaseLease { lease_id: self.id });
    }
}

#[cfg(unix)]
fn exit_code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(1))
}
#[cfg(not(unix))]
fn exit_code(s: std::process::ExitStatus) -> i32 {
    s.code().unwrap_or(1)
}
