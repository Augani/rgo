//! `rgo <anything>` → `cargo <anything>` with the same stdio, signals and exit code.
//! Before exec'ing, record the workspace → build-dir attribution so GC can detect orphans
//! even when the rustc wrapper is not installed.

use std::ffi::OsString;
use std::process::Command;

use anyhow::{Context, Result};
use rgo_protocol::BYPASS_ENV;

pub fn run(args: Vec<OsString>) -> Result<()> {
    let cargo = std::env::var_os("CARGO")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "cargo".into());

    if std::env::var_os(BYPASS_ENV).is_none() {
        if let Err(e) = attribute(&cargo) {
            tracing::debug!(%e, "could not attribute workspace; continuing");
        }
    }

    let status = Command::new(&cargo)
        .args(&args)
        .status()
        .with_context(|| format!("running {}", cargo.to_string_lossy()))?;
    std::process::exit(exit_code(status));
}

/// Ask Cargo for the workspace root; the build-dir itself is only learned when a compile
/// happens (via the wrapper), so here we just make sure a `cargo locate-project` succeeds
/// and stash the result for the wrapper / future daemon.
fn attribute(cargo: &OsString) -> Result<()> {
    let out = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .output()?;
    if !out.status.success() {
        return Ok(()); // not inside a cargo project; nothing to attribute
    }
    let manifest = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    // TODO(phase 1, step 5): resolve the effective build-dir via `cargo metadata`/`cargo config get`
    // (nightly) or by letting the wrapper write the sidecar; export RGO_MANIFEST_PATH for the wrapper.
    // SAFETY(unsafe_code lint): set_var is only unsound with concurrent readers; we are single-threaded here.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("RGO_MANIFEST_PATH", manifest)
    };
    Ok(())
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
