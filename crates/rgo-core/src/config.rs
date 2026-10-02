//! `~/.rgo/config.toml`. Every field is optional; `"auto"` sizes are derived from the
//! volume that holds the storage root so defaults are machine-aware, not universal.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub storage: Storage,
    pub gc: Gc,
    pub cache: Cache,
    pub remote: Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    /// Configured recovery target for managed builds and CAS, not a strict
    /// quota while active/pinned work or filesystem constraints prevent GC.
    pub max_size: Size,
    /// Fraction of `max_size` at which background GC starts.
    pub soft_watermark: f64,
    /// GC runs immediately if the volume's free space drops below this.
    pub min_free_space: Size,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gc {
    #[serde(with = "humantime_serde")]
    pub incremental_retention: Duration,
    #[serde(with = "humantime_serde")]
    pub context_retention: Duration,
    /// Minimum inactivity before reclaiming a confirmed missing manifest;
    /// measured from last use, not from when deletion was first observed.
    #[serde(with = "humantime_serde")]
    pub orphan_grace: Duration,
    /// Age limit for unused compiler-result manifests. Shared objects remain
    /// until their final manifest reference is removed.
    #[serde(with = "humantime_serde")]
    pub cache_retention: Duration,
    pub auto: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    /// Phase 3. Off until differential testing gates pass.
    pub enabled: bool,
    /// Phase 4. Remapping changes observable source paths, so it is opt-in.
    pub remap_workspace_paths: bool,
    /// Maximum time a cache waiter spends waiting for another compiler.
    #[serde(with = "humantime_serde")]
    pub single_flight_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Remote {
    /// Remote CAS is opt-in and is ignored when `cache.enabled` is false.
    pub enabled: bool,
    pub endpoint: String,
    pub namespace: String,
    pub token_env: String,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    pub max_object_size: Size,
    pub upload: bool,
    /// Only intended for an in-process loopback test server. Production endpoints must use TLS.
    pub allow_insecure_loopback: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            enabled: false,
            remap_workspace_paths: false,
            single_flight_timeout: Duration::from_secs(
                rgo_protocol::DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS as u64,
            ),
        }
    }
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            namespace: String::new(),
            token_env: "RGO_REMOTE_TOKEN".into(),
            timeout: Duration::from_secs(5),
            max_object_size: Size::Bytes(2 * (1 << 30)),
            upload: true,
            allow_insecure_loopback: false,
        }
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            max_size: Size::Auto,
            soft_watermark: 0.80,
            min_free_space: Size::Auto,
        }
    }
}
impl Default for Gc {
    fn default() -> Self {
        Self {
            incremental_retention: Duration::from_secs(7 * 86400),
            context_retention: Duration::from_secs(30 * 86400),
            orphan_grace: Duration::from_secs(3600),
            cache_retention: Duration::from_secs(30 * 86400),
            // Whole-context deletion is not yet proven safe for Cargo processes
            // waiting on a lock whose directory is renamed. Keep unattended
            // destructive maintenance off until the P2 lifecycle gate closes.
            auto: false,
        }
    }
}

/// A byte size or `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Auto,
    Bytes(u64),
}

impl Serialize for Size {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Size::Auto => s.serialize_str("auto"),
            Size::Bytes(b) => s.serialize_str(&bytesize::ByteSize(*b).display().iec().to_string()),
        }
    }
}
impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_size(&s).map_err(serde::de::Error::custom)
    }
}

pub fn parse_size(s: &str) -> Result<Size> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("auto") {
        return Ok(Size::Auto);
    }
    let b: bytesize::ByteSize = t
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid size {t:?}: {e}"))?;
    Ok(Size::Bytes(b.as_u64()))
}

/// Config with every `auto` resolved against the volume holding `root`.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub max_size: u64,
    pub soft_watermark: u64,
    pub min_free_space: u64,
    pub gc: Gc,
    pub cache: Cache,
    pub remote: Remote,
    pub volume_total: u64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn resolve(&self, root: &Path) -> Result<Resolved> {
        if !(0.1..=1.0).contains(&self.storage.soft_watermark) {
            bail!("storage.soft_watermark must be within 0.1..=1.0");
        }
        let volume_total = volume_total_bytes_checked(root).context("resolving storage volume")?;
        const GB: u64 = 1 << 30;
        let max_size = match self.storage.max_size {
            Size::Bytes(b) => b,
            // 15% of the volume, clamped to [20 GiB, 150 GiB]
            Size::Auto => (volume_total * 15 / 100).clamp(20 * GB, 150 * GB),
        };
        let min_free_space = match self.storage.min_free_space {
            Size::Bytes(b) => b,
            Size::Auto => (volume_total / 10).max(20 * GB),
        };
        Ok(Resolved {
            max_size,
            soft_watermark: (max_size as f64 * self.storage.soft_watermark) as u64,
            min_free_space,
            gc: self.gc.clone(),
            cache: self.cache.clone(),
            remote: self.remote.clone(),
            volume_total,
        })
    }
}

