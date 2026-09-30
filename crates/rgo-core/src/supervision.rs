//! Lifecycle locks for build directories used by a supervised Cargo launcher.
//!
//! Lock files live outside evictable contexts, so renaming a context cannot
//! strand a waiting launcher on a replaced lock inode. These locks protect only
//! launchers that take them; they do not make native-only Cargo GC safe.
//! Unix sessions hold a process-associated fcntl lock and an inherited flock.
//! The latter conservatively protects compiler descendants if Cargo is killed.
//! GC also checks the same flock filenames used by earlier pilots.

use std::fs::File;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
#[cfg(unix)]
use rustix::fs::{CWD, FlockOperation, Mode, OFlags, fcntl_lock, openat};
#[cfg(unix)]
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::paths::RgoPaths;

/// rgo owns this directory identity; it does not interpret Cargo's
/// `{workspace-path-hash}` layout. The root comes from `cargo locate-project`.
pub fn context_for_workspace(paths: &RgoPaths, workspace_root: &Path) -> Result<PathBuf> {
    if !workspace_root.is_absolute() {
        bail!(
            "Cargo workspace root is not absolute: {}",
            workspace_root.display()
        );
    }
    let digest = hash_path(workspace_root);
    let hex = digest.to_hex().to_string();
    Ok(paths.builds_dir().join(&hex[..2]).join(&hex[2..]))
}

/// A supervised launch leaves this small record outside the evictable build
/// tree. The daemon retries maintenance after the context becomes idle, even
/// if a bounded byte sweep has not reached its new files yet.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingMaintenance {
    pub context: PathBuf,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

fn pending_path(paths: &RgoPaths, context: &Path) -> Result<PathBuf> {
    let relative = context.strip_prefix(paths.builds_dir())?;
    let mut parts = relative.components();
    if !matches!(parts.next(), Some(std::path::Component::Normal(_)))
        || !matches!(parts.next(), Some(std::path::Component::Normal(_)))
        || parts.next().is_some()
    {
        bail!("invalid managed context path {}", context.display());
    }
    Ok(paths
        .pending_maintenance_dir()
        .join(hash_path(relative).to_hex().to_string()))
}

fn pending_lock(paths: &RgoPaths) -> Result<File> {
    paths.ensure_layout()?;
    let path = paths.state_dir().join("locks/pending-maintenance.lock");
    let file = open_lock_file(&path)?;
    FileExt::lock_exclusive(&file)?;
    verify_lock_identity(&path, &file)?;
    Ok(file)
}

/// Call after attribution and before Cargo enters the managed directory.
/// The generation prevents a completed pass from erasing a newer launch.
pub fn mark_pending_maintenance(paths: &RgoPaths, context: &Path) -> Result<()> {
    let path = pending_path(paths, context)?;
    let _lock = pending_lock(paths)?;
    let generation = match std::fs::read(&path) {
        Ok(bytes) => {
            let record: PendingMaintenance = serde_json::from_slice(&bytes).with_context(|| {
                format!("invalid pending maintenance record {}", path.display())
            })?;
            anyhow::ensure!(
                record.context == context,
                "pending maintenance context mismatch at {}",
                path.display()
            );
            record
                .generation
                .checked_add(1)
                .context("pending maintenance generation exhausted")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 1,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let record = PendingMaintenance {
        context: context.to_path_buf(),
        generation,
        retry_after: None,
    };
    write_pending_record(paths, &path, &record)
}

fn write_pending_record(paths: &RgoPaths, path: &Path, record: &PendingMaintenance) -> Result<()> {
    // The caller holds the pending lock. Keep one staging name outside the
    // scanned record directory, so a crash cannot leave a partial record or
    // an unbounded collection of ignored entries.
    let staging = paths
        .state_dir()
        .join("locks/pending-maintenance-write.tmp");
    match std::fs::symlink_metadata(&staging) {
        Ok(metadata) if metadata.is_dir() => {
            bail!("pending maintenance staging path is a directory");
        }
        Ok(_) => std::fs::remove_file(&staging)?,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&staging)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&record)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&staging, path)
        .with_context(|| format!("replacing pending maintenance record {}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(path.parent().context("pending record has no parent")?)?.sync_all()?;
    Ok(())
}

/// Carry the directory cursor across daemon ticks so launch signals from many
/// projects cannot make one maintenance tick scan an unbounded number of files.
#[derive(Default)]
pub struct PendingMaintenanceScanner {
    entries: Option<std::fs::ReadDir>,
}

impl PendingMaintenanceScanner {
    /// Examine at most `limit` shallow records. A new launch during a sweep
    /// remains durable and is seen when the directory is opened again.
    pub fn scan(&mut self, paths: &RgoPaths, limit: usize) -> Result<Vec<PendingMaintenance>> {
        let _lock = pending_lock(paths)?;
        if self.entries.is_none() {
            self.entries = Some(std::fs::read_dir(paths.pending_maintenance_dir())?);
        }
        let mut pending = Vec::new();
        for _ in 0..limit {
            let entry = match self.entries.as_mut().unwrap().next() {
                Some(entry) => entry?,
                None => {
                    self.entries = None;
                    break;
                }
            };
            let path = entry.path();
            let result = (|| -> Result<PendingMaintenance> {
                let metadata = std::fs::symlink_metadata(&path)?;
                anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink());
                let record: PendingMaintenance = serde_json::from_slice(&std::fs::read(&path)?)?;
                anyhow::ensure!(pending_path(paths, &record.context)? == path);
                Ok(record)
            })();
            match result {
                Ok(record) => pending.push(record),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "invalid pending maintenance record")
                }
            }
        }
        Ok(pending)
    }
}

