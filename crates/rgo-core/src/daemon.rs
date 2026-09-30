//! The local coordination daemon. It owns state-changing metadata operations and serializes
//! lease admission with GC. Native Cargo builds may start before a wrapper lease exists;
//! complete deletion safety still requires supervised Cargo-session exclusion.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rgo_protocol::{
    CACHE_EVENT_LOG_FILE, CACHE_EVENT_LOG_LOCK, CACHE_EVENT_LOG_MAX_BYTES,
    CACHE_EVENT_LOG_TRUNCATED, CacheManifest as WireManifest, CacheObject as WireObject,
    CacheOutput as WireOutput, GcAction, GcReport, PROTOCOL_VERSION, Request, Response,
    StatusReport,
};

use crate::config::{Resolved, volume_free_bytes, volume_free_bytes_checked};
use crate::context;
use crate::db::{CacheBuildDecision, RemoteFetchDecision, StateDb};
use crate::gc::{self, Inputs};
use crate::ipc::{self, Connection, Listener};
use crate::paths::RgoPaths;
use rgo_cas::{MANIFEST_VERSION, Manifest, ManifestOutput, ObjectRef, Store};
use rgo_remote::{Client as RemoteClient, Config as RemoteConfig, Fetch as RemoteFetch};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_SCAN_INTERVAL: Duration = Duration::from_secs(120);
const AGE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);
const MAX_AUTO_SCAN_ENTRIES_PER_PASS: usize = 8192;
const MAX_EVENT_DRAINS_PER_PASS: usize = 4;
const MAX_EVENT_SCAN_ENTRIES_PER_PASS: usize = 256;
const MAX_EVENT_BATCH_PRUNE_PER_PASS: usize = 64;
const MAX_PIN_SCAN_ENTRIES_PER_PASS: usize = 32;
/// Bound on concurrent client connections; excess connections are refused so a flood
/// of stalled or malformed clients cannot exhaust daemon threads or file descriptors.
/// Clients see a dropped connection and fall back to ordinary cargo behavior.
const MAX_CONNECTIONS: usize = 64;

