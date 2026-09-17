//! rgo-rustc-wrapper — invoked by Cargo as `<wrapper> <rustc> <args...>`.
//!
//! Phase 1 contract: be invisible. Do one cheap side effect (write/refresh the
//! build-dir sidecar so `rgo` can attribute the build-dir to a workspace) and then
//! `exec` the real compiler with identical args, env, stdio and exit code.
//! Any internal failure is swallowed: rustc always runs.
//!
//! Phase 3 adds the cacheability classifier + CAS lookup in front of the exec.
//! Everything here must stay fast: no heavy deps, no network, no blocking on a daemon.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use rgo_protocol::{BYPASS_ENV, CLIENT_TIMEOUT_MILLIS, ContextSidecar, DEFAULT_HEARTBEAT_SECS,
    DEFAULT_LEASE_TTL_SECS, HOME_ENV, LEASE_ENV, LeaseScope, PROTOCOL_VERSION, Request, Response,
    SIDECAR_FILE, decode_frame, encode_frame, MAX_FRAME_SIZE};

#[cfg(unix)]
type PlatformStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type PlatformStream = uds_windows::UnixStream;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("rgo-rustc-wrapper: usage: <rustc> <args...>");
        std::process::exit(2);
    };
    let args: Vec<OsString> = args.collect();

    let mut lease_id = None;
    if std::env::var_os(BYPASS_ENV).is_none() {
        if let Some(build_dir) = attribute(&args) {
            let workspace_root = workspace_root();
            if let Some(raw) = std::env::var_os(LEASE_ENV) {
                if let Ok(id) = raw.to_string_lossy().parse::<u64>() {
                    let bound = request(Request::BindLease {
                        lease_id: id,
                        build_dir: build_dir.to_string_lossy().into_owned(),
                        workspace_root: workspace_root.clone(),
                    })
                    .is_ok();
                    if !bound {
                        lease_id = acquire_context_lease(&build_dir);
                    }
                }
            } else if let Ok(Response::Lease { lease_id: id, .. }) = request(Request::AcquireLease {
                scope: LeaseScope::Context { build_dir: build_dir.to_string_lossy().into_owned() },
                pid: std::process::id(),
                ttl_secs: DEFAULT_LEASE_TTL_SECS,
            }) {
                lease_id = Some(id);
            }
        }
    }
    if let Some(lease_id) = lease_id {
        run_supervised(rustc, &args, lease_id);
    } else {
        exec(rustc, &args);
    }
}

fn acquire_context_lease(build_dir: &Path) -> Option<u64> {
    match request(Request::AcquireLease {
        scope: LeaseScope::Context { build_dir: build_dir.to_string_lossy().into_owned() },
        pid: std::process::id(),
        ttl_secs: DEFAULT_LEASE_TTL_SECS,
    }) {
        Ok(Response::Lease { lease_id, .. }) => Some(lease_id),
        _ => None,
    }
}

