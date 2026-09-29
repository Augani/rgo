//! A "context" is one Cargo build-dir under `builds/`. rgo treats its contents as opaque
//! except for the top-level files it writes itself (the `.rgo-context.json` sidecar and
//! the `.rgo-pin` marker) and the documented `<profile>/incremental/` sub-tier.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceState {
    Present,
    MissingManifest,
    Unavailable,
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

    /// A missing manifest is confirmed only while its workspace volume is
    /// available. Permission failures and missing mounts are not orphan proof.
    pub fn is_orphan(&self) -> bool {
        self.workspace_state() == Some(WorkspaceState::MissingManifest)
    }

    pub fn workspace_unavailable(&self) -> bool {
        // Without rgo's sidecar, this directory has no verified workspace
        // owner. Keep it out of unattended cleanup rather than treating the
        // missing attribution as an eligible idle context.
        self.workspace_state()
            .is_none_or(|state| state == WorkspaceState::Unavailable)
    }

    pub fn workspace_state(&self) -> Option<WorkspaceState> {
        self.sidecar.as_ref().map(workspace_state)
    }

    /// Pin intent is recorded outside Cargo's build tree. The in-context
    /// marker is recognized for installations created before that record.
    pub fn is_pinned(&self, paths: &RgoPaths) -> bool {
        is_pinned(paths, &self.dir)
    }

    pub fn idle_for(&self, now: SystemTime) -> Duration {
        now.duration_since(self.last_used).unwrap_or_default()
    }

    /// Cargo holds `<build-dir>/<profile>/.cargo-build-lock` for the duration of a build.
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

pub fn workspace_state(sidecar: &ContextSidecar) -> WorkspaceState {
    let root = Path::new(&sidecar.workspace_root);
    let manifest = Path::new(&sidecar.manifest_path);
    if manifest.parent() != Some(root) {
        return WorkspaceState::Unavailable;
    }
    let root_metadata = match std::fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !matches!(
                std::fs::symlink_metadata(root),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ) {
                return WorkspaceState::Unavailable;
            }
            return root
                .parent()
                .and_then(|parent| {
                    let metadata = std::fs::metadata(parent).ok()?;
                    workspace_identity_matches(parent, &metadata, sidecar)
                })
                .filter(|matches| *matches)
                .map_or(WorkspaceState::Unavailable, |_| {
                    WorkspaceState::MissingManifest
                });
        }
        Err(_) => return WorkspaceState::Unavailable,
    };
    let identity = workspace_identity_matches(root, &root_metadata, sidecar);
    if identity == Some(false) || (sidecar.workspace_mount_id.is_some() && identity.is_none()) {
        return WorkspaceState::Unavailable;
    }
    match std::fs::symlink_metadata(manifest) {
        Ok(_) => match std::fs::metadata(manifest) {
            Ok(metadata) if metadata.is_file() => WorkspaceState::Present,
            _ => WorkspaceState::Unavailable,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if identity == Some(true) {
                WorkspaceState::MissingManifest
            } else {
                WorkspaceState::Unavailable
            }
        }
        Err(_) => WorkspaceState::Unavailable,
    }
}

/// Defense-in-depth liveness heuristic used immediately before destructive operations.
/// A held documented profile lock blocks deletion, but its absence does not prove the
/// whole Cargo session is idle.
pub fn lock_files_for_safety(build_dir: &Path) -> bool {
    let Ok(profiles) = checked_profile_dirs(build_dir) else {
        return true;
    };
    profiles.into_iter().any(|profile| {
        let path = profile.join(".cargo-build-lock");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A disappeared profile is a race, not evidence that its
                // lock was absent throughout this deletion check.
                !std::fs::symlink_metadata(&profile)
                    .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            }
            Err(_) => true,
            Ok(metadata) if !metadata.file_type().is_file() => true,
            Ok(_) => {
                let Ok(file) = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                else {
                    return true;
                };
                !matches!(file.try_lock_exclusive(), Ok(true))
            }
        }
    })
}

