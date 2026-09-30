//! Physical size accounting. Reports *allocated* bytes (what the disk actually loses),
//! counting each hard-linked inode once. Cargo hardlinks uplifted binaries between the
//! build-dir and target-dir, so logical sums overstate reality.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use walkdir::WalkDir;

use crate::context::{self, BuildContext};
use crate::paths::RgoPaths;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub physical_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
}

/// One accounting snapshot of all rgo-owned storage. Hard-linked inodes are
/// attributed to the first domain scanned: builds, CAS, then auxiliary files.
pub struct ManagedSnapshot {
    pub contexts: Vec<BuildContext>,
    pub build_bytes: u64,
    pub cas_bytes: u64,
    pub auxiliary_bytes: u64,
}

impl ManagedSnapshot {
    pub fn total_bytes(&self) -> u64 {
        self.build_bytes
            .saturating_add(self.cas_bytes)
            .saturating_add(self.auxiliary_bytes)
    }
}

pub fn managed_snapshot(paths: &RgoPaths) -> Result<ManagedSnapshot> {
    let mut scanner = Scanner::new();
    let contexts = context::list_with_scanner(paths, &mut scanner)?;
    let build_bytes = contexts.iter().fold(0u64, |total, context| {
        total.saturating_add(context.usage.physical_bytes)
    });
    let cas_bytes = scanner.measure_optional(&paths.cas_dir())?.physical_bytes;
    let auxiliary_bytes = auxiliary_usage_with_scanner(paths, &mut scanner)?.physical_bytes;
    Ok(ManagedSnapshot {
        contexts,
        build_bytes,
        cas_bytes,
        auxiliary_bytes,
    })
}

/// Accumulates across multiple `measure` calls so shared inodes are counted once per scan.
#[derive(Default)]
pub struct Scanner {
    seen: HashSet<(u64, u64)>,
}

struct FileIdentity {
    inode: Option<(u64, u64)>,
    links: u64,
    logical_bytes: u64,
}

/// Only rgo-owned top-level entries belong to its storage budget. In
/// particular, a user may select a root near Cargo's registry or a checkout;
/// those neighboring files must not become apparent rgo pressure.
fn budget_root_entry(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("builds" | "cas" | "state" | "tmp" | "quarantine" | "logs" | "config.toml")
    )
}

impl FileIdentity {
    fn read(path: &Path, metadata: &std::fs::Metadata) -> Self {
        Self {
            inode: inode_key(path, metadata),
            links: md_nlink(path, metadata),
            logical_bytes: metadata.len(),
        }
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn measure(&mut self, dir: &Path) -> Usage {
        let mut u = Usage::default();
        for entry in WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let Ok(md) = entry.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            if self.record_identity(&FileIdentity::read(entry.path(), &md), &mut u) {
                u.physical_bytes = u
                    .physical_bytes
                    .saturating_add(physical_len(entry.path(), &md));
            }
        }
        u
    }

