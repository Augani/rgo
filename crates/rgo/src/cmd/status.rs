use std::time::SystemTime;

use anyhow::Result;
use rgo_core::config::volume_free_bytes;
use rgo_core::{context, gc};

use super::{env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    let contexts = context::list(&e.paths)?;
    let managed: u64 = contexts.iter().map(|c| c.usage.physical_bytes).sum();
    let incremental: u64 = contexts
        .iter()
        .map(|c| c.incremental_usage.physical_bytes)
        .sum();
    let orphans = contexts.iter().filter(|c| c.is_orphan()).count();
    let plan = gc::plan(&gc::Inputs {
        paths: &e.paths,
        cfg: &e.cfg,
        contexts: &contexts,
        pinned: &[],
        now: SystemTime::now(),
        aggressive: true,
    });
    let free = volume_free_bytes(&e.paths.root).unwrap_or(0);

    println!(
        "Managed storage      {:>10}   ({} contexts, {} orphaned)",
        human(managed),
        contexts.len(),
        orphans
    );
    println!("  incremental state  {:>10}", human(incremental));
    println!("Soft GC watermark    {:>10}", human(e.cfg.soft_watermark));
    println!("Hard limit           {:>10}", human(e.cfg.max_size));
    println!(
        "Volume free          {:>10}   (reserve {})",
        human(free),
        human(e.cfg.min_free_space)
    );
    println!(
        "Reclaimable now      {:>10}   (`rgo gc --aggressive`)",
        human(plan.reclaim_bytes())
    );
    Ok(())
}
