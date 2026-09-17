use anyhow::{Result, bail};

pub fn run(_foreground: bool) -> Result<()> {
    // TODO(phase 2): single-instance lock on state/daemon.pid; user-private UDS / named pipe;
    // rgo_protocol::{Request,Response} over length-prefixed JSON; leases with TTL+heartbeat;
    // SQLite (WAL) for contexts/leases/pins/gc_runs; periodic rescans; free-space watchdog.
    bail!(
        "daemon arrives in Phase 2; today GC runs from `rgo gc` and the scheduled `rgo gc --auto`"
    )
}