/// How often the daemon runs maintenance (lease expiry, reconciliation, automatic
/// GC, remote-job dispatch). `RGO_DAEMON_POLL_SECS` overrides for tests/operations.
fn poll_interval() -> Duration {
    std::env::var("RGO_DAEMON_POLL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (1..=3600).contains(v))
        .map(Duration::from_secs)
        .unwrap_or(POLL_INTERVAL)
}

fn auto_scan_interval() -> Duration {
    if std::env::var_os("RGO_DAEMON_POLL_SECS").is_some() {
        poll_interval()
    } else {
        AUTO_SCAN_INTERVAL
    }
}

struct ConnectionPermit {
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl ConnectionPermit {
    fn try_acquire(active: &Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let mut count = active.load(Ordering::Acquire);
        loop {
            if count >= MAX_CONNECTIONS {
                return None;
            }
            match active.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        active: active.clone(),
                    });
                }
                Err(actual) => count = actual,
            }
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[derive(Clone)]
struct State {
    paths: RgoPaths,
    cfg: Resolved,
    db: Arc<Mutex<StateDb>>,
    cas: Store,
    operation_lock: Arc<Mutex<()>>,
    pin_pruner: Arc<Mutex<context::PinPruneScanner>>,
    trigger_scan: Arc<Mutex<crate::size::TriggerScan>>,
    event_drainer: Arc<Mutex<CacheEventDrainScanner>>,
    batch_prune_cursor: Arc<Mutex<i64>>,
    pid: u32,
    remote: Option<RemoteClient>,
    remote_error: Arc<Mutex<Option<String>>>,
    connection_count: Arc<std::sync::atomic::AtomicUsize>,
    shutting_down: Arc<AtomicBool>,
}

struct InstanceLock {
    _file: File,
}

fn configured_remote(
    cfg: &Resolved,
    remote_error: &Arc<Mutex<Option<String>>>,
) -> Option<RemoteClient> {
    if !cfg.cache.enabled || !cfg.remote.enabled {
        return None;
    }
    let token = match std::env::var(&cfg.remote.token_env) {
        Ok(token) if !token.is_empty() => token,
        Ok(_) => {
            *remote_error.lock().unwrap() = Some("remote token is empty".into());
            return None;
        }
        Err(_) => {
            *remote_error.lock().unwrap() = Some(format!(
                "remote token environment variable {} is not set",
                cfg.remote.token_env
            ));
            return None;
        }
    };
    let config = RemoteConfig {
        endpoint: cfg.remote.endpoint.clone(),
        namespace: cfg.remote.namespace.clone(),
        token,
        timeout: cfg.remote.timeout,
        max_object_size: match cfg.remote.max_object_size {
            crate::config::Size::Bytes(value) => value,
            crate::config::Size::Auto => 2 * (1 << 30),
        },
        allow_insecure_loopback: cfg.remote.allow_insecure_loopback,
    };
    match RemoteClient::new(config) {
        Ok(client) => Some(client),
        Err(error) => {
            *remote_error.lock().unwrap() = Some(error.to_string());
            None
        }
    }
}

pub fn run(paths: RgoPaths, cfg: Resolved) -> Result<()> {
    paths.ensure_layout()?;
    if cfg.gc.auto {
        paths.require_supervised_deletion()?;
    }
    restrict_state_permissions(&paths)?;
    let lock_path = paths.state_dir().join("daemon.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if !file.try_lock_exclusive()? {
        bail!("rgo daemon is already running");
    }
    // Keep this guard alive for the daemon lifetime. StateDb::open may move a
    // confirmed corrupt WAL aside during recovery; no other rgo daemon writer
    // can be live when that recovery path runs.
    let _instance_lock = InstanceLock { _file: file };
    std::fs::write(
        paths.state_dir().join("daemon.pid"),
        format!("{}\n", std::process::id()),
    )?;

    let cas = Store::new(paths.cas_dir(), paths.quarantine_dir())?;
    let mut db = StateDb::open(&paths)?;
    db.recover_cache_builds()?;
    db.recover_remote_jobs()?;
    // Explicit status/GC requests reconcile contexts before using them. In
    // default manual mode there is no destructive background work, so avoid
    // delaying daemon startup with a full build-tree walk.
    if cfg.gc.auto {
        db.reconcile(&paths)?;
    } else {
        // Keep the upgrade recovery contract for legacy in-context pins
        // without measuring every file beneath each build directory.
        for dir in paths.checked_managed_build_dirs()? {
            if context::is_pinned_dir(&dir) {
                context::migrate_legacy_pin(&paths, &dir)?;
            }
        }
    }
    if cfg.gc.auto || cfg.cache.enabled {
        reconcile_cache(&mut db, &cas)?;
    }
    let mut event_drainer = CacheEventDrainScanner::default();
    drain_cache_events(&paths, &db, &mut event_drainer)?;
    let mut batch_prune_cursor = 0;
    db.prune_missing_cache_event_batches(
        &paths.state_dir(),
        &mut batch_prune_cursor,
        MAX_EVENT_BATCH_PRUNE_PER_PASS,
    )?;
    db.prune_operational_history()?;
    match volume_free_bytes_checked(&paths.root)
        .and_then(|free| db.migrate_legacy_auto_vacuum(&paths.db_file(), free))
    {
        Ok(true) => {
            batch_prune_cursor = 0;
            tracing::info!("converted legacy SQLite metadata for incremental reclamation");
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "deferred legacy SQLite metadata conversion"),
    }
    db.reclaim_unused_pages()?;
    let remote_error = Arc::new(Mutex::new(None));
    let remote = configured_remote(&cfg, &remote_error);
    if remote.is_some() {
        let fingerprint =
            blake3::hash(format!("{}:{}", cfg.remote.namespace, cfg.remote.endpoint).as_bytes())
                .to_hex()
                .to_string();
        db.set_remote_config(&cfg.remote.endpoint, &cfg.remote.namespace, &fingerprint)?;
    }
    let db = Arc::new(Mutex::new(db));
    let listener = Listener::bind(&paths.socket_path())?;
    let state = State {
        paths: paths.clone(),
        cfg,
        db,
        cas,
        operation_lock: Arc::new(Mutex::new(())),
        pin_pruner: Arc::new(Mutex::new(context::PinPruneScanner::default())),
        trigger_scan: Arc::new(Mutex::new(crate::size::TriggerScan::default())),
        event_drainer: Arc::new(Mutex::new(event_drainer)),
        batch_prune_cursor: Arc::new(Mutex::new(batch_prune_cursor)),
        pid: std::process::id(),
        remote,
        remote_error,
        connection_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        shutting_down: Arc::new(AtomicBool::new(false)),
    };

    tracing::info!(socket = %paths.socket_path().display(), "rgo daemon listening");
    let maintenance_state = state.clone();
    let maintenance_thread = thread::spawn(move || {
        loop {
            thread::park_timeout(poll_interval());
            if maintenance_state.shutting_down.load(Ordering::Acquire) {
                break;
            }
            if let Err(error) = maintenance(&maintenance_state) {
                tracing::warn!(%error, "daemon maintenance failed");
            }
        }
    });
    loop {
        match listener.accept() {
            Ok(connection) => {
                if state.shutting_down.load(Ordering::Acquire) {
                    break;
                }
                let state = state.clone();
                let Some(permit) = ConnectionPermit::try_acquire(&state.connection_count) else {
                    continue;
                };
                thread::spawn(move || {
                    let _permit = permit;
                    if let Err(error) = serve_connection(connection, &state) {
                        tracing::warn!(error = ?error, "daemon client disconnected with error");
                    }
                });
            }
            Err(error) => return Err(error).context("accepting daemon client"),
        }
    }
    while state.connection_count.load(Ordering::Acquire) > 0 {
        thread::sleep(Duration::from_millis(25));
    }
    maintenance_thread.thread().unpark();
    maintenance_thread
        .join()
        .map_err(|_| anyhow::anyhow!("daemon maintenance thread panicked during shutdown"))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_state_permissions(paths: &RgoPaths) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(paths.state_dir())?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(paths.state_dir(), permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_state_permissions(_paths: &RgoPaths) -> Result<()> {
    Ok(())
}

fn serve_connection(mut connection: Connection, state: &State) -> Result<()> {
    connection.set_timeout(Duration::from_secs(2))?;
    let hello: Request = ipc::read_message(&mut connection)?;
    match hello {
        Request::Hello { version, .. } if version == PROTOCOL_VERSION => {
            ipc::write_message(
                &mut connection,
                &Response::Hello {
                    version: PROTOCOL_VERSION,
                },
            )?;
        }
        Request::Hello { version, .. } => {
            ipc::write_message(
                &mut connection,
                &Response::Error {
                    code: "protocol_mismatch".into(),
                    message: format!("server={PROTOCOL_VERSION}, client={version}"),
                },
            )?;
            return Ok(());
        }
        _ => {
            ipc::write_message(
                &mut connection,
                &Response::Error {
                    code: "handshake_required".into(),
                    message: "Hello must be the first request".into(),
                },
            )?;
            return Ok(());
        }
    }

    let request: Request = ipc::read_message(&mut connection)?;
    let shutdown = matches!(request, Request::Shutdown);
    let response = handle_request(state, request);
    let written = ipc::write_message(&mut connection, &response);
    if shutdown && matches!(response, Response::Ok) {
        // Wake the blocking accept loop after the acknowledgement is sent.
        // The connection is dropped there without another request handler.
        let _ = ipc::connect(&state.paths.socket_path(), Duration::from_millis(500));
    }
    written
}

fn handle_request(state: &State, request: Request) -> Response {
    match handle_request_result(state, request) {
        Ok(response) => response,
        Err(error) => Response::Error {
            code: "request_failed".into(),
            message: format!("{error:#}"),
        },
    }
}

fn handle_request_result(state: &State, request: Request) -> Result<Response> {
    if state.shutting_down.load(Ordering::Acquire) && !matches!(request, Request::Shutdown) {
        bail!("daemon is shutting down");
    }
    match request {
        Request::Shutdown => {
            state.shutting_down.store(true, Ordering::Release);
            Ok(Response::Ok)
        }
        Request::AcquireLease {
            scope,
            pid,
            ttl_secs,
        } => {
            let _operation = state.operation_lock.lock().unwrap();
            let db = state.db.lock().unwrap();
            let (lease_id, expires_in_secs) = db.acquire(&scope, pid, ttl_secs)?;
            Ok(Response::Lease {
                lease_id,
                expires_in_secs,
            })
        }
        Request::BindLease {
            lease_id,
            build_dir,
            workspace_root,
        } => {
            // Order binding against GC's lease snapshot and deletion. A bind
            // accepted before GC begins then protects its context in the plan.
            let _operation = state.operation_lock.lock().unwrap();
            let db = state.db.lock().unwrap();
            db.bind(lease_id, Path::new(&build_dir), workspace_root.as_deref())?;
            Ok(Response::Ok)
        }
        Request::Heartbeat { lease_id } => {
            let db = state.db.lock().unwrap();
            let expires_in_secs = db.heartbeat(lease_id, rgo_protocol::DEFAULT_LEASE_TTL_SECS)?;
            Ok(Response::Lease {
                lease_id,
                expires_in_secs,
            })
        }
        Request::ReleaseLease { lease_id } => {
            let db = state.db.lock().unwrap();
            db.release(lease_id)?;
            Ok(Response::Ok)
        }
        Request::Touch {
            build_dir,
            workspace_root,
            physical_bytes,
            incremental_bytes,
        } => {
            let db = state.db.lock().unwrap();
            db.touch(
                Path::new(&build_dir),
                workspace_root.as_deref(),
                physical_bytes,
                incremental_bytes,
            )?;
            Ok(Response::Ok)
        }
        Request::QueryStatus => Ok(Response::Status(status_report(state)?)),
        Request::TriggerGc {
            dry_run,
            aggressive,
            auto,
            target_bytes,
        } => Ok(Response::Gc(run_gc(
            state,
            dry_run,
            aggressive,
            auto,
            target_bytes,
        )?)),
        Request::Pin { build_dir } => {
            let _operation = state.operation_lock.lock().unwrap();
            let path = Path::new(&build_dir);
            validate_managed_path(&state.paths, path)?;
            // The CLI may have listed this context before a GC pass removed
            // it. Check again under the operation lock, and reject symlinked
            // shards or contexts before writing the compatibility marker.
            if !state
                .paths
                .checked_managed_build_dirs()?
                .iter()
                .any(|context| context == path)
            {
                bail!("managed context does not exist: {}", path.display());
            }
            context::write_durable_pin(&state.paths, path)?;
            // Compatibility with older rgo versions that read only the
            // in-context marker. The stable record remains authoritative.
            context::write_pin_marker(path)?;
            let db = state.db.lock().unwrap();
            db.set_pin(path, true)?;
            Ok(Response::Ok)
        }
        Request::Unpin { build_dir } => {
            let _operation = state.operation_lock.lock().unwrap();
            let path = Path::new(&build_dir);
            validate_managed_path(&state.paths, path)?;
            context::remove_durable_pin(&state.paths, path)?;
            let db = state.db.lock().unwrap();
            db.set_pin(path, false)?;
            drop(db);
            if let Err(error) = context::try_prune_unpin_decision(&state.paths, path) {
                tracing::warn!(path = %path.display(), %error, "deferred unpin record pruning");
            }
            Ok(Response::Ok)
        }
        Request::Clean { build_dir } => {
            validate_managed_path(&state.paths, Path::new(&build_dir))?;
            let _operation = state.operation_lock.lock().unwrap();
            state.paths.require_supervised_deletion()?;
            let contexts = context::list(&state.paths)?;
            let db = state.db.lock().unwrap();
            db.expire_leases()?;
            let pinned = db.pinned_paths()?;
            let leased = db.protected_paths(&contexts)?;
            let path = Path::new(&build_dir);
            if same_path_in(&pinned, path) || context::is_pinned(&state.paths, path) {
                bail!("context is pinned; unpin it before cleaning");
            }
            if same_path_in(&leased, path) {
                bail!("context has an active lease; refusing to clean");
            }
            let Some(context) = contexts.iter().find(|c| same_path(&c.dir, path)) else {
                bail!("managed context does not exist: {}", path.display());
            };
            let reclaimed = context.usage.physical_bytes;
            let victim = context.dir.clone();
            drop(db);
            gc::remove_atomically(&state.paths, &victim)?;
            let contexts = context::list(&state.paths)?;
            let mut db = state.db.lock().unwrap();
            db.reconcile_contexts(&state.paths, &contexts)?;
            Ok(Response::Gc(GcReport {
                reclaimed_bytes: reclaimed,
                planned_bytes: reclaimed,
                ..Default::default()
            }))
        }
        Request::CacheLookup { key, pid, ttl_secs } => {
            if !state.cfg.cache.enabled {
                return Ok(Response::CacheMiss {
                    reason: "cache_disabled".into(),
                });
            }
            let _operation = state.operation_lock.lock().unwrap();
            match state.cas.read_manifest(&key) {
                Ok(Some(manifest)) => {
                    let db = state.db.lock().unwrap();
                    let (lease_id, _) = db.acquire(
                        &rgo_protocol::LeaseScope::Cache { key: key.clone() },
                        pid,
                        ttl_secs,
                    )?;
                    db.touch_cache_entry(&key)?;
                    Ok(Response::CacheHit {
                        manifest: wire_manifest(&manifest),
                        lease_id: Some(lease_id),
                    })
                }
                Ok(None) => Ok(Response::CacheMiss {
                    reason: "not_found".into(),
                }),
                Err(error) => Ok(Response::CacheMiss {
                    reason: format!("integrity: {error:#}"),
                }),
            }
        }
        Request::CacheAcquire { key, pid, ttl_secs } => {
            if !state.cfg.cache.enabled {
                return Ok(Response::CacheMiss {
                    reason: "cache_disabled".into(),
                });
            }
            if !cache_admission_allowed(state) {
                return Ok(Response::CacheMiss {
                    reason: "free_space_pressure".into(),
                });
            }
            let _operation = state.operation_lock.lock().unwrap();
            if let Ok(Some(manifest)) = state.cas.read_manifest(&key) {
                let db = state.db.lock().unwrap();
                let (lease_id, _) = db.acquire(
                    &rgo_protocol::LeaseScope::Cache { key: key.clone() },
                    pid,
                    ttl_secs,
                )?;
                db.touch_cache_entry(&key)?;
                return Ok(Response::CacheReady {
                    manifest: wire_manifest(&manifest),
                    lease_id: Some(lease_id),
                });
            }
            if state.remote.is_some() {
                let decision = {
                    let db = state.db.lock().unwrap();
                    db.acquire_remote_fetch(&key, pid, ttl_secs)?
                };
                return match decision {
                    RemoteFetchDecision::Started { lease_id } => {
                        let worker_state = state.clone();
                        let worker_key = key.clone();
                        thread::spawn(move || {
                            remote_fetch_worker(&worker_state, &worker_key, lease_id)
                        });
                        Ok(Response::CacheRemotePending {
                            key,
                            retry_after_millis: 100,
                        })
                    }
                    RemoteFetchDecision::Wait => Ok(Response::CacheRemotePending {
                        key,
                        retry_after_millis: 100,
                    }),
                    RemoteFetchDecision::Build(decision) => Ok(cache_build_response(key, decision)),
                };
            }
            let db = state.db.lock().unwrap();
            match db.acquire_cache_build(&key, pid, ttl_secs, true)? {
                CacheBuildDecision::Producer {
                    lease_id,
                    expires_in_secs,
                } => Ok(Response::CacheProducer {
                    key,
                    lease_id,
                    expires_in_secs,
                }),
                CacheBuildDecision::Wait { expires_in_secs } => Ok(Response::CacheWait {
                    key,
                    retry_after_millis: 100,
                    expires_in_secs,
                }),
            }
        }
        Request::CacheWait { key, pid, ttl_secs } => {
            if !state.cfg.cache.enabled {
                return Ok(Response::CacheMiss {
                    reason: "cache_disabled".into(),
                });
            }
            let _operation = state.operation_lock.lock().unwrap();
            if let Ok(Some(manifest)) = state.cas.read_manifest(&key) {
                let db = state.db.lock().unwrap();
                let (lease_id, _) = db.acquire(
                    &rgo_protocol::LeaseScope::Cache { key: key.clone() },
                    pid,
                    ttl_secs,
                )?;
                db.touch_cache_entry(&key)?;
                return Ok(Response::CacheReady {
                    manifest: wire_manifest(&manifest),
                    lease_id: Some(lease_id),
                });
            }
            let db = state.db.lock().unwrap();
            db.expire_leases()?;
            if state.remote.is_some() && db.remote_fetch_active(&key)? {
                return Ok(Response::CacheRemotePending {
                    key,
                    retry_after_millis: 100,
                });
            }
            match db.acquire_cache_build(&key, pid, ttl_secs, false)? {
                CacheBuildDecision::Producer {
                    lease_id,
                    expires_in_secs,
                } => Ok(Response::CacheProducer {
                    key,
                    lease_id,
                    expires_in_secs,
                }),
                CacheBuildDecision::Wait { expires_in_secs } => Ok(Response::CacheWait {
                    key,
                    retry_after_millis: 100,
                    expires_in_secs,
                }),
            }
        }
        Request::CacheCommit {
            key,
            lease_id,
            manifest,
        } => {
            if !state.cfg.cache.enabled {
                return Ok(Response::CacheCommitted { accepted: false });
            }
            if !cache_admission_allowed(state) {
                let db = state.db.lock().unwrap();
                db.fail_cache_build(&key, lease_id, "free_space_pressure")?;
                return Ok(Response::CacheCommitted { accepted: false });
            }
            let _operation = state.operation_lock.lock().unwrap();
            let manifest = native_manifest(&manifest)?;
            if manifest.key != key {
                let db = state.db.lock().unwrap();
                db.release(lease_id)?;
                return Ok(Response::CacheCommitted { accepted: false });
            }
            let db = state.db.lock().unwrap();
            if !db.begin_cache_commit(&key, lease_id)? {
                db.release(lease_id)?;
                return Ok(Response::CacheCommitted { accepted: false });
            }
            if let Err(error) = state.cas.write_manifest(&manifest) {
                db.fail_cache_build(&key, lease_id, &format!("publish: {error:#}"))?;
                return Err(error);
            }
            db.record_cache_manifest(
                &wire_manifest(&manifest),
                &state.cas.manifest_path(&manifest.key),
            )?;
            let accepted =
                db.finish_cache_commit(&key, lease_id, &state.cas.manifest_path(&manifest.key))?;
            if accepted && state.remote.is_some() && state.cfg.remote.upload {
                db.queue_remote_job(&manifest.key, "manifest", None)?;
                if db.claim_remote_job(&manifest.key)? {
                    let worker_state = state.clone();
                    let worker_key = manifest.key.clone();
                    thread::spawn(move || remote_upload_worker(&worker_state, &worker_key));
                }
            }
            Ok(Response::CacheCommitted { accepted })
        }
        Request::CacheFail {
            key,
            lease_id,
            reason,
        } => {
            let db = state.db.lock().unwrap();
            db.fail_cache_build(&key, lease_id, &reason)?;
            Ok(Response::Ok)
        }
        Request::RecordCacheEvent { event } => {
            let db = state.db.lock().unwrap();
            db.record_cache_event(&event)?;
            Ok(Response::Ok)
        }
        Request::QueryCacheStats => {
            let db = state.db.lock().unwrap();
            drain_cache_events(&state.paths, &db, &mut state.event_drainer.lock().unwrap())?;
            let mut report = db.cache_stats(state.cfg.cache.enabled, state.cas.object_bytes()?)?;
            report.observations_incomplete = cache_event_log_truncated(&state.paths);
            report.remote = remote_status_from_db(state, &db)?;
            Ok(Response::CacheStats(report))
        }
        Request::QueryRemoteStatus => Ok(Response::RemoteStatus(remote_status(state)?)),
        Request::ProbeRemote => {
            let Some(remote) = &state.remote else {
                return Ok(Response::RemoteProbe(rgo_protocol::RemoteProbeReport {
                    ok: false,
                    namespace: (!state.cfg.remote.namespace.is_empty())
                        .then(|| state.cfg.remote.namespace.clone()),
                    message: state
                        .remote_error
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| "remote CAS is disabled".into()),
                }));
            };
            match remote.probe() {
                Ok(()) => {
                    *state.remote_error.lock().unwrap() = None;
                    state.db.lock().unwrap().set_remote_probe(true, None)?;
                    Ok(Response::RemoteProbe(rgo_protocol::RemoteProbeReport {
                        ok: true,
                        namespace: Some(remote.namespace().into()),
                        message: "remote endpoint is reachable".into(),
                    }))
                }
                Err(error) => {
                    let message = error.to_string();
                    *state.remote_error.lock().unwrap() = Some(message.clone());
                    state
                        .db
                        .lock()
                        .unwrap()
                        .set_remote_probe(false, Some(&message))?;
                    Ok(Response::RemoteProbe(rgo_protocol::RemoteProbeReport {
                        ok: false,
                        namespace: Some(remote.namespace().into()),
                        message,
                    }))
                }
            }
        }
        Request::ExplainCache { key } => {
            let db = state.db.lock().unwrap();
            Ok(Response::CacheExplanation(db.cache_explanation(&key)?))
        }
        Request::VerifyCache => {
            let _operation = state.operation_lock.lock().unwrap();
            let mut report = rgo_protocol::CacheVerifyReport::default();
            for manifest in state.cas.list_manifests()? {
                report.checked_manifests += 1;
                report.checked_objects += manifest.outputs.len() as u64
                    + u64::from(manifest.stdout.is_some())
                    + u64::from(manifest.stderr.is_some());
                if let Err(error) = state.cas.read_manifest(&manifest.key) {
                    report.quarantined += 1;
                    report.errors.push(format!("{}: {error:#}", manifest.key));
                }
            }
            let db = state.db.lock().unwrap();
            db.record_cache_verify(&report)?;
            Ok(Response::CacheVerify(report))
        }
        Request::Hello { .. } => bail!("duplicate Hello"),
    }
}

fn status_report(state: &State) -> Result<StatusReport> {
    let _operation = state.operation_lock.lock().unwrap();
    let snapshot = crate::size::managed_snapshot(&state.paths)?;
    let contexts = snapshot.contexts;
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.prune_failed_cache_builds(Duration::from_secs(7 * 86_400))?;
    db.reconcile_contexts(&state.paths, &contexts)?;
    reconcile_cache(&mut db, &state.cas)?;
    drain_cache_events(&state.paths, &db, &mut state.event_drainer.lock().unwrap())?;
    let pinned = db.pinned_paths()?;
    let mut leased = db.protected_paths(&contexts)?;
    let stats = db.stats()?;
    let last_gc = db.last_gc()?.unwrap_or_default();
    let active_cache_keys = db.active_cache_keys()?;
    let cache_lru = db.cache_lru()?;
    drop(db);
    // The profile-lock mtime is only a recency heuristic. A long-running
    // supervised Cargo process can outlive it, so the status preview must
    // observe the same nonblocking lifecycle guard that deletion uses.
    for context in &contexts {
        if !leased.iter().any(|path| path == &context.dir)
            && !pinned.iter().any(|path| path == &context.dir)
            && crate::supervision::try_lock_gc(&state.paths, Some(&context.dir))?.is_none()
        {
            leased.push(context.dir.clone());
        }
    }
    let cas_bytes = snapshot.cas_bytes;
    let build_bytes = snapshot.build_bytes;
    let auxiliary_bytes = snapshot.auxiliary_bytes;
    let budget_inputs = Inputs {
        paths: &state.paths,
        cfg: &state.cfg,
        contexts: &contexts,
        other_managed_bytes: cas_bytes.saturating_add(auxiliary_bytes),
        pinned: &pinned,
        leased: &leased,
        now: SystemTime::now(),
        aggressive: true,
        age_maintenance: false,
        allow_pressure_contexts: false,
        target_bytes: Some(0),
    };
    let mut budget_plan = gc::plan(&budget_inputs)?;
    let mut build_preview = budget_plan.clone();
    append_pressure_for_preview(
        &mut build_preview,
        gc::pressure_candidates(&budget_inputs),
        state.cfg.min_free_space,
    );
    let reclaimable_build_bytes = build_preview.reclaim_bytes();
    append_unreferenced_cas(&mut budget_plan, &state.cas, &active_cache_keys)?;
    extend_gc_preview(
        &mut budget_plan,
        &budget_inputs,
        &state.cas,
        &cache_lru,
        &active_cache_keys,
        state.cfg.min_free_space,
    )?;
    let eligible_managed_bytes = budget_plan.reclaim_bytes().min(budget_plan.managed_bytes);
    let unmet_budget_bytes = budget_plan
        .managed_bytes
        .saturating_sub(state.cfg.max_size)
        .saturating_sub(eligible_managed_bytes);
    let unclassified = budget_plan
        .managed_bytes
        .saturating_sub(eligible_managed_bytes)
        .saturating_sub(budget_plan.protected_context_bytes)
        .saturating_sub(if active_cache_keys.is_empty() {
            0
        } else {
            cas_bytes
        });
    let unmet_budget_reason = if unmet_budget_bytes > 0 {
        let mut reasons = Vec::new();
        if budget_plan.protected_context_bytes > 0 {
            reasons.push("protected build contexts");
        }
        if !active_cache_keys.is_empty() {
            reasons.push("active cache work defers CAS eviction");
        }
        if unclassified > 0 {
            reasons.push("operational state or other ineligible data");
        }
        if reasons.is_empty() {
            reasons.push("eligible-byte estimates do not cover the excess");
        }
        Some(reasons.join("; "))
    } else {
        None
    };
    let cache = {
        let db = state.db.lock().unwrap();
        let mut report = db.cache_stats(state.cfg.cache.enabled, state.cas.object_bytes()?)?;
        report.observations_incomplete = cache_event_log_truncated(&state.paths);
        report.remote = remote_status_from_db(state, &db)?;
        report
    };
    let remote = cache.remote.clone();
    Ok(StatusReport {
        managed_bytes: budget_plan.managed_bytes,
        build_bytes: Some(build_bytes),
        cas_bytes: Some(cas_bytes),
        auxiliary_bytes,
        protected_context_bytes: budget_plan.protected_context_bytes,
        incremental_bytes: contexts
            .iter()
            .map(|c| c.incremental_usage.physical_bytes)
            .sum(),
        reclaimable_bytes: reclaimable_build_bytes,
        eligible_managed_bytes: Some(eligible_managed_bytes),
        unmet_budget_bytes: Some(unmet_budget_bytes),
        unmet_budget_reason,
        soft_watermark_bytes: state.cfg.soft_watermark,
        hard_limit_bytes: state.cfg.max_size,
        volume_free_bytes: budget_plan.free_bytes,
        volume_free_observed_bytes: Some(budget_plan.free_bytes),
        min_free_bytes: state.cfg.min_free_space,
        contexts: contexts.len() as u64,
        orphaned_contexts: contexts.iter().filter(|c| c.is_orphan()).count() as u64,
        active_leases: stats.active_leases,
        pinned_contexts: stats.pinned_contexts,
        daemon_pid: state.pid,
        last_gc_reclaimed_bytes: last_gc.reclaimed_bytes,
        last_gc_at: last_gc.finished_at,
        last_gc_error: last_gc.error,
        cache,
        protocol_compatible: true,
        remote,
    })
}

fn cache_build_response(key: String, decision: CacheBuildDecision) -> Response {
    match decision {
        CacheBuildDecision::Producer {
            lease_id,
            expires_in_secs,
        } => Response::CacheProducer {
            key,
            lease_id,
            expires_in_secs,
        },
        CacheBuildDecision::Wait { expires_in_secs } => Response::CacheWait {
            key,
            retry_after_millis: 100,
            expires_in_secs,
        },
    }
}

fn remote_status(state: &State) -> Result<rgo_protocol::RemoteStatusReport> {
    let db = state.db.lock().unwrap();
    remote_status_from_db(state, &db)
}

fn remote_status_from_db(state: &State, db: &StateDb) -> Result<rgo_protocol::RemoteStatusReport> {
    let mut report = db.remote_status(
        state.cfg.cache.enabled && state.cfg.remote.enabled,
        (!state.cfg.remote.endpoint.is_empty())
            .then(|| redact_endpoint(&state.cfg.remote.endpoint))
            .as_deref(),
        (!state.cfg.remote.namespace.is_empty()).then_some(state.cfg.remote.namespace.as_str()),
    )?;
    if let Some(error) = state.remote_error.lock().unwrap().clone() {
        report.healthy = false;
        report.protocol_compatible = false;
        report.last_error = Some(error);
    }
    Ok(report)
}

fn redact_endpoint(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return "<invalid endpoint>".into();
    };
    let Some(at) = rest.find('@') else {
        return rest.split(['?', '#']).next().map_or_else(
            || endpoint.to_owned(),
            |authority| format!("{scheme}://{authority}"),
        );
    };
    let authority = rest[at + 1..].split(['?', '#']).next().unwrap_or_default();
    format!("{scheme}://<redacted>@{authority}")
}

fn remote_fetch_worker(state: &State, key: &str, lease_id: u64) {
    let result = remote_fetch_worker_result(state, key, lease_id);
    if let Err(error) = result {
        tracing::debug!(key, %error, "remote cache fetch fell back to local compilation");
    }
}

fn remote_fetch_worker_result(state: &State, key: &str, lease_id: u64) -> Result<()> {
    let Some(remote) = &state.remote else {
        return Ok(());
    };
    let fetched = match remote.get_manifest(key) {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            let db = state.db.lock().unwrap();
            db.record_remote_counter(
                if matches!(error.kind, rgo_remote::RemoteErrorKind::Authentication) {
                    "authentication_failure"
                } else {
                    "miss"
                },
                0,
            )?;
            db.remote_fetch_fallback(key, lease_id, &message)?;
            *state.remote_error.lock().unwrap() = Some(message);
            return Ok(());
        }
    };
    let RemoteFetch::Hit(manifest_bytes) = fetched else {
        let db = state.db.lock().unwrap();
        db.record_remote_counter("miss", 0)?;
        db.remote_fetch_fallback(key, lease_id, "remote_manifest_missing")?;
        return Ok(());
    };
    let manifest: Manifest = match serde_json::from_slice::<Manifest>(&manifest_bytes) {
        Ok(manifest) if manifest.version == MANIFEST_VERSION && manifest.key == key => manifest,
        _ => {
            let db = state.db.lock().unwrap();
            db.record_remote_counter("corruption", 0)?;
            db.remote_fetch_fallback(key, lease_id, "invalid_remote_manifest")?;
            return Ok(());
        }
    };
    if manifest
        .outputs
        .iter()
        .any(|output| !safe_output_name(&output.name) || !valid_object_ref(&output.object))
        || manifest
            .stdout
            .as_ref()
            .is_some_and(|object| !valid_object_ref(object))
        || manifest
            .stderr
            .as_ref()
            .is_some_and(|object| !valid_object_ref(object))
    {
        let db = state.db.lock().unwrap();
        db.record_remote_counter("corruption", 0)?;
        db.remote_fetch_fallback(key, lease_id, "unsafe_remote_output_name")?;
        return Ok(());
    }
    for object in manifest
        .outputs
        .iter()
        .map(|output| &output.object)
        .chain(manifest.stdout.iter())
        .chain(manifest.stderr.iter())
    {
        let bytes = match remote.get_object(&object.digest) {
            Ok(RemoteFetch::Hit(bytes)) => bytes,
            Ok(RemoteFetch::Miss) => {
                let db = state.db.lock().unwrap();
                db.record_remote_counter("miss", 0)?;
                db.remote_fetch_fallback(key, lease_id, "remote_object_missing")?;
                return Ok(());
            }
            Err(error) => {
                let message = error.to_string();
                let db = state.db.lock().unwrap();
                db.record_remote_counter("corruption", 0)?;
                db.remote_fetch_fallback(key, lease_id, &message)?;
                return Ok(());
            }
        };
        if bytes.len() as u64 != object.size
            || blake3::hash(&bytes).to_hex().to_string() != object.digest
        {
            let db = state.db.lock().unwrap();
            db.record_remote_counter("corruption", bytes.len() as u64)?;
            db.remote_fetch_fallback(key, lease_id, "remote_object_digest_mismatch")?;
            return Ok(());
        }
        state.cas.put_bytes(&bytes, object.mode)?;
        state
            .db
            .lock()
            .unwrap()
            .record_remote_counter("download", bytes.len() as u64)?;
    }
    state.cas.write_manifest(&manifest)?;
    let wire = wire_manifest(&manifest);
    let db = state.db.lock().unwrap();
    db.record_cache_manifest(&wire, &state.cas.manifest_path(key))?;
    db.record_remote_counter("hit", manifest_bytes.len() as u64)?;
    db.record_remote_counter("download", manifest_bytes.len() as u64)?;
    db.finish_remote_fetch(key, lease_id, &state.cas.manifest_path(key))?;
    Ok(())
}

