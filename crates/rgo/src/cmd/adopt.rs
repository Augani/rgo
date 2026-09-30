use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use rgo_core::adopt;

use super::human;

pub fn run(roots: Vec<PathBuf>, delete: bool, preview_full_clean: bool) -> Result<()> {
    if delete {
        bail!(
            "legacy target deletion is unavailable: rgo cannot safely identify Cargo intermediates without depending on undocumented build-dir layout. `rgo adopt` remains read-only"
        );
    }
    if preview_full_clean {
        ensure!(
            roots.len() == 1,
            "--preview-full-clean requires exactly one explicit project or target path"
        );
    }
    let roots = if roots.is_empty() {
        adopt::default_roots()
    } else {
        roots
    };
    let report = adopt::scan(&roots)?;
    if preview_full_clean {
        ensure!(
            report.candidates.len() == 1,
            "--preview-full-clean found {} target directories; select exactly one target path",
            report.candidates.len()
        );
    }
    println!(
        "Scanned {} root(s); found {} legacy target director{}.",
        roots.len(),
        report.candidates.len(),
        if report.candidates.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    for skipped in &report.skipped {
        println!(
            "\nSkipped: {}\n  reason: {}",
            skipped.path.display(),
            skipped.reason
        );
    }
    for candidate in &report.candidates {
        println!("\nTarget: {}", candidate.target_dir.display());
        if let Some(project) = &candidate.project_root {
            println!("  workspace: {}", project.display());
        } else {
            println!("  workspace: abandoned or manifest not found");
        }
        if let Some(reason) = &candidate.skipped_reason {
            println!("  note: {reason}");
        }
        match &candidate.usage {
            Ok(usage) if usage.physical_bytes == 0 => {
                println!("  no allocated files observed");
            }
            Ok(usage) => {
                println!(
                    "  target storage estimate: {} (includes final outputs and user files)",
                    human(usage.physical_bytes)
                );
            }
            Err(error) => {
                println!("  target storage estimate unavailable: {error}");
            }
        }
    }
    if preview_full_clean {
        preview_cargo_full_clean(&report.candidates[0])?;
    }
    println!("\nReport only. No files were removed.");
    Ok(())
}

fn preview_cargo_full_clean(candidate: &adopt::Candidate) -> Result<()> {
    let project = candidate
        .project_root
        .as_ref()
        .context("Cargo full-clean preview needs a readable workspace manifest")?;
    if let Err(error) = &candidate.usage {
        bail!("cannot preview a target with an incomplete storage scan: {error}");
    }
    let target = candidate
        .target_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", candidate.target_dir.display()))?;
    let manifest = project
        .join("Cargo.toml")
        .canonicalize()
        .with_context(|| format!("resolving workspace manifest in {}", project.display()))?;
    let target_text = target
        .to_str()
        .context("Cargo full-clean preview requires a UTF-8 target path")?;
    let build_dir = format!("build.build-dir={}", serde_json::to_string(target_text)?);
    println!("\nCargo full-clean preview for {}:", target.display());
    println!("This includes final outputs and user files; it is not a selective migration plan.");
    let status = Command::new("cargo")
        .args(["clean", "--dry-run", "--verbose", "--offline"])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .arg("--config")
        .arg(build_dir)
        .current_dir(project)
        .status()
        .context("running Cargo full-clean preview")?;
    ensure!(
        status.success(),
        "Cargo full-clean preview failed: {status}"
    );
    Ok(())
}