#[cfg(test)]
fn pending_maintenance(paths: &RgoPaths) -> Result<Vec<PendingMaintenance>> {
    PendingMaintenanceScanner::default().scan(paths, usize::MAX)
}

/// The context guard makes the final comparison/removal atomic with respect
/// to admitted launches. A new launch that starts during GC retains its record.
pub fn clear_pending_maintenance(paths: &RgoPaths, record: &PendingMaintenance) -> Result<bool> {
    finish_pending_maintenance(paths, record, None)
}

/// Keep an unmet launch signal until Cargo's conservative profile-lock grace
/// expires. A new launch resets the delay by advancing its generation.
pub fn defer_pending_maintenance(
    paths: &RgoPaths,
    record: &PendingMaintenance,
    retry_after: u64,
) -> Result<bool> {
    finish_pending_maintenance(paths, record, Some(retry_after))
}

fn finish_pending_maintenance(
    paths: &RgoPaths,
    record: &PendingMaintenance,
    retry_after: Option<u64>,
) -> Result<bool> {
    let Some(_context_guard) = try_lock_gc(paths, Some(&record.context))? else {
        return Ok(false);
    };
    let _lock = pending_lock(paths)?;
    let path = pending_path(paths, &record.context)?;
    let current = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<PendingMaintenance>(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if current.context != record.context || current.generation != record.generation {
        return Ok(false);
    }
    if let Some(retry_after) = retry_after {
        write_pending_record(
            paths,
            &path,
            &PendingMaintenance {
                retry_after: Some(retry_after),
                ..current
            },
        )?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(true)
}

pub struct SessionGuard {
    _file: File,
    #[cfg(unix)]
    _local: File,
    #[cfg(unix)]
    _descendants: File,
}

impl SessionGuard {
    /// Keep both lifecycle descriptors open when a Unix launcher replaces
    /// itself with Cargo. The record lock tracks Cargo's process, and the
    /// inherited flock protects descendants that outlive it.
    #[cfg(unix)]
    pub fn retain_across_exec(&self) -> Result<()> {
        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};

        for file in [&self._file, &self._descendants] {
            let flags = fcntl_getfd(file).context("reading lock descriptor flags")?;
            fcntl_setfd(file, flags & !FdFlags::CLOEXEC)
                .context("retaining Cargo lifecycle lock")?;
        }
        Ok(())
    }
}

pub struct GcGuards {
    #[cfg(unix)]
    _local: File,
    #[cfg(unix)]
    _legacy_global: File,
    _global: File,
    #[cfg(unix)]
    _legacy_context: Option<File>,
    _context: Option<File>,
}

// POSIX record locks are process-associated. A separate flock file supplies
// same-process exclusion and closes on exec, while the fcntl lock survives.
#[cfg(unix)]
fn open_local_guard(paths: &RgoPaths) -> Result<File> {
    paths.ensure_layout()?;
    let path = paths.state_dir().join("locks/process-guard.lock");
    let file = open_lock_file(&path)
        .with_context(|| format!("opening local lifecycle guard {}", path.display()))?;
    #[cfg(unix)]
    {
        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
        let flags = fcntl_getfd(&file)?;
        fcntl_setfd(&file, flags | FdFlags::CLOEXEC)?;
    }
    Ok(file)
}

fn lock_path(paths: &RgoPaths, context: Option<&Path>, legacy: bool) -> Result<PathBuf> {
    let name = if let Some(context) = context {
        let relative = context
            .strip_prefix(paths.builds_dir())
            .with_context(|| format!("{} is outside managed build storage", context.display()))?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            bail!("invalid managed context path {}", context.display());
        }
        let digest = hash_path(relative);
        if legacy {
            format!("context-{}.lock", digest.to_hex())
        } else {
            format!("context-{}.fcntl.lock", digest.to_hex())
        }
    } else if legacy {
        "global-cargo-session.lock".to_owned()
    } else {
        "global-cargo-session.fcntl.lock".to_owned()
    };
    Ok(paths.state_dir().join("locks").join(name))
}

