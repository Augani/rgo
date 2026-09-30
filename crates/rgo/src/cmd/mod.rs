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

fn workspace_matches(stored: &str, requested: &str) -> bool {
    if stored == requested {
        return true;
    }
    if !std::path::Path::new(requested).is_absolute() {
        return false;
    }
    match (
        std::fs::canonicalize(stored),
        std::fs::canonicalize(requested),
    ) {
        (Ok(stored), Ok(requested)) => stored == requested,
        _ => false,
    }
}

fn display_workspace_path(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(rest) = raw.strip_prefix(r"\\?\") {
            let bytes = rest.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && bytes[2] == b'\\'
            {
                return rest.to_owned();
            }
        }
    }
    raw.into_owned()
}
