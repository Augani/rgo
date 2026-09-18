//! A "context" is one Cargo build-dir under `builds/`. rgo treats its contents as opaque
//! except for the top-level files it writes itself (the `.rgo-context.json` sidecar and
//! the `.rgo-pin` marker) and the documented `<profile>/incremental/` sub-tier.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use fs4::fs_std::FileExt;
use rgo_protocol::{ContextSidecar, PROTOCOL_VERSION, SIDECAR_FILE};
use tracing::debug;

use crate::paths::RgoPaths;
use crate::size::{Scanner, Usage};

#[derive(Debug, Clone)]
pub struct BuildContext {
    pub dir: PathBuf,
    pub sidecar: Option<ContextSidecar>,
    pub last_used: SystemTime,
    pub usage: Usage,
    pub incremental_usage: Usage,
}

impl BuildContext {
    /// `xx/yyyy…` — the two components Cargo's `{workspace-path-hash}` produced.
    pub fn id(&self) -> String {
        let name = |p: &Path| {
            p.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_owned()
        };
        match self.dir.parent() {
            Some(shard) => format!("{}/{}", name(shard), name(&self.dir)),
            None => name(&self.dir),
        }
    }

    /// Workspace manifest recorded in the sidecar no longer exists.
    pub fn is_orphan(&self) -> bool {
        self.sidecar
            .as_ref()
            .is_some_and(|s| !Path::new(&s.manifest_path).exists())
    }

    /// Whether the context carries the pin marker. The marker file inside the build
    /// dir is the durable record of a pin; the SQLite `pins` table is a derived index
    /// that `StateDb::reconcile_contexts` rebuilds from these markers so pins survive
    /// database loss.
    pub fn is_pinned(&self) -> bool {
        is_pinned_dir(&self.dir)
    }

    pub fn idle_for(&self, now: SystemTime) -> Duration {
        now.duration_since(self.last_used).unwrap_or_default()
    }

    /// Cargo holds `<build-dir>/<profile>/.cargo-build-lock` for the duration of a build
    /// (the target-dir gets `.cargo-lock`; we check both names for older layouts).
    /// A recently modified lock is treated as "probably live" until Phase 2 leases exist.
    pub fn recently_locked(&self, within: Duration, now: SystemTime) -> bool {
        lock_files(&self.dir).any(|p| {
            std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .map(|m| now.duration_since(m).unwrap_or_default() < within)
                .unwrap_or(false)
        })
    }
}

/// Defense-in-depth liveness check used immediately before destructive operations. Cargo keeps
/// one of these locks for the duration of a build; an unavailable lock file is treated as live.
pub fn lock_files_for_safety(build_dir: &Path) -> bool {
    lock_files(build_dir).any(|path| {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            return true;
        };
        match file.try_lock_exclusive() {
            Ok(true) => false,
            Ok(false) | Err(_) => true,
        }
    })
}

/// Marker file rgo writes at the top level of a managed build dir while the context
/// is pinned. Removing it out-of-band drops the pin on the next reconcile.
pub const PIN_MARKER: &str = ".rgo-pin";

pub fn is_pinned_dir(dir: &Path) -> bool {
    dir.join(PIN_MARKER).is_file()
}

pub fn write_pin_marker(dir: &Path) -> Result<()> {
    let marker = dir.join(PIN_MARKER);
    std::fs::File::create(&marker)
        .with_context(|| format!("writing pin marker {}", marker.display()))?;
    Ok(())
}

pub fn remove_pin_marker(dir: &Path) -> Result<()> {
    let marker = dir.join(PIN_MARKER);
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", marker.display())),
    }
}