fn remote_upload_worker(state: &State, key: &str) {
    if let Err(error) = remote_upload_worker_result(state, key) {
        let message = error.to_string();
        if let Ok(db) = state.db.lock() {
            let terminal =
                error
                    .downcast_ref::<rgo_remote::RemoteError>()
                    .is_some_and(|remote_error| {
                        matches!(
                            remote_error.kind,
                            rgo_remote::RemoteErrorKind::Authentication
                                | rgo_remote::RemoteErrorKind::Conflict
                                | rgo_remote::RemoteErrorKind::InvalidResponse
                                | rgo_remote::RemoteErrorKind::Unsupported
                        )
                    });
            let _ = if terminal {
                db.finish_remote_job(key, Some(&message))
            } else {
                db.retry_remote_job(key, &message)
            };
        }
        tracing::debug!(key, error = %message, "remote cache upload deferred");
    }
}

fn remote_upload_worker_result(state: &State, key: &str) -> Result<()> {
    let Some(remote) = &state.remote else {
        return Ok(());
    };
    let manifest = state
        .cas
        .read_manifest(key)?
        .context("manifest disappeared before remote upload")?;
    let mut objects = Vec::new();
    objects.extend(manifest.outputs.iter().map(|output| &output.object));
    objects.extend(manifest.stdout.iter());
    objects.extend(manifest.stderr.iter());
    for object in objects {
        let bytes = state.cas.read_object(object)?;
        remote
            .put_object(&object.digest, &bytes)
            .map_err(|error| anyhow::anyhow!(error))?;
        state
            .db
            .lock()
            .unwrap()
            .record_remote_counter("upload", bytes.len() as u64)?;
    }
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    remote
        .put_manifest(key, &manifest_bytes)
        .map_err(|error| anyhow::anyhow!(error))?;
    let db = state.db.lock().unwrap();
    db.record_remote_counter("upload", manifest_bytes.len() as u64)?;
    db.finish_remote_job(key, None)?;
    Ok(())
}

