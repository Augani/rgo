//! A copied `rgo.exe` can be installed as a PATH-owned `cargo.exe` on Windows.
//! It reads the owning Cargo home's activation record before forwarding to the
//! same guarded Cargo launcher as the hidden `rgo cargo-shim` pilot.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
pub(crate) struct ShimFallback {
    pub schema_version: u32,
    pub cargo_home: PathBuf,
    pub real_cargo: PathBuf,
}

pub(crate) fn fallback_path(shim: &Path) -> Result<PathBuf> {
    Ok(shim
        .parent()
        .context("Cargo launcher has no parent")?
        .join(".rgo-cargo-fallback.json"))
}

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
    let container = executable
        .parent()
        .context("Cargo launcher has no parent")?;
    let shims = if container
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("shims"))
    {
        container
    } else {
        container
            .parent()
            .context("Cargo launcher is outside its Cargo home")?
    };
    if !shims
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("shims"))
        || !shims
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("rgo"))
    {
        bail!("Cargo launcher is outside its Cargo home");
    }
    let cargo_home = shims
        .parent()
        .and_then(Path::parent)
        .context("Cargo launcher is outside its Cargo home")?;
    let record_path = cargo_home.join(".rgo-install.json");
    let fallback_path = fallback_path(&executable)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&fallback_path) {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("Cargo launcher fallback is not a plain file");
        }
    }
    let fallback: Option<ShimFallback> = match std::fs::read(&fallback_path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "parsing Cargo launcher fallback {}",
                fallback_path.display()
            )
        })?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("reading Cargo launcher fallback"),
    };
    if let Some(fallback) = &fallback {
        if fallback.schema_version != 1 || !same_path(&fallback.cargo_home, cargo_home) {
            bail!("Cargo launcher fallback does not match its Cargo home");
        }
        validate_real_cargo(&fallback.real_cargo, &executable)?;
    }
    let record: Option<InstallationRecord> = match std::fs::read(&record_path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "parsing Cargo launcher activation {}",
                record_path.display()
            )
        })?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("reading Cargo launcher activation"),
    };
    let active = record.as_ref().and_then(|record| {
        record
            .supervised_cargo
            .as_ref()
            .filter(|shim| {
                record.schema_version == 3
                    && same_path(&record.cargo_home, cargo_home)
                    && record.binary_version == env!("CARGO_PKG_VERSION")
                    && record.protocol_version == rgo_protocol::PROTOCOL_VERSION
                    && same_path(&shim.shim_path, &executable)
            })
            .map(|shim| (record, shim))
    });
    if let Some((record, shim)) = active {
        validate_real_cargo(&shim.real_cargo, &executable)?;
        if fallback
            .as_ref()
            .is_some_and(|fallback| !same_path(&fallback.real_cargo, &shim.real_cargo))
        {
            bail!("Cargo launcher fallback disagrees with its active installation record");
        }
        return super::cargo_shim::run(
            &shim.real_cargo,
            Some(&record.cargo_home),
            Some(&record.rgo_home),
            std::env::args_os().skip(1).collect(),
        );
    }
    let real_cargo = fallback
        .map(|fallback| fallback.real_cargo)
        .or_else(|| record.and_then(|record| record.supervised_cargo.map(|shim| shim.real_cargo)))
        .context("Cargo launcher has no verified fallback Cargo proxy")?;
    validate_real_cargo(&real_cargo, &executable)?;
    eprintln!("rgo: this Cargo launcher is no longer active; using ordinary Cargo storage");
    super::cargo_shim::exec_real_cargo(
        &real_cargo,
        &std::env::args_os().skip(1).collect::<Vec<_>>(),
    )
}

fn validate_real_cargo(real_cargo: &Path, executable: &Path) -> Result<()> {
    let canonical = real_cargo
        .canonicalize()
        .context("locating the recorded real Cargo proxy")?;
    if !real_cargo.is_absolute()
        || canonical == executable.canonicalize()?
        || is_rgo_shim(&canonical)
        || real_cargo
            .file_name()
            .is_none_or(|name| name != "cargo.exe")
    {
        bail!("Cargo launcher does not name a safe real Cargo proxy");
    }
    Ok(())
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
