use std::time::SystemTime;

use anyhow::Result;
use rgo_core::{context, gc};

use super::{env, human};

pub fn run(dry_run: bool, aggressive: bool) -> Result<()> {
    let e = env()?;
    let contexts = context::list(&e.paths)?;
    let plan = gc::plan(&gc::Inputs {
        paths: &e.paths,
        cfg: &e.cfg,
        contexts: &contexts,
        pinned: &[], // TODO(phase 2): pins from SQLite
        now: SystemTime::now(),
        aggressive,
    });
    if plan.skipped_live > 0 {
        println!(
            "{} context(s) skipped: built within the last {} min",
            plan.skipped_live,
            gc::LIVE_WINDOW.as_secs() / 60
        );
    }
    if plan.actions.is_empty() {
        println!(
            "nothing to reclaim (managed {}, target {})",
            human(plan.managed_bytes),
            human(plan.target_bytes)
        );
        return Ok(());
    }
    let reclaimed = gc::execute(&e.paths, &plan, dry_run)?;
    println!(
        "{} {}",
        if dry_run {
            "would reclaim"
        } else {
            "reclaimed"
        },
        human(if dry_run {
            plan.reclaim_bytes()
        } else {
            reclaimed
        })
    );
    Ok(())
}