fn safe_output_name(name: &str) -> bool {
    let path = Path::new(name);
    !name.is_empty() && !path.is_absolute() && !name.contains("..") && !name.contains(['/', '\\'])
}

fn valid_object_ref(object: &ObjectRef) -> bool {
    object.digest.len() == 64
        && object.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && object.mode <= 0o177777
}

fn run_gc(
    state: &State,
    dry_run: bool,
    aggressive: bool,
    auto: bool,
    target_bytes: Option<u64>,
) -> Result<GcReport> {
    let _operation = state.operation_lock.lock().unwrap();
    if !dry_run {
        state.paths.require_supervised_deletion()?;
    }
    let snapshot = crate::size::managed_snapshot(&state.paths)?;
    let managed_bytes = snapshot.total_bytes();
    let contexts = snapshot.contexts;
    let build_bytes = snapshot.build_bytes;
    let cas_bytes = snapshot.cas_bytes;
    let auxiliary_bytes = snapshot.auxiliary_bytes;
    let free_bytes = volume_free_bytes_checked(&state.paths.root)?;
    let trigger = managed_bytes > state.cfg.soft_watermark || free_bytes < state.cfg.min_free_space;
    let age_due = auto
        && crate::context::unix_now().saturating_sub(state.db.lock().unwrap().last_real_gc_at()?)
            >= AGE_MAINTENANCE_INTERVAL.as_secs();
    if auto && !trigger && !age_due {
        return Ok(GcReport {
            dry_run,
            managed_bytes,
            target_bytes: managed_bytes,
            remaining_managed_bytes: (!dry_run).then_some(managed_bytes),
            remaining_build_bytes: (!dry_run).then_some(build_bytes),
            remaining_cas_bytes: (!dry_run).then_some(cas_bytes),
            remaining_auxiliary_bytes: (!dry_run).then_some(auxiliary_bytes),
            min_free_bytes: state.cfg.min_free_space,
            volume_free_before_bytes: Some(free_bytes),
            volume_free_after_bytes: (!dry_run).then_some(free_bytes),
            ..Default::default()
        });
    }
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.reconcile_contexts(&state.paths, &contexts)?;
    reconcile_cache(&mut db, &state.cas)?;
    let pinned = db.pinned_paths()?;
    let leased = db.protected_paths(&contexts)?;
    let active_cache_keys = db.active_cache_keys()?;
    let inputs = Inputs {
        paths: &state.paths,
        cfg: &state.cfg,
        contexts: &contexts,
        other_managed_bytes: cas_bytes.saturating_add(auxiliary_bytes),
        pinned: &pinned,
        leased: &leased,
        now: SystemTime::now(),
        aggressive,
        age_maintenance: age_due,
        allow_pressure_contexts: false,
        target_bytes,
    };
    let mut plan = gc::plan(&inputs)?;
    append_unreferenced_cas(&mut plan, &state.cas, &active_cache_keys)?;
    let cache_lru = db.cache_lru()?;
    drop(db);
    if dry_run {
        extend_gc_preview(
            &mut plan,
            &inputs,
            &state.cas,
            &cache_lru,
            &active_cache_keys,
            state.cfg.min_free_space,
        )?;
    }
    let mut report = GcReport {
        dry_run,
        managed_bytes: plan.managed_bytes,
        target_bytes: plan.target_bytes,
        reclaimed_bytes: 0,
        planned_bytes: plan.reclaim_bytes(),
        skipped_execution_actions: 0,
        skipped_execution_bytes: 0,
        first_execution_skip: None,
        skipped_live: plan.skipped_live as u64,
        skipped_leased: plan.skipped_leased as u64,
        skipped_pinned: plan.skipped_pinned as u64,
        skipped_unavailable: plan.skipped_unavailable as u64,
        protected_context_bytes: plan.protected_context_bytes,
        cas_eviction_deferred_bytes: if active_cache_keys.is_empty() {
            0
        } else {
            cas_bytes
        },
        min_free_bytes: state.cfg.min_free_space,
        volume_free_before_bytes: Some(free_bytes),
        remaining_managed_bytes: None,
        remaining_build_bytes: None,
        remaining_cas_bytes: None,
        remaining_auxiliary_bytes: None,
        volume_free_after_bytes: None,
        actions: plan.actions.iter().map(wire_gc_action).collect(),
    };
    if !dry_run {
        let mut execution = gc::Execution::default();
        match gc::execute(&state.paths, &plan, false) {
            Ok(outcome) => execution.absorb(outcome),
            Err(error) => {
                let message = format!("{error:#}");
                let db = state.db.lock().unwrap();
                db.record_gc(dry_run, aggressive, &report, Some(&message))?;
                return Err(error);
            }
        }
        forget_removed_object_rows(state, &plan.actions)?;
        let remaining = crate::size::managed_snapshot(&state.paths)?;
        let remaining_total = remaining.total_bytes();
        let needed = remaining_total.saturating_sub(plan.target_bytes).max(
            state
                .cfg
                .min_free_space
                .saturating_sub(volume_free_bytes_checked(&state.paths.root)?),
        );
        let selected = select_cas_manifests(
            &state.cas,
            &cache_lru,
            &active_cache_keys,
            needed,
            state.cfg.gc.cache_retention,
            crate::context::unix_now(),
        )?;
        for item in selected {
            report.planned_bytes = report.planned_bytes.saturating_add(item.action.bytes);
            report.actions.push(wire_gc_action(&item.action));
            match gc::remove_atomically(&state.paths, &item.action.path) {
                Ok(()) => {
                    execution.reclaimed_bytes =
                        execution.reclaimed_bytes.saturating_add(item.action.bytes);
                    state.db.lock().unwrap().forget_cache_entry(&item.key)?;
                }
                Err(error) => {
                    tracing::warn!(key = %item.key, %error, "skipped CAS manifest eviction");
                    execution.note_skip(&item.action.path, item.action.bytes, &error);
                }
            }
        }
        if active_cache_keys.is_empty() {
            let mut post = gc::Plan::default();
            for action in unreferenced_cas_actions(&state.cas, &[])? {
                if !plan
                    .actions
                    .iter()
                    .any(|existing| existing.path == action.path)
                {
                    post.actions.push(action);
                }
            }
            report.planned_bytes = report.planned_bytes.saturating_add(post.reclaim_bytes());
            report
                .actions
                .extend(post.actions.iter().map(wire_gc_action));
            execution.absorb(gc::execute(&state.paths, &post, false)?);
            forget_removed_object_rows(state, &post.actions)?;
        }
        let remaining = crate::size::managed_snapshot(&state.paths)?;
        let remaining_total = remaining.total_bytes();
        let contexts = remaining.contexts;
        let remaining_cas_bytes = remaining.cas_bytes;
        let remaining_auxiliary_bytes = remaining.auxiliary_bytes;
        let needed = remaining_total.saturating_sub(plan.target_bytes).max(
            state
                .cfg
                .min_free_space
                .saturating_sub(volume_free_bytes_checked(&state.paths.root)?),
        );
        if needed > 0 {
            let (pinned, leased) = {
                let db = state.db.lock().unwrap();
                (db.pinned_paths()?, db.protected_paths(&contexts)?)
            };
            let pressure_inputs = Inputs {
                paths: &state.paths,
                cfg: &state.cfg,
                contexts: &contexts,
                other_managed_bytes: remaining_cas_bytes.saturating_add(remaining_auxiliary_bytes),
                pinned: &pinned,
                leased: &leased,
                now: SystemTime::now(),
                aggressive,
                age_maintenance: false,
                allow_pressure_contexts: true,
                target_bytes: Some(plan.target_bytes),
            };
            let mut remaining = needed;
            let actions = gc::pressure_candidates(&pressure_inputs)
                .into_iter()
                .take_while(|action| {
                    if remaining == 0 {
                        return false;
                    }
                    remaining = remaining.saturating_sub(action.bytes);
                    true
                })
                .collect::<Vec<_>>();
            let pressure_plan = gc::Plan {
                actions,
                ..Default::default()
            };
            report.planned_bytes = report
                .planned_bytes
                .saturating_add(pressure_plan.reclaim_bytes());
            report
                .actions
                .extend(pressure_plan.actions.iter().map(wire_gc_action));
            execution.absorb(gc::execute(&state.paths, &pressure_plan, false)?);
        }
        let remaining = crate::size::managed_snapshot(&state.paths)?;
        let contexts = remaining.contexts;
        let remaining_build_bytes = remaining.build_bytes;
        let remaining_cas_bytes = remaining.cas_bytes;
        let remaining_auxiliary_bytes = remaining.auxiliary_bytes;
        report.remaining_managed_bytes = Some(
            remaining_build_bytes
                .saturating_add(remaining_cas_bytes)
                .saturating_add(remaining_auxiliary_bytes),
        );
        report.remaining_build_bytes = Some(remaining_build_bytes);
        report.remaining_cas_bytes = Some(remaining_cas_bytes);
        report.remaining_auxiliary_bytes = Some(remaining_auxiliary_bytes);
        report.volume_free_after_bytes = volume_free_bytes(&state.paths.root);
        report.reclaimed_bytes = execution.reclaimed_bytes;
        report.skipped_execution_actions = execution.skipped_actions;
        report.skipped_execution_bytes = execution.skipped_bytes;
        report.first_execution_skip = execution.first_skip;
        let mut db = state.db.lock().unwrap();
        db.reconcile_contexts(&state.paths, &contexts)?;
        db.record_gc(
            dry_run,
            aggressive,
            &report,
            report.first_execution_skip.as_deref(),
        )?;
        return Ok(report);
    }
    let db = state.db.lock().unwrap();
    db.record_gc(dry_run, aggressive, &report, None)?;
    Ok(report)
}

