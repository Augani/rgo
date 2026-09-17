//! The local coordination daemon. It owns all state-changing metadata operations and serializes
//! lease admission with GC so a new build cannot race a deletion decision.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rgo_protocol::{GcAction, GcReport, PROTOCOL_VERSION, Request, Response,
    StatusReport};

use crate::config::{Resolved, volume_free_bytes};
use crate::context;
use crate::db::StateDb;
use crate::gc::{self, Inputs};
use crate::ipc::{self, Connection, Listener};
use crate::paths::RgoPaths;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct State {
    paths: RgoPaths,
    cfg: Resolved,
    db: Arc<Mutex<StateDb>>,
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
    std::fs::write(paths.state_dir().join("daemon.pid"), format!("{}\n", std::process::id()))?;

    let mut db = StateDb::open(&paths)?;
    db.reconcile(&paths)?;
    let db = Arc::new(Mutex::new(db));
    let listener = Listener::bind(&paths.socket_path())?;
    let state = State {
        paths: paths.clone(),
        cfg,
        db,
        operation_lock: Arc::new(Mutex::new(())),
        pid: std::process::id(),
    };

    tracing::info!(socket = %paths.socket_path().display(), "rgo daemon listening");
    let maintenance_state = state.clone();
    thread::spawn(move || loop {
        thread::sleep(POLL_INTERVAL);
        if let Err(error) = maintenance(&maintenance_state) {
            tracing::warn!(%error, "daemon maintenance failed");
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
            ipc::write_message(&mut connection, &Response::Hello { version: PROTOCOL_VERSION })?;
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
        Request::AcquireLease { scope, pid, ttl_secs } => {
            let _operation = state.operation_lock.lock().unwrap();
            let db = state.db.lock().unwrap();
            let (lease_id, expires_in_secs) = db.acquire(&scope, pid, ttl_secs)?;
            Ok(Response::Lease { lease_id, expires_in_secs })
        }
        Request::BindLease { lease_id, build_dir, workspace_root } => {
            let db = state.db.lock().unwrap();
            db.bind(lease_id, Path::new(&build_dir), workspace_root.as_deref())?;
            Ok(Response::Ok)
        }
        Request::Heartbeat { lease_id } => {
            let db = state.db.lock().unwrap();
            let expires_in_secs = db.heartbeat(lease_id, rgo_protocol::DEFAULT_LEASE_TTL_SECS)?;
            Ok(Response::Lease { lease_id, expires_in_secs })
        }
        Request::ReleaseLease { lease_id } => {
            let db = state.db.lock().unwrap();
            db.release(lease_id)?;
            Ok(Response::Ok)
        }
        Request::Touch { build_dir, workspace_root, physical_bytes, incremental_bytes } => {
            let db = state.db.lock().unwrap();
            db.touch(Path::new(&build_dir), workspace_root.as_deref(), physical_bytes, incremental_bytes)?;
            Ok(Response::Ok)
        }
        Request::QueryStatus => Ok(Response::Status(status_report(state)?)),
        Request::TriggerGc { dry_run, aggressive } => Ok(Response::Gc(run_gc(state, dry_run, aggressive)?)),
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
        Request::Hello { .. } => bail!("duplicate Hello"),
    }
}

fn status_report(state: &State) -> Result<StatusReport> {
    let _operation = state.operation_lock.lock().unwrap();
    let contexts = context::list(&state.paths)?;
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.reconcile_contexts(&contexts)?;
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
    Ok(StatusReport {
        managed_bytes: contexts.iter().map(|c| c.usage.physical_bytes).sum(),
        incremental_bytes: contexts.iter().map(|c| c.incremental_usage.physical_bytes).sum(),
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
    let plan = gc::plan(&Inputs {
        paths: &state.paths,
        cfg: &state.cfg,
        contexts: &contexts,
        pinned: &pinned,
        leased: &leased,
        now: SystemTime::now(),
        aggressive,
    });
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

fn maintenance(state: &State) -> Result<()> {
    let _operation = state.operation_lock.lock().unwrap();
    let contexts = context::list(&state.paths)?;
    let mut db = state.db.lock().unwrap();
    db.expire_leases()?;
    db.reconcile_contexts(&contexts)?;
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