fn attribute(args: &[OsString]) -> Option<PathBuf> {
    let out_dir = arg_value(args, "--out-dir")?;
    let build_dir = find_managed_build_dir(Path::new(&out_dir))?;
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
    // CARGO_MANIFEST_DIR is the *package*; RGO_MANIFEST_PATH (set by `rgo <cmd>`) is the workspace root.
    let manifest_path = std::env::var_os("RGO_MANIFEST_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&manifest_dir).join("Cargo.toml"));
    let workspace_root = manifest_path.parent()?.to_path_buf();

    let sidecar_path = build_dir.join(SIDECAR_FILE);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let existing: Option<ContextSidecar> = std::fs::read_to_string(&sidecar_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(e) = &existing {
        // Refresh at most once a day to avoid write churn on every rustc invocation.
        if now.saturating_sub(e.last_seen) < 86_400
            && e.manifest_path == manifest_path.to_string_lossy()
        {
            return Some(build_dir);
        }
    }
    let sc = ContextSidecar {
        version: PROTOCOL_VERSION,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        toolchain: std::env::var("RUSTUP_TOOLCHAIN").ok(),
        first_seen: existing.map(|e| e.first_seen).unwrap_or(now),
        last_seen: now,
    };
    let tmp = build_dir.join(format!("{SIDECAR_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&sc).ok()?).ok()?;
    std::fs::rename(tmp, sidecar_path).ok()?;
    Some(build_dir)
}

fn workspace_root() -> Option<String> {
    let manifest = std::env::var_os("RGO_MANIFEST_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|p| PathBuf::from(p).join("Cargo.toml")))?;
    manifest.parent().map(|p| p.to_string_lossy().into_owned())
}

fn request(message: Request) -> Result<Response, String> {
    let home = std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).map(|p| p.join(".rgo")))
        .ok_or_else(|| "cannot determine RGO_HOME".to_owned())?;
    let socket = home.join("state").join("daemon.sock");
    let mut stream = PlatformStream::connect(socket).map_err(|e| e.to_string())?;
    let timeout = Some(Duration::from_millis(CLIENT_TIMEOUT_MILLIS));
    stream.set_read_timeout(timeout).map_err(|e| e.to_string())?;
    stream.set_write_timeout(timeout).map_err(|e| e.to_string())?;
    write_frame(&mut stream, &Request::Hello { version: PROTOCOL_VERSION, client: "rustc-wrapper".into() })?;
    match read_message::<Response>(&mut stream)? {
        Response::Hello { version } if version == PROTOCOL_VERSION => {}
        Response::Hello { version } => return Err(format!("protocol mismatch: {version}")),
        Response::Error { message, .. } => return Err(message),
        _ => return Err("invalid handshake response".into()),
    }
    write_frame(&mut stream, &message)?;
    read_message(&mut stream)
}

fn write_frame<T: serde::Serialize>(stream: &mut PlatformStream, value: &T) -> Result<(), String> {
    let frame = encode_frame(value).map_err(|e| e.to_string())?;
    stream.write_all(&frame).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

fn read_message<T: for<'de> serde::Deserialize<'de>>(stream: &mut PlatformStream) -> Result<T, String> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME_SIZE {
        return Err("invalid IPC frame length".into());
    }
    let mut frame = vec![0u8; len + 4];
    frame[..4].copy_from_slice(&header);
    stream.read_exact(&mut frame[4..]).map_err(|e| e.to_string())?;
    decode_frame(&frame).map_err(|e| e.to_string())
}

fn run_supervised(rustc: OsString, args: &[OsString], lease_id: u64) -> ! {
    let (stop, stop_thread) = mpsc::channel();
    let heartbeat = thread::spawn(move || {
        loop {
            match stop_thread.recv_timeout(Duration::from_secs(u64::from(DEFAULT_HEARTBEAT_SECS))) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = request(Request::Heartbeat { lease_id });
                }
            }
        }
    });
    let status = Command::new(&rustc).args(args).status();
    let _ = stop.send(());
    let _ = heartbeat.join();
    let _ = request(Request::ReleaseLease { lease_id });
    match status {
        Ok(status) => std::process::exit(exit_code(status)),
        Err(error) => {
            eprintln!("rgo-rustc-wrapper: failed to run {}: {error}", rustc.to_string_lossy());
            std::process::exit(127);
        }
    }
}

fn arg_value(args: &[OsString], flag: &str) -> Option<OsString> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(rest) = a
            .to_str()
            .and_then(|s| s.strip_prefix(flag))
            .and_then(|s| s.strip_prefix('='))
        {
            return Some(rest.into());
        }
    }
    None
}

/// Walk up from `--out-dir` until the grandparent is `$RGO_HOME/builds`
/// (Cargo's `{workspace-path-hash}` expands to `xx/yyyy…`).
fn find_managed_build_dir(out_dir: &Path) -> Option<PathBuf> {
    let builds = match std::env::var_os(HOME_ENV) {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?)
            .join(".rgo"),
    }
    .join("builds");
    let mut p = out_dir;
    while let Some(parent) = p.parent() {
        if parent.parent() == Some(builds.as_path()) {
            return Some(p.to_path_buf());
        }
        p = parent;
    }
    None
}

#[cfg(unix)]
fn exec(rustc: OsString, args: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = Command::new(&rustc).args(args).exec();
    eprintln!(
        "rgo-rustc-wrapper: failed to exec {}: {err}",
        rustc.to_string_lossy()
    );
    std::process::exit(127);
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

#[cfg(not(unix))]
fn exec(rustc: OsString, args: &[OsString]) -> ! {
    match Command::new(&rustc).args(args).status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(err) => {
            eprintln!(
                "rgo-rustc-wrapper: failed to run {}: {err}",
                rustc.to_string_lossy()
            );
            std::process::exit(127);
        }
    }
}