/// The fast inventory path may skip unreadable entries for display. A deletion
/// check must instead fail closed on any directory or lock it cannot inspect.
fn checked_profile_dirs(build_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let metadata = std::fs::symlink_metadata(build_dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "build context is not a real directory",
        ));
    }
    let first = checked_subdirs(build_dir)?;
    let mut profiles = first.clone();
    for dir in first {
        profiles.extend(checked_subdirs(&dir)?);
    }
    Ok(profiles)
}

fn checked_subdirs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(std::io::Error::other("uninspectable profile symlink"));
        }
        if kind.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// Compatibility marker at the top of a managed build dir. New pin intent is
/// also stored under `state/pins`, which Cargo cannot remove with `clean`.
pub const PIN_MARKER: &str = ".rgo-pin";

pub fn is_pinned_dir(dir: &Path) -> bool {
    match std::fs::symlink_metadata(dir.join(PIN_MARKER)) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// A pin intent lives outside Cargo's build directory so an explicit or
/// interrupted `cargo clean` cannot silently turn it into an unpinned context.
/// The in-context marker is still accepted for older installations.
pub fn is_pinned(paths: &RgoPaths, dir: &Path) -> bool {
    use std::io::Read;

    let Ok(record) = pin_record_path(paths, dir) else {
        return true;
    };
    match std::fs::symlink_metadata(&record) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => true,
        Ok(metadata) if metadata.len() != b"unpin\n".len() as u64 => true,
        Ok(_) => {
            let mut contents = Vec::with_capacity(7);
            !(std::fs::File::open(record)
                .and_then(|file| file.take(7).read_to_end(&mut contents))
                .is_ok()
                && contents == b"unpin\n") // Malformed/torn writes protect the context.
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => is_pinned_dir(dir),
        Err(_) => true,
    }
}

fn pin_record_path(paths: &RgoPaths, dir: &Path) -> Result<PathBuf> {
    let relative = dir.strip_prefix(paths.builds_dir())?;
    ensure!(
        paths.is_managed_build_dir(dir),
        "not a managed build context"
    );
    let mut components = relative.components();
    let shard = components.next().context("missing context shard")?;
    let name = components.next().context("missing context name")?;
    ensure!(
        components.next().is_none()
            && matches!(shard, std::path::Component::Normal(_))
            && matches!(name, std::path::Component::Normal(_)),
        "invalid managed context path"
    );
    let mut file = name.as_os_str().to_os_string();
    file.push(".pin");
    Ok(paths.pin_records_dir().join(shard.as_os_str()).join(file))
}

pub fn write_durable_pin(paths: &RgoPaths, dir: &Path) -> Result<()> {
    let _decision_lock = lock_pin_decisions(paths)?;
    let record = pin_record_path(paths, dir)?;
    let parent = ensure_pin_parent(paths, &record)?;
    let metadata = std::fs::symlink_metadata(&record);
    match metadata {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "unsafe pin record {}",
            record.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", record.display())),
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&record)
        .with_context(|| format!("writing {}", record.display()))?;
    file.write_all(b"pin\n")?;
    file.sync_all()?;
    sync_pin_parent(parent)?;
    Ok(())
}

/// Import an old marker only if no newer explicit pin/unpin decision exists.
pub fn migrate_legacy_pin(paths: &RgoPaths, dir: &Path) -> Result<()> {
    let _decision_lock = lock_pin_decisions(paths)?;
    let record = pin_record_path(paths, dir)?;
    let parent = ensure_pin_parent(paths, &record)?;
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&record)
    {
        Ok(mut file) => {
            file.write_all(b"pin\n")?;
            file.sync_all()?;
            sync_pin_parent(parent)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&record)?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "unsafe pin record {}",
                record.display()
            );
        }
        Err(error) => return Err(error).with_context(|| format!("writing {}", record.display())),
    }
    Ok(())
}

fn ensure_pin_parent<'a>(paths: &RgoPaths, record: &'a Path) -> Result<&'a Path> {
    paths.ensure_layout()?;
    let parent = record.parent().context("pin record has no parent")?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unsafe pin record directory {}",
            parent.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(parent)?;
            sync_pin_parent(&paths.pin_records_dir())?;
        }
        Err(error) => return Err(error).with_context(|| format!("checking {}", parent.display())),
    }
    Ok(parent)
}

