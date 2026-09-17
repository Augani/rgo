//! Tiered garbage collection. `plan` is pure policy (testable without a filesystem);
//! `execute` is the mechanism and enforces the safety rules regardless of policy.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::{Resolved, volume_free_bytes};
use crate::context::{BuildContext, incremental_dirs};
use crate::paths::RgoPaths;

/// Contexts whose `.cargo-lock` changed within this window are assumed live (Phase 1 heuristic;
/// Phase 2 replaces this with daemon leases).
pub const LIVE_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Tmp = 0,
    Orphan = 1,
    StaleIncremental = 2,
    StaleContext = 3,
    Pressure = 4,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub tier: Tier,
    pub path: PathBuf,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub managed_bytes: u64,
    pub target_bytes: u64,
    pub free_bytes: u64,
    /// Contexts that looked live (recent `.cargo-build-lock`) and were therefore never considered.
    pub skipped_live: usize,
    /// Contexts protected by daemon leases and therefore never considered.
    pub skipped_leased: usize,
}

impl Plan {
    pub fn reclaim_bytes(&self) -> u64 {
        self.actions.iter().map(|a| a.bytes).sum()
    }
}

pub struct Inputs<'a> {
    pub paths: &'a RgoPaths,
    pub cfg: &'a Resolved,
    pub contexts: &'a [BuildContext],
    pub pinned: &'a [PathBuf],
    pub leased: &'a [PathBuf],
    pub now: SystemTime,
    pub aggressive: bool,
}

pub fn plan(inp: &Inputs) -> Plan {
    let managed: u64 = inp.contexts.iter().map(|c| c.usage.physical_bytes).sum();
    let free = volume_free_bytes(&inp.paths.root).unwrap_or(u64::MAX);
    let free_deficit = inp.cfg.min_free_space.saturating_sub(free);
    let over_soft = managed.saturating_sub(inp.cfg.soft_watermark);
    let target = if over_soft > 0 || inp.aggressive {
        (inp.cfg.soft_watermark as f64 * 0.9) as u64
    } else {
        managed
    };
    let mut needed = managed.saturating_sub(target).max(free_deficit);
    let skipped_live = inp
        .contexts
        .iter()
        .filter(|c| c.recently_locked(LIVE_WINDOW, inp.now))
        .count();
    let skipped_leased = inp
        .contexts
        .iter()
        .filter(|c| inp.leased.iter().any(|path| same_path(path, &c.dir)))
        .count();
    let mut plan = Plan {
        actions: vec![],
        managed_bytes: managed,
        target_bytes: target,
        free_bytes: free,
        skipped_live,
        skipped_leased,
    };

    let eligible = |c: &BuildContext| {
        !inp.pinned.iter().any(|path| same_path(path, &c.dir))
            && !inp.leased.iter().any(|path| same_path(path, &c.dir))
            && !c.recently_locked(LIVE_WINDOW, inp.now)
    };

    // Tier 0 and 1 are always taken: they are garbage regardless of budget.
    for c in inp.contexts.iter().filter(|c| eligible(c) && c.is_orphan()) {
        if c.idle_for(inp.now) >= inp.cfg.gc.orphan_grace {
            let m = c
                .sidecar
                .as_ref()
                .map(|s| s.manifest_path.clone())
                .unwrap_or_default();
            plan.actions.push(Action {
                tier: Tier::Orphan,
                path: c.dir.clone(),
                bytes: c.usage.physical_bytes,
                reason: format!("workspace manifest no longer exists: {m}"),
            });
            needed = needed.saturating_sub(c.usage.physical_bytes);
        }
    }
    if needed == 0 && !inp.aggressive {
        return plan;
    }

    // Tier 2: stale incremental state, oldest first.
    let mut by_age: Vec<&BuildContext> = inp
        .contexts
        .iter()
        .filter(|c| eligible(c) && !c.is_orphan())
        .collect();
    by_age.sort_by_key(|c| c.last_used);
    for c in &by_age {
        if needed == 0 && !inp.aggressive {
            break;
        }
        if c.idle_for(inp.now) >= inp.cfg.gc.incremental_retention
            && c.incremental_usage.physical_bytes > 0
        {
            for d in incremental_dirs(&c.dir) {
                plan.actions.push(Action {
                    tier: Tier::StaleIncremental,
                    path: d,
                    bytes: 0,
                    reason: format!(
                        "incremental state idle for {}",
                        humantime::format_duration(trunc(c.idle_for(inp.now)))
                    ),
                });
            }
            needed = needed.saturating_sub(c.incremental_usage.physical_bytes);
        }
    }

    // Tier 3: whole contexts idle past retention, LRU.
    for c in &by_age {
        if needed == 0 {
            break;
        }
        if c.idle_for(inp.now) >= inp.cfg.gc.context_retention {
            plan.actions.push(Action {
                tier: Tier::StaleContext,
                path: c.dir.clone(),
                bytes: c.usage.physical_bytes,
                reason: format!(
                    "idle for {}",
                    humantime::format_duration(trunc(c.idle_for(inp.now)))
                ),
            });
            needed = needed.saturating_sub(c.usage.physical_bytes);
        }
    }

    // Tier 4: only under real pressure. LRU regardless of age, but keep the most recent
    // context per workspace so an active project never loses everything.
    if needed > 0 && (free_deficit > 0 || inp.aggressive || managed > inp.cfg.max_size) {
        let mut keep_latest: std::collections::HashMap<String, &BuildContext> = Default::default();
        for c in &by_age {
            let ws = c
                .sidecar
                .as_ref()
                .map(|s| s.workspace_root.clone())
                .unwrap_or_else(|| c.id().to_owned());
            keep_latest.insert(ws, c);
        }
        let protected: Vec<&Path> = keep_latest.values().map(|c| c.dir.as_path()).collect();
        for c in &by_age {
            if needed == 0 {
                break;
            }
            if protected.contains(&c.dir.as_path()) || plan.actions.iter().any(|a| a.path == c.dir)
            {
                continue;
            }
            plan.actions.push(Action {
                tier: Tier::Pressure,
                path: c.dir.clone(),
                bytes: c.usage.physical_bytes,
                reason: "disk pressure (LRU)".into(),
            });
            needed = needed.saturating_sub(c.usage.physical_bytes);
        }
    }
    plan.actions.sort_by_key(|a| a.tier);
    plan
}