    /// Measure an expected path without hiding unreadable or disappearing
    /// entries. Budget decisions must not treat a partial traversal as a
    /// complete byte count.
    pub fn measure_checked(&mut self, path: &Path) -> Result<Usage> {
        let mut usage = Usage::default();
        for entry in WalkDir::new(path).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", path.display()))?;
            let metadata = entry
                .metadata()
                .with_context(|| format!("reading metadata for {}", entry.path().display()))?;
            if metadata.is_file()
                && self.record_identity(&FileIdentity::read(entry.path(), &metadata), &mut usage)
            {
                usage.physical_bytes = usage
                    .physical_bytes
                    .saturating_add(physical_len(entry.path(), &metadata));
            }
        }
        Ok(usage)
    }

    /// Measure a context and its already-validated incremental directories in
    /// one filesystem walk. The subtotal has its own inode set because a hard
    /// link already charged to the overall total outside incremental state
    /// must still appear when it is present under an incremental directory.
    pub(crate) fn measure_checked_with_subtotal(
        &mut self,
        path: &Path,
        subpaths: &[PathBuf],
        subtotal_scanner: &mut Scanner,
    ) -> Result<(Usage, Usage)> {
        let mut usage = Usage::default();
        let mut subtotal = Usage::default();
        for entry in WalkDir::new(path).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", path.display()))?;
            let metadata = entry
                .metadata()
                .with_context(|| format!("reading metadata for {}", entry.path().display()))?;
            if !metadata.is_file() {
                continue;
            }
            let identity = FileIdentity::read(entry.path(), &metadata);
            let subtotal_counted = subpaths
                .iter()
                .any(|subpath| entry.path().starts_with(subpath))
                && subtotal_scanner.record_identity(&identity, &mut subtotal);
            let total_counted = self.record_identity(&identity, &mut usage);
            if total_counted || subtotal_counted {
                let bytes = physical_len(entry.path(), &metadata);
                if total_counted {
                    usage.physical_bytes = usage.physical_bytes.saturating_add(bytes);
                }
                if subtotal_counted {
                    subtotal.physical_bytes = subtotal.physical_bytes.saturating_add(bytes);
                }
            }
        }
        Ok((usage, subtotal))
    }

    fn record_identity(&mut self, identity: &FileIdentity, usage: &mut Usage) -> bool {
        if let Some(key) = identity.inode {
            if identity.links > 1 && !self.seen.insert(key) {
                return false;
            }
        }
        usage.files = usage.files.saturating_add(1);
        usage.logical_bytes = usage.logical_bytes.saturating_add(identity.logical_bytes);
        true
    }

    /// Missing optional domains count as empty before initial setup. Once a
    /// domain exists, a failed traversal is an error rather than zero usage.
    pub fn measure_optional(&mut self, path: &Path) -> Result<Usage> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                self.measure_checked(path)
            }
            Ok(_) => anyhow::bail!("unsafe storage domain {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Usage::default()),
            Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
        }
    }
}

/// A bounded, advisory trigger scan for automatic maintenance. A completed
/// result can request an authoritative GC pass, but it is never used to plan a
/// deletion: Cargo may change files between chunks, and explicit GC takes its
/// own checked snapshot under the operation lock.
#[derive(Default)]
pub(crate) struct TriggerScan {
    walk: Option<walkdir::IntoIter>,
    root: Option<PathBuf>,
    scanner: Scanner,
    usage: Usage,
    last_completed: Option<Instant>,
}

impl TriggerScan {
    /// Bytes observed so far in an incomplete sweep. This is enough to request
    /// an authoritative pass once it exceeds the watermark, but never enough
    /// to plan a deletion or conclude that storage is below budget.
    pub(crate) fn observed_bytes(&self) -> u64 {
        self.usage.physical_bytes
    }

