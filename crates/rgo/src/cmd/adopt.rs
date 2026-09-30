use std::path::PathBuf;

use anyhow::{Result, bail};
use rgo_core::adopt;

use super::human;

pub fn run(roots: Vec<PathBuf>, delete: bool) -> Result<()> {
    if delete {
        bail!(
            "legacy target deletion is unavailable: rgo cannot safely identify Cargo intermediates without depending on undocumented build-dir layout. `rgo adopt` remains read-only"
        );
    }
    let roots = if roots.is_empty() {
        adopt::default_roots()
    } else {
        roots
    };
    let report = adopt::scan(&roots)?;
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
    println!("\nReport only. No files were removed.");
    Ok(())
}
