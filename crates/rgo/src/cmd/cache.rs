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
    /// Inspect the optional authenticated remote CAS.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Show remote endpoint health and transfer counters.
    Status,
    /// Perform an authenticated, read-only connectivity check.
    Probe,
}

pub fn run(command: Command) -> Result<()> {
    let e = env()?;
    match command {
        Command::Stats => stats(&e),
        Command::Explain { key } => explain(&e, &key),
        Command::Verify => verify(&e),
        Command::Remote { command } => remote(&e, command),
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
            let mut report = db.cache_stats(
                e.cfg.cache.enabled,
                Store::new(e.paths.cas_dir(), e.paths.quarantine_dir())?.object_bytes()?,
            )?;
            report.observations_incomplete = e
                .paths
                .state_dir()
                .join(rgo_protocol::CACHE_EVENT_LOG_TRUNCATED)
                .is_file();
            report.remote = db.remote_status(
                e.cfg.cache.enabled && e.cfg.remote.enabled,
                None,
                (!e.cfg.remote.namespace.is_empty()).then_some(e.cfg.remote.namespace.as_str()),
            )?;
            report
        }
    };
    println!("Cache enabled        {}", report.enabled);
    println!("Cache manifests      {}", report.manifests);
    println!("CAS objects          {}", report.objects);
    println!("CAS bytes            {}", human(report.cas_bytes));
    println!("Cache hits           {}", report.hits);
    println!("Cache misses         {}", report.misses);
    println!("Cache bypasses       {}", report.bypasses);
    if report.observations_incomplete {
        println!("Cache observations   incomplete (events dropped, malformed, or still draining)");
    }
    println!("Single-flight active {}", report.active_builds);
    println!("Single-flight waits  {}", report.single_flight_waiters);
    println!("Single-flight timeouts {}", report.single_flight_timeouts);
    println!("Single-flight takeovers {}", report.single_flight_takeovers);
    println!("Remote enabled       {}", report.remote.enabled);
    println!("Remote healthy       {}", report.remote.healthy);
    println!("Remote queue         {}", report.remote.queue_depth);
    println!("Remote hits          {}", report.remote.hits);
    println!("Remote misses        {}", report.remote.misses);
    println!("Remote uploads       {}", report.remote.uploads);
    println!("Remote downloads     {}", report.remote.downloads);
    println!(
        "Remote bytes up/down {}/{}",
        human(report.remote.upload_bytes),
        human(report.remote.download_bytes)
    );
    if let Some(error) = report.last_verify_error {
        println!("Last verify error    {error}");
    }
    Ok(())
}

fn remote(e: &super::Env, command: RemoteCommand) -> Result<()> {
    match command {
        RemoteCommand::Status => {
            let response = if daemon::ensure_running(&e.paths) {
                ipc::request_with_timeout(
                    &e.paths.socket_path(),
                    Request::QueryRemoteStatus,
                    std::time::Duration::from_secs(10),
                )
                .ok()
            } else {
                None
            };
            let report = match response {
                Some(Response::RemoteStatus(report)) => report,
                _ => StateDb::open_read_only(&e.paths)?.remote_status(
                    e.cfg.cache.enabled && e.cfg.remote.enabled,
                    None,
                    (!e.cfg.remote.namespace.is_empty()).then_some(e.cfg.remote.namespace.as_str()),
                )?,
            };
            println!("Remote enabled       {}", report.enabled);
            println!("Remote configured    {}", report.configured);
            println!("Remote healthy       {}", report.healthy);
            if let Some(endpoint) = report.endpoint {
                println!("Remote endpoint      {endpoint}");
            }
            if let Some(namespace) = report.namespace {
                println!("Remote namespace     {namespace}");
            }
            println!("Remote queue         {}", report.queue_depth);
            println!("Remote hits/misses   {}/{}", report.hits, report.misses);
            println!(
                "Remote bytes up/down {}/{}",
                human(report.upload_bytes),
                human(report.download_bytes)
            );
            if let Some(error) = report.last_error {
                println!("Remote last error    {error}");
            }
            Ok(())
        }
        RemoteCommand::Probe => {
            if !daemon::ensure_running(&e.paths) {
                bail!("rgo daemon is unavailable; refusing a remote probe")
            }
            match ipc::request_with_timeout(
                &e.paths.socket_path(),
                Request::ProbeRemote,
                std::time::Duration::from_secs(10),
            )? {
                Response::RemoteProbe(report) if report.ok => {
                    println!("remote probe succeeded: {}", report.message);
                    Ok(())
                }
                Response::RemoteProbe(report) => bail!("remote probe failed: {}", report.message),
                Response::Error { message, .. } => bail!("{message}"),
                other => bail!("unexpected daemon response: {other:?}"),
            }
        }
    }
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
