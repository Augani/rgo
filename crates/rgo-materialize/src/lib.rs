//! Safe output materialization with a conservative copy fallback.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    CloneFile,
    HardLink,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub hard_links: bool,
}

pub fn capabilities(target: &Path) -> Capabilities {
    Capabilities {
        hard_links: target
            .components()
            .any(|component| component.as_os_str() == "deps"),
    }
}

/// Materialize one verified CAS object into Cargo's output path. The copy path is atomic from
/// Cargo's point of view: the temporary sibling is renamed only after the complete object has
/// been written.
pub fn materialize(
    object: &Path,
    destination: &Path,
    mode: u32,
    allow_hard_link: bool,
) -> Result<Strategy> {
    let parent = destination
        .parent()
        .context("materialization target has no parent")?;
    fs::create_dir_all(parent)?;
    let clone_temp = temporary_sibling(destination);
    let _ = fs::remove_file(&clone_temp);
    if try_clone_file(object, &clone_temp) {
        set_mode(&clone_temp, mode)?;
        replace(&clone_temp, destination)?;
        return Ok(Strategy::CloneFile);
    }
    if allow_hard_link && capabilities(destination).hard_links && same_volume(object, parent) {
        let temp = temporary_sibling(destination);
        let _ = fs::remove_file(&temp);
        if is_read_only(object) && fs::hard_link(object, &temp).is_ok() {
            replace(&temp, destination)?;
            return Ok(Strategy::HardLink);
        }
        let _ = fs::remove_file(&temp);
    }
    let temp = temporary_sibling(destination);
    let _ = fs::remove_file(&temp);
    fs::copy(object, &temp)
        .with_context(|| format!("copying {} to {}", object.display(), destination.display()))?;
    set_mode(&temp, mode)?;
    replace(&temp, destination)?;
    Ok(Strategy::Copy)
}

pub fn materialize_bytes(bytes: &[u8], destination: &Path, mode: u32) -> Result<()> {
    let parent = destination
        .parent()
        .context("materialization target has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = temporary_sibling(destination);
    fs::write(&temp, bytes)?;
    set_mode(&temp, mode)?;
    replace(&temp, destination)
}

fn replace(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        let _ = fs::remove_file(destination);
    }
    fs::rename(source, destination)
        .with_context(|| format!("renaming {} to {}", source.display(), destination.display()))
}

fn temporary_sibling(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("output");
    destination.with_file_name(format!(".{name}.rgo-{}.tmp", std::process::id()))
}

fn set_mode(_path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    Ok(())
}

fn same_volume(left: &Path, right: &Path) -> bool {
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(left).map(|m| m.dev()).ok() == fs::metadata(right).map(|m| m.dev()).ok()
    };
    #[cfg(not(unix))]
    let result = left.components().next() == right.components().next();
    result
}

fn is_read_only(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.permissions().readonly())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe extern "C" {
    fn clonefile(
        source: *const std::ffi::c_char,
        destination: *const std::ffi::c_char,
        flags: u32,
    ) -> i32;
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn try_clone_file(source: &Path, destination: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(source) = CString::new(source.as_os_str().as_bytes()) else {
        return false;
    };
    let Ok(destination) = CString::new(destination.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { clonefile(source.as_ptr(), destination.as_ptr(), 0) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn try_clone_file(_source: &Path, _destination: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_with_copy_and_preserves_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let object = temp.path().join("object");
        let output = temp.path().join("deps").join("libdemo.rlib");
        fs::write(&object, b"artifact").unwrap();
        let strategy = materialize(&object, &output, 0o644, false).unwrap();
        assert!(matches!(strategy, Strategy::CloneFile | Strategy::Copy));
        assert_eq!(fs::read(output).unwrap(), b"artifact");
    }
}
