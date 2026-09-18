//! The local coordination daemon. It owns all state-changing metadata operations and serializes
//! lease admission with GC so a new build cannot race a deletion decision.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rgo_protocol::{
    CacheManifest as WireManifest, CacheObject as WireObject, CacheOutput as WireOutput, GcAction,
    GcReport, PROTOCOL_VERSION, Request, Response, StatusReport,
};

use crate::config::{Resolved, volume_free_bytes};
use crate::context;
use crate::db::{CacheBuildDecision, RemoteFetchDecision, StateDb};
use crate::gc::{self, Inputs};
use crate::ipc::{self, Connection, Listener};
use crate::paths::RgoPaths;
use rgo_cas::{MANIFEST_VERSION, Manifest, ManifestOutput, ObjectRef, Store};
use rgo_remote::{Client as RemoteClient, Config as RemoteConfig, Fetch as RemoteFetch};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
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
    pid: u32,
    remote: Option<RemoteClient>,
    remote_error: Arc<Mutex<Option<String>>>,
    connection_count: Arc<std::sync::atomic::AtomicUsize>,
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
    let _instance_lock = InstanceLock { _file: file };
    std::fs::write(
        paths.state_dir().join("daemon.pid"),
        format!("{}\n", std::process::id()),
    )?;

    let cas = Store::new(paths.cas_dir(), paths.quarantine_dir())?;
    let mut db = StateDb::open(&paths)?;
    db.recover_cache_builds()?;
    db.recover_remote_jobs()?;
    db.reconcile(&paths)?;
    reconcile_cache(&mut db, &cas)?;
    drain_cache_events(&paths, &db)?;
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
        pid: std::process::id(),
        remote,
        remote_error,
        connection_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };

    tracing::info!(socket = %paths.socket_path().display(), "rgo daemon listening");
    let maintenance_state = state.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(poll_interval());
            if let Err(error) = maintenance(&maintenance_state) {
                tracing::warn!(%error, "daemon maintenance failed");
            }
        }
    });
    loop {
        match listener.accept() {
            Ok(connection) => {
                let state = state.clone();
                let Some(permit) = ConnectionPermit::try_acquire(&state.connection_count) else {
                    continue;
                };
                thread::spawn(move || {
                    let _permit = permit;
                    if let Err(error) = serve_connection(connection, &state) {
                        tracing::debug!(%error, "daemon client disconnected with error");
                    }
                });
            }
            Err(error) => return Err(error).context("accepting daemon client"),
        }
    }
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
    let response = handle_request(state, request);
    ipc::write_message(&mut connection, &response)
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
    match request {
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
            let path = Path::new(&build_dir);
            validate_managed_path(&state.paths, path)?;
            context::write_pin_marker(path)?;
            let db = state.db.lock().unwrap();
            db.set_pin(path, true)?;
            Ok(Response::Ok)
        }
        Request::Unpin { build_dir } => {
            let path = Path::new(&build_dir);
            validate_managed_path(&state.paths, path)?;
            context::remove_pin_marker(path)?;
            let db = state.db.lock().unwrap();
            db.set_pin(path, false)?;
            Ok(Response::Ok)
        }
        Request::Clean { build_dir } => {
            validate_managed_path(&state.paths, Path::new(&build_dir))?;
            let _operation = state.operation_lock.lock().unwrap();
            let contexts = context::list(&state.paths)?;
            let db = state.db.lock().unwrap();
            db.expire_leases()?;
            let pinned = db.pinned_paths()?;
            let leased = db.protected_paths(&contexts)?;
            let path = Path::new(&build_dir);
            if same_path_in(&pinned, path) {
                bail!("context is pinned; unpin it before cleaning");
            }
            if same_path_in(&leased, path) {
                bail!("context has an active lease; refusing to clean");
            }
            let Some(context) = contexts.iter().find(|c| same_path(&c.dir, path)) else {
                bail!("managed context does not exist: {}", path.display());
            };
            let plan = gc::Plan {
                actions: vec![gc::Action {
                    tier: gc::Tier::StaleContext,
                    path: context.dir.clone(),
                    bytes: context.usage.physical_bytes,
                    reason: "requested".into(),
                }],
                ..Default::default()
            };
            drop(db);
            let reclaimed = gc::execute(&state.paths, &plan, false)?;
            let contexts = context::list(&state.paths)?;
            let mut db = state.db.lock().unwrap();
            db.reconcile_contexts(&contexts)?;
            Ok(Response::Gc(GcReport {
                reclaimed_bytes: reclaimed,
                planned_bytes: plan.reclaim_bytes(),
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
        Request::CachePublish { manifest } => {
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
            let manifest = native_manifest(&manifest)?;
            state.cas.write_manifest(&manifest)?;
            let db = state.db.lock().unwrap();
            db.record_cache_manifest(
                &wire_manifest(&manifest),
                &state.cas.manifest_path(&manifest.key),
            )?;
            Ok(Response::Ok)
        }
        Request::RecordCacheEvent { event } => {
            let db = state.db.lock().unwrap();
            db.record_cache_event(&event)?;
            Ok(Response::Ok)
        }
        Request::QueryCacheStats => {
            let db = state.db.lock().unwrap();
            drain_cache_events(&state.paths, &db)?;
            let mut report = db.cache_stats(state.cfg.cache.enabled, state.cas.object_bytes()?)?;
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
    let contexts = context::list(&state.paths)?;
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.prune_failed_cache_builds(Duration::from_secs(7 * 86_400))?;
    db.reconcile_contexts(&contexts)?;
    drain_cache_events(&state.paths, &db)?;
    let pinned = db.pinned_paths()?;
    let leased = db.protected_paths(&contexts)?;
    let stats = db.stats()?;
    let last_gc = db.last_gc()?.unwrap_or_default();
    drop(db);
    let plan = gc::plan(&Inputs {
        paths: &state.paths,
        cfg: &state.cfg,
        contexts: &contexts,
        pinned: &pinned,
        leased: &leased,
        now: SystemTime::now(),
        aggressive: true,
        target_bytes: None,
    });
    let cache = {
        let db = state.db.lock().unwrap();
        let mut report = db.cache_stats(state.cfg.cache.enabled, state.cas.object_bytes()?)?;
        report.remote = remote_status_from_db(state, &db)?;
        report
    };
    let remote = cache.remote.clone();
    Ok(StatusReport {
        managed_bytes: contexts.iter().map(|c| c.usage.physical_bytes).sum(),
        incremental_bytes: contexts
            .iter()
            .map(|c| c.incremental_usage.physical_bytes)
            .sum(),
        reclaimable_bytes: plan.reclaim_bytes(),
        soft_watermark_bytes: state.cfg.soft_watermark,
        hard_limit_bytes: state.cfg.max_size,
        volume_free_bytes: volume_free_bytes(&state.paths.root).unwrap_or(0),
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
    let contexts = context::list(&state.paths)?;
    let managed_bytes: u64 = contexts
        .iter()
        .map(|context| context.usage.physical_bytes)
        .sum();
    let free_bytes = volume_free_bytes(&state.paths.root).unwrap_or(u64::MAX);
    let trigger = managed_bytes > state.cfg.soft_watermark || free_bytes < state.cfg.min_free_space;
    if auto && !trigger {
        return Ok(GcReport {
            dry_run,
            managed_bytes,
            target_bytes: managed_bytes,
            ..Default::default()
        });
    }
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.reconcile_contexts(&contexts)?;
    let pinned = db.pinned_paths()?;
    let leased = db.protected_paths(&contexts)?;
    let active_cache_keys = db.active_cache_keys()?;
    let plan = gc::plan(&Inputs {
        paths: &state.paths,
        cfg: &state.cfg,
        contexts: &contexts,
        pinned: &pinned,
        leased: &leased,
        now: SystemTime::now(),
        aggressive,
        target_bytes,
    });
    let mut plan = plan;
    append_unreferenced_cas(&mut plan, &state.cas, &active_cache_keys)?;
    drop(db);
    let actions = plan
        .actions
        .iter()
        .map(|action| GcAction {
            tier: action.tier as u8,
            path: action.path.to_string_lossy().into_owned(),
            bytes: action.bytes,
            reason: action.reason.clone(),
        })
        .collect();
    let mut report = GcReport {
        dry_run,
        managed_bytes: plan.managed_bytes,
        target_bytes: plan.target_bytes,
        reclaimed_bytes: 0,
        planned_bytes: plan.reclaim_bytes(),
        skipped_live: plan.skipped_live as u64,
        skipped_leased: plan.skipped_leased as u64,
        actions,
    };
    if !dry_run {
        match gc::execute(&state.paths, &plan, false) {
            Ok(reclaimed) => report.reclaimed_bytes = reclaimed,
            Err(error) => {
                let message = format!("{error:#}");
                let db = state.db.lock().unwrap();
                db.record_gc(dry_run, aggressive, &report, Some(&message))?;
                return Err(error);
            }
        }
        let contexts = context::list(&state.paths)?;
        let mut db = state.db.lock().unwrap();
        db.reconcile_contexts(&contexts)?;
        db.record_gc(dry_run, aggressive, &report, None)?;
        return Ok(report);
    }
    let db = state.db.lock().unwrap();
    db.record_gc(dry_run, aggressive, &report, None)?;
    Ok(report)
}

fn append_unreferenced_cas(
    plan: &mut gc::Plan,
    cas: &Store,
    _active_cache_keys: &[String],
) -> Result<()> {
    let manifests = cas.list_manifests()?;
    let mut referenced = std::collections::HashSet::new();
    for manifest in manifests {
        referenced.extend(
            manifest
                .outputs
                .iter()
                .map(|output| output.object.digest.clone()),
        );
        if let Some(object) = manifest.stdout {
            referenced.insert(object.digest);
        }
        if let Some(object) = manifest.stderr {
            referenced.insert(object.digest);
        }
    }
    let objects = cas.root().join("objects");
    if !objects.is_dir() {
        return Ok(());
    }
    for shard in std::fs::read_dir(objects)? {
        for entry in std::fs::read_dir(shard?.path())? {
            let path = entry?.path();
            let Some(digest) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if path.is_file() && !referenced.contains(digest) {
                let bytes = std::fs::metadata(&path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                plan.actions.push(gc::Action {
                    tier: gc::Tier::Pressure,
                    path,
                    bytes,
                    reason: "unreferenced CAS object".into(),
                });
            }
        }
    }
    plan.actions.sort_by_key(|action| action.tier);
    Ok(())
}

fn maintenance(state: &State) -> Result<()> {
    let should_gc = {
        let _operation = state.operation_lock.lock().unwrap();
        let contexts = context::list(&state.paths)?;
        let managed_bytes: u64 = contexts
            .iter()
            .map(|context| context.usage.physical_bytes)
            .sum();
        let free_bytes = volume_free_bytes(&state.paths.root).unwrap_or(u64::MAX);
        let should_gc = state.cfg.gc.auto
            && (managed_bytes > state.cfg.soft_watermark || free_bytes < state.cfg.min_free_space);
        let mut db = state.db.lock().unwrap();
        db.expire_leases()?;
        db.prune_failed_cache_builds(Duration::from_secs(7 * 86_400))?;
        db.reconcile_contexts(&contexts)?;
        reconcile_cache(&mut db, &state.cas)?;
        drain_cache_events(&state.paths, &db)?;
        should_gc
    };
    if should_gc {
        let _ = run_gc(state, false, false, true, None)?;
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
    for manifest in cas.list_manifests()? {
        db.record_cache_manifest(&wire_manifest(&manifest), &cas.manifest_path(&manifest.key))?;
    }
    Ok(())
}

fn drain_cache_events(paths: &RgoPaths, db: &StateDb) -> Result<()> {
    let source = paths.state_dir().join("cache-events.log");
    if !source.is_file() {
        return Ok(());
    }
    let drained = paths
        .state_dir()
        .join(format!("cache-events.{}.drain", std::process::id()));
    if std::fs::rename(&source, &drained).is_err() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&drained)?;
    for line in text.lines() {
        if let Ok(event) = serde_json::from_str::<rgo_protocol::CacheEvent>(line) {
            db.record_cache_event(&event)?;
        }
    }
    let _ = std::fs::remove_file(drained);
    Ok(())
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