fn append_pressure_for_preview(
    plan: &mut gc::Plan,
    candidates: Vec<gc::Action>,
    min_free_space: u64,
) {
    let mut needed = plan
        .managed_bytes
        .saturating_sub(plan.target_bytes)
        .max(min_free_space.saturating_sub(plan.free_bytes))
        .saturating_sub(planned_managed_reclaim(plan));
    for candidate in candidates {
        if needed == 0 {
            break;
        }
        // Whole-context eviction subsumes any planned incremental deletion.
        let nested_bytes = plan
            .actions
            .iter()
            .filter(|action| action.path.starts_with(&candidate.path))
            .map(|action| action.bytes)
            .sum::<u64>();
        plan.actions
            .retain(|action| !action.path.starts_with(&candidate.path));
        needed = needed
            .saturating_add(nested_bytes)
            .saturating_sub(candidate.bytes);
        plan.actions.push(candidate);
    }
}

fn extend_gc_preview(
    plan: &mut gc::Plan,
    inputs: &Inputs<'_>,
    cas: &Store,
    cache_lru: &[(String, u64)],
    active_cache_keys: &[String],
    min_free_space: u64,
) -> Result<()> {
    let needed = plan
        .managed_bytes
        .saturating_sub(plan.target_bytes)
        .max(min_free_space.saturating_sub(plan.free_bytes))
        .saturating_sub(planned_managed_reclaim(plan));
    let selected = select_cas_manifests(
        cas,
        cache_lru,
        active_cache_keys,
        needed,
        inputs.cfg.gc.cache_retention,
        crate::context::unix_now(),
    )?;
    if !selected.is_empty() {
        let keys = selected
            .iter()
            .map(|item| item.key.clone())
            .collect::<Vec<_>>();
        plan.actions
            .extend(selected.into_iter().map(|item| item.action));
        for action in unreferenced_cas_actions(cas, &keys)? {
            if !plan
                .actions
                .iter()
                .any(|existing| existing.path == action.path)
            {
                plan.actions.push(action);
            }
        }
    }
    append_pressure_for_preview(plan, gc::pressure_candidates(inputs), min_free_space);
    Ok(())
}

fn planned_managed_reclaim(plan: &gc::Plan) -> u64 {
    plan.reclaim_bytes()
}

