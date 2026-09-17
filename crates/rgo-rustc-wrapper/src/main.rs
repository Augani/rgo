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
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rgo_protocol::{BYPASS_ENV, ContextSidecar, HOME_ENV, PROTOCOL_VERSION, SIDECAR_FILE};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("rgo-rustc-wrapper: usage: <rustc> <args...>");
        std::process::exit(2);
    };
    let args: Vec<OsString> = args.collect();

    if std::env::var_os(BYPASS_ENV).is_none() {
        let _ = attribute(&args); // best-effort, never fatal
    }
    exec(rustc, &args);
}

fn attribute(args: &[OsString]) -> Option<()> {
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
            return Some(());
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
    std::fs::rename(tmp, sidecar_path).ok()
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
