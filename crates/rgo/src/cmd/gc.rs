use anyhow::{Result, bail};
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, human};

pub fn run(dry_run: bool, aggressive: bool) -> Result<()> {
    let e = super::env()?;
    if !daemon::ensure_running(&e.paths) {
        bail!("rgo daemon is unavailable; refusing to run coordinated GC");
    }
    let report = match ipc::request_with_timeout(
        &e.paths.socket_path(),
        Request::TriggerGc {
            dry_run,
            aggressive,
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
        println!(
            "nothing to reclaim (managed {}, target {})",
            human(report.managed_bytes),
            human(report.target_bytes)
        );
        return Ok(());
    }
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
    Ok(())
}