fn wire_gc_action(action: &gc::Action) -> GcAction {
    GcAction {
        tier: action.tier as u8,
        path: action.path.to_string_lossy().into_owned(),
        bytes: action.bytes,
        reason: action.reason.clone(),
    }
}

fn forget_removed_object_rows(state: &State, actions: &[gc::Action]) -> Result<()> {
    let objects = state.cas.root().join("objects");
    let db = state.db.lock().unwrap();
    for action in actions {
        if let Some(digest) = action
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|_| action.path.starts_with(&objects) && !action.path.exists())
        {
            db.forget_unreferenced_cache_object(digest)?;
        }
    }
    Ok(())
}

fn append_unreferenced_cas(
    plan: &mut gc::Plan,
    cas: &Store,
    active_cache_keys: &[String],
) -> Result<()> {
    // Wrappers publish CAS objects before the manifest is committed. A remote
    // fetch does the same. Those objects look unreferenced until commit, and
    // their digest is not known to the daemon yet, so defer sweeping the whole
    // unreferenced set while any cache producer/consumer/fetch lease is live.
    if !active_cache_keys.is_empty() {
        return Ok(());
    }
    plan.actions.extend(unreferenced_cas_actions(cas, &[])?);
    plan.actions.sort_by_key(|action| action.tier);
    Ok(())
}

fn unreferenced_cas_actions(cas: &Store, excluded_manifests: &[String]) -> Result<Vec<gc::Action>> {
    let excluded: HashSet<&str> = excluded_manifests.iter().map(String::as_str).collect();
    let manifests = cas.list_manifests()?;
    let mut referenced = HashSet::new();
    for manifest in manifests
        .iter()
        .filter(|m| !excluded.contains(m.key.as_str()))
    {
        referenced.extend(
            manifest
                .outputs
                .iter()
                .map(|output| output.object.digest.clone()),
        );
        if let Some(object) = &manifest.stdout {
            referenced.insert(object.digest.clone());
        }
        if let Some(object) = &manifest.stderr {
            referenced.insert(object.digest.clone());
        }
    }
    let objects = cas.root().join("objects");
    match std::fs::symlink_metadata(&objects) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Ok(_) => anyhow::bail!("unsafe CAS object root {}", objects.display()),
        Err(error) => return Err(error).context("checking CAS object root"),
    }
    let mut actions = Vec::new();
    for shard in std::fs::read_dir(objects)? {
        let shard = shard?;
        anyhow::ensure!(
            shard.file_type()?.is_dir(),
            "unsafe CAS object shard {}",
            shard.path().display()
        );
        for entry in std::fs::read_dir(shard.path())? {
            let entry = entry?;
            anyhow::ensure!(
                entry.file_type()?.is_file(),
                "unsafe CAS object {}",
                entry.path().display()
            );
            let path = entry.path();
            let Some(digest) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !referenced.contains(digest) {
                let bytes = crate::size::Scanner::new()
                    .measure_checked(&path)?
                    .physical_bytes;
                actions.push(gc::Action {
                    tier: gc::Tier::Pressure,
                    path,
                    bytes,
                    reason: "unreferenced CAS object".into(),
                });
            }
        }
    }
    Ok(actions)
}

struct CasManifestEviction {
    key: String,
    action: gc::Action,
}

fn select_cas_manifests(
    cas: &Store,
    lru: &[(String, u64)],
    protected_keys: &[String],
    mut needed: u64,
    retention: Duration,
    now: u64,
) -> Result<Vec<CasManifestEviction>> {
    // A producer can publish objects before committing its manifest, and an
    // upload can read objects without the operation lock. Defer all manifest
    // eviction while either is active; partial protection needs a separate
    // tested reference protocol.
    if !protected_keys.is_empty() {
        return Ok(Vec::new());
    }
    let last_used: HashMap<&str, u64> = lru.iter().map(|(key, at)| (key.as_str(), *at)).collect();
    let mut manifests = cas.list_manifests()?;
    manifests.sort_by_key(|manifest| {
        (
            last_used
                .get(manifest.key.as_str())
                .copied()
                .filter(|at| *at > 0)
                .unwrap_or(manifest.created_at),
            manifest.key.clone(),
        )
    });
    let digests = |manifest: &Manifest| -> HashSet<String> {
        manifest
            .outputs
            .iter()
            .map(|output| output.object.digest.clone())
            .chain(manifest.stdout.iter().map(|object| object.digest.clone()))
            .chain(manifest.stderr.iter().map(|object| object.digest.clone()))
            .collect()
    };
    let mut references: HashMap<String, usize> = HashMap::new();
    for manifest in &manifests {
        for digest in digests(manifest) {
            *references.entry(digest).or_default() += 1;
        }
    }
    let mut selected = Vec::new();
    for manifest in manifests {
        let used = last_used
            .get(manifest.key.as_str())
            .copied()
            .filter(|at| *at > 0)
            .unwrap_or(manifest.created_at);
        let expired = now.saturating_sub(used) >= retention.as_secs();
        if needed == 0 && !expired {
            break;
        }
        let path = cas.manifest_path(&manifest.key);
        let manifest_bytes = crate::size::Scanner::new()
            .measure_checked(&path)?
            .physical_bytes;
        let mut projected = manifest_bytes;
        for digest in digests(&manifest) {
            if let Some(count) = references.get_mut(&digest) {
                *count -= 1;
                if *count == 0 {
                    projected = projected.saturating_add(
                        crate::size::Scanner::new()
                            .measure_checked(&cas.object_path(&digest))?
                            .physical_bytes,
                    );
                }
            }
        }
        selected.push(CasManifestEviction {
            key: manifest.key,
            action: gc::Action {
                tier: gc::Tier::Pressure,
                path,
                bytes: manifest_bytes,
                reason: if expired {
                    "unused CAS manifest past retention".into()
                } else {
                    "CAS pressure (LRU)".into()
                },
            },
        });
        needed = needed.saturating_sub(projected);
    }
    Ok(selected)
}

fn maintenance(state: &State) -> Result<()> {
    let migrated = {
        let _operation = state.operation_lock.lock().unwrap();
        let free_bytes = volume_free_bytes_checked(&state.paths.root)?;
        let db = state.db.lock().unwrap();
        db.expire_leases()?;
        db.prune_failed_cache_builds(Duration::from_secs(7 * 86_400))?;
        drain_cache_events(&state.paths, &db, &mut state.event_drainer.lock().unwrap())?;
        db.prune_missing_cache_event_batches(
            &state.paths.state_dir(),
            &mut state.batch_prune_cursor.lock().unwrap(),
            MAX_EVENT_BATCH_PRUNE_PER_PASS,
        )?;
        db.prune_operational_history()?;
        let migrated = match db.migrate_legacy_auto_vacuum(&state.paths.db_file(), free_bytes) {
            Ok(true) => {
                // VACUUM may renumber implicit rowids used by this scan cursor.
                *state.batch_prune_cursor.lock().unwrap() = 0;
                // Re-measure the managed budget next cycle after the rebuild.
                tracing::info!("converted legacy SQLite metadata for incremental reclamation");
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(%error, "deferred legacy SQLite metadata conversion");
                false
            }
        };
        db.reclaim_unused_pages()?;
        drop(db);
        // This metadata-only recovery runs even while automatic destructive GC is
        // disabled. A nonblocking lifecycle guard defers active Cargo sessions.
        // Keep the probe under the operation lock so explicit clean cannot
        // collide with it and report a spurious busy-session error.
        match state
            .pin_pruner
            .lock()
            .unwrap()
            .scan(&state.paths, MAX_PIN_SCAN_ENTRIES_PER_PASS)
        {
            Ok(0) => {}
            Ok(pruned) => tracing::debug!(pruned, "pruned stale unpin decisions"),
            Err(error) => tracing::warn!(%error, "pin decision recovery failed"),
        }
        migrated
    };
    if state.cfg.gc.auto {
        if migrated {
            // A legacy VACUUM changed the measured budget. Start a fresh
            // trigger sweep on the next maintenance tick.
            state.trigger_scan.lock().unwrap().invalidate();
        } else {
            let free_bytes = volume_free_bytes_checked(&state.paths.root)?;
            let age_due = crate::context::unix_now()
                .saturating_sub(state.db.lock().unwrap().last_real_gc_at()?)
                >= AGE_MAINTENANCE_INTERVAL.as_secs();
            let urgent = free_bytes < state.cfg.min_free_space || age_due;
            // This advisory scan runs outside the operation lock. It visits a
            // bounded number of entries per tick; the GC pass takes a fresh
            // authoritative snapshot under the lock before selecting anything.
            let pressure = if urgent {
                false
            } else {
                let mut scan = state.trigger_scan.lock().unwrap();
                let completed = scan.advance(
                    &state.paths,
                    MAX_AUTO_SCAN_ENTRIES_PER_PASS,
                    auto_scan_interval(),
                )?;
                completed.is_some_and(|bytes| bytes > state.cfg.soft_watermark)
                    || scan.observed_bytes() > state.cfg.soft_watermark
            };
            if urgent || pressure {
                match run_gc(state, false, false, true, None) {
                    Ok(_) => state.trigger_scan.lock().unwrap().mark_completed(),
                    Err(error) => {
                        state.trigger_scan.lock().unwrap().invalidate();
                        return Err(error);
                    }
                }
            }
        }
    }
    process_remote_jobs(state)?;
    Ok(())
}

fn cache_admission_allowed(state: &State) -> bool {
    volume_free_bytes(&state.paths.root)
        .map(|free| free >= state.cfg.min_free_space)
        .unwrap_or(false)
}

fn process_remote_jobs(state: &State) -> Result<()> {
    if state.remote.is_none() || !state.cfg.remote.upload {
        return Ok(());
    }
    let key = {
        let db = state.db.lock().unwrap();
        db.next_remote_job()?
    };
    let Some(key) = key else { return Ok(()) };
    let claimed = state.db.lock().unwrap().claim_remote_job(&key)?;
    if claimed {
        let worker_state = state.clone();
        thread::spawn(move || remote_upload_worker(&worker_state, &key));
    }
    Ok(())
}

