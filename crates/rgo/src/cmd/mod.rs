pub mod adopt;
pub mod cache;
pub mod cargo_shim;
pub mod clean;
pub mod daemon;
pub mod doctor;
pub mod gc;
pub mod ls;
pub mod passthrough;
pub mod pin;
pub mod setup;
pub mod status;
#[cfg(windows)]
pub mod windows_cargo_entry;
#[cfg(windows)]
mod windows_job;

use anyhow::Result;
use rgo_core::config::{Config, Resolved};
use rgo_core::paths::RgoPaths;

pub struct Env {
    pub paths: RgoPaths,
    pub cfg: Resolved,
}

pub fn env() -> Result<Env> {
    let paths = RgoPaths::discover()?;
    let cfg = Config::load(&paths.config_file())?.resolve(&paths.root)?;
    Ok(Env { paths, cfg })
}

pub fn human(b: u64) -> String {
    bytesize::ByteSize(b).display().iec().to_string()
}