/// Total capacity of the filesystem containing `path` (walks up until a mount is found).
pub fn volume_total_bytes(path: &Path) -> Option<u64> {
    volume_total_bytes_checked(path).ok()
}
/// Free (available to this user) bytes on the filesystem containing `path`.
pub fn volume_free_bytes(path: &Path) -> Option<u64> {
    volume_free_bytes_checked(path).ok()
}

pub fn volume_total_bytes_checked(path: &Path) -> Result<u64> {
    volume_stats_checked(path).map(|(total, _)| total)
}

pub fn volume_free_bytes_checked(path: &Path) -> Result<u64> {
    volume_stats_checked(path).map(|(_, free)| free)
}

fn volume_stats_checked(path: &Path) -> Result<(u64, u64)> {
    let mut probe = path;
    loop {
        // Query the filesystem containing the actual path rather than
        // inferring a mount from its spelling. This follows symlinked roots,
        // including a custom RGO_HOME on another volume. A not-yet-created
        // root inherits the nearest existing ancestor's filesystem. Windows
        // can return drive-level stats for a path beneath a regular file, so
        // check that the ancestor is a directory before asking fs4.
        match std::fs::metadata(probe) {
            Ok(metadata) if metadata.is_dir() => {
                let stats = fs4::statvfs(probe)
                    .with_context(|| format!("probing volume at {}", probe.display()))?;
                return Ok((stats.total_space(), stats.available_space()));
            }
            Ok(_) => anyhow::bail!("probing volume at {}: not a directory", probe.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                probe = probe.parent().with_context(|| {
                    format!("no existing volume ancestor for {}", path.display())
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("probing volume at {}", probe.display()));
            }
        }
    }
}

mod humantime_serde {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        humantime::format_duration(*d).to_string().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_default() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.storage.max_size, Size::Auto);
        assert_eq!(c.gc.orphan_grace, Duration::from_secs(3600));
        assert!(!c.gc.auto);
        assert!(!c.cache.enabled);
        assert!(!c.remote.enabled);
        assert_eq!(c.remote.token_env, "RGO_REMOTE_TOKEN");
        assert!(!c.cache.remap_workspace_paths);
        assert_eq!(
            c.cache.single_flight_timeout,
            Duration::from_secs(rgo_protocol::DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS as u64)
        );
    }

    #[test]
    fn volume_probe_uses_existing_parent_of_a_new_storage_root() {
        let temporary = tempfile::tempdir().unwrap();
        let new_root = temporary.path().join("not-created/rgo");
        assert_eq!(
            volume_total_bytes(&new_root),
            volume_total_bytes(temporary.path())
        );
        assert!(volume_free_bytes(&new_root).is_some());
    }

    #[test]
    fn failed_volume_probe_does_not_invent_automatic_limits() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("not-a-directory");
        std::fs::write(&file, b"file").unwrap();
        let impossible_root = file.join("rgo");
        assert!(volume_free_bytes_checked(&impossible_root).is_err());
        assert!(volume_free_bytes(&impossible_root).is_none());
        assert!(Config::default().resolve(&impossible_root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn volume_probe_follows_a_symlinked_storage_root() {
        let temporary = tempfile::tempdir().unwrap();
        let link = temporary.path().join("storage-link");
        std::os::unix::fs::symlink(temporary.path(), &link).unwrap();
        assert_eq!(
            volume_total_bytes(&link),
            volume_total_bytes(temporary.path())
        );
    }

    #[test]
    fn parses_sizes_and_durations() {
        let c: Config = toml::from_str(
            r#"
            [storage]
            max_size = "60GB"
            min_free_space = "auto"
            [gc]
            incremental_retention = "3d"
            [cache]
            remap_workspace_paths = true
            single_flight_timeout = "7s"
            [remote]
            timeout = "2s"
            max_object_size = "4MiB"
            "#,
        )
        .unwrap();
        assert_eq!(c.storage.max_size, Size::Bytes(60_000_000_000));
        assert_eq!(c.storage.min_free_space, Size::Auto);
        assert_eq!(c.gc.incremental_retention, Duration::from_secs(3 * 86400));
        assert!(c.cache.remap_workspace_paths);
        assert_eq!(c.cache.single_flight_timeout, Duration::from_secs(7));
        assert_eq!(c.remote.timeout, Duration::from_secs(2));
        assert_eq!(c.remote.max_object_size, Size::Bytes(4 * 1024 * 1024));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("[storage]\nmax_sz = \"1GB\"").is_err());
    }
}