fn reconcile_cache(db: &mut StateDb, cas: &Store) -> Result<()> {
    let manifests = cas.list_manifests()?;
    let present = manifests
        .iter()
        .map(|manifest| manifest.key.as_str())
        .collect::<HashSet<_>>();
    for (key, _) in db.cache_lru()? {
        if !present.contains(key.as_str()) {
            db.forget_cache_entry(&key)?;
        }
    }
    for manifest in manifests {
        db.record_cache_manifest(&wire_manifest(&manifest), &cas.manifest_path(&manifest.key))?;
    }
    Ok(())
}

#[derive(Default)]
struct CacheEventDrainScanner {
    entries: Option<std::fs::ReadDir>,
}

impl CacheEventDrainScanner {
    fn next_batch(&mut self, state: &Path) -> Result<Vec<PathBuf>> {
        if self.entries.is_none() {
            self.entries = Some(std::fs::read_dir(state)?);
        }
        let mut examined = 0;
        let mut pending = Vec::new();
        while examined < MAX_EVENT_SCAN_ENTRIES_PER_PASS
            && pending.len() < MAX_EVENT_DRAINS_PER_PASS
        {
            match self.entries.as_mut().unwrap().next() {
                Some(Ok(entry)) => {
                    examined += 1;
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("cache-events.") && name.ends_with(".drain") {
                        if !entry.file_type()?.is_file() {
                            bail!(
                                "cache event drain is not a regular file: {}",
                                entry.path().display()
                            );
                        }
                        pending.push(entry.path());
                    }
                }
                Some(Err(error)) => return Err(error).context("reading cache event drains"),
                None => {
                    self.entries = None;
                    break;
                }
            }
        }
        pending.sort();
        Ok(pending)
    }
}

fn drain_cache_events(
    paths: &RgoPaths,
    db: &StateDb,
    scanner: &mut CacheEventDrainScanner,
) -> Result<()> {
    let state = paths.state_dir();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join(CACHE_EVENT_LOG_LOCK))?;
    lock.lock_exclusive()?;
    let source = state.join(CACHE_EVENT_LOG_FILE);
    match std::fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let drained = state.join(format!("cache-events.{}-{nonce}.drain", std::process::id()));
            std::fs::rename(&source, &drained)
                .with_context(|| format!("rotating {}", source.display()))?;
        }
        Ok(_) => bail!(
            "cache event log is not a regular file: {}",
            source.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", source.display())),
    }
    drop(lock);

    let pending = scanner
        .next_batch(&state)
        .inspect_err(|_| scanner.entries = None)?;
    for drained in pending {
        let result = (|| -> Result<()> {
            let metadata = std::fs::symlink_metadata(&drained)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "cache event drain is not a regular file: {}",
                    drained.display()
                );
            }
            // Older installations could leave a much larger event file. Bound
            // each daemon pass even when such a file predates the wrapper cap.
            let file = File::open(&drained)?;
            let over_limit = file.metadata()?.len() > CACHE_EVENT_LOG_MAX_BYTES;
            let mut bytes = Vec::new();
            file.take(CACHE_EVENT_LOG_MAX_BYTES)
                .read_to_end(&mut bytes)?;
            let mut malformed = over_limit;
            if over_limit && bytes.last() != Some(&b'\n') {
                bytes.truncate(
                    bytes
                        .iter()
                        .rposition(|byte| *byte == b'\n')
                        .map_or(0, |i| i + 1),
                );
            }
            let events = bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(
                    |line| match serde_json::from_slice::<rgo_protocol::CacheEvent>(line) {
                        Ok(event) => Some(event),
                        Err(_) => {
                            malformed = true;
                            None
                        }
                    },
                )
                .collect::<Vec<_>>();
            if malformed {
                let _ = std::fs::write(state.join(CACHE_EVENT_LOG_TRUNCATED), b"1\n");
            }
            let name = drained
                .file_name()
                .and_then(|name| name.to_str())
                .context("drained cache event filename is not UTF-8")?;
            db.record_cache_event_batch(name, &events)?;
            std::fs::remove_file(&drained)?;
            if let Err(error) = db.forget_cache_event_batch(name) {
                tracing::warn!(%error, name, "could not clear processed cache-event batch marker");
            }
            Ok(())
        })();
        if let Err(error) = result {
            scanner.entries = None;
            return Err(error);
        }
    }
    Ok(())
}

fn cache_event_log_truncated(paths: &RgoPaths) -> bool {
    let state = paths.state_dir();
    if state.join(CACHE_EVENT_LOG_TRUNCATED).is_file() {
        return true;
    }
    // A wrapper may have appended after the last drain. A regular nonempty
    // source file is pending too, and unreadable or unusual state is unknown.
    match std::fs::symlink_metadata(state.join(CACHE_EVENT_LOG_FILE)) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            if metadata.len() > 0 {
                return true;
            }
        }
        Ok(_) => return true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return true,
    }
    // A bounded drain may leave valid events for the next pass. If the state
    // directory itself exceeds this scan budget, report unknown rather than
    // spending unbounded time under the daemon's database lock.
    let Ok(entries) = std::fs::read_dir(state) else {
        return true;
    };
    for (examined, entry) in entries.enumerate() {
        if examined >= MAX_EVENT_SCAN_ENTRIES_PER_PASS {
            return true;
        }
        let Ok(entry) = entry else {
            return true;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("cache-events.") && name.ends_with(".drain") {
            return true;
        }
    }
    false
}

fn validate_managed_path(paths: &RgoPaths, path: &Path) -> Result<()> {
    if !paths.is_managed_build_dir(path) {
        bail!("not a managed build context: {}", path.display());
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf())
        == std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf())
}

fn same_path_in(paths: &[PathBuf], wanted: &Path) -> bool {
    paths.iter().any(|path| same_path(path, wanted))
}

fn wire_manifest(manifest: &Manifest) -> WireManifest {
    WireManifest {
        version: manifest.version,
        key: manifest.key.clone(),
        outputs: manifest
            .outputs
            .iter()
            .map(|output| WireOutput {
                kind: output.kind.clone(),
                name: output.name.clone(),
                object: wire_object(&output.object),
            })
            .collect(),
        stdout: manifest.stdout.as_ref().map(wire_object),
        stderr: manifest.stderr.as_ref().map(wire_object),
        created_at: manifest.created_at,
    }
}

fn wire_object(object: &ObjectRef) -> WireObject {
    WireObject {
        digest: object.digest.clone(),
        size: object.size,
        mode: object.mode,
    }
}

fn native_manifest(manifest: &WireManifest) -> Result<Manifest> {
    if manifest.version != MANIFEST_VERSION {
        bail!("unsupported cache manifest version {}", manifest.version)
    }
    Ok(Manifest {
        version: manifest.version,
        key: manifest.key.clone(),
        outputs: manifest
            .outputs
            .iter()
            .map(|output| ManifestOutput {
                kind: output.kind.clone(),
                name: output.name.clone(),
                object: native_object(&output.object),
            })
            .collect(),
        stdout: manifest.stdout.as_ref().map(native_object),
        stderr: manifest.stderr.as_ref().map(native_object),
        created_at: manifest.created_at,
    })
}

