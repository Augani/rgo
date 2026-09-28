use anyhow::{Context, Result, bail};
use rgo_core::config::volume_free_bytes;
use rgo_core::context;
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    if daemon::ensure_running(&e.paths) {
        let status = match ipc::request_with_timeout(
            &e.paths.socket_path(),
            Request::QueryStatus,
            std::time::Duration::from_secs(10),
        )
        .context("requesting daemon status")?
        {
            Response::Status(status) => status,
            Response::Error { code, message } => {
                bail!("daemon status failed ({code}): {message}")
            }
            other => bail!("unexpected daemon status response: {other:?}"),
        };
        println!(
            "Managed storage      {:>10}   ({} contexts, {} orphaned)",
            human(status.managed_bytes),
            status.contexts,
            status.orphaned_contexts
        );
        if let Some(build_bytes) = status.build_bytes {
            println!("  build contexts     {:>10}", human(build_bytes));
        }
        println!(
            "  incremental state  {:>10}",
            human(status.incremental_bytes)
        );
        if let Some(cas_bytes) = status.cas_bytes {
            println!("  compiler cache     {:>10}", human(cas_bytes));
        }
        println!("  other rgo state    {:>10}", human(status.auxiliary_bytes));
        println!(
            "Soft GC watermark    {:>10}",
            human(status.soft_watermark_bytes)
        );
        println!(
            "Configured budget    {:>10}",
            human(status.hard_limit_bytes)
        );
        println!(
            "Automatic GC         {}",
            if e.cfg.gc.auto {
                "enabled (experimental lifecycle safety)"
            } else {
                "disabled (lifecycle safety gate open)"
            }
        );
        println!(
            "Volume free          {:>10}   (reserve {})",
            status
                .volume_free_observed_bytes
                .or_else(|| (status.volume_free_bytes > 0).then_some(status.volume_free_bytes))
                .map(human)
                .unwrap_or_else(|| "unknown".to_owned()),
            human(status.min_free_bytes)
        );
        if let Some(eligible) = status.eligible_managed_bytes {
            println!(
                "Potentially eligible {:>10}   (allocated-byte estimate; aggressive GC)",
                human(eligible)
            );
        } else {
            println!(
                "Build/temp eligible {:>10}   (CAS not included)",
                human(status.reclaimable_bytes)
            );
        }
        println!(
            "Protected builds     {:>10}",
            human(status.protected_context_bytes)
        );
        let budget_excess = status.managed_bytes.saturating_sub(status.hard_limit_bytes);
        if budget_excess > 0 {
            println!("Over budget          {:>10}", human(budget_excess));
            if let Some(unmet) = status.unmet_budget_bytes.filter(|unmet| *unmet > 0) {
                println!("Budget unmet est.    {:>10}", human(unmet));
                if let Some(reason) = status.unmet_budget_reason {
                    println!("Reason               {reason}");
                }
            }
        }
        println!(
            "Active leases        {:>10}   pinned {}",
            status.active_leases, status.pinned_contexts
        );
        println!(
            "Last GC estimate     {:>10}",
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
        if status.cache.observations_incomplete {
            println!(
                "Cache observations   incomplete (events dropped, malformed, or still draining)"
            );
        }
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
        println!(
            "Daemon               running (pid {}, protocol {})",
            status.daemon_pid,
            if status.protocol_compatible {
                "compatible"
            } else {
                "incompatible"
            }
        );
        return Ok(());
    }
    println!("Daemon               unavailable (coordination disabled; filesystem fallback)");
    let contexts = context::list(&e.paths)?;
    let cas_bytes = rgo_core::size::Scanner::new()
        .measure_optional(&e.paths.cas_dir())?
        .physical_bytes;
    let auxiliary_bytes = rgo_core::size::auxiliary_usage(&e.paths)?.physical_bytes;
    let build_bytes: u64 = contexts.iter().map(|c| c.usage.physical_bytes).sum();
    let managed: u64 = build_bytes
        .saturating_add(cas_bytes)
        .saturating_add(auxiliary_bytes);
    let incremental: u64 = contexts
        .iter()
        .map(|c| c.incremental_usage.physical_bytes)
        .sum();
    let orphans = contexts.iter().filter(|c| c.is_orphan()).count();
    let free = volume_free_bytes(&e.paths.root);
    println!(
        "Managed storage      {:>10}   ({} contexts, {} orphaned)",
        human(managed),
        contexts.len(),
        orphans
    );
    println!("  build contexts     {:>10}", human(build_bytes));
    println!("  incremental state  {:>10}", human(incremental));
    println!("  compiler cache     {:>10}", human(cas_bytes));
    if e.paths
        .state_dir()
        .join(rgo_protocol::CACHE_EVENT_LOG_TRUNCATED)
        .is_file()
    {
        println!("Cache observations   incomplete (events dropped, malformed, or still draining)");
    }
    println!("  other rgo state    {:>10}", human(auxiliary_bytes));
    println!("Soft GC watermark    {:>10}", human(e.cfg.soft_watermark));
    println!("Configured budget    {:>10}", human(e.cfg.max_size));
    println!(
        "Automatic GC         {}",
        if e.cfg.gc.auto {
            "enabled (experimental lifecycle safety)"
        } else {
            "disabled (lifecycle safety gate open)"
        }
    );
    println!(
        "Volume free          {:>10}   (reserve {})",
        free.map(human).unwrap_or_else(|| "unknown".to_owned()),
        human(e.cfg.min_free_space)
    );
    println!("Potentially eligible    unknown   (daemon unavailable)");
    println!("Protected builds        unknown   (daemon unavailable)");
    let budget_excess = managed.saturating_sub(e.cfg.max_size);
    if budget_excess > 0 {
        println!("Over budget          {:>10}", human(budget_excess));
    }
    Ok(())
}
