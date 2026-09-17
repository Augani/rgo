use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rgo_protocol::HOME_ENV;

/// Resolved locations of everything rgo owns.
#[derive(Debug, Clone)]
pub struct RgoPaths {
    pub root: PathBuf,
}

impl RgoPaths {
    /// `$RGO_HOME`, else `~/.rgo`.
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os(HOME_ENV) {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => directories::UserDirs::new()
                .context("cannot determine home directory")?
                .home_dir()
                .join(".rgo"),
        };
        Ok(Self { root })
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn db_file(&self) -> PathBuf {
        self.state_dir().join("meta.sqlite")
    }
    pub fn socket_path(&self) -> PathBuf {
        self.state_dir().join("daemon.sock")
    }
    /// Parent of every Cargo build-dir rgo manages. Cargo's `{workspace-path-hash}`
    /// template expands to **two** components (`<2hex>/<rest>`, observed on Cargo 1.98),
    /// so a managed build-dir is `builds/xx/yyyy…`.
    pub fn builds_dir(&self) -> PathBuf {
        self.root.join("builds")
    }
    pub fn cas_dir(&self) -> PathBuf {
        self.root.join("cas")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
    pub fn quarantine_dir(&self) -> PathBuf {
        self.root.join("quarantine")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The exact string written into Cargo's `build.build-dir`.
    pub fn build_dir_template(&self) -> String {
        format!("{}/{{workspace-path-hash}}", self.builds_dir().display())
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for d in [
            &self.root,
            &self.state_dir(),
            &self.state_dir().join("locks"),
            &self.builds_dir(),
            &self.tmp_dir(),
            &self.quarantine_dir(),
            &self.logs_dir(),
        ] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// True if `p` is exactly two levels below `builds/` (i.e. a Cargo build-dir we manage).
    pub fn is_managed_build_dir(&self, p: &Path) -> bool {
        p.parent()
            .and_then(Path::parent)
            .is_some_and(|gp| gp == self.builds_dir())
    }

    /// Enumerate managed build-dirs (`builds/xx/yyyy`).
    pub fn managed_build_dirs(&self) -> Vec<PathBuf> {
        let Ok(shards) = std::fs::read_dir(self.builds_dir()) else {
            return vec![];
        };
        shards
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .flat_map(|shard| {
                std::fs::read_dir(shard)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|e| e.path())
            })
            .filter(|p| p.is_dir())
            .collect()
    }
}

/// `$CARGO_HOME`, else `~/.cargo`.
pub fn cargo_home() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    Ok(directories::UserDirs::new()
        .context("cannot determine home directory")?
        .home_dir()
        .join(".cargo"))
}
