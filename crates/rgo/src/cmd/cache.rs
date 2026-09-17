use anyhow::{Result, bail};
use clap::Subcommand;
use rgo_cas::Store;
use rgo_core::db::StateDb;
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, env, human};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show cache counters and physical CAS usage.
    Stats,
    /// Explain a cache key's current state and recorded outcome.
    Explain { key: String },
    /// Verify every manifest and referenced CAS object.
    Verify,
}

pub fn run(command: Command) -> Result<()> {
    let e = env()?;
    match command {
        Command::Stats => stats(&e),
        Command::Explain { key } => explain(&e, &key),
        Command::Verify => verify(&e),
    }
}

fn stats(e: &super::Env) -> Result<()> {
    let response = if daemon::ensure_running(&e.paths) {
        ipc::request_with_timeout(
            &e.paths.socket_path(),
            Request::QueryCacheStats,
            std::time::Duration::from_secs(10),
        )
        .ok()
    } else {
        None
    };
    let report = match response {
        Some(Response::CacheStats(report)) => report,
        _ => {
            let db = StateDb::open_read_only(&e.paths)?;
            db.cache_stats(
                e.cfg.cache.enabled,
                Store::new(e.paths.cas_dir(), e.paths.quarantine_dir())?.object_bytes()?,
            )?
        }
    };
    println!("Cache enabled        {}", report.enabled);
    println!("Cache manifests      {}", report.manifests);
    println!("CAS objects          {}", report.objects);
    println!("CAS bytes            {}", human(report.cas_bytes));
    println!("Cache hits           {}", report.hits);
    println!("Cache misses         {}", report.misses);
    println!("Cache bypasses       {}", report.bypasses);
    println!("Single-flight active {}", report.active_builds);
    println!("Single-flight waits  {}", report.single_flight_waiters);
    println!("Single-flight timeouts {}", report.single_flight_timeouts);
    println!("Single-flight takeovers {}", report.single_flight_takeovers);
    if let Some(error) = report.last_verify_error {
        println!("Last verify error    {error}");
    }
    Ok(())
}

fn explain(e: &super::Env, key: &str) -> Result<()> {
    let response = if daemon::ensure_running(&e.paths) {
        ipc::request_with_timeout(
            &e.paths.socket_path(),
            Request::ExplainCache { key: key.into() },
            std::time::Duration::from_secs(10),
        )
        .ok()
    } else {
        None
    };
    let report = match response {
        Some(Response::CacheExplanation(report)) => report,
        Some(Response::Error { message, .. }) => bail!("{message}"),
        _ => StateDb::open_read_only(&e.paths)?.cache_explanation(key)?,
    };
    println!("Key                 {}", report.key);
    println!("State               {}", report.state);
    if let Some(reason) = report.reason {
        println!("Reason              {reason}");
    }
    if !report.outputs.is_empty() {
        println!("Objects             {}", report.outputs.join(", "));
    }
    Ok(())
}

fn verify(e: &super::Env) -> Result<()> {
    if !daemon::ensure_running(&e.paths) {
        bail!("rgo daemon is unavailable; refusing coordinated cache verification");
    }
    match ipc::request_with_timeout(
        &e.paths.socket_path(),
        Request::VerifyCache,
        std::time::Duration::from_secs(30),
    )? {
        Response::CacheVerify(report) => {
            println!("Manifests checked    {}", report.checked_manifests);
            println!("Objects checked      {}", report.checked_objects);
            println!("Quarantined          {}", report.quarantined);
            if report.errors.is_empty() {
                println!("cache verification passed");
                Ok(())
            } else {
                for error in report.errors {
                    println!("error: {error}");
                }
                bail!("cache verification found corruption")
            }
        }
        Response::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}