fn sync_pin_parent(parent: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

pub fn remove_durable_pin(paths: &RgoPaths, dir: &Path) -> Result<()> {
    let _decision_lock = lock_pin_decisions(paths)?;
    let record = pin_record_path(paths, dir)?;
    let parent = ensure_pin_parent(paths, &record)?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&record)
        .with_context(|| format!("writing {}", record.display()))?;
    file.write_all(b"unpin\n")?;
    file.sync_all()?;
    sync_pin_parent(parent)?;
    remove_pin_marker(dir)?;
    Ok(())
}

/// Remove an obsolete unpin decision only when no supervised Cargo session
/// can be between reading that decision and restoring a legacy marker.
pub fn try_prune_unpin_decision(paths: &RgoPaths, dir: &Path) -> Result<bool> {
    let Some(_lifecycle) = crate::supervision::try_lock_gc(paths, Some(dir))? else {
        return Ok(false);
    };
    prune_unpin_decision_guarded(paths, dir)
}

/// Walk durable decisions incrementally so a daemon restart can recover
/// tombstones left by a crash or by a session that was active at unpin time.
/// The open directory iterators carry the cursor between maintenance passes;
/// no pass revisits the prefix of a large shard just to reach its next entry.
#[derive(Default)]
pub(crate) struct PinPruneScanner {
    shards: Option<std::fs::ReadDir>,
    records: Option<std::fs::ReadDir>,
    shard_name: Option<std::ffi::OsString>,
}

impl PinPruneScanner {
    /// Inspect at most `limit` directory entries, counting both shards and
    /// records. A completed sweep restarts on the next call so records that
    /// appeared during a sweep are eventually examined too.
    pub(crate) fn scan(&mut self, paths: &RgoPaths, limit: usize) -> Result<usize> {
        let mut examined = 0;
        let mut pruned = 0;
        while examined < limit {
            if let Some(records) = &mut self.records {
                match records.next() {
                    Some(Ok(entry)) => {
                        examined += 1;
                        if !entry.file_type()?.is_file() {
                            continue;
                        }
                        let record = entry.path();
                        if record.extension() != Some(std::ffi::OsStr::new("pin")) {
                            continue;
                        }
                        let Some(name) = record.file_stem() else {
                            continue;
                        };
                        let Some(shard) = &self.shard_name else {
                            continue;
                        };
                        let context = paths.builds_dir().join(shard).join(name);
                        // This cheap read excludes pins and malformed records.
                        // The guarded prune rechecks the decision under its lock.
                        if !is_pinned(paths, &context) && try_prune_unpin_decision(paths, &context)?
                        {
                            pruned += 1;
                        }
                    }
                    Some(Err(error)) => {
                        examined += 1;
                        tracing::warn!(%error, "reading durable pin record");
                    }
                    None => {
                        self.records = None;
                        self.shard_name = None;
                    }
                }
                continue;
            }
            if self.shards.is_none() {
                self.shards = match std::fs::read_dir(paths.pin_records_dir()) {
                    Ok(entries) => Some(entries),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(pruned);
                    }
                    Err(error) => return Err(error).context("reading durable pin records"),
                };
            }
            match self.shards.as_mut().unwrap().next() {
                Some(Ok(entry)) => {
                    examined += 1;
                    if entry.file_type()?.is_dir() {
                        self.records = Some(std::fs::read_dir(entry.path())?);
                        self.shard_name = Some(entry.file_name());
                    }
                }
                Some(Err(error)) => {
                    examined += 1;
                    tracing::warn!(%error, "reading durable pin shard");
                }
                None => {
                    self.shards = None;
                    break;
                }
            }
        }
        Ok(pruned)
    }
}

/// The caller must hold the context's exclusive lifecycle guard through this
/// call. GC uses this before releasing its guard after removing a context.
pub(crate) fn prune_unpin_decision_guarded(paths: &RgoPaths, dir: &Path) -> Result<bool> {
    let _decision_lock = lock_pin_decisions(paths)?;
    let record = pin_record_path(paths, dir)?;
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("checking {}", dir.display())),
    }
    match std::fs::symlink_metadata(&record) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() == b"unpin\n".len() as u64 => {}
        Ok(_) => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("checking {}", record.display())),
    }
    use std::io::Read;
    let mut contents = Vec::with_capacity(7);
    std::fs::File::open(&record)?
        .take(7)
        .read_to_end(&mut contents)?;
    if contents != b"unpin\n" {
        return Ok(false);
    }
    std::fs::remove_file(&record)?;
    sync_pin_parent(record.parent().context("pin record has no parent")?)?;
    Ok(true)
}

