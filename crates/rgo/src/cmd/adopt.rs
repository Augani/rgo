use std::path::PathBuf;

use anyhow::Result;
use rgo_core::adopt;

use super::human;

pub fn run(roots: Vec<PathBuf>, delete: bool) -> Result<()> {
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
    for candidate in &report.candidates {
        println!("\nTarget: {}", candidate.target_dir.display());
        if let Some(project) = &candidate.project_root {
            println!("  workspace: {}", project.display());
        } else {
            println!("  workspace: abandoned or manifest not found");
        }
        if let Some(reason) = &candidate.skipped_reason {
            println!("  SKIP: {reason}");
        } else if candidate.intermediates.is_empty() {
            println!("  nothing reclaimable (final artifacts are preserved)");
        } else {
            for item in &candidate.intermediates {
                println!(
                    "  candidate: {} ({})",
                    item.path.display(),
                    human(item.usage.physical_bytes)
                );
            }
            println!("  reclaimable: {}", human(candidate.reclaimable_bytes()));
        }
    }
    if !delete {
        println!(
            "\nReport only. Re-run with `rgo adopt --delete` to remove approved intermediates."
        );
        return Ok(());
    }
    let paths = rgo_core::paths::RgoPaths::discover()?;
    paths.ensure_layout()?;
    let reclaimed = adopt::delete(&report, &paths)?;
    println!(
        "Removed {} of approved legacy intermediates.",
        human(reclaimed)
    );
    Ok(())
}