/// Resolve the Git common directory a workspace shares with its linked
/// worktrees, if it is a Git checkout. Reads `.git` and `commondir` files
/// directly so no `git` binary is required. Returns `None` for non-Git
/// workspaces and unreadable or deleted checkouts.
pub fn git_common_dir(workspace_root: &Path) -> Option<PathBuf> {
    let dotgit = workspace_root.join(".git");
    let metadata = std::fs::metadata(&dotgit).ok()?;
    let gitdir = if metadata.is_dir() {
        dotgit
    } else if metadata.is_file() {
        // Linked worktrees and submodules keep a `.git` file containing
        // `gitdir: <path>` instead of a directory.
        let text = std::fs::read_to_string(&dotgit).ok()?;
        let target = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            workspace_root.join(target)
        }
    } else {
        return None;
    };
    // A linked worktree's gitdir carries `commondir` pointing at the shared
    // administrative directory (usually `../..`, i.e. `<repo>/.git`).
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(text) => {
            let target = PathBuf::from(text.trim());
            if target.is_absolute() {
                target
            } else {
                gitdir.join(target)
            }
        }
        Err(_) => gitdir,
    };
    Some(std::fs::canonicalize(&common).unwrap_or(common))
}

pub fn list(paths: &RgoPaths) -> Result<Vec<BuildContext>> {
    let mut out = Vec::new();
    let mut scanner = Scanner::new();
    for dir in paths.managed_build_dirs() {
        let sidecar = read_sidecar(&dir);
        let usage = scanner.measure(&dir);
        let incremental_usage =
            incremental_dirs(&dir)
                .map(|d| scanner.measure(&d))
                .fold(Usage::default(), |a, b| Usage {
                    physical_bytes: a.physical_bytes + b.physical_bytes,
                    logical_bytes: a.logical_bytes + b.logical_bytes,
                    files: a.files + b.files,
                });
        let last_used = last_used(&dir, sidecar.as_ref());
        out.push(BuildContext {
            dir,
            sidecar,
            last_used,
            usage,
            incremental_usage,
        });
    }
    out.sort_by_key(|c| c.last_used);
    Ok(out)
}

pub fn read_sidecar(dir: &Path) -> Option<ContextSidecar> {
    let text = std::fs::read_to_string(dir.join(SIDECAR_FILE)).ok()?;
    match serde_json::from_str::<ContextSidecar>(&text) {
        Ok(s) if s.version <= PROTOCOL_VERSION => Some(s),
        Ok(s) => {
            debug!(dir = %dir.display(), version = s.version, "sidecar from newer rgo; ignoring");
            None
        }
        Err(e) => {
            debug!(dir = %dir.display(), %e, "unreadable sidecar");
            None
        }
    }
}

