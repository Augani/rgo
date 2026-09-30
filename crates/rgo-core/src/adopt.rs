//! Read-only discovery of legacy Cargo target directories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::{DirEntry, WalkDir};

use crate::cargo_config;
use crate::size::{Scanner, Usage};

const DEFAULT_ROOTS: [&str; 4] = ["Projects", "src", "code", "dev"];
#[derive(Debug, Clone)]
pub struct Candidate {
    pub target_dir: PathBuf,
    pub project_root: Option<PathBuf>,
    /// Allocated-byte estimate for this target directory, including requested
    /// outputs and user files. External hardlinks may make this larger than
    /// unique physical usage. It is not a reclaimable-byte estimate.
    /// A failed walk is not a zero-byte target. Keep the error so callers
    /// cannot present a partial scan as a complete storage estimate.
    pub usage: Result<Usage, String>,
    pub skipped_reason: Option<String>,
}

impl Candidate {
    pub fn has_storage(&self) -> bool {
        self.usage
            .as_ref()
            .is_ok_and(|usage| usage.physical_bytes > 0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<SkippedPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedPath {
    pub path: PathBuf,
    pub reason: String,
}

pub fn default_roots() -> Vec<PathBuf> {
    directories::UserDirs::new()
        .map(|dirs| {
            DEFAULT_ROOTS
                .iter()
                .map(|name| dirs.home_dir().join(name))
                .collect()
        })
        .unwrap_or_default()
}

pub fn scan(roots: &[PathBuf]) -> Result<ScanReport> {
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut entries = WalkDir::new(root).follow_links(false).into_iter();
        while let Some(item) = entries.next() {
            let entry = match item {
                Ok(entry) => entry,
                Err(error) => {
                    skipped.push(SkippedPath {
                        path: error.path().unwrap_or(root).to_path_buf(),
                        reason: format!("unreadable path: {error}"),
                    });
                    continue;
                }
            };
            if entry.depth() > 0 && !should_descend(&entry) {
                entries.skip_current_dir();
                continue;
            }
            if entry.file_type().is_dir() && entry.file_name() == "target" {
                if !seen
                    .iter()
                    .any(|parent: &PathBuf| entry.path().starts_with(parent))
                    && seen.insert(entry.path().to_path_buf())
                {
                    let candidate = scan_target(entry.path());
                    if let Err(reason) = &candidate.usage {
                        skipped.push(SkippedPath {
                            path: candidate.target_dir.clone(),
                            reason: format!("target storage estimate unavailable: {reason}"),
                        });
                    }
                    candidates.push(candidate);
                }
                // `scan_target` measures the directory. Do not traverse its
                // artifact tree again while searching for other projects.
                entries.skip_current_dir();
            }
        }
    }
    candidates.sort_by(|left, right| left.target_dir.cmp(&right.target_dir));
    Ok(ScanReport {
        candidates,
        skipped,
    })
}

fn scan_target(target: &Path) -> Candidate {
    let project_root = find_project_root(target);
    let override_reason = project_root
        .as_deref()
        .and_then(|root| project_override_reason_in_workspace(target, root));
    let usage = Scanner::new()
        .measure_checked(target)
        .map_err(|error| format!("{error:#}"));
    Candidate {
        target_dir: target.to_path_buf(),
        project_root,
        usage,
        skipped_reason: override_reason,
    }
}

fn should_descend(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !entry.file_type().is_dir()
        || (!name.starts_with('.') && name != "node_modules" && name != "vendor")
}

fn find_project_root(target: &Path) -> Option<PathBuf> {
    let mut current = target.parent();
    let mut package_root = None;
    while let Some(path) = current {
        if path.join("Cargo.toml").is_file() {
            package_root = Some(path.to_path_buf());
            break;
        }
        current = path.parent();
    }
    let mut workspace_root = package_root.clone()?;
    let mut current = workspace_root.parent().map(Path::to_path_buf);
    while let Some(path) = current {
        let manifest = path.join("Cargo.toml");
        if manifest.is_file()
            && std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
                .is_some_and(|value| value.get("workspace").is_some())
        {
            workspace_root = path.to_path_buf();
        }
        current = path.parent().map(Path::to_path_buf);
    }
    Some(workspace_root)
}

fn project_override_reason(project: &Path) -> Option<String> {
    for name in ["config.toml", "config"] {
        let path = project.join(".cargo").join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(inspection) = cargo_config::inspect(&text) {
                if inspection.target_dir.is_some() {
                    return Some(format!("project overrides target-dir: {}", path.display()));
                }
                if inspection.build_dir_outside_fence.is_some() {
                    return Some(format!("project overrides build-dir: {}", path.display()));
                }
            }
        }
    }
    None
}

fn project_override_reason_in_workspace(target: &Path, workspace_root: &Path) -> Option<String> {
    let mut current = target.parent();
    while let Some(path) = current {
        if !path.starts_with(workspace_root) {
            break;
        }
        if let Some(reason) = project_override_reason(path) {
            return Some(reason);
        }
        if path == workspace_root {
            break;
        }
        current = path.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_total_target_storage_without_classifying_private_subdirectories() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let target = project.join("target/debug");
        std::fs::create_dir_all(target.join("deps")).unwrap();
        std::fs::create_dir_all(target.join("incremental")).unwrap();
        std::fs::create_dir_all(target.join("examples")).unwrap();
        std::fs::create_dir_all(target.join("custom/user/deps")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(target.join("x"), b"binary").unwrap();
        std::fs::write(target.join("deps/libx.rlib"), b"intermediate").unwrap();
        let report = scan(&[root.path().to_path_buf()]).unwrap();
        let candidate = &report.candidates[0];
        assert_eq!(candidate.usage.as_ref().unwrap().files, 2);
        assert!(candidate.usage.as_ref().unwrap().physical_bytes > 0);
    }

    #[test]
    fn project_target_override_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(project.join(".cargo")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".cargo/config.toml"),
            "[build]\ntarget-dir='other'\n",
        )
        .unwrap();
        let report = scan(&[root.path().to_path_buf()]).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert!(
            report.candidates[0]
                .skipped_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("target-dir"))
        );
    }

    #[test]
    fn scans_target_roots_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("workspace");
        let target = project.join("target/aarch64-unknown-linux-gnu/custom");
        std::fs::create_dir_all(target.join("deps")).unwrap();
        std::fs::create_dir_all(target.join("incremental")).unwrap();
        std::fs::create_dir_all(project.join("target/debug/examples")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.join("member")).unwrap();
        std::fs::write(
            project.join("member/Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.join("member/.cargo")).unwrap();
        std::fs::write(
            project.join("member/.cargo/config.toml"),
            "[build]\nbuild-dir = '../member-build'\n",
        )
        .unwrap();
        let member_target = project.join("member/target/debug/deps");
        std::fs::create_dir_all(&member_target).unwrap();
        std::fs::write(member_target.join("libmember.rlib"), b"member").unwrap();
        std::fs::write(target.join("deps/libmember.rlib"), b"intermediate").unwrap();
        std::fs::write(project.join("target/debug/examples/keep"), b"example").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            project.join("target/debug/examples"),
            target.join("deps/examples-link"),
        )
        .unwrap();

        let report = scan(&[root.path().to_path_buf()]).unwrap();
        assert_eq!(report.candidates.len(), 2);
        let candidate = report
            .candidates
            .iter()
            .find(|candidate| candidate.target_dir == project.join("target"))
            .unwrap();
        assert_eq!(candidate.usage.as_ref().unwrap().files, 2);
        let member_candidate = report
            .candidates
            .iter()
            .find(|candidate| candidate.target_dir == project.join("member/target"))
            .unwrap();
        assert!(
            member_candidate
                .skipped_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("build-dir"))
        );
    }

    #[test]
    fn attributes_nested_workspace_targets_to_the_workspace_manifest() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let member = workspace.join("member");
        let target = member.join("target/debug/deps");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n",
        )
        .unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(target.join("libmember.rlib"), b"intermediate").unwrap();

        let report = scan(&[root.path().to_path_buf()]).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(
            report.candidates[0].project_root.as_deref(),
            Some(workspace.as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn adopt_accounting_deduplicates_hardlinked_target_files() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let target = project.join("target/debug");
        std::fs::create_dir_all(target.join("deps")).unwrap();
        std::fs::create_dir_all(target.join("build")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        let source = target.join("deps/libx.rlib");
        std::fs::write(&source, vec![0u8; 8192]).unwrap();
        std::fs::hard_link(&source, target.join("build/libx.rlib")).unwrap();

        let report = scan(&[root.path().to_path_buf()]).unwrap();
        let candidate = &report.candidates[0];
        assert_eq!(candidate.usage.as_ref().unwrap().files, 1);
        assert!(candidate.usage.as_ref().unwrap().physical_bytes > 0);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_scan_paths_are_reported() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let unreadable = root.path().join("unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let report = scan(&[root.path().to_path_buf()]).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            report
                .skipped
                .iter()
                .any(|item| item.path == unreadable || item.path.starts_with(&unreadable))
        );
        assert!(report.candidates.is_empty());

        let project = root.path().join("project");
        let target = project.join("target");
        let unreadable_child = target.join("debug/deps/unreadable");
        std::fs::create_dir_all(&unreadable_child).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::set_permissions(&unreadable_child, std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let report = scan(&[root.path().to_path_buf()]).unwrap();
        std::fs::set_permissions(&unreadable_child, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert!(report.candidates[0].usage.is_err());
        assert!(report.skipped.iter().any(|item| item.path == target));
    }
}
