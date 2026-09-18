use anyhow::{Result, bail};
use rgo_core::config::{self, Size};
use rgo_core::context;
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, human};

pub fn run(dry_run: bool, aggressive: bool, auto: bool, target: Option<String>) -> Result<()> {
    let e = super::env()?;
    if auto && !e.cfg.gc.auto {
        return Ok(());
    }
    if auto {
        let managed_bytes: u64 = context::list(&e.paths)?
            .iter()
            .map(|context| context.usage.physical_bytes)
            .sum();
        let free_bytes = config::volume_free_bytes(&e.paths.root).unwrap_or(u64::MAX);
        if managed_bytes <= e.cfg.soft_watermark && free_bytes >= e.cfg.min_free_space {
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
            "{} context(s) skipped: built within the last {} min",
            report.skipped_live, 10
        );
    }
    if report.skipped_leased > 0 {
        println!(
            "{} context(s) skipped: protected by active leases",
            report.skipped_leased
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
        return Ok(());
    }
    if let Some(requested) = target_bytes {
        println!(
            "target {}; planned {}; {} {}",
            human(requested),
            human(report.planned_bytes),
            if dry_run {
                "would reclaim"
            } else {
                "reclaimed"
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
                "would reclaim"
            } else {
                "reclaimed"
            },
            human(if dry_run {
                report.planned_bytes
            } else {
                report.reclaimed_bytes
            })
        );
    }
    Ok(())
}