/// Write (or refresh) the sidecar. Atomic via temp file + rename.
pub fn write_sidecar(
    dir: &Path,
    workspace_root: &Path,
    manifest_path: &Path,
    toolchain: Option<String>,
) -> Result<()> {
    let now = unix_now();
    let first_seen = read_sidecar(dir).map(|s| s.first_seen).unwrap_or(now);
    let sc = ContextSidecar {
        version: PROTOCOL_VERSION,
        workspace_root: workspace_root.display().to_string(),
        manifest_path: manifest_path.display().to_string(),
        toolchain,
        first_seen,
        last_seen: now,
    };
    let tmp = dir.join(format!("{SIDECAR_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&sc)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(SIDECAR_FILE))?;
    Ok(())
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `<build-dir>/<profile>/incremental` and `<build-dir>/<triple>/<profile>/incremental`.
pub fn incremental_dirs(build_dir: &Path) -> impl Iterator<Item = PathBuf> {
    profile_dirs(build_dir)
        .map(|p| p.join("incremental"))
        .filter(|p| p.is_dir())
}

fn lock_files(build_dir: &Path) -> impl Iterator<Item = PathBuf> {
    profile_dirs(build_dir)
        .flat_map(|p| [p.join(".cargo-build-lock"), p.join(".cargo-lock")])
        .filter(|p| p.is_file())
}

/// Depth-1 and depth-2 directories: covers host builds (`debug/`) and `--target` builds
/// (`<triple>/debug/`). Documented by Cargo's build-cache reference.
fn profile_dirs(build_dir: &Path) -> impl Iterator<Item = PathBuf> {
    let level1: Vec<PathBuf> = subdirs(build_dir).collect();
    let level2: Vec<PathBuf> = level1.iter().flat_map(|d| subdirs(d)).collect();
    level1.into_iter().chain(level2)
}

fn subdirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
}

fn last_used(dir: &Path, sidecar: Option<&ContextSidecar>) -> SystemTime {
    let mut best = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    for lock in lock_files(dir) {
        if let Ok(m) = std::fs::metadata(&lock).and_then(|m| m.modified()) {
            best = best.max(m);
        }
    }
    if let Some(s) = sidecar {
        best = best.max(UNIX_EPOCH + Duration::from_secs(s.last_seen));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Usage;

    fn context_at(last_used: SystemTime) -> BuildContext {
        BuildContext {
            dir: PathBuf::from("/nonexistent"),
            sidecar: None,
            last_used,
            usage: Usage::default(),
            incremental_usage: Usage::default(),
        }
    }

    #[test]
    fn clock_jumps_backward_saturate_to_fresh_not_negative() {
        // A clock that moved backwards must not make a context look older than it
        // is: idle_for saturates at zero (treated as just-used, never collected).
        let now = SystemTime::now();
        let future = context_at(now + Duration::from_secs(86_400));
        assert_eq!(future.idle_for(now), Duration::ZERO);
        let present = context_at(now - Duration::from_secs(60));
        assert_eq!(present.idle_for(now), Duration::from_secs(60));
    }

    #[test]
    fn pin_marker_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ctx");
        std::fs::create_dir(&dir).unwrap();
        assert!(!is_pinned_dir(&dir));
        write_pin_marker(&dir).unwrap();
        assert!(is_pinned_dir(&dir));
        remove_pin_marker(&dir).unwrap();
        assert!(!is_pinned_dir(&dir));
        // Removing a missing marker is idempotent.
        remove_pin_marker(&dir).unwrap();
        // Writing a marker for a missing directory fails loudly.
        assert!(write_pin_marker(&root.path().join("gone")).is_err());
    }

    #[test]
    fn git_common_dir_resolves_checkouts_worktrees_and_non_git() {
        let root = tempfile::tempdir().unwrap();
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap();

        // A plain checkout resolves to its own `.git` directory.
        let main_ws = root.path().join("repo");
        std::fs::create_dir_all(main_ws.join(".git")).unwrap();
        assert_eq!(
            git_common_dir(&main_ws).as_deref(),
            Some(canonical(&main_ws.join(".git")).as_path())
        );

        // A linked worktree has a `.git` file pointing at its per-worktree
        // gitdir, which carries `commondir` back to the shared admin dir.
        let worktree = root.path().join("repo-wt2");
        let wt_gitdir = main_ws.join(".git/worktrees/repo-wt2");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
        assert_eq!(
            git_common_dir(&worktree).as_deref(),
            Some(canonical(&main_ws.join(".git")).as_path()),
            "linked worktree must resolve to the same common dir as the main checkout"
        );

        // A submodule-style `.git` file with no `commondir` resolves to the
        // gitdir itself.
        let sub = root.path().join("sub");
        let sub_gitdir = main_ws.join(".git/modules/sub");
        std::fs::create_dir_all(&sub_gitdir).unwrap();
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(
            sub.join(".git"),
            format!("gitdir: {}\n", sub_gitdir.display()),
        )
        .unwrap();
        assert_eq!(
            git_common_dir(&sub).as_deref(),
            Some(canonical(&sub_gitdir).as_path())
        );

        // Non-Git workspaces and missing checkouts are ungrouped.
        let plain = root.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(git_common_dir(&plain), None);
        assert_eq!(git_common_dir(&root.path().join("deleted")), None);
    }
}
