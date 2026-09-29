//! Tiered garbage collection. `plan` selects policy from measured contexts and
//! checked filesystem state; `execute` repeats safety checks before deletion.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::{Resolved, volume_free_bytes_checked};
use crate::context::{BuildContext, incremental_dirs_checked};
use crate::paths::RgoPaths;
use crate::size::Scanner;

/// Contexts whose `.cargo-build-lock` changed within this window are assumed live (Phase 1 heuristic;
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
    /// Contexts protected by a pin or unavailable workspace, regardless of
    /// whether another protection also applies.
    pub skipped_pinned: usize,
    pub skipped_unavailable: usize,
    /// Allocated context bytes that cannot be selected for cleanup now.
    pub protected_context_bytes: u64,
}

impl Plan {
    pub fn reclaim_bytes(&self) -> u64 {
        self.actions.iter().map(|a| a.bytes).sum()
    }
}

#[derive(Debug, Default)]
pub struct Execution {
    pub reclaimed_bytes: u64,
    pub skipped_actions: u64,
    pub skipped_bytes: u64,
    pub first_skip: Option<String>,
}

impl Execution {
    pub fn absorb(&mut self, other: Self) {
        self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(other.reclaimed_bytes);
        self.skipped_actions = self.skipped_actions.saturating_add(other.skipped_actions);
        self.skipped_bytes = self.skipped_bytes.saturating_add(other.skipped_bytes);
        if self.first_skip.is_none() {
            self.first_skip = other.first_skip;
        }
    }

    pub fn note_skip(&mut self, path: &Path, bytes: u64, error: &anyhow::Error) {
        self.skipped_actions = self.skipped_actions.saturating_add(1);
        self.skipped_bytes = self.skipped_bytes.saturating_add(bytes);
        if self.first_skip.is_none() {
            self.first_skip = Some(format!("{}: {error:#}", path.display()));
        }
    }
}

pub struct Inputs<'a> {
    pub paths: &'a RgoPaths,
    pub cfg: &'a Resolved,
    pub contexts: &'a [BuildContext],
    /// Managed storage outside build contexts: CAS plus temporary, quarantine,
    /// and operational files. The caller owns non-context eviction policy.
    pub other_managed_bytes: u64,
    pub pinned: &'a [PathBuf],
    pub leased: &'a [PathBuf],
    pub now: SystemTime,
    pub aggressive: bool,
    /// Periodic retention pass even when the byte target is already met.
    pub age_maintenance: bool,
    /// Defer whole-context pressure eviction until other managed storage has
    /// had a chance to satisfy the same byte target.
    pub allow_pressure_contexts: bool,
    pub target_bytes: Option<u64>,
}