    /// Visit at most `limit` filesystem entries. A quiet completed sweep waits
    /// for `interval` before starting another; an incomplete sweep resumes on
    /// the next call. Any scan error discards the partial total.
    pub(crate) fn advance(
        &mut self,
        paths: &RgoPaths,
        limit: usize,
        interval: Duration,
    ) -> Result<Option<u64>> {
        ensure!(limit > 0, "trigger scan limit must be positive");
        if self.walk.is_none() {
            if self
                .last_completed
                .is_some_and(|completed| completed.elapsed() < interval)
            {
                return Ok(None);
            }
            // RGO_HOME itself may intentionally be a symlink. Canonicalize
            // that one entry, then never follow links inside the owned tree.
            let root = paths
                .root
                .canonicalize()
                .with_context(|| format!("resolving {}", paths.root.display()))?;
            ensure!(root.is_dir(), "storage root is not a directory");
            self.walk = Some(WalkDir::new(&root).follow_links(false).into_iter());
            self.root = Some(root);
            self.scanner = Scanner::new();
            self.usage = Usage::default();
        }
        for _ in 0..limit {
            let next = self.walk.as_mut().unwrap().next();
            let entry = match next {
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    self.invalidate();
                    return Err(error).context("walking managed storage for automatic trigger");
                }
                None => {
                    let expected = self.root.as_ref().unwrap();
                    let actual = paths.root.canonicalize();
                    if !actual.as_ref().is_ok_and(|actual| actual == expected) {
                        self.invalidate();
                        anyhow::bail!("storage root changed during automatic trigger scan");
                    }
                    let total = self.usage.physical_bytes;
                    self.mark_completed();
                    return Ok(Some(total));
                }
            };
            if entry.depth() == 1 && !budget_root_entry(entry.file_name()) {
                if entry.file_type().is_dir() {
                    self.walk.as_mut().unwrap().skip_current_dir();
                }
                continue;
            }
            if entry.depth() == 1 && entry.file_type().is_symlink() {
                let path = entry.path().to_path_buf();
                self.invalidate();
                anyhow::bail!("unsafe symlinked storage entry {}", path.display());
            }
            if entry.depth() == 1
                && ((entry.file_name() == "config.toml" && !entry.file_type().is_file())
                    || (entry.file_name() != "config.toml" && !entry.file_type().is_dir()))
            {
                let path = entry.path().to_path_buf();
                self.invalidate();
                anyhow::bail!("unsafe managed storage entry {}", path.display());
            }
            if matches!(entry.depth(), 2 | 3) {
                let builds = self.root.as_ref().unwrap().join("builds");
                let is_build_shard =
                    entry.depth() == 2 && entry.path().parent() == Some(builds.as_path());
                let is_build_context = entry.depth() == 3
                    && entry.path().parent().and_then(Path::parent) == Some(builds.as_path());
                if (is_build_shard || is_build_context) && !entry.file_type().is_dir() {
                    let path = entry.path().to_path_buf();
                    self.invalidate();
                    anyhow::bail!("unsafe managed build entry {}", path.display());
                }
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    let path = entry.path().to_path_buf();
                    self.invalidate();
                    return Err(error).with_context(|| format!("reading {}", path.display()));
                }
            };
            if metadata.is_file()
                && self.scanner.record_identity(
                    &FileIdentity::read(entry.path(), &metadata),
                    &mut self.usage,
                )
            {
                self.usage.physical_bytes = self
                    .usage
                    .physical_bytes
                    .saturating_add(physical_len(entry.path(), &metadata));
            }
        }
        Ok(None)
    }

    /// Finish a quiet sweep or supersede it after a successful full GC pass.
    pub(crate) fn mark_completed(&mut self) {
        self.walk = None;
        self.root = None;
        self.scanner = Scanner::new();
        self.usage = Usage::default();
        self.last_completed = Some(Instant::now());
    }

    pub(crate) fn invalidate(&mut self) {
        self.walk = None;
        self.root = None;
        self.scanner = Scanner::new();
        self.usage = Usage::default();
        self.last_completed = None;
    }
}

/// Rgo-owned files outside managed build contexts and CAS. Unknown neighbors
/// under the selected root are outside the budget and never selected here.
pub fn auxiliary_usage(paths: &RgoPaths) -> Result<Usage> {
    auxiliary_usage_with_scanner(paths, &mut Scanner::new())
}

fn auxiliary_usage_with_scanner(paths: &RgoPaths, scanner: &mut Scanner) -> Result<Usage> {
    let mut total = Usage::default();
    for path in [
        paths.state_dir(),
        paths.tmp_dir(),
        paths.quarantine_dir(),
        paths.logs_dir(),
    ] {
        let usage = scanner.measure_optional(&path)?;
        total.physical_bytes = total.physical_bytes.saturating_add(usage.physical_bytes);
        total.logical_bytes = total.logical_bytes.saturating_add(usage.logical_bytes);
        total.files = total.files.saturating_add(usage.files);
    }
    let config = paths.config_file();
    match std::fs::symlink_metadata(&config) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let usage = scanner.measure_checked(&config)?;
            total.physical_bytes = total.physical_bytes.saturating_add(usage.physical_bytes);
            total.logical_bytes = total.logical_bytes.saturating_add(usage.logical_bytes);
            total.files = total.files.saturating_add(usage.files);
        }
        Ok(_) => anyhow::bail!("unsafe storage configuration {}", config.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", config.display())),
    }
    Ok(total)
}

