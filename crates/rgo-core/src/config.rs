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
    /// Hard ceiling for everything under `builds/` + `cas/`.
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
    #[serde(with = "humantime_serde")]
    pub orphan_grace: Duration,
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
            auto: true,
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
        let volume_total = volume_total_bytes(root).unwrap_or(0);
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
    volume_stats(path).map(|(t, _)| t)
}
/// Free (available to this user) bytes on the filesystem containing `path`.
pub fn volume_free_bytes(path: &Path) -> Option<u64> {
    volume_stats(path).map(|(_, f)| f)
}

fn volume_stats(path: &Path) -> Option<(u64, u64)> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut probe = path;
    loop {
        // Pick the mount point that is the longest prefix of `probe`.
        if let Some(d) = disks
            .iter()
            .filter(|d| probe.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len())
        {
            return Some((d.total_space(), d.available_space()));
        }
        probe = probe.parent()?;
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
