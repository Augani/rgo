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
    let mut seen = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.depth() == 0 || should_descend(entry))
            .filter_map(Result::ok)
        {
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
    Ok(ScanReport { candidates })
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
    let skipped_reason = project_root.as_deref().and_then(project_override_reason);
    let mut intermediates = Vec::new();
    if skipped_reason.is_none() && !recently_locked(target) {
        for entry in WalkDir::new(target)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.depth() == 0 || should_descend(entry))
            .filter_map(Result::ok)
        {
            if entry.file_type().is_dir()
                && matches!(
                    entry.file_name().to_str(),
                    Some("deps" | "build" | "incremental" | ".fingerprint")
                )
            {
                let mut scanner = Scanner::new();
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

fn should_descend(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !entry.file_type().is_dir()
        || ((!name.starts_with('.') || name == ".fingerprint")
            && name != "node_modules"
            && name != "vendor")
}

fn find_project_root(target: &Path) -> Option<PathBuf> {
    let mut current = target.parent();
    while let Some(path) = current {
        if path.join("Cargo.toml").is_file() {
            return Some(path.to_path_buf());
        }
        current = path.parent();
    }
    None
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
                Some(".cargo-lock" | ".cargo-build-lock")
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
}