#[cfg(unix)]
fn inode_key(_path: &Path, md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), md.ino()))
}
#[cfg(unix)]
fn md_nlink(_path: &Path, md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.nlink()
}
#[cfg(unix)]
fn physical_len(_path: &Path, md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks() * 512
}

#[cfg(windows)]
fn inode_key(path: &Path, _md: &std::fs::Metadata) -> Option<(u64, u64)> {
    let info = windows_file_info(path)?;
    Some((
        u64::from(info.volume_serial),
        (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
    ))
}
#[cfg(windows)]
pub(crate) fn windows_volume_serial(path: &Path) -> Option<u64> {
    windows_file_info(path)
        .map(|info| u64::from(info.volume_serial))
        .filter(|serial| *serial != 0)
}
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn windows_open_file_id(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;

    let mut info = ByHandleFileInformation::default();
    // SAFETY: `file` owns a live Win32 handle and `info` has the expected layout.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    Some((
        u64::from(info.volume_serial),
        (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
    ))
}
#[cfg(windows)]
fn md_nlink(path: &Path, _md: &std::fs::Metadata) -> u64 {
    windows_file_info(path)
        .map(|info| u64::from(info.number_of_links))
        .unwrap_or(1)
}
#[cfg(windows)]
#[allow(unsafe_code)]
fn physical_len(path: &Path, md: &std::fs::Metadata) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == u32::MAX && std::io::Error::last_os_error().raw_os_error() != Some(0) {
        md.len()
    } else {
        (u64::from(high) << 32) | u64::from(low)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
unsafe extern "system" {
    fn GetCompressedFileSizeW(file_name: *const u16, file_size_high: *mut u32) -> u32;
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut std::ffi::c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        info: *mut ByHandleFileInformation,
    ) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)]
