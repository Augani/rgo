use std::time::SystemTime;

use anyhow::Result;
use rgo_core::context;

use super::{env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    let now = SystemTime::now();
    println!(
        "{:<18} {:>10} {:>10} {:>10}  WORKSPACE",
        "ID", "SIZE", "INCR", "IDLE"
    );
    for c in context::list(&e.paths)? {
        let ws = match &c.sidecar {
            Some(s) if c.is_orphan() => format!("{} (orphan)", s.workspace_root),
            Some(s) => s.workspace_root.clone(),
            None => "? (unattributed)".to_owned(),
        };
        let idle = humantime::format_duration(std::time::Duration::from_secs(
            c.idle_for(now).as_secs() / 60 * 60,
        ));
        println!(
            "{:<18} {:>10} {:>10} {:>10}  {}",
            c.id(),
            human(c.usage.physical_bytes),
            human(c.incremental_usage.physical_bytes),
            idle,
            ws
        );
    }
    Ok(())
}
