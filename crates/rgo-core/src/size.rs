//! Physical size accounting. Reports *allocated* bytes (what the disk actually loses),
//! counting each hard-linked inode once. Cargo hardlinks uplifted binaries between the
//! build-dir and target-dir, so logical sums overstate reality.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use walkdir::WalkDir;

use crate::paths::RgoPaths;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub physical_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
}

/// Accumulates across multiple `measure` calls so shared inodes are counted once per scan.
#[derive(Default)]
pub struct Scanner {
    seen: HashSet<(u64, u64)>,
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
            if let Some(key) = inode_key(entry.path(), &md) {
                if md_nlink(entry.path(), &md) > 1 && !self.seen.insert(key) {
                    continue;
                }
            }
            u.files += 1;
            u.logical_bytes += md.len();
            u.physical_bytes += physical_len(entry.path(), &md);
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
            if !metadata.is_file() {
                continue;
            }
            if let Some(key) = inode_key(entry.path(), &metadata) {
                if md_nlink(entry.path(), &metadata) > 1 && !self.seen.insert(key) {
                    continue;
                }
            }
            usage.files = usage.files.saturating_add(1);
            usage.logical_bytes = usage.logical_bytes.saturating_add(metadata.len());
            usage.physical_bytes = usage
                .physical_bytes
                .saturating_add(physical_len(entry.path(), &metadata));
        }
        Ok(usage)
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

/// Files under rgo's root other than managed build contexts and CAS. This
/// includes temporary, quarantine, state, logs, and owned root-level files.
/// Unknown entries are counted but never selected for deletion by this scan.
pub fn auxiliary_usage(paths: &RgoPaths) -> Result<Usage> {
    let entries = match std::fs::read_dir(&paths.root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Usage::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", paths.root.display()));
        }
    };
    let mut scanner = Scanner::new();
    let mut total = Usage::default();
    for entry in entries {
        let path = entry?.path();
        if path == paths.builds_dir() || path == paths.cas_dir() {
            continue;
        }
        ensure!(
            !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "unsafe symlinked auxiliary entry {}",
            path.display()
        );
        let usage = scanner.measure_checked(&path)?;
        total.physical_bytes = total.physical_bytes.saturating_add(usage.physical_bytes);
        total.logical_bytes = total.logical_bytes.saturating_add(usage.logical_bytes);
        total.files = total.files.saturating_add(usage.files);
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
            0x8000_0000,
            0x0000_0001 | 0x0000_0002 | 0x0000_0004,
            std::ptr::null_mut(),
            3,
            0x0000_0080,
            std::ptr::null_mut(),
        )
    };
    if handle as isize == -1 {
        return None;
    }
    let mut info = ByHandleFileInformation {
        attributes: 0,
        creation_time_low: 0,
        creation_time_high: 0,
        last_access_time_low: 0,
        last_access_time_high: 0,
        last_write_time_low: 0,
        last_write_time_high: 0,
        volume_serial: 0,
        file_size_high: 0,
        file_size_low: 0,
        number_of_links: 0,
        file_index_high: 0,
        file_index_low: 0,
    };
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
        let expected = Scanner::new().measure(&paths.tmp_dir()).physical_bytes
            + Scanner::new().measure(&paths.state_dir()).physical_bytes;
        assert_eq!(auxiliary_usage(&paths).unwrap().physical_bytes, expected);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(paths.tmp_dir(), paths.root.join("unexpected-link"))
                .unwrap();
            assert!(auxiliary_usage(&paths).is_err());
        }
    }
}