fn lock_pin_decisions(paths: &RgoPaths) -> Result<std::fs::File> {
    paths.ensure_layout()?;
    let path = paths.state_dir().join("locks/pin-decisions.lock");
    let file = crate::supervision::open_lock_file(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()?;
    crate::supervision::verify_lock_identity(&path, &file)?;
    Ok(file)
}

/// Enumerate durable pin intents, including contexts removed by Cargo clean.
pub fn durable_pin_contexts(paths: &RgoPaths) -> Result<Vec<PathBuf>> {
    let shards = match std::fs::read_dir(paths.pin_records_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading durable pin records"),
    };
    let mut contexts = Vec::new();
    for shard in shards {
        let shard = shard?;
        let kind = shard.file_type()?;
        ensure!(
            kind.is_dir(),
            "unsafe durable pin shard {}",
            shard.path().display()
        );
        for record in std::fs::read_dir(shard.path())? {
            let record = record?;
            let kind = record.file_type()?;
            ensure!(
                kind.is_file(),
                "unsafe durable pin record {}",
                record.path().display()
            );
            let path = record.path();
            ensure!(
                path.extension() == Some(std::ffi::OsStr::new("pin")),
                "unknown durable pin record {}",
                path.display()
            );
            let name = path.file_stem().context("pin record has no context name")?;
            let context = paths.builds_dir().join(shard.file_name()).join(name);
            if is_pinned(paths, &context) {
                contexts.push(context);
            }
        }
    }
    contexts.sort();
    Ok(contexts)
}

pub fn workspace_device(root: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(root).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.dev())
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        crate::size::windows_volume_serial(root)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

pub fn workspace_mount_id(root: &Path) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{AtFlags, CWD, StatxFlags, statx};
        let stat = statx(CWD, root, AtFlags::empty(), StatxFlags::MNT_ID).ok()?;
        (stat.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        None
    }
}

fn workspace_identity_matches(
    path: &Path,
    metadata: &std::fs::Metadata,
    sidecar: &ContextSidecar,
) -> Option<bool> {
    let device = workspace_device_matches(path, metadata, sidecar.workspace_device)?;
    if !device {
        return Some(false);
    }
    #[cfg(target_os = "linux")]
    {
        Some(workspace_mount_id(path)? == sidecar.workspace_mount_id?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, sidecar);
        Some(true)
    }
}

fn workspace_device_matches(
    path: &Path,
    metadata: &std::fs::Metadata,
    recorded: Option<u64>,
) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        recorded.map(|device| device == metadata.dev())
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        Some(recorded? == crate::size::windows_volume_serial(path)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, metadata, recorded);
        None
    }
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
    let mut incremental_scanner = Scanner::new();
    for dir in paths.checked_managed_build_dirs()? {
        let sidecar = read_sidecar(&dir);
        let usage = scanner.measure_checked(&dir)?;
        let mut incremental_usage = Usage::default();
        for directory in incremental_dirs_checked(&dir)? {
            let measured = incremental_scanner.measure_checked(&directory)?;
            incremental_usage.physical_bytes = incremental_usage
                .physical_bytes
                .saturating_add(measured.physical_bytes);
            incremental_usage.logical_bytes = incremental_usage
                .logical_bytes
                .saturating_add(measured.logical_bytes);
            incremental_usage.files = incremental_usage.files.saturating_add(measured.files);
        }
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
        workspace_device: workspace_device(workspace_root),
        workspace_mount_id: workspace_mount_id(workspace_root),
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

/// Prepare the two rgo-owned path components for a supervised build without
/// following a pre-existing shard or context symlink. The storage root is
/// already private and initialized before the launcher takes its session lock.
pub fn ensure_managed_context_dir(paths: &RgoPaths, dir: &Path) -> Result<()> {
    let root = paths.builds_dir();
    let relative = dir.strip_prefix(&root)?;
    let mut components = relative.components();
    let shard = components.next().context("missing context shard")?;
    let name = components.next().context("missing context name")?;
    ensure!(
        components.next().is_none()
            && matches!(shard, std::path::Component::Normal(_))
            && matches!(name, std::path::Component::Normal(_)),
        "invalid managed context path {}",
        dir.display()
    );
    let root_metadata = std::fs::symlink_metadata(&root)?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "unsafe managed build root {}",
        root.display()
    );
    for path in [root.join(shard.as_os_str()), dir.to_path_buf()] {
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unsafe managed build path {}",
            path.display()
        );
    }
    Ok(())
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `<build-dir>/<profile>/incremental` and `<build-dir>/<triple>/<profile>/incremental`.
/// A deletion plan must not mistake an unreadable or symlinked profile for
/// absent incremental state.
pub(crate) fn incremental_dirs_checked(build_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for profile in checked_profile_dirs(build_dir)
        .with_context(|| format!("enumerating profiles under {}", build_dir.display()))?
    {
        let incremental = profile.join("incremental");
        match std::fs::symlink_metadata(&incremental) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "unsafe incremental directory {}",
                    incremental.display()
                );
                dirs.push(incremental);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", incremental.display()));
            }
        }
    }
    Ok(dirs)
}

