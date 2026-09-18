//! Discovery and safe removal of legacy Cargo target intermediates.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use fs4::fs_std::FileExt;
use walkdir::{DirEntry, WalkDir};

use crate::cargo_config;
use crate::gc;
use crate::paths::RgoPaths;
use crate::size::{Scanner, Usage};

const DEFAULT_ROOTS: [&str; 4] = ["Projects", "src", "code", "dev"];
const ADOPT_LIVE_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct AdoptPath {
    pub path: PathBuf,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub target_dir: PathBuf,
    pub project_root: Option<PathBuf>,
    pub intermediates: Vec<AdoptPath>,
    pub skipped_reason: Option<String>,
}

impl Candidate {
    pub fn reclaimable_bytes(&self) -> u64 {
        self.intermediates
            .iter()
            .map(|item| item.usage.physical_bytes)
            .sum()
    }

    pub fn eligible(&self) -> bool {
        self.skipped_reason.is_none() && !self.intermediates.is_empty()
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
        for item in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.depth() == 0 || should_descend(entry))
        {
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
            if entry.file_type().is_dir()
                && entry.file_name() == "target"
                && !seen
                    .iter()
                    .any(|parent: &PathBuf| entry.path().starts_with(parent))
                && seen.insert(entry.path().to_path_buf())
            {
                candidates.push(scan_target(entry.path())?);
            }
        }
    }
    candidates.sort_by(|left, right| left.target_dir.cmp(&right.target_dir));
    Ok(ScanReport {
        candidates,
        skipped,
    })
}

pub fn delete(report: &ScanReport, paths: &RgoPaths) -> Result<u64> {
    let mut reclaimed = 0;
    for candidate in &report.candidates {
        if !candidate.eligible() || recently_locked(&candidate.target_dir) {
            continue;
        }
        for item in &candidate.intermediates {
            if item.path.exists() {
                match gc::remove_atomically(paths, &item.path) {
                    Ok(()) => reclaimed += item.usage.physical_bytes,
                    Err(error) => tracing::warn!(
                        path = %item.path.display(),
                        %error,
                        "skipping legacy intermediate"
                    ),
                }
            }
        }
    }
    Ok(reclaimed)
}

fn scan_target(target: &Path) -> Result<Candidate> {
    let project_root = find_project_root(target);
    let skipped_reason = project_root
        .as_deref()
        .and_then(|root| project_override_reason_in_workspace(target, root));
    let mut intermediates = Vec::new();
    let skipped_reason = skipped_reason.or_else(|| {
        target
            .read_dir()
            .is_err()
            .then(|| "target directory is unreadable".to_owned())
    });
    if skipped_reason.is_none() && !recently_locked(target) {
        let mut scanner = Scanner::new();
        for entry in WalkDir::new(target)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.depth() == 0 || should_descend(entry))
            .filter_map(Result::ok)
        {
            if entry.file_type().is_dir() && is_documented_intermediate(target, entry.path()) {
                let usage = scanner.measure(entry.path());
                intermediates.push(AdoptPath {
                    path: entry.path().to_path_buf(),
                    usage,
                });
            }
        }
    }
    let skipped_reason = skipped_reason
        .or_else(|| recently_locked(target).then(|| "active Cargo lock detected".into()));
    Ok(Candidate {
        target_dir: target.to_path_buf(),
        project_root,
        intermediates,
        skipped_reason,
    })
}

fn is_documented_intermediate(target: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(target) else {
        return false;
    };
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    let Some(name) = components.last().map(|value| value.as_ref()) else {
        return false;
    };
    if !matches!(name, "deps" | "build" | "incremental" | ".fingerprint") {
        return false;
    }
    match components.as_slice() {
        // Host profile: target/debug/deps, target/release/build, and custom profiles.
        [profile, _] if !profile.is_empty() => true,
        // Cross-target profile: target/<triple>/debug/deps. Target triples contain a dash;
        // this prevents a user-created target/debug/custom/deps tree from being selected.
        [triple, profile, _] if triple.contains('-') && !profile.is_empty() => true,
        _ => false,
    }
}

fn should_descend(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !entry.file_type().is_dir()
        || ((!name.starts_with('.') || name == ".fingerprint")
            && name != "node_modules"
            && name != "vendor")
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

fn recently_locked(target: &Path) -> bool {
    let now = SystemTime::now();
    WalkDir::new(target)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            matches!(
                entry.file_name().to_str(),
                Some(".cargo-lock" | ".cargo-build-lock" | ".cargo-artifact-lock")
            )
        })
        .any(|entry| {
            let lock_held = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())
                .ok()
                .map(|file| match file.try_lock_exclusive() {
                    Ok(true) => false,
                    Ok(false) | Err(_) => true,
                })
                .unwrap_or(true);
            if lock_held {
                return true;
            }
            std::fs::metadata(entry.path())
                .and_then(|metadata| metadata.modified())
                .map(|modified| {
                    now.duration_since(modified).unwrap_or_default() < ADOPT_LIVE_WINDOW
                })
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_documented_intermediates() {
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
        assert!(
            candidate
                .intermediates
                .iter()
                .any(|item| item.path.ends_with("deps"))
        );
        assert!(
            !candidate
                .intermediates
                .iter()
                .any(|item| item.path.ends_with("examples"))
        );
        assert!(
            !candidate
                .intermediates
                .iter()
                .any(|item| item.path.ends_with("custom/user/deps"))
        );
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
    fn scans_cross_target_profiles_without_following_symlinks() {
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
        assert!(
            candidate
                .intermediates
                .iter()
                .any(|item| item.path.ends_with("aarch64-unknown-linux-gnu/custom/deps"))
        );
        assert!(
            !candidate
                .intermediates
                .iter()
                .any(|item| item.path.ends_with("examples"))
        );
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
    fn adopt_accounting_deduplicates_hardlinked_intermediates() {
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
        let files = candidate
            .intermediates
            .iter()
            .map(|item| item.usage.files)
            .sum::<u64>();
        assert_eq!(files, 1);
        assert!(candidate.reclaimable_bytes() > 0);
    }

    #[test]
    fn active_lock_detected_before_delete() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let target = project.join("target/debug");
        std::fs::create_dir_all(target.join("deps")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(target.join("deps/libx.rlib"), b"intermediate").unwrap();
        let report = scan(&[root.path().to_path_buf()]).unwrap();
        let lock_path = target.join(".cargo-build-lock");
        std::fs::write(&lock_path, b"live").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(lock.lock_exclusive().is_ok());
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        assert_eq!(delete(&report, &paths).unwrap(), 0);
        assert!(target.join("deps/libx.rlib").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_scan_paths_are_reported_instead_of_deleted() {
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
    }
}