#[cfg(unix)]
fn hash_path(path: &Path) -> blake3::Hash {
    use std::os::unix::ffi::OsStrExt;
    blake3::hash(path.as_os_str().as_bytes())
}

#[cfg(windows)]
fn hash_path(path: &Path) -> blake3::Hash {
    use std::os::windows::ffi::OsStrExt;
    let mut hasher = blake3::Hasher::new();
    // Paths returned by Windows directory enumeration use backslashes, while
    // a caller can spell the same context with forward slashes. Hash path
    // components, not the raw separator bytes, so both lock the same file.
    for component in path.components() {
        let words: Vec<u16> = component.as_os_str().encode_wide().collect();
        hasher.update(&(words.len() as u64).to_le_bytes());
        for word in words {
            hasher.update(&word.to_le_bytes());
        }
    }
    hasher.finalize()
}

#[cfg(not(any(unix, windows)))]
fn hash_path(path: &Path) -> blake3::Hash {
    blake3::hash(path.to_string_lossy().as_bytes())
}

fn open_lock(paths: &RgoPaths, context: Option<&Path>, legacy: bool) -> Result<File> {
    paths.ensure_layout()?;
    let path = lock_path(paths, context, legacy)?;
    open_lock_file(&path).with_context(|| format!("opening lifecycle lock {}", path.display()))
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    // The filename is stable for a context's entire lifetime. Reject an
    // existing non-file before opening so a FIFO cannot stall the launcher.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            bail!("lifecycle lock is not a regular file: {}", path.display());
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error)
                .with_context(|| format!("checking lifecycle lock {}", path.display()));
        }
        _ => {}
    }
    #[cfg(unix)]
    let file = {
        let parent = path.parent().context("lifecycle lock has no parent")?;
        let name = path.file_name().context("lifecycle lock has no filename")?;
        let directory = openat(
            CWD,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("opening lifecycle lock directory {}", parent.display()))?;
        // A heavily concurrent first launch has returned ENOENT here on macOS
        // despite O_CREAT and an opened parent. Retry that one error briefly;
        // the identity check below still rejects a replaced lock path.
        let mut opened = None;
        for attempt in 0..3 {
            match openat(
                &directory,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(file) => {
                    opened = Some(file);
                    break;
                }
                Err(Errno::NOENT) if attempt < 2 => {
                    std::thread::sleep(std::time::Duration::from_millis(1 << attempt));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("creating lifecycle lock {}", path.display()));
                }
            }
        }
        File::from(opened.context("lifecycle lock retry exhausted")?)
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;

        // Other sessions may read/write and lock the same file, but no caller
        // may rename or delete its name while any rgo guard is open.
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .open(path)?
    };
    #[cfg(not(any(unix, windows)))]
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    verify_lock_identity(path, &file)?;
    Ok(file)
}

