use std::time::SystemTime;

use anyhow::Result;
use rgo_core::context;
use rgo_core::db::StateDb;

use super::{env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    let contexts = context::list(&e.paths)?;
    let (pinned, leased) = match StateDb::open_read_only(&e.paths) {
        Ok(db) => (db.pinned_paths()?, db.protected_paths(&contexts)?),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let now = SystemTime::now();
    println!(
        "{:<18} {:>10} {:>10} {:>10}  {:<3} {:<3} WORKSPACE",
        "ID", "SIZE", "INCR", "IDLE", "PIN", "USE"
    );
    for c in contexts {
        let ws = match &c.sidecar {
            Some(s) if c.is_orphan() => format!("{} (orphan)", s.workspace_root),
            Some(s) => s.workspace_root.clone(),
            None => "? (unattributed)".to_owned(),
        };
        let idle = humantime::format_duration(std::time::Duration::from_secs(
            c.idle_for(now).as_secs() / 60 * 60,
        ));
        println!(
            "{:<18} {:>10} {:>10} {:>10}  {:<3} {:<3} {}",
            c.id(),
            human(c.usage.physical_bytes),
            human(c.incremental_usage.physical_bytes),
            idle,
            if pinned.iter().any(|p| same_path(p, &c.dir)) {
                "PIN"
            } else {
                ""
            },
            if leased.iter().any(|p| same_path(p, &c.dir)) {
                "LIVE"
            } else {
                ""
            },
            ws
        );
    }
    Ok(())
}

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf())
        == std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf())
}