/// Deletes are `rename` into `tmp/` (atomic, same volume) then recursive remove, so a
/// crash mid-way leaves only tier-0 garbage. A path that cannot be renamed (open files
/// on Windows, live build racing us) is skipped, never force-deleted.
pub fn execute(paths: &RgoPaths, plan: &Plan, dry_run: bool) -> Result<u64> {
    let mut reclaimed = 0;
    for a in &plan.actions {
        if dry_run {
            info!(tier = ?a.tier, path = %a.path.display(), bytes = a.bytes, "would remove: {}", a.reason);
            continue;
        }
        match stage_and_remove(paths, &a.path) {
            Ok(()) => {
                info!(tier = ?a.tier, path = %a.path.display(), "removed: {}", a.reason);
                reclaimed += a.bytes;
            }
            Err(e) => warn!(path = %a.path.display(), %e, "skipped"),
        }
    }
    sweep_tmp(paths)?;
    Ok(reclaimed)
}

fn stage_and_remove(paths: &RgoPaths, victim: &Path) -> Result<()> {
    std::fs::create_dir_all(paths.tmp_dir())?;
    let staged = paths.tmp_dir().join(format!(
        "gc-{}-{}",
        std::process::id(),
        victim.file_name().and_then(|s| s.to_str()).unwrap_or("x")
    ));
    std::fs::rename(victim, &staged).with_context(|| format!("staging {}", victim.display()))?;
    if staged.is_dir() {
        std::fs::remove_dir_all(&staged)
    } else {
        std::fs::remove_file(&staged)
    }
    .with_context(|| format!("removing {}", staged.display()))
}

/// Tier 0: anything left in `tmp/` older than an hour is abandoned.
pub fn sweep_tmp(paths: &RgoPaths) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(paths.tmp_dir()) else {
        return Ok(());
    };
    for e in rd.filter_map(Result::ok) {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| m.elapsed().unwrap_or_default() > Duration::from_secs(3600))
            .unwrap_or(true);
        if old {
            let p = e.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    Ok(())
}

fn trunc(d: Duration) -> Duration {
    Duration::from_secs(d.as_secs())
}

fn same_path(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf())
        == std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Cache, Gc, Resolved};
    use crate::size::Usage;
    use rgo_protocol::ContextSidecar;

    #[test]
    fn leased_contexts_are_excluded_from_pressure_gc() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let leased_dir = paths.builds_dir().join("aa/leased");
        let reclaimable_dir = paths.builds_dir().join("bb/reclaimable");
        let newest_dir = paths.builds_dir().join("cc/newest");
        std::fs::create_dir_all(&leased_dir).unwrap();
        std::fs::create_dir_all(&reclaimable_dir).unwrap();
        std::fs::create_dir_all(&newest_dir).unwrap();
        let manifest = root.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let sidecar = || ContextSidecar {
            version: rgo_protocol::PROTOCOL_VERSION,
            workspace_root: root.path().display().to_string(),
            manifest_path: manifest.display().to_string(),
            toolchain: None,
            first_seen: 0,
            last_seen: 0,
        };
        let context = |dir: PathBuf, last_used: SystemTime| BuildContext {
            dir,
            sidecar: Some(sidecar()),
            last_used,
            usage: Usage {
                physical_bytes: 100,
                logical_bytes: 100,
                files: 1,
            },
            incremental_usage: Usage::default(),
        };
        let now = SystemTime::now();
        let contexts = vec![
            context(leased_dir.clone(), now - Duration::from_secs(3600)),
            context(reclaimable_dir.clone(), now - Duration::from_secs(3500)),
            context(newest_dir, now - Duration::from_secs(3400)),
        ];
        let cfg = Resolved {
            max_size: 1,
            soft_watermark: 0,
            min_free_space: 0,
            gc: Gc::default(),
            cache: Cache::default(),
            volume_total: 1,
        };
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &contexts,
            pinned: &[],
            leased: &[leased_dir],
            now,
            aggressive: true,
        });
        assert_eq!(plan.skipped_leased, 1);
        assert!(
            plan.actions
                .iter()
                .all(|action| action.path != contexts[0].dir)
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.path == contexts[1].dir)
        );
    }
}