pub(crate) fn verify_lock_identity(path: &Path, file: &File) -> Result<()> {
    let opened = file.metadata()?;
    let named = std::fs::symlink_metadata(path)
        .with_context(|| format!("checking lifecycle lock {}", path.display()))?;
    if !opened.is_file() || !named.is_file() {
        bail!("lifecycle lock is not a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != named.dev() || opened.ino() != named.ino() {
            bail!("lifecycle lock was replaced: {}", path.display());
        }
    }
    #[cfg(windows)]
    {
        let named_file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("opening named lifecycle lock {}", path.display()))?;
        let opened_id = crate::size::windows_open_file_id(file)
            .with_context(|| format!("identifying opened lifecycle lock {}", path.display()))?;
        let named_id = crate::size::windows_open_file_id(&named_file)
            .with_context(|| format!("identifying named lifecycle lock {}", path.display()))?;
        if opened_id != named_id {
            bail!("lifecycle lock was replaced: {}", path.display());
        }
    }
    Ok(())
}

/// Hold for the full Cargo invocation. An unknown or multi-workspace command
/// uses `None` so GC defers every destructive action while it runs.
pub fn lock_cargo_session(paths: &RgoPaths, context: Option<&Path>) -> Result<SessionGuard> {
    #[cfg(unix)]
    let local = open_local_guard(paths)?;
    #[cfg(debug_assertions)]
    mark_session_lock_attempt_for_test()?;
    #[cfg(unix)]
    FileExt::lock_shared(&local)?;
    #[cfg(unix)]
    verify_lock_identity(&paths.state_dir().join("locks/process-guard.lock"), &local)?;
    #[cfg(unix)]
    let descendants = {
        let file = open_lock(paths, context, true)?;
        FileExt::lock_shared(&file)?;
        verify_lock_identity(&lock_path(paths, context, true)?, &file)?;
        file
    };
    let file = open_lock(paths, context, cfg!(not(unix)))?;
    lock_shared(&file)?;
    verify_lock_identity(&lock_path(paths, context, cfg!(not(unix)))?, &file)?;
    Ok(SessionGuard {
        _file: file,
        #[cfg(unix)]
        _local: local,
        #[cfg(unix)]
        _descendants: descendants,
    })
}

/// A debug-build marker for a real-Cargo fixture that starts a launcher while
/// GC holds its exclusion guard. It fires immediately before the first wait.
#[cfg(debug_assertions)]
fn mark_session_lock_attempt_for_test() -> Result<()> {
    let Some(marker) = std::env::var_os("RGO_TEST_SESSION_LOCK_MARKER") else {
        return Ok(());
    };
    let marker = PathBuf::from(marker);
    let staging = marker.with_extension("tmp");
    std::fs::write(&staging, b"waiting")?;
    std::fs::rename(staging, marker)?;
    Ok(())
}

/// GC never waits behind a Cargo session: a queued writer could block a nested
/// shared lock and deadlock its parent build. Keep both guards through removal.
pub fn try_lock_gc(paths: &RgoPaths, context: Option<&Path>) -> Result<Option<GcGuards>> {
    #[cfg(unix)]
    let local = open_local_guard(paths)?;
    #[cfg(unix)]
    if !FileExt::try_lock_exclusive(&local)? {
        tracing::debug!(guard = "process", "GC lifecycle guard is busy");
        return Ok(None);
    }
    #[cfg(unix)]
    verify_lock_identity(&paths.state_dir().join("locks/process-guard.lock"), &local)?;
    #[cfg(unix)]
    let legacy_global = {
        let file = open_lock(paths, None, true)?;
        if !FileExt::try_lock_exclusive(&file)? {
            tracing::debug!(guard = "legacy-global", "GC lifecycle guard is busy");
            return Ok(None);
        }
        verify_lock_identity(&lock_path(paths, None, true)?, &file)?;
        file
    };
    let global = open_lock(paths, None, cfg!(not(unix)))?;
    if !try_lock_exclusive(&global)? {
        tracing::debug!(guard = "global", "GC lifecycle guard is busy");
        return Ok(None);
    }
    verify_lock_identity(&lock_path(paths, None, cfg!(not(unix)))?, &global)?;
    #[cfg(unix)]
    let legacy_context_guard = if let Some(context) = context {
        let file = open_lock(paths, Some(context), true)?;
        if !FileExt::try_lock_exclusive(&file)? {
            tracing::debug!(guard = "legacy-context", "GC lifecycle guard is busy");
            return Ok(None);
        }
        verify_lock_identity(&lock_path(paths, Some(context), true)?, &file)?;
        Some(file)
    } else {
        None
    };
    let context_guard = if let Some(context) = context {
        let file = open_lock(paths, Some(context), cfg!(not(unix)))?;
        if !try_lock_exclusive(&file)? {
            tracing::debug!(guard = "context", "GC lifecycle guard is busy");
            return Ok(None);
        }
        verify_lock_identity(&lock_path(paths, Some(context), cfg!(not(unix)))?, &file)?;
        Some(file)
    } else {
        None
    };
    Ok(Some(GcGuards {
        #[cfg(unix)]
        _local: local,
        #[cfg(unix)]
        _legacy_global: legacy_global,
        _global: global,
        #[cfg(unix)]
        _legacy_context: legacy_context_guard,
        _context: context_guard,
    }))
}

