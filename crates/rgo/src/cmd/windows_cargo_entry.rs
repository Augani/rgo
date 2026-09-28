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
    if record.schema_version != 3
        || record.binary_version != env!("CARGO_PKG_VERSION")
        || record.protocol_version != rgo_protocol::PROTOCOL_VERSION
        || !same_path(&record.cargo_home, cargo_home)
        || !same_path(&shim.shim_path, &executable)
    {
        bail!("Cargo launcher does not match its installation record; repair or undo rgo setup");
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