struct ByHandleFileInformation {
    attributes: u32,
    creation_time_low: u32,
    creation_time_high: u32,
    last_access_time_low: u32,
    last_access_time_high: u32,
    last_write_time_low: u32,
    last_write_time_high: u32,
    volume_serial: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_file_info(path: &Path) -> Option<ByHandleFileInformation> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x0000_0080, // FILE_READ_ATTRIBUTES
            0x0000_0001 | 0x0000_0002 | 0x0000_0004,
            std::ptr::null_mut(),
            3,
            0x0200_0000, // FILE_FLAG_BACKUP_SEMANTICS also opens directories
            std::ptr::null_mut(),
        )
    };
    if handle as isize == -1 {
        return None;
    }
    let mut info = ByHandleFileInformation::default();
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) != 0 };
    let _ = unsafe { CloseHandle(handle) };
    ok.then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_hardlinks_once() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        std::fs::write(&a, vec![0u8; 8192]).unwrap();
        std::fs::hard_link(&a, d.path().join("b")).unwrap();
        let u = Scanner::new().measure(d.path());
        assert_eq!(u.files, 1);
        assert_eq!(u.logical_bytes, 8192);
        assert!(u.physical_bytes >= 8192);
    }

    #[test]
    fn optional_budget_domain_rejects_a_file_in_place_of_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let domain = root.path().join("cas");
        assert_eq!(
            Scanner::new().measure_optional(&domain).unwrap(),
            Usage::default()
        );
        std::fs::write(&domain, b"unexpected").unwrap();
        assert!(Scanner::new().measure_optional(&domain).is_err());
    }

    #[test]
    fn auxiliary_usage_counts_temp_and_state_without_builds_or_cas() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        std::fs::write(paths.tmp_dir().join("staging"), vec![0u8; 8192]).unwrap();
        std::fs::write(paths.state_dir().join("metadata"), vec![0u8; 8192]).unwrap();
        let build = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("output"), vec![0u8; 8192]).unwrap();
        std::fs::create_dir_all(paths.cas_dir()).unwrap();
        std::fs::write(paths.cas_dir().join("object"), vec![0u8; 8192]).unwrap();
        std::fs::create_dir_all(paths.root.join("registry")).unwrap();
        std::fs::write(paths.root.join("registry/download"), vec![0u8; 8192]).unwrap();
        let expected = Scanner::new().measure(&paths.tmp_dir()).physical_bytes
            + Scanner::new().measure(&paths.state_dir()).physical_bytes;
        assert_eq!(auxiliary_usage(&paths).unwrap().physical_bytes, expected);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(paths.tmp_dir(), paths.root.join("unexpected-link"))
                .unwrap();
            assert_eq!(auxiliary_usage(&paths).unwrap().physical_bytes, expected);
            let logs = paths.logs_dir();
            std::fs::remove_dir(&logs).unwrap();
            std::os::unix::fs::symlink(paths.tmp_dir(), logs).unwrap();
            assert!(auxiliary_usage(&paths).is_err());
        }
    }

    #[test]
    fn managed_snapshot_counts_cross_domain_hardlinks_once() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let later_context = paths.builds_dir().join("aa/context-z");
        let first_context = paths.builds_dir().join("aa/context-a");
        std::fs::create_dir_all(&later_context).unwrap();
        std::fs::create_dir_all(&first_context).unwrap();
        let original = later_context.join("artifact");
        std::fs::write(&original, vec![b'x'; 8192]).unwrap();
        std::fs::hard_link(&original, first_context.join("linked-artifact")).unwrap();
        std::fs::create_dir_all(paths.cas_dir()).unwrap();
        std::fs::hard_link(&original, paths.cas_dir().join("linked-object")).unwrap();
        std::fs::hard_link(&original, paths.tmp_dir().join("linked-temp")).unwrap();

        let expected = Scanner::new()
            .measure_checked(&original)
            .unwrap()
            .physical_bytes;
        let snapshot = managed_snapshot(&paths).unwrap();
        assert_eq!(snapshot.contexts.len(), 2);
        assert_eq!(
            snapshot
                .contexts
                .iter()
                .find(|context| context.dir == first_context)
                .unwrap()
                .usage
                .physical_bytes,
            expected
        );
        assert_eq!(
            snapshot
                .contexts
                .iter()
                .find(|context| context.dir == later_context)
                .unwrap()
                .usage
                .physical_bytes,
            0
        );
        assert_eq!(snapshot.build_bytes, expected);
        assert_eq!(snapshot.cas_bytes, 0);
        assert_eq!(snapshot.auxiliary_bytes, 0);
        assert_eq!(snapshot.total_bytes(), expected);
    }

    #[test]
    fn bounded_trigger_sweep_matches_checked_managed_total() {
        let root = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let context = paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(context.join("debug/incremental")).unwrap();
        let artifact = context.join("debug/incremental/artifact");
        std::fs::write(&artifact, vec![b'x'; 8192]).unwrap();
        std::fs::create_dir_all(paths.cas_dir()).unwrap();
        std::fs::hard_link(&artifact, paths.cas_dir().join("linked-object")).unwrap();
        std::fs::write(paths.state_dir().join("other-state"), vec![b'y'; 4096]).unwrap();
        std::fs::create_dir_all(paths.root.join("registry")).unwrap();
        std::fs::write(paths.root.join("registry/download"), vec![b'z'; 8192]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&artifact, paths.root.join("other-link")).unwrap();

        let expected = managed_snapshot(&paths).unwrap().total_bytes();
        let mut scan = TriggerScan::default();
        let mut chunks = 0;
        let mut observed_partial_pressure = false;
        let measured = loop {
            chunks += 1;
            if let Some(total) = scan.advance(&paths, 1, Duration::from_secs(3600)).unwrap() {
                break total;
            }
            observed_partial_pressure |= scan.observed_bytes() > 0;
            assert!(chunks < 100);
        };
        assert!(chunks > 1);
        assert!(observed_partial_pressure);
        assert_eq!(measured, expected);
        assert_eq!(
            scan.advance(&paths, 1, Duration::from_secs(3600)).unwrap(),
            None
        );
    }
}
