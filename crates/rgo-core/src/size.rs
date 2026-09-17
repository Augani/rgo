//! Physical size accounting. Reports *allocated* bytes (what the disk actually loses),
//! counting each hard-linked inode once. Cargo hardlinks uplifted binaries between the
//! build-dir and target-dir, so logical sums overstate reality.

use std::collections::HashSet;
use std::path::Path;

use walkdir::WalkDir;

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
}
