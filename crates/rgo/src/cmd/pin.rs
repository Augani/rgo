use anyhow::{Result, bail};

pub fn run(_id: &str, _pin: bool) -> Result<()> {
    // TODO(phase 2): pins live in SQLite `pins` table, owned by the daemon.
    bail!("pin/unpin arrive with the daemon (Phase 2)")
}
