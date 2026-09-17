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
            if let Some(key) = inode_key(&md) {
                if md_nlink(&md) > 1 && !self.seen.insert(key) {
                    continue;
                }
            }
            u.files += 1;
            u.logical_bytes += md.len();
            u.physical_bytes += physical_len(&md);
        }
        u
    }
}

#[cfg(unix)]
fn inode_key(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), md.ino()))
}
#[cfg(unix)]
fn md_nlink(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.nlink()
}
#[cfg(unix)]
fn physical_len(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks() * 512
}

#[cfg(windows)]
fn inode_key(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::fs::MetadataExt;
    Some((md.volume_serial_number()? as u64, md.file_index()?))
}
#[cfg(windows)]
fn md_nlink(md: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    md.number_of_links().unwrap_or(1) as u64
}
#[cfg(windows)]
fn physical_len(md: &std::fs::Metadata) -> u64 {
    // TODO(windows): use GetCompressedFileSizeW / FILE_STANDARD_INFO.AllocationSize for
    // true allocated size; logical length is an acceptable upper bound for now.
    md.len()
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
