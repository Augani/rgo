use anyhow::{Result, bail};
use rgo_core::config::{self, Size};
use rgo_core::context;
use rgo_core::ipc;
use rgo_protocol::{GcReport, Request, Response};

use super::{daemon, human};

pub fn run(dry_run: bool, aggressive: bool, auto: bool, target: Option<String>) -> Result<()> {
    let e = super::env()?;
    if auto && !e.cfg.gc.auto {
        return Ok(());
    }
    if auto {
        e.paths.require_supervised_deletion()?;
        let managed_bytes = rgo_core::size::managed_snapshot(&e.paths)?.total_bytes();
        let free_bytes = config::volume_free_bytes_checked(&e.paths.root)?;
        let age_due = rgo_core::db::StateDb::open_read_only(&e.paths)
            .and_then(|db| db.last_real_gc_at())
            .map(|at| context::unix_now().saturating_sub(at) >= 3600)
            .unwrap_or(managed_bytes > 0);
        if managed_bytes <= e.cfg.soft_watermark && free_bytes >= e.cfg.min_free_space && !age_due {
            return Ok(());
        }
    }
    let target_bytes = target
        .as_deref()
        .map(config::parse_size)
        .transpose()?
        .map(|size| match size {
            Size::Bytes(bytes) => Ok(bytes),
            Size::Auto => Err(anyhow::anyhow!(
                "--target must be a concrete byte size, not `auto`"
            )),
        })
        .transpose()?;
    if !auto && !dry_run {
        e.paths.require_supervised_deletion()?;
    }
    if !daemon::ensure_running(&e.paths) {
        bail!("rgo daemon is unavailable; refusing to run coordinated GC");
    }
    let report = match ipc::request_with_timeout(
        &e.paths.socket_path(),
        Request::TriggerGc {
            dry_run,
            aggressive,
            auto,
            target_bytes,
        },
        std::time::Duration::from_secs(30),
    )? {
        Response::Gc(report) => report,
        Response::Error { code, message } => bail!("GC failed ({code}): {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    };
    if report.skipped_live > 0 {
        println!(
            "{} context(s) skipped: recently changed or held Cargo profile lock",
            report.skipped_live
        );
    }
    if report.skipped_leased > 0 {
        println!(
            "{} context(s) skipped: protected by active leases",
            report.skipped_leased
        );
    }
    if report.skipped_pinned > 0 {
        println!("{} context(s) skipped: pinned", report.skipped_pinned);
    }
    if report.skipped_unavailable > 0 {
        println!(
            "{} context(s) skipped: workspace attribution or availability unverified",
            report.skipped_unavailable
        );
    }
    if report.skipped_execution_actions > 0 {
        println!(
            "{} planned action(s) could not be removed (estimated {}); first: {}",
            report.skipped_execution_actions,
            human(report.skipped_execution_bytes),
            report
                .first_execution_skip
                .as_deref()
                .unwrap_or("unknown error")
        );
    }
    if report.actions.is_empty() {
        if auto {
            return Ok(());
        }
        println!(
            "nothing to reclaim (managed {}, target {})",
            human(report.managed_bytes),
            human(report.target_bytes)
        );
        print_remaining(&report);
        return Ok(());
    }
    if let Some(requested) = target_bytes {
        println!(
            "target {}; planned estimate {}; {} {}",
            human(requested),
            human(report.planned_bytes),
            if dry_run {
                "would unlink approximately"
            } else {
                "unlinked approximately"
            },
            human(if dry_run {
                report.planned_bytes
            } else {
                report.reclaimed_bytes
            })
        );
    } else {
        println!(
            "{} {}",
            if dry_run {
                "would unlink approximately"
            } else {
                "unlinked approximately"
            },
            human(if dry_run {
                report.planned_bytes
            } else {
                report.reclaimed_bytes
            })
        );
    }
    print_remaining(&report);
    Ok(())
}

fn print_remaining(report: &GcReport) {
    let Some(remaining) = report.remaining_managed_bytes else {
        return;
    };
    let unmet = remaining.saturating_sub(report.target_bytes);
    println!("Managed after GC     {:>10}", human(remaining));
    if let (Some(builds), Some(cas), Some(auxiliary)) = (
        report.remaining_build_bytes,
        report.remaining_cas_bytes,
        report.remaining_auxiliary_bytes,
    ) {
        println!("  build contexts     {:>10}", human(builds));
        println!("  compiler cache     {:>10}", human(cas));
        println!("  other rgo state    {:>10}", human(auxiliary));
    }
    if unmet > 0 {
        println!("Target still unmet   {:>10}", human(unmet));
        if report.protected_context_bytes > 0 {
            println!(
                "Protected builds     {:>10}",
                human(report.protected_context_bytes)
            );
        }
        if report.cas_eviction_deferred_bytes > 0 {
            println!(
                "Cache eviction deferred {:>10}   (active cache work)",
                human(report.cas_eviction_deferred_bytes)
            );
        }
        println!("Some remaining data was protected, ineligible, or could not be removed.");
    }
    if let Some(free) = report.volume_free_after_bytes {
        if let Some(before) = report.volume_free_before_bytes {
            println!(
                "Volume free observed {} before, {} after (other disk activity can affect this)",
                human(before),
                human(free)
            );
        } else {
            println!("Volume free after    {:>10}", human(free));
        }
        let deficit = report.min_free_bytes.saturating_sub(free);
        if deficit > 0 {
            println!("Free-space reserve short by {}", human(deficit));
        }
    }
}
