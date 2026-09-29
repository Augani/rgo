use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rgo_protocol::HOME_ENV;

/// Resolved locations of everything rgo owns.
#[derive(Debug, Clone)]
pub struct RgoPaths {
    pub root: PathBuf,
}

impl RgoPaths {
    /// `$RGO_HOME`, then the Cargo-home activation pointer, else `~/.rgo`.
    /// The pointer lets a fresh Cargo process and an IDE find a custom root
    /// without inheriting the environment of the setup shell.
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os(HOME_ENV) {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => match activated_home()? {
                Some(root) => root,
                None => directories::UserDirs::new()
                    .context("cannot determine home directory")?
                    .home_dir()
                    .join(".rgo"),
            },
        };
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        Ok(Self { root })
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn pin_records_dir(&self) -> PathBuf {
        self.state_dir().join("pins")
    }
    pub fn db_file(&self) -> PathBuf {
        self.state_dir().join("meta.sqlite")
    }
    pub fn socket_path(&self) -> PathBuf {
        self.state_dir().join("daemon.sock")
    }
    /// Destructive cleanup needs the Cargo-session guard. Native relocation
    /// and roots with no verified mode do not provide that guard.
    pub fn require_supervised_deletion(&self) -> Result<()> {
        let mode_path = self.state_dir().join("storage-mode");
        let mode = match std::fs::symlink_metadata(&mode_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                std::fs::read(&mode_path)
                    .with_context(|| format!("reading {}", mode_path.display()))?
            }
            Ok(_) => anyhow::bail!("unsafe storage mode record {}", mode_path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", mode_path.display()));
            }
        };
        ensure!(
            mode == b"supervised\n",
            "destructive cleanup requires an activated supervised Cargo installation; use a fresh RGO_HOME with `rgo setup --supervised`"
        );
        let owner_path = self.state_dir().join("owner-cargo-home");
        let owner_metadata = match std::fs::symlink_metadata(&owner_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "destructive cleanup requires an active supervised Cargo installation; supervised Cargo home owner is missing at {}",
                    owner_path.display()
                );
            }
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", owner_path.display()));
            }
        };
        ensure!(
            owner_metadata.is_file() && !owner_metadata.file_type().is_symlink(),
            "unsafe supervised Cargo home owner {}",
            owner_path.display()
        );
        let owner = std::fs::read(&owner_path).with_context(|| {
            format!(
                "reading supervised Cargo home owner {}",
                owner_path.display()
            )
        })?;
        let home = std::str::from_utf8(&owner)
            .with_context(|| format!("decoding {}", owner_path.display()))?;
        let Some(home) = home.strip_suffix('\n') else {
            anyhow::bail!(
                "invalid supervised Cargo home owner {}",
                owner_path.display()
            );
        };
        let home = Path::new(home);
        ensure!(
            home.is_absolute() && !home.components().any(|part| part == Component::ParentDir),
            "invalid supervised Cargo home owner {}",
            owner_path.display()
        );
        let config_path = crate::cargo_config::effective_home_config(home);
        match std::fs::symlink_metadata(&config_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let may_override = crate::cargo_config::may_set_build_dir_in_file(&config_path)
                    .with_context(|| {
                        format!(
                            "destructive cleanup cannot verify that {} keeps direct Cargo outside managed storage",
                            config_path.display()
                        )
                    })?;
                ensure!(
                    !may_override,
                    "destructive cleanup cannot verify that {} keeps direct Cargo outside managed storage; remove its build.build-dir/include override",
                    config_path.display()
                );
            }
            Ok(_) => anyhow::bail!("unsafe Cargo configuration {}", config_path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", config_path.display()));
            }
        }
        Ok(())
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

    fn private_dirs(&self) -> [PathBuf; 7] {
        [
            self.state_dir(),
            self.state_dir().join("locks"),
            self.pin_records_dir(),
            self.builds_dir(),
            self.tmp_dir(),
            self.quarantine_dir(),
            self.logs_dir(),
        ]
    }

    pub fn validate_root(&self) -> Result<()> {
        let root_text = self
            .root
            .to_str()
            .context("RGO_HOME must be UTF-8 to persist activation")?;
        ensure!(
            !root_text.contains(['\r', '\n']),
            "RGO_HOME must not contain a newline"
        );
        ensure!(
            !self
                .root
                .components()
                .any(|part| part == Component::ParentDir),
            "RGO_HOME must not contain `..` path components"
        );
        let resolved = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        ensure!(
            resolved.parent().is_some(),
            "RGO_HOME must not be a filesystem root"
        );
        #[cfg(unix)]
        if self.root.exists() {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&self.root)?.permissions().mode();
            ensure!(
                mode & 0o022 == 0,
                "RGO_HOME {} is writable by other users; choose a private storage directory",
                self.root.display()
            );
        }
        for directory in self.private_dirs() {
            refuse_symlink(&directory)?;
        }
        Ok(())
    }

    pub fn ensure_layout(&self) -> Result<()> {
        #[cfg(unix)]
        let root_existed = self.root.exists();
        self.validate_root()?;
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        for directory in self.private_dirs() {
            refuse_symlink(&directory)?;
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut owned = self.private_dirs().to_vec();
            if !root_existed {
                owned.push(self.root.clone());
            }
            for d in &owned {
                let mut permissions = std::fs::metadata(d)?.permissions();
                permissions.set_mode(0o700);
                std::fs::set_permissions(d, permissions)
                    .with_context(|| format!("restricting {}", d.display()))?;
            }
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

    /// Strict enumeration for budget and deletion decisions. A partial read
    /// must not make a storage root appear smaller or hide a context.
    pub fn checked_managed_build_dirs(&self) -> Result<Vec<PathBuf>> {
        let root = self.builds_dir();
        match std::fs::symlink_metadata(&root) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "unsafe managed build root {}",
                root.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", root.display()));
            }
        }
        let mut contexts = Vec::new();
        for shard in std::fs::read_dir(&root)? {
            let shard = shard?;
            ensure!(
                shard.file_type()?.is_dir(),
                "unsafe managed build shard {}",
                shard.path().display()
            );
            for context in std::fs::read_dir(shard.path())? {
                let context = context?;
                ensure!(
                    context.file_type()?.is_dir(),
                    "unsafe managed build context {}",
                    context.path().display()
                );
                contexts.push(context.path());
            }
        }
        Ok(contexts)
    }
}