fn lock_files(build_dir: &Path) -> impl Iterator<Item = PathBuf> {
    profile_dirs(build_dir)
        .map(|p| p.join(".cargo-build-lock"))
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

    #[cfg(unix)]
    #[test]
    fn symlinked_pin_decision_lock_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let external = root.path().join("external");
        std::fs::write(&external, "untouched").unwrap();
        symlink(
            &external,
            paths.state_dir().join("locks/pin-decisions.lock"),
        )
        .unwrap();
        assert!(lock_pin_decisions(&paths).is_err());
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "untouched");
    }

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
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("missing-target"), dir.join(PIN_MARKER)).unwrap();
            assert!(
                is_pinned_dir(&dir),
                "a broken pin marker must protect the context"
            );
            std::fs::remove_file(dir.join(PIN_MARKER)).unwrap();
        }
        // Writing a marker for a missing directory fails loudly.
        assert!(write_pin_marker(&root.path().join("gone")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn context_budget_scan_rejects_symlinked_build_shard() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let external = root.path().join("external");
        std::fs::create_dir_all(external.join("context")).unwrap();
        std::os::unix::fs::symlink(&external, paths.builds_dir().join("aa")).unwrap();
        assert!(list(&paths).is_err(), "a shard symlink hid build bytes");
    }

    #[test]
    fn incremental_subtotal_counts_hardlinks_within_incremental_state() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let context = paths.builds_dir().join("aa/context");
        let incremental = context.join("debug/incremental");
        std::fs::create_dir_all(&incremental).unwrap();
        let file = incremental.join("cache");
        std::fs::write(&file, vec![0u8; 8192]).unwrap();
        std::fs::hard_link(&file, context.join("uplifted")).unwrap();
        let contexts = list(&paths).unwrap();
        assert_eq!(contexts.len(), 1);
        assert_eq!(
            contexts[0].usage.physical_bytes,
            contexts[0].incremental_usage.physical_bytes
        );
        assert!(contexts[0].incremental_usage.physical_bytes > 0);
    }

    #[test]
    fn absent_context_unpin_record_waits_for_the_supervised_session_to_end() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let dir = paths.builds_dir().join("aa/removed");
        write_durable_pin(&paths, &dir).unwrap();
        remove_durable_pin(&paths, &dir).unwrap();
        let record = pin_record_path(&paths, &dir).unwrap();
        assert!(record.is_file());

        let session = crate::supervision::lock_cargo_session(&paths, Some(&dir)).unwrap();
        assert!(!try_prune_unpin_decision(&paths, &dir).unwrap());
        assert!(record.is_file());
        drop(session);

        assert!(try_prune_unpin_decision(&paths, &dir).unwrap());
        assert!(!record.exists());
        assert!(!is_pinned(&paths, &dir));
    }

    #[test]
    fn incremental_pin_scan_recovers_absent_unpin_decisions() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let absent = (0..9)
            .map(|index| paths.builds_dir().join(format!("aa/gone-{index}")))
            .collect::<Vec<_>>();
        for dir in &absent {
            remove_durable_pin(&paths, dir).unwrap();
        }
        let present = paths.builds_dir().join("bb/present");
        std::fs::create_dir_all(&present).unwrap();
        remove_durable_pin(&paths, &present).unwrap();
        let pinned = paths.builds_dir().join("cc/pinned");
        write_durable_pin(&paths, &pinned).unwrap();

        let mut scan = PinPruneScanner::default();
        // Two entries per pass is smaller than the number of decisions. The
        // iterator must progress rather than repeatedly checking its prefix.
        for _ in 0..20 {
            scan.scan(&paths, 2).unwrap();
        }
        for dir in &absent {
            assert!(!pin_record_path(&paths, dir).unwrap().exists());
        }
        assert!(pin_record_path(&paths, &present).unwrap().exists());
        assert!(pin_record_path(&paths, &pinned).unwrap().exists());
        assert!(is_pinned(&paths, &pinned));

        std::fs::remove_dir(&present).unwrap();
        for _ in 0..20 {
            scan.scan(&paths, 2).unwrap();
        }
        assert!(!pin_record_path(&paths, &present).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_profile_lock_state_blocks_deletion() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let context = root.path().join("context");
        let profile = context.join("debug");
        std::fs::create_dir_all(&profile).unwrap();
        assert!(!lock_files_for_safety(&context));
        symlink(
            profile.join("missing-lock"),
            profile.join(".cargo-build-lock"),
        )
        .unwrap();
        assert!(lock_files_for_safety(&context));
        std::fs::remove_file(profile.join(".cargo-build-lock")).unwrap();
        symlink(profile.join("missing-directory"), profile.join("deps")).unwrap();
        assert!(lock_files_for_safety(&context));
    }

    #[test]
    fn missing_manifest_requires_an_available_workspace_volume() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let manifest = workspace.join("Cargo.toml");
        std::fs::write(&manifest, "[workspace]\n").unwrap();
        let sidecar = ContextSidecar {
            version: PROTOCOL_VERSION,
            workspace_root: workspace.display().to_string(),
            manifest_path: manifest.display().to_string(),
            workspace_device: workspace_device(&workspace),
            workspace_mount_id: workspace_mount_id(&workspace),
            toolchain: None,
            first_seen: 0,
            last_seen: 0,
        };
        assert_eq!(workspace_state(&sidecar), WorkspaceState::Present);
        std::fs::remove_file(&manifest).unwrap();
        let expected_deleted = if sidecar.workspace_device.is_some()
            && (!cfg!(target_os = "linux") || sidecar.workspace_mount_id.is_some())
        {
            WorkspaceState::MissingManifest
        } else {
            WorkspaceState::Unavailable
        };
        assert_eq!(workspace_state(&sidecar), expected_deleted);
        std::fs::remove_dir(&workspace).unwrap();
        assert_eq!(workspace_state(&sidecar), expected_deleted);
        std::fs::create_dir(&workspace).unwrap();
        let legacy = ContextSidecar {
            workspace_device: None,
            workspace_mount_id: None,
            ..sidecar.clone()
        };
        assert_eq!(workspace_state(&legacy), WorkspaceState::Unavailable);
        #[cfg(target_os = "linux")]
        {
            let old_linux = ContextSidecar {
                workspace_mount_id: None,
                ..sidecar.clone()
            };
            assert_eq!(workspace_state(&old_linux), WorkspaceState::Unavailable);
        }
        let changed_volume = ContextSidecar {
            workspace_device: Some(u64::MAX),
            ..sidecar.clone()
        };
        assert_eq!(
            workspace_state(&changed_volume),
            WorkspaceState::Unavailable
        );
        #[cfg(target_os = "linux")]
        {
            let changed_mount = ContextSidecar {
                workspace_mount_id: Some(u64::MAX),
                ..sidecar
            };
            assert_eq!(workspace_state(&changed_mount), WorkspaceState::Unavailable);
        }
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
