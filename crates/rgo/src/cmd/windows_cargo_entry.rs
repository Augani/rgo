//! A copied `rgo.exe` can be installed as a PATH-owned `cargo.exe` on Windows.
//! It reads the owning Cargo home's activation record before forwarding to the
//! same guarded Cargo launcher as the hidden `rgo cargo-shim` pilot.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Deserialize)]
struct InstallationRecord {
    schema_version: u32,
    cargo_home: PathBuf,
    rgo_home: PathBuf,
    binary_version: String,
    protocol_version: u32,
    supervised_cargo: Option<SupervisedCargo>,
}

#[derive(Deserialize)]
struct SupervisedCargo {
    shim_path: PathBuf,
    real_cargo: PathBuf,
}

pub fn is_shim_invocation() -> Result<bool> {
    Ok(std::env::current_exe()?
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("cargo.exe")))
}

pub fn run() -> Result<()> {
    let executable = std::env::current_exe().context("locating Cargo launcher")?;
    let cargo_home = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("Cargo launcher is outside its Cargo home")?;
    let record_path = cargo_home.join(".rgo-install.json");
    let record: InstallationRecord =
        serde_json::from_slice(&std::fs::read(&record_path).with_context(|| {
            format!(
                "reading Cargo launcher activation {}",
                record_path.display()
            )
        })?)
        .with_context(|| {
            format!(
                "parsing Cargo launcher activation {}",
                record_path.display()
            )
        })?;
    let shim = record
        .supervised_cargo
        .context("Cargo launcher has no supervised installation record")?;
    if record.schema_version != 3 || !same_path(&record.cargo_home, cargo_home) {
        bail!("Cargo launcher does not match its installation record; repair or undo rgo setup");
    }
    let real_cargo = shim
        .real_cargo
        .canonicalize()
        .context("locating the recorded real Cargo proxy")?;
    if !shim.real_cargo.is_absolute()
        || real_cargo == executable.canonicalize()?
        || is_rgo_shim(&real_cargo)
        || shim
            .real_cargo
            .file_name()
            .is_none_or(|name| name != "cargo.exe")
    {
        bail!("Cargo launcher record does not name a safe real Cargo proxy");
    }
    if record.binary_version != env!("CARGO_PKG_VERSION")
        || record.protocol_version != rgo_protocol::PROTOCOL_VERSION
        || !same_path(&shim.shim_path, &executable)
    {
        eprintln!("rgo: this Cargo launcher is no longer active; using ordinary Cargo storage");
        return super::cargo_shim::exec_real_cargo(
            &shim.real_cargo,
            &std::env::args_os().skip(1).collect::<Vec<_>>(),
        );
    }
    super::cargo_shim::run(
        &shim.real_cargo,
        Some(&record.cargo_home),
        Some(&record.rgo_home),
        std::env::args_os().skip(1).collect(),
    )
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.canonicalize()
        .ok()
        .is_some_and(|left| right.canonicalize().ok() == Some(left))
}

fn is_rgo_shim(path: &Path) -> bool {
    path.ancestors().any(|directory| {
        directory
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("shims"))
            && directory
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("rgo"))
    })
}