pub fn plan(inp: &Inputs) -> Result<Plan> {
    let managed: u64 = inp
        .contexts
        .iter()
        .map(|c| c.usage.physical_bytes)
        .sum::<u64>()
        .saturating_add(inp.other_managed_bytes);
    let free = volume_free_bytes_checked(&inp.paths.root)?;
    let free_deficit = inp.cfg.min_free_space.saturating_sub(free);
    let over_soft = managed.saturating_sub(inp.cfg.soft_watermark);
    let target = if let Some(target) = inp.target_bytes {
        target.min(managed)
    } else if over_soft > 0 || inp.aggressive {
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
    let skipped_pinned = inp
        .contexts
        .iter()
        .filter(|c| {
            crate::context::is_pinned(inp.paths, &c.dir)
                || inp.pinned.iter().any(|path| same_path(path, &c.dir))
        })
        .count();
    let skipped_unavailable = inp
        .contexts
        .iter()
        .filter(|c| c.workspace_unavailable())
        .count();
    let eligible = |c: &BuildContext| {
        !crate::context::is_pinned(inp.paths, &c.dir)
            && !c.workspace_unavailable()
            && !inp.pinned.iter().any(|path| same_path(path, &c.dir))
            && !inp.leased.iter().any(|path| same_path(path, &c.dir))
            && !c.recently_locked(LIVE_WINDOW, inp.now)
    };
    let protected_context_bytes = inp
        .contexts
        .iter()
        .filter(|c| !eligible(c))
        .map(|c| c.usage.physical_bytes)
        .sum();
    let mut plan = Plan {
        actions: vec![],
        managed_bytes: managed,
        target_bytes: target,
        free_bytes: free,
        skipped_live,
        skipped_leased,
        skipped_pinned,
        skipped_unavailable,
        protected_context_bytes,
    };

    // Tier 0 and 1 are always taken: they are garbage regardless of budget.
    let temporary = temporary_actions(inp.paths, inp.now, needed > 0 || inp.aggressive)?;
    needed = needed.saturating_sub(temporary.iter().map(|action| action.bytes).sum::<u64>());
    plan.actions.extend(temporary);
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
    if needed == 0 && !inp.aggressive && !inp.age_maintenance {
        return Ok(plan);
    }

    // Tier 2: stale incremental state, oldest first.
    let mut by_age: Vec<&BuildContext> = inp
        .contexts
        .iter()
        .filter(|c| eligible(c) && !c.is_orphan())
        .collect();
    by_age.sort_by_key(|c| c.last_used);
    for c in &by_age {
        if needed == 0 && !inp.aggressive && !inp.age_maintenance {
            break;
        }
        if c.idle_for(inp.now) >= inp.cfg.gc.incremental_retention
            && c.incremental_usage.physical_bytes > 0
        {
            let mut scanner = Scanner::new();
            let mut incremental_bytes = 0u64;
            for d in incremental_dirs_checked(&c.dir)? {
                let bytes = scanner.measure_checked(&d)?.physical_bytes;
                incremental_bytes = incremental_bytes.saturating_add(bytes);
                plan.actions.push(Action {
                    tier: Tier::StaleIncremental,
                    path: d,
                    bytes,
                    reason: format!(
                        "incremental state idle for {}",
                        humantime::format_duration(trunc(c.idle_for(inp.now)))
                    ),
                });
            }
            needed = needed.saturating_sub(incremental_bytes);
        }
    }

    // Tier 3: whole contexts idle past retention, LRU.
    for c in &by_age {
        if needed == 0 && !inp.age_maintenance {
            break;
        }
        if c.idle_for(inp.now) >= inp.cfg.gc.context_retention {
            let nested_bytes = plan
                .actions
                .iter()
                .filter(|action| action.path.starts_with(&c.dir))
                .map(|action| action.bytes)
                .sum::<u64>();
            plan.actions
                .retain(|action| !action.path.starts_with(&c.dir));
            needed = needed.saturating_add(nested_bytes);
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

    // Tier 4: under real pressure, idle contexts are evictable even when they are a
    // workspace's only context. Pins, leases, and recent locks are the hard protections.
    if inp.allow_pressure_contexts
        && needed > 0
        && (free_deficit > 0
            || inp.aggressive
            || inp.target_bytes.is_some()
            || managed > inp.cfg.max_size)
    {
        for c in &by_age {
            if needed == 0 {
                break;
            }
            if plan.actions.iter().any(|a| a.path == c.dir) {
                continue;
            }
            let nested_bytes = plan
                .actions
                .iter()
                .filter(|action| action.path.starts_with(&c.dir))
                .map(|action| action.bytes)
                .sum::<u64>();
            plan.actions
                .retain(|action| !action.path.starts_with(&c.dir));
            needed = needed.saturating_add(nested_bytes);
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
    Ok(plan)
}

/// Whole-context pressure candidates in LRU order. The daemon uses these only
/// after garbage, expired state, and cold CAS have had their turn at the same
/// target; the executor still repeats its lock safety check before removal.
pub fn pressure_candidates(inp: &Inputs) -> Vec<Action> {
    let mut contexts: Vec<&BuildContext> = inp
        .contexts
        .iter()
        .filter(|context| {
            !context.is_orphan()
                && !crate::context::is_pinned(inp.paths, &context.dir)
                && !context.workspace_unavailable()
                && !inp.pinned.iter().any(|path| same_path(path, &context.dir))
                && !inp.leased.iter().any(|path| same_path(path, &context.dir))
                && !context.recently_locked(LIVE_WINDOW, inp.now)
        })
        .collect();
    contexts.sort_by_key(|context| context.last_used);
    contexts
        .into_iter()
        .map(|context| Action {
            tier: Tier::Pressure,
            path: context.dir.clone(),
            bytes: context.usage.physical_bytes,
            reason: "disk pressure (LRU)".into(),
        })
        .collect()
}

/// Deletes are `rename` into `tmp/` (atomic, same volume) then recursive remove, so a
/// crash mid-way leaves only tier-0 garbage. A path that cannot be renamed (open files
/// on Windows, live build racing us) is skipped, never force-deleted.
pub fn execute(paths: &RgoPaths, plan: &Plan, dry_run: bool) -> Result<Execution> {
    let mut execution = Execution::default();
    for a in &plan.actions {
        if dry_run {
            info!(tier = ?a.tier, path = %a.path.display(), bytes = a.bytes, "would remove: {}", a.reason);
            continue;
        }
        match stage_and_remove(paths, &a.path) {
            Ok(()) => {
                info!(tier = ?a.tier, path = %a.path.display(), "removed: {}", a.reason);
                execution.reclaimed_bytes = execution.reclaimed_bytes.saturating_add(a.bytes);
            }
            Err(e) => {
                warn!(path = %a.path.display(), %e, "skipped");
                execution.note_skip(&a.path, a.bytes, &e);
            }
        }
    }
    Ok(execution)
}

/// Remove a path using rgo's atomic rename-before-delete rule.
pub fn remove_atomically(paths: &RgoPaths, victim: &Path) -> Result<()> {
    stage_and_remove(paths, victim)
}

fn stage_and_remove(paths: &RgoPaths, victim: &Path) -> Result<()> {
    stage_and_remove_with(paths, victim, |_| {})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeletePhase {
    Locked,
    Staged,
}

fn stage_and_remove_with(
    paths: &RgoPaths,
    victim: &Path,
    mut after_phase: impl FnMut(DeletePhase),
) -> Result<()> {
    let context = paths
        .checked_managed_build_dirs()?
        .into_iter()
        .find(|context| victim == context || victim.starts_with(context));
    if victim.starts_with(paths.builds_dir()) && context.is_none() {
        anyhow::bail!(
            "cannot identify the managed build context containing {}",
            victim.display()
        );
    }
    let _lifecycle = crate::supervision::try_lock_gc(paths, context.as_deref())?
        .context("a supervised Cargo invocation is using managed storage")?;
    let _cas_staging = if victim.parent() == Some(paths.cas_dir().as_path()) && is_cas_stage(victim)
    {
        Some(
            rgo_cas::try_lock_staging_cleanup(&paths.cas_dir())?
                .context("a CAS producer is publishing staging data")?,
        )
    } else {
        None
    };
    validate_deletion_path(paths, victim)?;
    after_phase(DeletePhase::Locked);
    #[cfg(debug_assertions)]
    pause_after_gc_lock_for_test()?;
    if context
        .as_ref()
        .is_some_and(|context| crate::context::is_pinned(paths, context))
    {
        anyhow::bail!("pinned build context at {}", victim.display());
    }
    if context.as_ref().is_some_and(|context| {
        crate::context::read_sidecar(context).is_none_or(|sidecar| {
            crate::context::workspace_state(&sidecar) == crate::context::WorkspaceState::Unavailable
        })
    }) {
        anyhow::bail!(
            "workspace attribution or availability is unverified for {}",
            victim.display()
        );
    }
    if is_live_managed_path(paths, victim)? {
        anyhow::bail!("live Cargo build lock detected near {}", victim.display());
    }
    std::fs::create_dir_all(paths.tmp_dir())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staged = paths.tmp_dir().join(format!(
        "gc-{}-{nonce}-{}",
        std::process::id(),
        victim.file_name().and_then(|s| s.to_str()).unwrap_or("x")
    ));
    std::fs::rename(victim, &staged).with_context(|| format!("staging {}", victim.display()))?;
    after_phase(DeletePhase::Staged);
    let removed = if staged.is_dir() {
        std::fs::remove_dir_all(&staged)
    } else {
        std::fs::remove_file(&staged)
    };
    let prune_error = (removed.is_ok() && context.as_deref() == Some(victim))
        .then(|| crate::context::prune_unpin_decision_guarded(paths, victim))
        .and_then(Result::err);
    if let Some(error) = prune_error {
        warn!(path = %victim.display(), %error, "deferred unpin record pruning");
    }
    removed.with_context(|| format!("removing {}", staged.display()))
}

/// A debug-build fault point for a real-Cargo race fixture. GC holds the
/// stable exclusion guard while the test starts another Cargo invocation.
#[cfg(debug_assertions)]
fn pause_after_gc_lock_for_test() -> Result<()> {
    use std::time::Instant;

    let Some(marker) = std::env::var_os("RGO_TEST_GC_LOCKED_MARKER") else {
        return Ok(());
    };
    let marker = PathBuf::from(marker);
    let release = PathBuf::from(
        std::env::var_os("RGO_TEST_GC_LOCKED_RELEASE")
            .context("RGO_TEST_GC_LOCKED_RELEASE is required with the marker")?,
    );
    let staging = marker.with_extension("tmp");
    std::fs::write(&staging, b"locked")?;
    std::fs::rename(&staging, &marker)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.is_file() {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out at the GC lifecycle lock test point");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Only rgo-owned child paths may enter the rename-before-delete sequence.
/// Refuse traversal through symlinked children of a private storage domain.
fn validate_deletion_path(paths: &RgoPaths, victim: &Path) -> Result<()> {
    let domains = [
        paths.builds_dir(),
        paths.cas_dir(),
        paths.tmp_dir(),
        paths.quarantine_dir(),
    ];
    let (domain, relative) = domains
        .iter()
        .find_map(|domain| {
            victim
                .strip_prefix(domain)
                .ok()
                .map(|relative| (domain, relative))
        })
        .context("deletion path is outside rgo-managed storage")?;
    anyhow::ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "invalid managed deletion path {}",
        victim.display()
    );
    let mut prefix = domain.clone();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            prefix.push(component);
            let metadata = std::fs::symlink_metadata(&prefix)
                .with_context(|| format!("checking deletion parent {}", prefix.display()))?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "refusing symlinked or non-directory deletion parent {}",
                prefix.display()
            );
        }
    }
    let metadata = std::fs::symlink_metadata(victim)
        .with_context(|| format!("checking deletion target {}", victim.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "refusing symlinked deletion target {}",
        victim.display()
    );
    Ok(())
}

fn temporary_actions(paths: &RgoPaths, now: SystemTime, pressure: bool) -> Result<Vec<Action>> {
    let mut actions = Vec::new();
    let mut scanner = Scanner::new();
    let cutoff = Duration::from_secs(3600);
    let tmp = paths.tmp_dir();
    let quarantine = paths.quarantine_dir();
    for directory in [&tmp, &quarantine] {
        match std::fs::symlink_metadata(directory) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "unsafe temporary storage directory {}",
                directory.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", directory.display()));
            }
        }
        let entries = std::fs::read_dir(directory)
            .with_context(|| format!("reading {}", directory.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("checking temporary entry {}", path.display()))?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "unsafe symlinked temporary entry {}",
                path.display()
            );
            let modified = metadata
                .modified()
                .with_context(|| format!("reading modification time for {}", path.display()))?;
            let old = now.duration_since(modified).unwrap_or_default() > cutoff;
            let interrupted_gc = directory == &tmp && is_gc_stage(&path);
            let pressured_quarantine = directory == &quarantine && pressure;
            if !old && !interrupted_gc && !pressured_quarantine {
                continue;
            }
            let bytes = scanner.measure_checked(&path)?.physical_bytes;
            actions.push(Action {
                tier: Tier::Tmp,
                path,
                bytes,
                reason: if interrupted_gc {
                    "interrupted GC staging entry"
                } else if pressured_quarantine {
                    "quarantined cache data under storage pressure"
                } else {
                    "abandoned temporary or quarantine entry"
                }
                .into(),
            });
        }
    }
    // Store writers hold a shared lock before creating a staging file and
    // through its final rename. Skip the whole CAS staging domain while a
    // writer is active, then recheck the lock at deletion time.
    let cas = paths.cas_dir();
    if let Some(_staging_guard) = rgo_cas::try_lock_staging_cleanup(&cas)? {
        for entry in std::fs::read_dir(&cas)? {
            let path = entry?.path();
            if !is_cas_stage(&path) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "unsafe CAS staging entry {}",
                path.display()
            );
            let modified = metadata.modified()?;
            let old = now.duration_since(modified).unwrap_or_default() > cutoff;
            if !old && !pressure {
                continue;
            }
            actions.push(Action {
                tier: Tier::Tmp,
                bytes: scanner.measure_checked(&path)?.physical_bytes,
                path,
                reason: "abandoned CAS publication staging file".into(),
            });
        }
    }
    Ok(actions)
}

fn is_cas_stage(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let rest = name
        .strip_prefix(".object.")
        .or_else(|| name.strip_prefix(".manifest."));
    let Some(rest) = rest.and_then(|rest| rest.strip_suffix(".tmp")) else {
        return false;
    };
    let Some((stamp, pid)) = rest.split_once('.') else {
        return false;
    };
    stamp.parse::<u128>().is_ok() && pid.parse::<u32>().is_ok()
}

fn is_gc_stage(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(rest) = name.strip_prefix("gc-") else {
        return false;
    };
    let mut parts = rest.splitn(3, '-');
    matches!(parts.next(), Some(pid) if pid.parse::<u32>().is_ok())
        && matches!(parts.next(), Some(nonce) if nonce.parse::<u128>().is_ok())
        && matches!(parts.next(), Some(name) if !name.is_empty())
}

fn is_live_managed_path(paths: &RgoPaths, victim: &Path) -> Result<bool> {
    Ok(paths
        .checked_managed_build_dirs()?
        .into_iter()
        .find(|context| victim == context || victim.starts_with(context))
        .is_some_and(|context| crate::context::lock_files_for_safety(&context)))
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

    fn test_cfg() -> Resolved {
        Resolved {
            max_size: 1,
            soft_watermark: 0,
            min_free_space: 0,
            gc: Gc::default(),
            cache: Cache::default(),
            remote: crate::config::Remote::default(),
            volume_total: 1,
        }
    }

    fn attribute_context(workspace: &Path, dir: &Path) {
        let manifest = workspace.join("Cargo.toml");
        std::fs::write(&manifest, "[workspace]\n").unwrap();
        crate::context::write_sidecar(dir, workspace, &manifest, None).unwrap();
    }

    #[test]
    fn tier_zero_is_reported_and_execute_matches_the_plan() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let stale = paths.tmp_dir().join("stale.tmp");
        let quarantined = paths.quarantine_dir().join("corrupt-object");
        std::fs::write(&stale, b"temporary").unwrap();
        std::fs::write(&quarantined, b"quarantine").unwrap();
        let now = SystemTime::now() + Duration::from_secs(7200);
        let cfg = test_cfg();
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &[],
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: false,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: None,
        })
        .unwrap();
        assert_eq!(plan.actions.len(), 2);
        assert!(plan.actions.iter().all(|action| action.tier == Tier::Tmp));
        assert!(plan.reclaim_bytes() >= 19);
        let planned = plan.reclaim_bytes();
        let reclaimed = execute(&paths, &plan, false).unwrap();
        assert_eq!(reclaimed.reclaimed_bytes, planned);
        assert!(!stale.exists());
        assert!(!quarantined.exists());
    }

    #[test]
    fn interrupted_gc_staging_is_immediate_and_fresh_quarantine_yields_to_pressure() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let staged = paths.tmp_dir().join("gc-123-456-context");
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("output"), vec![b'a'; 8192]).unwrap();
        let unknown = paths.tmp_dir().join("active-unknown");
        std::fs::write(&unknown, vec![b'b'; 8192]).unwrap();
        let quarantined = paths.quarantine_dir().join("corrupt-object.bad");
        std::fs::write(&quarantined, vec![b'c'; 8192]).unwrap();
        let auxiliary = crate::size::auxiliary_usage(&paths).unwrap().physical_bytes;
        let mut cfg = test_cfg();
        cfg.max_size = u64::MAX;
        cfg.soft_watermark = u64::MAX;
        let now = SystemTime::now();
        let preview = |target_bytes| {
            plan(&Inputs {
                paths: &paths,
                cfg: &cfg,
                contexts: &[],
                other_managed_bytes: auxiliary,
                pinned: &[],
                leased: &[],
                now,
                aggressive: false,
                age_maintenance: false,
                allow_pressure_contexts: true,
                target_bytes,
            })
            .unwrap()
        };
        let relaxed = preview(None);
        assert_eq!(relaxed.actions.len(), 1);
        assert_eq!(relaxed.actions[0].path, staged);

        let pressured = preview(Some(0));
        assert_eq!(pressured.actions.len(), 2);
        assert!(pressured.actions.iter().any(|action| action.path == staged));
        assert!(
            pressured
                .actions
                .iter()
                .any(|action| action.path == quarantined)
        );
        let active_gc = crate::supervision::try_lock_gc(&paths, None)
            .unwrap()
            .unwrap();
        let blocked = execute(&paths, &pressured, false).unwrap();
        assert_eq!(blocked.skipped_actions, 2);
        assert!(staged.exists());
        assert!(quarantined.exists());
        drop(active_gc);
        execute(&paths, &pressured, false).unwrap();
        assert!(!staged.exists());
        assert!(!quarantined.exists());
        assert!(unknown.exists());
    }

    #[test]
    fn abandoned_cas_staging_waits_for_publication_lock() {
        use fs4::fs_std::FileExt;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let _store = rgo_cas::Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
        let staged = paths.cas_dir().join(".object.123.456.tmp");
        let unknown = paths.cas_dir().join(".object.unknown.tmp");
        std::fs::write(&staged, vec![b'x'; 8192]).unwrap();
        std::fs::write(&unknown, vec![b'y'; 8192]).unwrap();
        let now = SystemTime::now() + Duration::from_secs(7200);
        let publication = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.cas_dir().join(".staging.lock"))
            .unwrap();
        FileExt::lock_shared(&publication).unwrap();
        assert!(temporary_actions(&paths, now, false).unwrap().is_empty());
        drop(publication);

        let actions = temporary_actions(&paths, now, false).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].path, staged);
        let plan = Plan {
            actions,
            ..Default::default()
        };
        let publication = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.cas_dir().join(".staging.lock"))
            .unwrap();
        FileExt::lock_shared(&publication).unwrap();
        let blocked = execute(&paths, &plan, false).unwrap();
        assert_eq!(blocked.skipped_actions, 1);
        assert!(staged.exists());
        drop(publication);
        execute(&paths, &plan, false).unwrap();
        assert!(!staged.exists());
        assert!(unknown.exists());
    }

    #[test]
    fn a_failed_volume_probe_cannot_look_like_plentiful_free_space() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"file").unwrap();
        let paths = RgoPaths {
            root: file.join("rgo"),
        };
        let result = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &[],
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now: SystemTime::now(),
            aggressive: false,
            age_maintenance: false,
            allow_pressure_contexts: false,
            target_bytes: None,
        });
        assert!(result.unwrap_err().to_string().contains("probing volume"));
    }

    #[cfg(unix)]
    #[test]
    fn temporary_planning_refuses_an_unreclaimable_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"preserve").unwrap();
        symlink(&outside, paths.tmp_dir().join("unowned-link")).unwrap();
        let result = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &[],
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now: SystemTime::now() + Duration::from_secs(7200),
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsafe symlinked temporary entry")
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"preserve");

        std::fs::remove_file(paths.tmp_dir().join("unowned-link")).unwrap();
        std::fs::rename(paths.tmp_dir(), paths.root.join("tmp.saved")).unwrap();
        let external_dir = root.path().join("external-dir");
        std::fs::create_dir(&external_dir).unwrap();
        std::fs::write(external_dir.join("outside"), b"preserve").unwrap();
        symlink(&external_dir, paths.tmp_dir()).unwrap();
        let result = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &[],
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now: SystemTime::now() + Duration::from_secs(7200),
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsafe temporary storage directory")
        );
        assert_eq!(
            std::fs::read(external_dir.join("outside")).unwrap(),
            b"preserve"
        );
    }

    #[cfg(unix)]
    #[test]
    fn incremental_planning_rechecks_a_context_after_its_size_snapshot() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        let incremental = context.join("debug/incremental");
        std::fs::create_dir_all(&incremental).unwrap();
        std::fs::write(incremental.join("old"), b"old state").unwrap();
        attribute_context(root.path(), &context);
        let contexts = crate::context::list(&paths).unwrap();
        assert!(contexts[0].incremental_usage.physical_bytes > 0);

        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"preserve").unwrap();
        std::fs::remove_dir_all(&incremental).unwrap();
        symlink(&outside, &incremental).unwrap();
        let mut cfg = test_cfg();
        cfg.gc.incremental_retention = Duration::ZERO;
        cfg.gc.context_retention = Duration::from_secs(7200);
        let planned = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now: SystemTime::now() + Duration::from_secs(3600),
            aggressive: false,
            age_maintenance: true,
            allow_pressure_contexts: false,
            target_bytes: None,
        });
        let error = planned.unwrap_err();
        assert!(
            format!("{error:#}").contains("uninspectable profile symlink"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"preserve");
    }

    #[test]
    fn unknown_build_tree_path_is_never_deleted() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let unknown = paths.builds_dir().join("unknown");
        std::fs::create_dir_all(&unknown).unwrap();
        std::fs::write(unknown.join("output"), b"preserve").unwrap();
        assert!(remove_atomically(&paths, &unknown).is_err());
        assert_eq!(std::fs::read(unknown.join("output")).unwrap(), b"preserve");
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_external_paths_and_symlinked_storage_children() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let file = outside.join("user-file");
        std::fs::write(&file, b"preserve").unwrap();
        assert!(remove_atomically(&paths, &file).is_err());
        std::fs::create_dir_all(paths.cas_dir()).unwrap();
        symlink(&outside, paths.cas_dir().join("linked")).unwrap();
        assert!(remove_atomically(&paths, &paths.cas_dir().join("linked/user-file")).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve");
    }

    #[test]
    fn eligible_temp_bytes_satisfy_pressure_before_context_eviction() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context_dir = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(context_dir.join("output"), vec![0u8; 8192]).unwrap();
        let stale = paths.tmp_dir().join("stale");
        std::fs::write(&stale, vec![0u8; 8192]).unwrap();
        let now = SystemTime::now();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(now - Duration::from_secs(7200))
            .unwrap();
        let context_usage = Scanner::new().measure(&context_dir);
        let auxiliary = crate::size::auxiliary_usage(&paths).unwrap().physical_bytes;
        let contexts = [BuildContext {
            dir: context_dir.clone(),
            sidecar: None,
            last_used: now - Duration::from_secs(7200),
            usage: context_usage,
            incremental_usage: Usage::default(),
        }];
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &contexts,
            other_managed_bytes: auxiliary,
            pinned: &[],
            leased: &[],
            now,
            aggressive: false,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(context_usage.physical_bytes),
        })
        .unwrap();
        assert_eq!(plan.managed_bytes, context_usage.physical_bytes + auxiliary);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].path, stale);
        assert_eq!(plan.actions[0].tier, Tier::Tmp);
    }

    #[test]
    fn a_pin_added_after_planning_blocks_destructive_execution() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("output"), b"keep").unwrap();
        attribute_context(root.path(), &context);
        crate::context::write_pin_marker(&context).unwrap();
        assert!(remove_atomically(&paths, &context).is_err());
        assert!(context.join("output").is_file());
    }

    #[test]
    fn deleting_an_unpinned_context_prunes_its_unpin_decision() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("output"), b"remove").unwrap();
        attribute_context(root.path(), &context);
        crate::context::write_durable_pin(&paths, &context).unwrap();
        crate::context::remove_durable_pin(&paths, &context).unwrap();
        let record = paths.pin_records_dir().join("aa/context.pin");
        assert!(record.is_file());

        remove_atomically(&paths, &context).unwrap();
        assert!(!context.exists());
        assert!(!record.exists());
    }

    #[test]
    fn missing_sidecar_blocks_planning_and_a_previously_planned_delete() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let missing = paths.builds_dir().join("aa/missing");
        let eligible = paths.builds_dir().join("bb/eligible");
        for dir in [&missing, &eligible] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("output"), b"reclaimable").unwrap();
            attribute_context(root.path(), dir);
        }
        let now = SystemTime::now();
        let mut contexts = crate::context::list(&paths).unwrap();
        for context in &mut contexts {
            context.last_used = now - Duration::from_secs(7200);
        }
        let planned = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        })
        .unwrap();
        assert!(planned.actions.iter().any(|action| action.path == missing));
        assert!(planned.actions.iter().any(|action| action.path == eligible));

        std::fs::remove_file(missing.join(rgo_protocol::SIDECAR_FILE)).unwrap();
        let reclaimed = execute(&paths, &planned, false).unwrap();
        assert!(
            reclaimed.reclaimed_bytes > 0,
            "the attributed context should still be reclaimed"
        );
        assert!(missing.join("output").is_file());
        assert!(!eligible.exists());

        let remaining = crate::context::list(&paths).unwrap();
        assert_eq!(remaining.len(), 1);
        let next = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &remaining,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        })
        .unwrap();
        assert!(next.actions.is_empty());
        assert_eq!(next.skipped_unavailable, 1);
        assert!(next.protected_context_bytes > 0);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_device_change_after_planning_blocks_destructive_execution() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let manifest = workspace.join("Cargo.toml");
        std::fs::write(&manifest, "[workspace]\n").unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("output"), b"keep").unwrap();
        crate::context::write_sidecar(&context, &workspace, &manifest, None).unwrap();
        let now = SystemTime::now();
        let mut contexts = crate::context::list(&paths).unwrap();
        contexts[0].last_used = now - Duration::from_secs(7200);
        let mut cfg = test_cfg();
        cfg.gc.context_retention = Duration::from_secs(1);
        let planned = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        })
        .unwrap();
        assert!(planned.actions.iter().any(|action| action.path == context));
        let mut sidecar = crate::context::read_sidecar(&context).unwrap();
        sidecar.workspace_device = Some(u64::MAX);
        std::fs::write(
            context.join(rgo_protocol::SIDECAR_FILE),
            serde_json::to_vec(&sidecar).unwrap(),
        )
        .unwrap();
        assert!(remove_atomically(&paths, &context).is_err());
        assert!(context.join("output").is_file());
    }

    #[test]
    fn supervised_session_blocks_context_removal_through_the_delete() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("output"), b"keep").unwrap();
        attribute_context(root.path(), &context);

        let session = crate::supervision::lock_cargo_session(&paths, Some(&context)).unwrap();
        let planned = Plan {
            actions: vec![Action {
                tier: Tier::Pressure,
                path: context.clone(),
                bytes: 4096,
                reason: "test".into(),
            }],
            ..Default::default()
        };
        let execution = execute(&paths, &planned, false).unwrap();
        assert_eq!(execution.reclaimed_bytes, 0);
        assert_eq!(execution.skipped_actions, 1);
        assert_eq!(execution.skipped_bytes, 4096);
        assert!(
            execution
                .first_skip
                .as_deref()
                .unwrap()
                .contains("supervised Cargo invocation")
        );
        assert!(remove_atomically(&paths, &context).is_err());
        assert!(context.join("output").is_file());
        drop(session);

        remove_atomically(&paths, &context).unwrap();
        assert!(!context.exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_keeps_its_stable_guard_through_rename_and_removal() {
        use fs4::fs_std::FileExt;
        use std::fs::OpenOptions;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("output"), b"unused").unwrap();
        attribute_context(root.path(), &context);
        let mut phases = Vec::new();
        let mut waiting_session = None;
        stage_and_remove_with(&paths, &context, |phase| {
            phases.push(phase);
            // A new supervised Cargo session first takes this stable lock.
            // It cannot enter while GC is checking liveness or removing the
            // already-renamed context.
            let local = OpenOptions::new()
                .read(true)
                .write(true)
                .open(paths.state_dir().join("locks/process-guard.lock"))
                .unwrap();
            assert!(!FileExt::try_lock_shared(&local).unwrap());
            assert_eq!(context.exists(), phase == DeletePhase::Locked);
            if phase == DeletePhase::Staged {
                let paths = paths.clone();
                let context = context.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                waiting_session = Some(std::thread::spawn(move || {
                    let local = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(paths.state_dir().join("locks/process-guard.lock"))
                        .unwrap();
                    tx.send(FileExt::try_lock_shared(&local).unwrap()).unwrap();
                    drop(local);
                    let _session =
                        crate::supervision::lock_cargo_session(&paths, Some(&context)).unwrap();
                    let removed_before_admission = !context.exists();
                    std::fs::create_dir_all(&context).unwrap();
                    std::fs::write(context.join("new-session"), b"new build").unwrap();
                    removed_before_admission
                }));
                assert!(!rx.recv_timeout(Duration::from_secs(2)).unwrap());
            }
        })
        .unwrap();
        assert_eq!(phases, [DeletePhase::Locked, DeletePhase::Staged]);
        assert!(waiting_session.unwrap().join().unwrap());
        assert_eq!(
            std::fs::read(context.join("new-session")).unwrap(),
            b"new build"
        );
    }

    #[cfg(windows)]
    #[test]
    fn gc_keeps_its_windows_guard_through_rename_and_removal() {
        use fs4::fs_std::FileExt;
        use std::fs::OpenOptions;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("output"), b"unused").unwrap();
        attribute_context(root.path(), &context);

        let mut phases = Vec::new();
        let mut waiting_session = None;
        let mut acquired = None;
        stage_and_remove_with(&paths, &context, |phase| {
            phases.push(phase);
            assert_eq!(context.exists(), phase == DeletePhase::Locked);
            // Windows byte-range locks apply to a second handle in this same
            // process too. Probe the stable context lock directly at both
            // phases, then also check a queued session below.
            let lock_path = std::fs::read_dir(paths.state_dir().join("locks"))
                .unwrap()
                .map(|entry| entry.unwrap())
                .find(|entry| entry.file_name().to_string_lossy().starts_with("context-"))
                .unwrap()
                .path();
            let probe = OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)
                .unwrap();
            assert!(!FileExt::try_lock_shared(&probe).unwrap());
            if phase == DeletePhase::Locked {
                let paths = paths.clone();
                let context = context.clone();
                let (started_tx, started_rx) = std::sync::mpsc::channel();
                let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
                waiting_session = Some(std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    let _session =
                        crate::supervision::lock_cargo_session(&paths, Some(&context)).unwrap();
                    let removed_before_admission = !context.exists();
                    std::fs::create_dir_all(&context).unwrap();
                    std::fs::write(context.join("new-session"), b"new build").unwrap();
                    acquired_tx.send(()).unwrap();
                    removed_before_admission
                }));
                started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                acquired = Some(acquired_rx);
            }
            assert!(
                acquired
                    .as_ref()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(100))
                    .is_err(),
                "a Windows Cargo session entered while GC held the context guard"
            );
        })
        .unwrap();
        assert_eq!(phases, [DeletePhase::Locked, DeletePhase::Staged]);
        assert!(waiting_session.unwrap().join().unwrap());
        assert_eq!(
            std::fs::read(context.join("new-session")).unwrap(),
            b"new build"
        );
    }

    #[test]
    fn tiers_are_ordered_and_stale_incremental_state_is_selected_before_contexts() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context_dir = paths.builds_dir().join("aa/context");
        let incremental = context_dir.join("debug/incremental");
        std::fs::create_dir_all(&incremental).unwrap();
        std::fs::write(incremental.join("old"), b"old state").unwrap();
        let manifest = root.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname='x'\nversion='0.1.0'\n").unwrap();
        let now = SystemTime::now();
        let context = BuildContext {
            dir: context_dir,
            sidecar: Some(ContextSidecar {
                version: rgo_protocol::PROTOCOL_VERSION,
                workspace_root: root.path().display().to_string(),
                manifest_path: manifest.display().to_string(),
                workspace_device: crate::context::workspace_device(root.path()),
                workspace_mount_id: crate::context::workspace_mount_id(root.path()),
                toolchain: None,
                first_seen: 0,
                last_seen: 0,
            }),
            last_used: now - Duration::from_secs(3600),
            usage: Usage {
                physical_bytes: 100,
                logical_bytes: 100,
                files: 1,
            },
            incremental_usage: Usage {
                physical_bytes: 9,
                logical_bytes: 9,
                files: 1,
            },
        };
        let mut cfg = test_cfg();
        cfg.gc.incremental_retention = Duration::from_secs(1);
        cfg.gc.context_retention = Duration::from_secs(7200);
        cfg.soft_watermark = 10_000;
        cfg.max_size = 20_000;
        cfg.min_free_space = 0;
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &[context],
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: false,
            age_maintenance: true,
            allow_pressure_contexts: true,
            target_bytes: None,
        })
        .unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|action| action.tier == Tier::StaleIncremental)
        );
        assert!(
            plan.actions
                .windows(2)
                .all(|pair| pair[0].tier <= pair[1].tier)
        );
    }

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
            workspace_device: crate::context::workspace_device(root.path()),
            workspace_mount_id: crate::context::workspace_mount_id(root.path()),
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
            remote: crate::config::Remote::default(),
            volume_total: 1,
        };
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[leased_dir],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: None,
        })
        .unwrap();
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

    #[cfg(unix)]
    #[test]
    fn orphan_contexts_are_selected_before_stale_contexts() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let orphan_dir = paths.builds_dir().join("aa/orphan");
        let stale_dir = paths.builds_dir().join("bb/stale");
        let unavailable_dir = paths.builds_dir().join("cc/unavailable");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::create_dir_all(&unavailable_dir).unwrap();
        let missing = root.path().join("gone/Cargo.toml");
        std::fs::create_dir_all(missing.parent().unwrap()).unwrap();
        let present = root.path().join("present/Cargo.toml");
        std::fs::create_dir_all(present.parent().unwrap()).unwrap();
        std::fs::write(&present, "[package]\nname='present'\nversion='0.1.0'\n").unwrap();
        let unavailable = root.path().join("offline/Cargo.toml");
        std::fs::create_dir_all(unavailable.parent().unwrap()).unwrap();
        std::fs::write(&unavailable, "[workspace]\n").unwrap();
        let now = SystemTime::now();
        let context = |dir: PathBuf, manifest: PathBuf| BuildContext {
            dir,
            sidecar: Some(ContextSidecar {
                version: rgo_protocol::PROTOCOL_VERSION,
                workspace_root: manifest.parent().unwrap().display().to_string(),
                manifest_path: manifest.display().to_string(),
                workspace_device: crate::context::workspace_device(manifest.parent().unwrap()),
                workspace_mount_id: crate::context::workspace_mount_id(manifest.parent().unwrap()),
                toolchain: None,
                first_seen: 0,
                last_seen: 0,
            }),
            last_used: now - Duration::from_secs(7200),
            usage: Usage {
                physical_bytes: 50,
                logical_bytes: 50,
                files: 1,
            },
            incremental_usage: Usage::default(),
        };
        let mut contexts = vec![
            context(orphan_dir.clone(), missing),
            context(stale_dir, present),
            context(unavailable_dir.clone(), unavailable.clone()),
        ];
        contexts[2].sidecar.as_mut().unwrap().workspace_device = Some(u64::MAX);
        std::fs::remove_file(&unavailable).unwrap();
        std::fs::remove_dir(unavailable.parent().unwrap()).unwrap();
        let mut cfg = test_cfg();
        cfg.gc.orphan_grace = Duration::from_secs(1);
        cfg.gc.context_retention = Duration::from_secs(1);
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        })
        .unwrap();
        if cfg!(target_os = "linux")
            && contexts[0]
                .sidecar
                .as_ref()
                .unwrap()
                .workspace_mount_id
                .is_none()
        {
            assert!(plan.actions.iter().all(|action| action.path != orphan_dir));
        } else {
            assert_eq!(plan.actions[0].tier, Tier::Orphan);
        }
        assert!(
            plan.actions
                .iter()
                .any(|action| action.tier == Tier::StaleContext)
        );
        assert!(
            plan.actions
                .iter()
                .all(|action| action.path != unavailable_dir)
        );
    }

    #[test]
    fn pins_and_recent_locks_are_protected_from_pressure_eviction() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let pinned_dir = paths.builds_dir().join("aa/pinned");
        let locked_dir = paths.builds_dir().join("bb/locked");
        let reclaimable_dir = paths.builds_dir().join("cc/reclaimable");
        let newest_dir = paths.builds_dir().join("dd/newest");
        for dir in [&pinned_dir, &locked_dir, &reclaimable_dir, &newest_dir] {
            std::fs::create_dir_all(dir.join("debug")).unwrap();
        }
        let lock_path = locked_dir.join("debug/.cargo-build-lock");
        std::fs::write(&lock_path, b"live").unwrap();
        let now = SystemTime::now();
        let context = |dir: PathBuf, workspace: &str| {
            let workspace = root.path().join(workspace);
            std::fs::create_dir_all(&workspace).unwrap();
            let manifest = workspace.join("Cargo.toml");
            std::fs::write(&manifest, "[package]\nname='x'\nversion='0.1.0'\n").unwrap();
            BuildContext {
                dir,
                sidecar: Some(ContextSidecar {
                    version: rgo_protocol::PROTOCOL_VERSION,
                    workspace_root: workspace.display().to_string(),
                    manifest_path: manifest.display().to_string(),
                    workspace_device: crate::context::workspace_device(&workspace),
                    workspace_mount_id: crate::context::workspace_mount_id(&workspace),
                    toolchain: None,
                    first_seen: 0,
                    last_seen: 0,
                }),
                last_used: now - Duration::from_secs(7200),
                usage: Usage {
                    physical_bytes: 100,
                    logical_bytes: 100,
                    files: 1,
                },
                incremental_usage: Usage::default(),
            }
        };
        let contexts = vec![
            context(pinned_dir.clone(), "pinned"),
            context(locked_dir.clone(), "locked"),
            context(reclaimable_dir.clone(), "shared"),
            context(newest_dir, "shared"),
        ];
        let mut cfg = test_cfg();
        cfg.gc.context_retention = Duration::from_secs(1);
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: std::slice::from_ref(&pinned_dir),
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        })
        .unwrap();
        assert!(
            plan.actions
                .iter()
                .all(|action| action.path != pinned_dir && action.path != locked_dir)
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.path == reclaimable_dir)
        );
    }

    #[test]
    fn pressure_can_evict_each_idle_workspaces_only_context() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let now = SystemTime::now();
        let contexts = (0..3)
            .map(|index| {
                let workspace = root.path().join(format!("workspace-{index}"));
                std::fs::create_dir_all(&workspace).unwrap();
                let manifest = workspace.join("Cargo.toml");
                std::fs::write(&manifest, "[workspace]\n").unwrap();
                let dir = paths.builds_dir().join(format!("{index:02x}/context"));
                std::fs::create_dir_all(&dir).unwrap();
                BuildContext {
                    dir,
                    sidecar: Some(ContextSidecar {
                        version: rgo_protocol::PROTOCOL_VERSION,
                        workspace_root: workspace.display().to_string(),
                        manifest_path: manifest.display().to_string(),
                        workspace_device: crate::context::workspace_device(&workspace),
                        workspace_mount_id: crate::context::workspace_mount_id(&workspace),
                        toolchain: None,
                        first_seen: 0,
                        last_seen: 0,
                    }),
                    last_used: now - Duration::from_secs(3600),
                    usage: Usage {
                        physical_bytes: 2 * 1024 * 1024,
                        logical_bytes: 2 * 1024 * 1024,
                        files: 1,
                    },
                    incremental_usage: Usage::default(),
                }
            })
            .collect::<Vec<_>>();
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &test_cfg(),
            contexts: &contexts,
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(1_000_000),
        })
        .unwrap();
        assert_eq!(plan.managed_bytes, 6 * 1024 * 1024);
        assert_eq!(plan.actions.len(), 3);
        assert!(
            plan.actions
                .iter()
                .all(|action| action.tier == Tier::Pressure)
        );
        assert!(plan.reclaim_bytes() >= plan.managed_bytes - plan.target_bytes);
    }

    #[test]
    fn liveness_is_rechecked_before_staging_a_deletion() {
        use fs4::fs_std::FileExt;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context_dir = paths.builds_dir().join("aa/race");
        std::fs::create_dir_all(&context_dir).unwrap();
        attribute_context(root.path(), &context_dir);
        let now = SystemTime::now();
        let context = BuildContext {
            dir: context_dir.clone(),
            sidecar: crate::context::read_sidecar(&context_dir),
            last_used: now - Duration::from_secs(7200),
            usage: Usage {
                physical_bytes: 10,
                logical_bytes: 10,
                files: 1,
            },
            incremental_usage: Usage::default(),
        };
        let mut cfg = test_cfg();
        cfg.gc.context_retention = Duration::from_secs(1);
        let plan = plan(&Inputs {
            paths: &paths,
            cfg: &cfg,
            contexts: &[context],
            other_managed_bytes: 0,
            pinned: &[],
            leased: &[],
            now,
            aggressive: true,
            age_maintenance: false,
            allow_pressure_contexts: true,
            target_bytes: Some(0),
        })
        .unwrap();
        let lock_path = context_dir.join("debug/.cargo-build-lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, b"live").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();
        assert_eq!(execute(&paths, &plan, false).unwrap().reclaimed_bytes, 0);
        assert!(context_dir.exists());
    }
}