fn refuse_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "refusing symlinked rgo storage directory {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", path.display())),
    }
    Ok(())
}

/// This file is deliberately outside the managed storage tree: GC never
/// removes it, and the dependency-light wrapper can read it as plain text.
pub fn activation_pointer(cargo_home: &Path) -> PathBuf {
    cargo_home.join(".rgo-home")
}

pub fn activated_home() -> Result<Option<PathBuf>> {
    let pointer = activation_pointer(&cargo_home()?);
    let value = match std::fs::read_to_string(&pointer) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", pointer.display())),
    };
    let root = PathBuf::from(value.trim_end_matches(['\r', '\n']));
    anyhow::ensure!(
        root.is_absolute(),
        "{} must contain an absolute RGO_HOME path",
        pointer.display()
    );
    Ok(Some(root))
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn existing_storage_root_keeps_its_permissions() {
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("existing");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        RgoPaths { root: root.clone() }.ensure_layout().unwrap();
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn shared_storage_root_is_rejected_without_chmod() {
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("shared");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(RgoPaths { root: root.clone() }.ensure_layout().is_err());
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[test]
    fn nonexistent_parent_traversal_cannot_retarget_storage() {
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("not-created").join("..");
        assert!(RgoPaths { root }.ensure_layout().is_err());
        assert!(!private.path().join("not-created").exists());
    }

    #[test]
    fn symlinked_state_directory_does_not_change_the_destination() {
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("rgo");
        let outside = private.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("state")).unwrap();

        assert!(RgoPaths { root }.ensure_layout().is_err());
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!outside.join("locks").exists());
    }
}
