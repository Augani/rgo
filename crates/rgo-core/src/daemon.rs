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
use crate::db::StateDb;
use crate::gc::{self, Inputs};
use crate::ipc::{self, Connection, Listener};
use crate::paths::RgoPaths;
use rgo_cas::{MANIFEST_VERSION, Manifest, ManifestOutput, ObjectRef, Store};

const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct State {
    paths: RgoPaths,
    cfg: Resolved,
    db: Arc<Mutex<StateDb>>,
    cas: Store,
    operation_lock: Arc<Mutex<()>>,
    pid: u32,
}

struct InstanceLock {
    _file: File,
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
    db.reconcile(&paths)?;
    reconcile_cache(&mut db, &cas)?;
    drain_cache_events(&paths, &db)?;
    let db = Arc::new(Mutex::new(db));
    let listener = Listener::bind(&paths.socket_path())?;
    let state = State {
        paths: paths.clone(),
        cfg,
        db,
        cas,
        operation_lock: Arc::new(Mutex::new(())),
        pid: std::process::id(),
    };

    tracing::info!(socket = %paths.socket_path().display(), "rgo daemon listening");
    let maintenance_state = state.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(POLL_INTERVAL);
            if let Err(error) = maintenance(&maintenance_state) {
                tracing::warn!(%error, "daemon maintenance failed");
            }
        }
    });
    loop {
        match listener.accept() {
            Ok(connection) => {
                let state = state.clone();
                thread::spawn(move || {
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
        } => Ok(Response::Gc(run_gc(state, dry_run, aggressive)?)),
        Request::Pin { build_dir } => {
            validate_managed_path(&state.paths, Path::new(&build_dir))?;
            let db = state.db.lock().unwrap();
            db.set_pin(Path::new(&build_dir), true)?;
            Ok(Response::Ok)
        }
        Request::Unpin { build_dir } => {
            let db = state.db.lock().unwrap();
            db.set_pin(Path::new(&build_dir), false)?;
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
        Request::CachePublish { manifest } => {
            if !state.cfg.cache.enabled {
                return Ok(Response::CacheMiss {
                    reason: "cache_disabled".into(),
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
            Ok(Response::CacheStats(db.cache_stats(
                state.cfg.cache.enabled,
                state.cas.object_bytes()?,
            )?))
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
    });
    let cache = {
        let db = state.db.lock().unwrap();
        db.cache_stats(state.cfg.cache.enabled, state.cas.object_bytes()?)?
    };
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
    })
}

fn run_gc(state: &State, dry_run: bool, aggressive: bool) -> Result<GcReport> {
    let _operation = state.operation_lock.lock().unwrap();
    let contexts = context::list(&state.paths)?;
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
    let _operation = state.operation_lock.lock().unwrap();
    let contexts = context::list(&state.paths)?;
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.reconcile_contexts(&contexts)?;
    reconcile_cache(&mut db, &state.cas)?;
    drain_cache_events(&state.paths, &db)?;
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