fn native_object(object: &WireObject) -> ObjectRef {
    ObjectRef {
        digest: object.digest.clone(),
        size: object.size,
        mode: object.mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn interrupted_event_drain_replays_without_duplicate_counters() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let event = rgo_protocol::CacheEvent {
            key: None,
            outcome: "bypass".into(),
            bytes: 0,
            reason: Some("unsafe invocation".into()),
        };
        let old_name = "cache-events.1-1.drain";
        let old_path = paths.state_dir().join(old_name);
        std::fs::write(
            &old_path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        db.record_cache_event_batch(old_name, std::slice::from_ref(&event))
            .unwrap();
        let mut scanner = CacheEventDrainScanner::default();
        drain_cache_events(&paths, &db, &mut scanner).unwrap();
        assert!(!old_path.exists());
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 1);

        std::fs::write(
            paths.state_dir().join(CACHE_EVENT_LOG_FILE),
            format!("{}\ninvalid\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        drain_cache_events(&paths, &db, &mut scanner).unwrap();
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 2);
        assert!(cache_event_log_truncated(&paths));
    }

    #[test]
    fn oversized_preexisting_event_file_is_drained_with_bounded_work() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let event = rgo_protocol::CacheEvent {
            key: None,
            outcome: "bypass".into(),
            bytes: 0,
            reason: Some("before oversized line".into()),
        };
        let mut log = File::create(paths.state_dir().join(CACHE_EVENT_LOG_FILE)).unwrap();
        writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        std::io::copy(
            &mut std::io::repeat(b'x').take(CACHE_EVENT_LOG_MAX_BYTES + 1),
            &mut log,
        )
        .unwrap();
        drop(log);

        drain_cache_events(&paths, &db, &mut CacheEventDrainScanner::default()).unwrap();
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 1);
        assert!(cache_event_log_truncated(&paths));
        assert!(std::fs::read_dir(paths.state_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".drain")
        }));
    }

    #[test]
    fn pending_event_drain_is_reported_incomplete_until_processed() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        for index in 0..=MAX_EVENT_DRAINS_PER_PASS {
            std::fs::write(
                paths
                    .state_dir()
                    .join(format!("cache-events.{index}.drain")),
                b"",
            )
            .unwrap();
        }
        let mut scanner = CacheEventDrainScanner::default();
        drain_cache_events(&paths, &db, &mut scanner).unwrap();
        assert!(cache_event_log_truncated(&paths));
        drain_cache_events(&paths, &db, &mut scanner).unwrap();
        assert!(!cache_event_log_truncated(&paths));
        let live = paths.state_dir().join(CACHE_EVENT_LOG_FILE);
        std::fs::write(&live, b"new event awaiting drain\n").unwrap();
        assert!(cache_event_log_truncated(&paths));
        std::fs::remove_file(live).unwrap();
        assert!(!cache_event_log_truncated(&paths));
    }

    #[test]
    fn large_event_directory_drains_incrementally_without_losing_a_backlog() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let event = rgo_protocol::CacheEvent {
            key: None,
            outcome: "bypass".into(),
            bytes: 0,
            reason: Some("backlog".into()),
        };
        let encoded = format!("{}\n", serde_json::to_string(&event).unwrap());
        for index in 0..600 {
            std::fs::write(paths.state_dir().join(format!("unrelated-{index}")), b"").unwrap();
        }
        for index in 0..9 {
            std::fs::write(
                paths
                    .state_dir()
                    .join(format!("cache-events.{index}.drain")),
                &encoded,
            )
            .unwrap();
        }
        let mut scanner = CacheEventDrainScanner::default();
        let mut prior = 9usize;
        for _ in 0..16 {
            drain_cache_events(&paths, &db, &mut scanner).unwrap();
            let remaining = std::fs::read_dir(paths.state_dir())
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".drain")
                })
                .count();
            assert!(prior.saturating_sub(remaining) <= MAX_EVENT_DRAINS_PER_PASS);
            prior = remaining;
            if remaining == 0 {
                break;
            }
        }
        assert_eq!(prior, 0);
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 9);
        assert!(cache_event_log_truncated(&paths)); // Too many unrelated state entries to prove completeness cheaply.
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_event_drain_is_refused_without_reading_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"private\n").unwrap();
        symlink(&outside, paths.state_dir().join("cache-events.evil.drain")).unwrap();
        let mut scanner = CacheEventDrainScanner::default();
        let error = drain_cache_events(&paths, &db, &mut scanner).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(std::fs::read(outside).unwrap(), b"private\n");
    }

    #[test]
    fn cas_only_pressure_eviction_respects_active_cache_leases() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
        std::fs::write(
            paths.state_dir().join("owner-cargo-home"),
            format!("{}\n", temp.path().join("cargo").display()),
        )
        .unwrap();
        let cas = Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
        let now = crate::context::unix_now();
        let old_key = "a".repeat(64);
        let new_key = "b".repeat(64);
        let old_object = cas.put_bytes(&vec![b'a'; 2 * 1024 * 1024], 0o444).unwrap();
        let new_object = cas.put_bytes(&vec![b'b'; 2 * 1024 * 1024], 0o444).unwrap();
        let make_manifest = |key: &str, object: ObjectRef, created_at| Manifest {
            version: MANIFEST_VERSION,
            key: key.into(),
            outputs: vec![ManifestOutput {
                kind: "rlib".into(),
                name: "libprobe.rlib".into(),
                object,
            }],
            stdout: None,
            stderr: None,
            created_at,
        };
        let old = make_manifest(&old_key, old_object.clone(), now - 100);
        let new = make_manifest(&new_key, new_object.clone(), now);
        cas.write_manifest(&old).unwrap();
        cas.write_manifest(&new).unwrap();
        let db = StateDb::open(&paths).unwrap();
        db.record_cache_manifest(&wire_manifest(&old), &cas.manifest_path(&old_key))
            .unwrap();
        db.record_cache_manifest(&wire_manifest(&new), &cas.manifest_path(&new_key))
            .unwrap();
        let expired = select_cas_manifests(
            &cas,
            &db.cache_lru().unwrap(),
            &[],
            0,
            Duration::from_secs(30),
            now,
        )
        .unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].key, old_key);
        let mut cfg = crate::config::Config::default()
            .resolve(&paths.root)
            .unwrap();
        cfg.max_size = 4 * 1024 * 1024;
        cfg.soft_watermark = 3 * 1024 * 1024;
        cfg.min_free_space = 0;
        cfg.gc.cache_retention = Duration::from_secs(365 * 86_400);
        let state = State {
            paths,
            cfg,
            db: Arc::new(Mutex::new(db)),
            cas,
            operation_lock: Arc::new(Mutex::new(())),
            pin_pruner: Arc::new(Mutex::new(context::PinPruneScanner::default())),
            trigger_scan: Arc::new(Mutex::new(crate::size::TriggerScan::default())),
            event_drainer: Arc::new(Mutex::new(CacheEventDrainScanner::default())),
            batch_prune_cursor: Arc::new(Mutex::new(0)),
            pid: std::process::id(),
            remote: None,
            remote_error: Arc::new(Mutex::new(None)),
            connection_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };
        let lease = state
            .db
            .lock()
            .unwrap()
            .acquire(
                &rgo_protocol::LeaseScope::Cache {
                    key: old_key.clone(),
                },
                std::process::id(),
                60,
            )
            .unwrap()
            .0;
        let blocked_status = status_report(&state).unwrap();
        assert!(blocked_status.unmet_budget_bytes.unwrap() > 0);
        assert!(
            blocked_status
                .unmet_budget_reason
                .as_deref()
                .unwrap()
                .contains("active cache work")
        );
        let protected = run_gc(&state, false, false, true, None).unwrap();
        assert!(protected.managed_bytes > protected.target_bytes);
        assert!(state.cas.manifest_path(&old_key).is_file());
        assert!(state.cas.manifest_path(&new_key).is_file());
        state.db.lock().unwrap().release(lease).unwrap();
        let available_status = status_report(&state).unwrap();
        assert!(available_status.eligible_managed_bytes.unwrap() >= 4 * 1024 * 1024);
        assert_eq!(available_status.unmet_budget_bytes, Some(0));

        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let manifest_path = workspace.join("Cargo.toml");
        std::fs::write(&manifest_path, "[package]\nname='probe'\nversion='0.1.0'\n").unwrap();
        let context_dir = state.paths.builds_dir().join("aa/context");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(context_dir.join("build-output"), vec![0u8; 1024 * 1024]).unwrap();
        context::write_sidecar(&context_dir, &workspace, &manifest_path, None).unwrap();
        // Require less than the cold object's size, allowing metadata overhead
        // to vary with the SQLite file layout and WAL checkpoint timing.
        let target = status_report(&state)
            .unwrap()
            .managed_bytes
            .saturating_sub(1536 * 1024);
        let preview = run_gc(&state, true, false, false, Some(target)).unwrap();
        assert!(
            preview.actions.iter().any(
                |action| action.path == state.cas.manifest_path(&old_key).display().to_string()
            )
        );
        assert!(
            !preview
                .actions
                .iter()
                .any(|action| action.path == context_dir.display().to_string())
        );

        let report = run_gc(&state, false, false, false, Some(target)).unwrap();
        assert!(report.reclaimed_bytes >= 2 * 1024 * 1024);
        assert!(!state.cas.manifest_path(&old_key).exists());
        assert!(!state.cas.object_path(&old_object.digest).exists());
        assert!(state.cas.manifest_path(&new_key).is_file());
        assert!(state.cas.object_path(&new_object.digest).is_file());
        assert!(context_dir.is_dir());
        let stats = state
            .db
            .lock()
            .unwrap()
            .cache_stats(true, state.cas.object_bytes().unwrap())
            .unwrap();
        assert_eq!(stats.manifests, 1);
        assert_eq!(stats.objects, 1);
    }

    #[test]
    fn reconciliation_removes_a_cache_row_after_interrupted_manifest_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RgoPaths {
            root: temp.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let store = Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
        let key = "a".repeat(64);
        let object = store.put_bytes(b"cached", 0o444).unwrap();
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            key: key.clone(),
            outputs: vec![ManifestOutput {
                kind: "rlib".into(),
                name: "libcached.rlib".into(),
                object,
            }],
            stdout: None,
            stderr: None,
            created_at: crate::context::unix_now(),
        };
        store.write_manifest(&manifest).unwrap();
        let mut db = StateDb::open(&paths).unwrap();
        db.record_cache_manifest(&wire_manifest(&manifest), &store.manifest_path(&key))
            .unwrap();
        std::fs::remove_file(store.manifest_path(&key)).unwrap();
        reconcile_cache(&mut db, &store).unwrap();
        assert!(db.cache_lru().unwrap().is_empty());
    }

    #[test]
    fn unreferenced_cas_sweep_waits_for_active_publications() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path().join("cas"), root.path().join("quarantine")).unwrap();
        let object = store.put_bytes(b"not committed yet", 0o444).unwrap();
        let mut plan = gc::Plan::default();
        append_unreferenced_cas(&mut plan, &store, &["active-key".into()]).unwrap();
        assert!(plan.actions.is_empty());
        append_unreferenced_cas(&mut plan, &store, &[]).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].path, store.object_path(&object.digest));

        for key in ["a".repeat(64), "b".repeat(64)] {
            store
                .write_manifest(&Manifest {
                    version: MANIFEST_VERSION,
                    key,
                    outputs: vec![ManifestOutput {
                        kind: "rlib".into(),
                        name: "libprobe.rlib".into(),
                        object: object.clone(),
                    }],
                    stdout: None,
                    stderr: None,
                    created_at: crate::context::unix_now(),
                })
                .unwrap();
        }
        assert!(unreferenced_cas_actions(&store, &[]).unwrap().is_empty());
        std::fs::remove_file(store.manifest_path(&"a".repeat(64))).unwrap();
        assert!(unreferenced_cas_actions(&store, &[]).unwrap().is_empty());
        std::fs::remove_file(store.manifest_path(&"b".repeat(64))).unwrap();
        assert_eq!(unreferenced_cas_actions(&store, &[]).unwrap().len(), 1);
    }

    #[test]
    fn remote_output_names_reject_traversal_and_separators() {
        for bad in [
            "",
            "/etc/passwd",
            "../escape",
            "a/../b",
            "a/b",
            "a\\b",
            "C:\\Windows",
            "..",
        ] {
            assert!(!safe_output_name(bad), "{bad} should be rejected");
        }
        for good in ["libfoo.rlib", "bar-1.0.d", ".hidden", "x"] {
            assert!(safe_output_name(good), "{good} should be accepted");
        }
    }

    #[test]
    fn remote_object_refs_require_64_lower_hex_digests() {
        let digest = "a".repeat(64);
        assert!(valid_object_ref(&ObjectRef {
            digest: digest.clone(),
            size: 1,
            mode: 0o444,
        }));
        let too_long = format!("{digest}a");
        for bad in [&digest[..63], too_long.as_str(), "../ab", "zz"] {
            let object = ObjectRef {
                digest: bad.to_string(),
                size: 1,
                mode: 0o444,
            };
            assert!(!valid_object_ref(&object), "{bad} should be rejected");
        }
        let mut uppercase = ObjectRef {
            digest: "A".repeat(64),
            size: 1,
            mode: 0o444,
        };
        assert!(valid_object_ref(&uppercase), "uppercase hex is valid hex");
        uppercase.digest = "../".to_string() + &"a".repeat(61);
        assert!(!valid_object_ref(&uppercase));
    }
}
