use std::time::SystemTime;

use anyhow::Result;
use rgo_core::config::volume_free_bytes;
use rgo_core::ipc;
use rgo_core::{context, gc};
use rgo_protocol::{Request, Response};

use super::{daemon, env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    if daemon::ensure_running(&e.paths) {
        if let Ok(Response::Status(status)) = ipc::request_with_timeout(
            &e.paths.socket_path(),
            Request::QueryStatus,
            std::time::Duration::from_secs(10),
        ) {
            println!(
                "Managed storage      {:>10}   ({} contexts, {} orphaned)",
                human(status.managed_bytes),
                status.contexts,
                status.orphaned_contexts
            );
            println!(
                "  incremental state  {:>10}",
                human(status.incremental_bytes)
            );
            println!(
                "Soft GC watermark    {:>10}",
                human(status.soft_watermark_bytes)
            );
            println!(
                "Hard limit           {:>10}",
                human(status.hard_limit_bytes)
            );
            println!(
                "Volume free          {:>10}   (reserve {})",
                human(status.volume_free_bytes),
                human(status.min_free_bytes)
            );
            println!(
                "Reclaimable now      {:>10}",
                human(status.reclaimable_bytes)
            );
            println!(
                "Active leases        {:>10}   pinned {}",
                status.active_leases, status.pinned_contexts
            );
            println!(
                "Last GC              {:>10}",
                human(status.last_gc_reclaimed_bytes)
            );
            if let Some(error) = status.last_gc_error {
                println!("Last GC error        {error}");
            }
            println!(
                "Cache                {}   {} hits, {} misses, {} bypasses",
                if status.cache.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.cache.hits,
                status.cache.misses,
                status.cache.bypasses
            );
            println!(
                "Physical CAS         {:>10}   ({} manifests)",
                human(status.cache.cas_bytes),
                status.cache.manifests
            );
            println!(
                "Single-flight        {} active, {} producer(s), {} waiter(s), {} timeout(s), {} takeover(s)",
                status.cache.active_builds,
                status.cache.single_flight_producers,
                status.cache.single_flight_waiters,
                status.cache.single_flight_timeouts,
                status.cache.single_flight_takeovers
            );
            println!(
                "Workspace remap      {}",
                if e.cfg.cache.remap_workspace_paths {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!(
                "Remote CAS           {}   {} hit(s), {} miss(es), queue {}",
                if status.remote.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.remote.hits,
                status.remote.misses,
                status.remote.queue_depth
            );
            println!("Daemon               running (pid {})", status.daemon_pid);
            return Ok(());
        }
    }
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
        leased: &[],
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