#[cfg(unix)]
fn lock_shared(file: &File) -> Result<()> {
    fcntl_lock(file, FlockOperation::LockShared).context("acquiring Cargo lifecycle lock")?;
    Ok(())
}

#[cfg(not(unix))]
fn lock_shared(file: &File) -> Result<()> {
    FileExt::lock_shared(file)?;
    Ok(())
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<bool> {
    match fcntl_lock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::AGAIN | Errno::ACCESS) => Ok(false),
        Err(error) => Err(error).context("acquiring GC lifecycle lock"),
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(file: &File) -> Result<bool> {
    Ok(FileExt::try_lock_exclusive(file)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_scan_bounds_each_tick_and_revisits_new_launches() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let contexts: Vec<_> = (0..70)
            .map(|index| paths.builds_dir().join(format!("aa/context-{index}")))
            .collect();
        for context in &contexts {
            mark_pending_maintenance(&paths, context).unwrap();
        }
        let mut scanner = PendingMaintenanceScanner::default();
        let first = scanner.scan(&paths, 17).unwrap();
        assert_eq!(first.len(), 17);
        let late = paths.builds_dir().join("aa/late-launch");
        mark_pending_maintenance(&paths, &late).unwrap();

        let mut seen: std::collections::HashSet<_> =
            first.into_iter().map(|record| record.context).collect();
        for _ in 0..12 {
            let batch = scanner.scan(&paths, 17).unwrap();
            assert!(batch.len() <= 17);
            seen.extend(batch.into_iter().map(|record| record.context));
        }
        assert!(contexts.iter().all(|context| seen.contains(context)));
        assert!(seen.contains(&late));
    }

    #[test]
    fn completed_maintenance_cannot_erase_a_newer_or_active_launch() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let context = paths.builds_dir().join("aa/context");
        let session = lock_cargo_session(&paths, Some(&context)).unwrap();
        mark_pending_maintenance(&paths, &context).unwrap();
        let original = pending_maintenance(&paths).unwrap().pop().unwrap();
        assert!(!clear_pending_maintenance(&paths, &original).unwrap());
        mark_pending_maintenance(&paths, &context).unwrap();
        drop(session);
        assert!(!clear_pending_maintenance(&paths, &original).unwrap());
        let newer = pending_maintenance(&paths).unwrap().pop().unwrap();
        assert!(newer.generation > original.generation);
        let retry = crate::context::unix_now() + 60;
        assert!(defer_pending_maintenance(&paths, &newer, retry).unwrap());
        assert_eq!(
            pending_maintenance(&paths).unwrap()[0].retry_after,
            Some(retry)
        );
        mark_pending_maintenance(&paths, &context).unwrap();
        let latest = pending_maintenance(&paths).unwrap().pop().unwrap();
        assert!(latest.generation > newer.generation);
        assert_eq!(latest.retry_after, None);
        assert!(!clear_pending_maintenance(&paths, &newer).unwrap());
        assert!(clear_pending_maintenance(&paths, &latest).unwrap());
        assert!(pending_maintenance(&paths).unwrap().is_empty());

        mark_pending_maintenance(&paths, &context).unwrap();
        let path = pending_path(&paths, &context).unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let staging = paths
            .state_dir()
            .join("locks/pending-maintenance-write.tmp");
        std::fs::create_dir(&staging).unwrap();
        assert!(mark_pending_maintenance(&paths, &context).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        std::fs::remove_dir(&staging).unwrap();
        mark_pending_maintenance(&paths, &context).unwrap();
        assert_eq!(pending_maintenance(&paths).unwrap()[0].generation, 2);

        std::fs::write(&path, b"truncated").unwrap();
        assert!(mark_pending_maintenance(&paths, &context).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"truncated");
    }

    #[cfg(unix)]
    #[test]
    fn distinct_non_utf8_paths_never_share_context_or_lock_identity() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let one = root
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
        let two = root
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'a', 0xfe]));
        assert_eq!(one.to_string_lossy(), two.to_string_lossy());
        let first = context_for_workspace(&paths, &one).unwrap();
        let second = context_for_workspace(&paths, &two).unwrap();
        assert_ne!(first, second);
        assert_ne!(
            lock_path(&paths, Some(&first), false).unwrap(),
            lock_path(&paths, Some(&second), false).unwrap()
        );
        let raw_first = paths
            .builds_dir()
            .join(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
        let raw_second = paths
            .builds_dir()
            .join(std::ffi::OsString::from_vec(vec![b'a', 0xfe]));
        assert_ne!(
            lock_path(&paths, Some(&raw_first), false).unwrap(),
            lock_path(&paths, Some(&raw_second), false).unwrap()
        );
    }

    #[test]
    fn context_and_global_sessions_prevent_gc_without_replacing_lock_files() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        let context_session = lock_cargo_session(&paths, Some(&context)).unwrap();
        assert!(try_lock_gc(&paths, Some(&context)).unwrap().is_none());
        drop(context_session);
        let global_session = lock_cargo_session(&paths, None).unwrap();
        assert!(try_lock_gc(&paths, Some(&context)).unwrap().is_none());
        drop(global_session);
        assert!(try_lock_gc(&paths, Some(&context)).unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn an_active_windows_context_does_not_block_unrelated_gc() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let active = paths.builds_dir().join("aa/active");
        let idle = paths.builds_dir().join("bb/idle");
        let session = lock_cargo_session(&paths, Some(&active)).unwrap();
        assert!(try_lock_gc(&paths, Some(&active)).unwrap().is_none());
        assert!(try_lock_gc(&paths, Some(&idle)).unwrap().is_some());
        drop(session);
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_identity_ignores_separator_spelling() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let forward = paths.builds_dir().join("aa/context");
        let native = paths.builds_dir().join("aa").join("context");
        assert_eq!(
            lock_path(&paths, Some(&forward), true).unwrap(),
            lock_path(&paths, Some(&native), true).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn gc_still_observes_legacy_flock_sessions() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let context = paths.builds_dir().join("aa/context");
        let old_context = open_lock(&paths, Some(&context), true).unwrap();
        FileExt::lock_shared(&old_context).unwrap();
        assert!(try_lock_gc(&paths, Some(&context)).unwrap().is_none());
        FileExt::unlock(&old_context).unwrap();
        drop(old_context);
        assert!(try_lock_gc(&paths, Some(&context)).unwrap().is_some());

        let old_global = open_lock(&paths, None, true).unwrap();
        FileExt::lock_shared(&old_global).unwrap();
        assert!(try_lock_gc(&paths, Some(&context)).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_lock_files_fail_closed_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let external = root.path().join("external");
        std::fs::write(&external, "untouched").unwrap();

        let global = lock_path(&paths, None, true).unwrap();
        symlink(&external, &global).unwrap();
        assert!(try_lock_gc(&paths, None).is_err());
        std::fs::remove_file(&global).unwrap();

        let context = paths.builds_dir().join("aa/context");
        let session = lock_path(&paths, Some(&context), true).unwrap();
        symlink(&external, &session).unwrap();
        assert!(lock_cargo_session(&paths, Some(&context)).is_err());
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "untouched");
    }

    #[cfg(unix)]
    #[test]
    fn replaced_lock_inode_fails_identity_check() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let path = lock_path(&paths, None, true).unwrap();
        let old = open_lock(&paths, None, true).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        assert!(verify_lock_identity(&path, &old).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn replaced_windows_lock_file_fails_identity_check() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let path = lock_path(&paths, None, true).unwrap();
        // An external default-share handle can still see replacement; rgo's
        // own opener below denies this once it has taken a guard.
        let old = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        std::fs::rename(&path, root.path().join("old-lock")).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        assert!(verify_lock_identity(&path, &old).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn active_windows_lock_file_cannot_be_replaced() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        let path = lock_path(&paths, None, true).unwrap();
        let old = open_lock(&paths, None, true).unwrap();
        let moved = root.path().join("moved-lock");
        assert!(std::fs::rename(&path, &moved).is_err());
        drop(old);
        std::fs::rename(&path, &moved).unwrap();
    }
}
