//! rgo-rustc-wrapper — invoked by Cargo as `<wrapper> <rustc> <args...>`.
//!
//! Phase 1 contract: be invisible. Do one cheap side effect (write/refresh the
//! build-dir sidecar so `rgo` can attribute the build-dir to a workspace) and then
//! `exec` the real compiler with identical args, env, stdio and exit code.
//! Any internal failure is swallowed: rustc always runs.
//!
//! Phase 3 adds the cacheability classifier + CAS lookup in front of the exec.
//! Everything here must stay fast: no heavy deps, no network, no blocking on a daemon.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use rgo_cas::{MANIFEST_VERSION, Manifest, ManifestOutput, ObjectRef, Store};
use rgo_key::{AllowedRoots, BypassReason, Candidate, Classification, classify};
use rgo_materialize::materialize;
use rgo_protocol::{
    BYPASS_ENV, CLIENT_TIMEOUT_MILLIS, CacheEvent, CacheManifest, CacheObject, CacheOutput,
    ContextSidecar, DEFAULT_HEARTBEAT_SECS, DEFAULT_LEASE_TTL_SECS, HOME_ENV, LEASE_ENV,
    LeaseScope, MAX_FRAME_SIZE, PROTOCOL_VERSION, Request, Response, SIDECAR_FILE, decode_frame,
    encode_frame,
};

#[cfg(unix)]
type PlatformStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type PlatformStream = uds_windows::UnixStream;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("rgo-rustc-wrapper: usage: <rustc> <args...>");
        std::process::exit(2);
    };
    let args: Vec<OsString> = args.collect();

    let mut context_lease_id = None;
    if std::env::var_os(BYPASS_ENV).is_none() {
        if let Some(build_dir) = attribute(&args) {
            let workspace_root = workspace_root();
            if let Some(raw) = std::env::var_os(LEASE_ENV) {
                if let Ok(id) = raw.to_string_lossy().parse::<u64>() {
                    let bound = request(Request::BindLease {
                        lease_id: id,
                        build_dir: build_dir.to_string_lossy().into_owned(),
                        workspace_root: workspace_root.clone(),
                    })
                    .is_ok();
                    if !bound {
                        context_lease_id = acquire_context_lease(&build_dir);
                    } else {
                        context_lease_id = Some(id);
                    }
                }
            } else if let Ok(Response::Lease { lease_id: id, .. }) =
                request(Request::AcquireLease {
                    scope: LeaseScope::Context {
                        build_dir: build_dir.to_string_lossy().into_owned(),
                    },
                    pid: std::process::id(),
                    ttl_secs: DEFAULT_LEASE_TTL_SECS,
                })
            {
                context_lease_id = Some(id);
            }
        }
    }

    if std::env::var_os(BYPASS_ENV).is_some() {
        finish_passthrough(rustc, &args, context_lease_id);
    }

    let Some(_build_dir) = attribute(&args) else {
        finish_passthrough(rustc, &args, context_lease_id);
    };
    let Some(candidate) = classify_invocation(Path::new(&rustc), &args) else {
        finish_passthrough(rustc, &args, context_lease_id);
    };
    if let Some(inner) = configured_inner_wrapper()
        && (!is_sccache(Path::new(&inner))
            || candidate.source_kind != rgo_key::SourceKind::Workspace)
    {
        record_event(
            None,
            "bypass",
            0,
            Some(BypassReason::InnerWrapper.to_string()),
        );
        finish_passthrough(rustc, &args, context_lease_id);
    }

    let Some(home) = rgo_home() else {
        finish_passthrough(rustc, &args, context_lease_id);
    };
    let store = match Store::new(home.join("cas"), home.join("quarantine")) {
        Ok(store) => store,
        Err(_error) => finish_passthrough(rustc, &args, context_lease_id),
    };
    match acquire_cache_role(&candidate) {
        Some(CacheRole::Ready { manifest, lease_id }) => {
            if materialize_hit(&store, &candidate, &manifest).is_ok() {
                replay_output(&store, manifest.stdout.as_ref(), true);
                replay_output(&store, manifest.stderr.as_ref(), false);
                record_event(Some(candidate.key.to_string()), "hit", 0, None);
                if let Some(id) = lease_id {
                    let _ = request(Request::ReleaseLease { lease_id: id });
                }
                finish_success(context_lease_id);
            }
            if let Some(id) = lease_id {
                let _ = request(Request::ReleaseLease { lease_id: id });
            }
            record_event(
                Some(candidate.key.to_string()),
                "miss",
                0,
                Some("materialization_failed".into()),
            );
            run_cache_producer(rustc, &candidate, &store, context_lease_id, None);
        }
        Some(CacheRole::Producer { lease_id }) => {
            run_cache_producer(rustc, &candidate, &store, context_lease_id, Some(lease_id));
        }
        None => finish_passthrough(rustc, &args, context_lease_id),
    }
}

enum CacheRole {
    Producer {
        lease_id: u64,
    },
    Ready {
        manifest: CacheManifest,
        lease_id: Option<u64>,
    },
}

fn acquire_cache_role(candidate: &Candidate) -> Option<CacheRole> {
    let key = candidate.key.to_string();
    let timeout = cache_wait_timeout();
    let deadline = std::time::Instant::now() + timeout;
    let mut message = Request::CacheAcquire {
        key: key.clone(),
        pid: std::process::id(),
        ttl_secs: DEFAULT_LEASE_TTL_SECS,
    };
    loop {
        let response = request(message).ok()?;
        match response {
            Response::CacheProducer { lease_id, .. } => {
                return Some(CacheRole::Producer { lease_id });
            }
            Response::CacheReady { manifest, lease_id }
            | Response::CacheHit { manifest, lease_id } => {
                return Some(CacheRole::Ready { manifest, lease_id });
            }
            Response::CacheWait {
                retry_after_millis, ..
            } => {
                if std::time::Instant::now() >= deadline {
                    record_event(
                        Some(key.clone()),
                        "timeout",
                        0,
                        Some("single_flight_timeout".into()),
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(retry_after_millis.min(100)));
                message = Request::CacheWait {
                    key: key.clone(),
                    pid: std::process::id(),
                    ttl_secs: DEFAULT_LEASE_TTL_SECS,
                };
            }
            Response::CacheRemotePending {
                retry_after_millis, ..
            } => {
                if std::time::Instant::now() >= deadline {
                    record_event(
                        Some(key.clone()),
                        "timeout",
                        0,
                        Some("remote_fetch_timeout".into()),
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(retry_after_millis.min(100)));
                message = Request::CacheWait {
                    key: key.clone(),
                    pid: std::process::id(),
                    ttl_secs: DEFAULT_LEASE_TTL_SECS,
                };
            }
            Response::CacheMiss { reason } => {
                record_event(None, "bypass", 0, Some(reason));
                return None;
            }
            Response::CacheFailed { reason }
            | Response::CacheRemoteFailed { reason, .. }
            | Response::Error {
                message: reason, ..
            } => {
                record_event(Some(key), "bypass", 0, Some(reason));
                return None;
            }
            _ => return None,
        }
    }
}

fn run_cache_producer(
    rustc: OsString,
    candidate: &Candidate,
    store: &Store,
    context_lease_id: Option<u64>,
    cache_lease_id: Option<u64>,
) -> ! {
    let result = run_captured_with_leases(
        &rustc,
        &candidate.compiler_args,
        context_lease_id,
        cache_lease_id,
        configured_inner_wrapper().is_some()
            && candidate.source_kind != rgo_key::SourceKind::Workspace,
    );
    if let Some(lease_id) = cache_lease_id {
        if result.status.success() {
            match publish_result(store, candidate, &result) {
                Ok(manifest) => {
                    let _ = request(Request::CacheCommit {
                        key: candidate.key.to_string(),
                        lease_id,
                        manifest: wire_manifest(&manifest),
                    });
                }
                Err(error) => {
                    let _ = request(Request::CacheFail {
                        key: candidate.key.to_string(),
                        lease_id,
                        reason: format!("publish: {error:#}"),
                    });
                }
            }
        } else {
            let _ = request(Request::CacheFail {
                key: candidate.key.to_string(),
                lease_id,
                reason: "compiler_failed".into(),
            });
        }
    }
    if result.status.success() {
        record_event(
            Some(candidate.key.to_string()),
            "miss",
            result.stdout.len() as u64 + result.stderr.len() as u64,
            None,
        );
    }
    exit_with_status(result.status);
}

fn finish_passthrough(rustc: OsString, args: &[OsString], lease_id: Option<u64>) -> ! {
    if let Some(lease_id) = lease_id {
        run_supervised(rustc, args, lease_id);
    } else {
        exec(rustc, args);
    }
}

fn finish_success(lease_id: Option<u64>) -> ! {
    if let Some(lease_id) = lease_id {
        let _ = request(Request::ReleaseLease { lease_id });
    }
    std::process::exit(0);
}

fn rgo_home() -> Option<PathBuf> {
    std::env::var_os(HOME_ENV).map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|path| PathBuf::from(path).join(".rgo"))
    })
}

fn classify_invocation(rustc: &Path, args: &[OsString]) -> Option<Candidate> {
    let home = rgo_home()?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".cargo")))?;
    let env = std::env::vars_os().collect::<Vec<_>>();
    match classify(
        rustc,
        args,
        &env,
        &AllowedRoots {
            build_root: home.join("builds"),
            source_roots: vec![
                cargo_home.join("registry").join("src"),
                cargo_home.join("git").join("checkouts"),
            ],
            workspace_roots: workspace_root().into_iter().map(PathBuf::from).collect(),
            remap_workspace_paths: remap_workspace_paths_enabled(),
        },
    ) {
        Classification::Cacheable(candidate) => Some(*candidate),
        Classification::Bypass(reason) => {
            record_event(None, "bypass", 0, Some(reason.to_string()));
            None
        }
    }
}

fn remap_workspace_paths_enabled() -> bool {
    let Some(home) = rgo_home() else { return false };
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return false;
    };
    let mut in_cache = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_cache = line == "[cache]";
            continue;
        }
        if in_cache
            && line.split_once('=').is_some_and(|(key, value)| {
                key.trim() == "remap_workspace_paths" && value.trim() == "true"
            })
        {
            return true;
        }
    }
    false
}

fn cache_wait_timeout() -> Duration {
    let Some(home) = rgo_home() else {
        return Duration::from_secs(rgo_protocol::DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS as u64);
    };
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return Duration::from_secs(rgo_protocol::DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS as u64);
    };
    let mut in_cache = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_cache = line == "[cache]";
            continue;
        }
        if in_cache && line.starts_with("single_flight_timeout") {
            if let Some(value) = line.split('=').nth(1) {
                let value = value.trim().trim_matches('"');
                if let Some(duration) = parse_wait_duration(value) {
                    return duration;
                }
            }
        }
    }
    Duration::from_secs(rgo_protocol::DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS as u64)
}

fn parse_wait_duration(value: &str) -> Option<Duration> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 0.001)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1.0)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60.0)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3600.0)
    } else {
        (value, 1.0)
    };
    let seconds = number.trim().parse::<f64>().ok()? * multiplier;
    (seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn is_sccache(wrapper: &Path) -> bool {
    wrapper
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("sccache"))
}

fn configured_inner_wrapper() -> Option<OsString> {
    std::env::var_os("RGO_INNER_RUSTC_WRAPPER").or_else(|| {
        rgo_home()
            .and_then(|home| std::fs::read_to_string(home.join("state/inner-wrapper")).ok())
            .map(|value| OsString::from(value.trim()))
            .filter(|value| !value.is_empty())
    })
}

fn materialize_hit(
    store: &Store,
    candidate: &Candidate,
    manifest: &CacheManifest,
) -> Result<(), String> {
    if manifest.key != candidate.key.as_str() {
        return Err("cache manifest key mismatch".into());
    }
    let mut used = vec![false; candidate.outputs.len()];
    for output in &manifest.outputs {
        let Some((index, spec)) = candidate
            .outputs
            .iter()
            .enumerate()
            .find(|(index, spec)| !used[*index] && spec.kind == output.kind)
        else {
            return Err(format!(
                "manifest output kind has no target: {}",
                output.kind
            ));
        };
        used[index] = true;
        let destination = if spec.path.is_dir() {
            spec.path.join(&output.name)
        } else {
            spec.path.clone()
        };
        let object = ObjectRef {
            digest: output.object.digest.clone(),
            size: output.object.size,
            mode: output.object.mode,
        };
        store
            .verify_object(&object)
            .map_err(|error| error.to_string())?;
        materialize(
            &store.object_path(&object.digest),
            &destination,
            object.mode,
            true,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn replay_output(store: &Store, object: Option<&CacheObject>, stdout: bool) {
    let Some(object) = object else { return };
    let native = ObjectRef {
        digest: object.digest.clone(),
        size: object.size,
        mode: object.mode,
    };
    let Ok(bytes) = store.read_object(&native) else {
        return;
    };
    if stdout {
        let _ = std::io::stdout().write_all(&bytes);
        let _ = std::io::stdout().flush();
    } else {
        let _ = std::io::stderr().write_all(&bytes);
        let _ = std::io::stderr().flush();
    }
}

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    started_at: SystemTime,
}

fn run_captured_with_leases(
    rustc: &OsString,
    args: &[OsString],
    context_lease_id: Option<u64>,
    cache_lease_id: Option<u64>,
    inner: bool,
) -> Captured {
    let started_at = SystemTime::now();
    let mut command = if inner {
        let wrapper = configured_inner_wrapper().unwrap_or_else(|| OsString::from(rustc));
        let mut command = Command::new(wrapper);
        command.arg(rustc);
        command.args(args);
        command
    } else {
        let mut command = Command::new(rustc);
        command.args(args);
        command
    };
    let mut child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "rgo-rustc-wrapper: failed to run {}: {error}",
                rustc.to_string_lossy()
            );
            return Captured {
                status: exit_status(127),
                stdout: Vec::new(),
                stderr: Vec::new(),
                started_at,
            };
        }
    };
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let lease_ids = [context_lease_id, cache_lease_id]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let (stop, heartbeat) = if lease_ids.is_empty() {
        (None, None)
    } else {
        let (stop, stop_thread) = mpsc::channel();
        let heartbeat = thread::spawn(move || {
            loop {
                match stop_thread
                    .recv_timeout(Duration::from_secs(u64::from(DEFAULT_HEARTBEAT_SECS)))
                {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        for lease_id in &lease_ids {
                            if request(Request::Heartbeat {
                                lease_id: *lease_id,
                            })
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        });
        (Some(stop), Some(heartbeat))
    };
    let status = child.wait().unwrap_or_else(|_| exit_status(127));
    if let Some(stop) = stop {
        let _ = stop.send(());
    }
    if let Some(heartbeat) = heartbeat {
        let _ = heartbeat.join();
    }
    let stdout_bytes = stdout_thread.join().unwrap_or_default();
    let stderr_bytes = stderr_thread.join().unwrap_or_default();
    let _ = std::io::stdout().write_all(&stdout_bytes);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(&stderr_bytes);
    let _ = std::io::stderr().flush();
    if let Some(lease_id) = context_lease_id {
        let _ = request(Request::ReleaseLease { lease_id });
    }
    Captured {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        started_at,
    }
}

fn publish_result(
    store: &Store,
    candidate: &Candidate,
    result: &Captured,
) -> anyhow::Result<Manifest> {
    let outputs = collect_outputs(candidate, result.started_at)?;
    if outputs.is_empty() {
        anyhow::bail!("eligible rustc invocation produced no cacheable outputs");
    }
    let output_refs = outputs
        .into_iter()
        .map(|(kind, path)| {
            let mode = file_mode(&path);
            let object = store.put_file(&path, mode)?;
            Ok(ManifestOutput {
                kind,
                name: path
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("output")
                    .into(),
                object,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let stdout = (!result.stdout.is_empty())
        .then(|| store.put_bytes(&result.stdout, 0o644))
        .transpose()?;
    let stderr = (!result.stderr.is_empty())
        .then(|| store.put_bytes(&result.stderr, 0o644))
        .transpose()?;
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        key: candidate.key.to_string(),
        outputs: output_refs,
        stdout,
        stderr,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    store.write_manifest(&manifest)?;
    Ok(manifest)
}

fn collect_outputs(
    candidate: &Candidate,
    started_at: SystemTime,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut outputs = Vec::new();
    for spec in &candidate.outputs {
        if spec.path.is_file() {
            outputs.push((spec.kind.clone(), spec.path.clone()));
            continue;
        }
        if !spec.path.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&spec.path)? {
            let path = entry?.path();
            if !path.is_file() || !is_output_for_kind(&path, &spec.kind) {
                continue;
            }
            let modified = std::fs::metadata(&path)?.modified().unwrap_or(UNIX_EPOCH);
            if modified >= started_at {
                outputs.push((spec.kind.clone(), path));
            }
        }
    }
    outputs.sort_by(|left, right| left.1.cmp(&right.1));
    outputs.dedup_by(|left, right| left.1 == right.1);
    Ok(outputs)
}

fn is_output_for_kind(path: &Path, kind: &str) -> bool {
    match kind {
        "dep-info" => path.extension().and_then(|v| v.to_str()) == Some("d"),
        "metadata" => path.extension().and_then(|v| v.to_str()) == Some("rmeta"),
        "link" => matches!(
            path.extension().and_then(|v| v.to_str()),
            Some("rlib" | "rmeta" | "so" | "dylib")
        ),
        _ => false,
    }
}

fn file_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

fn wire_manifest(manifest: &Manifest) -> CacheManifest {
    CacheManifest {
        version: manifest.version,
        key: manifest.key.clone(),
        outputs: manifest
            .outputs
            .iter()
            .map(|output| CacheOutput {
                kind: output.kind.clone(),
                name: output.name.clone(),
                object: CacheObject {
                    digest: output.object.digest.clone(),
                    size: output.object.size,
                    mode: output.object.mode,
                },
            })
            .collect(),
        stdout: manifest.stdout.as_ref().map(|object| CacheObject {
            digest: object.digest.clone(),
            size: object.size,
            mode: object.mode,
        }),
        stderr: manifest.stderr.as_ref().map(|object| CacheObject {
            digest: object.digest.clone(),
            size: object.size,
            mode: object.mode,
        }),
        created_at: manifest.created_at,
    }
}

fn record_event(key: Option<String>, outcome: &str, bytes: u64, reason: Option<String>) {
    let Some(home) = rgo_home() else { return };
    let path = home.join("state/cache-events.log");
    let event = CacheEvent {
        key,
        outcome: outcome.into(),
        bytes,
        reason,
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(&event) {
        let _ = file.write_all(&bytes);
        let _ = file.write_all(b"\n");
    }
}

fn exit_status(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code)
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new("cmd")
            .args(["/C", "exit", &code.to_string()])
            .status()
            .unwrap()
    }
}

fn exit_with_status(status: ExitStatus) -> ! {
    std::process::exit(exit_code(status));
}

fn compiler_command(rustc: &OsString, args: &[OsString]) -> Command {
    if let Some(wrapper) = configured_inner_wrapper() {
        let mut command = Command::new(wrapper);
        command.arg(rustc).args(args);
        command
    } else {
        let mut command = Command::new(rustc);
        command.args(args);
        command
    }
}

fn acquire_context_lease(build_dir: &Path) -> Option<u64> {
    match request(Request::AcquireLease {
        scope: LeaseScope::Context {
            build_dir: build_dir.to_string_lossy().into_owned(),
        },
        pid: std::process::id(),
        ttl_secs: DEFAULT_LEASE_TTL_SECS,
    }) {
        Ok(Response::Lease { lease_id, .. }) => Some(lease_id),
        _ => None,
    }
}

fn attribute(args: &[OsString]) -> Option<PathBuf> {
    let out_dir = arg_value(args, "--out-dir")?;
    let build_dir = find_managed_build_dir(Path::new(&out_dir))?;
    if std::env::var_os("CARGO_PRIMARY_PACKAGE").is_none()
        && std::env::var_os("RGO_MANIFEST_PATH").is_none()
    {
        return Some(build_dir);
    }
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
    // CARGO_MANIFEST_DIR is the *package*; RGO_MANIFEST_PATH (set by `rgo <cmd>`) is the workspace root.
    let manifest_path = std::env::var_os("RGO_MANIFEST_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&manifest_dir).join("Cargo.toml"));
    let workspace_root = manifest_path.parent()?.to_path_buf();

    let sidecar_path = build_dir.join(SIDECAR_FILE);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let existing: Option<ContextSidecar> = std::fs::read_to_string(&sidecar_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(e) = &existing {
        // Refresh at most once a day to avoid write churn on every rustc invocation.
        if now.saturating_sub(e.last_seen) < 86_400
            && e.manifest_path == manifest_path.to_string_lossy()
        {
            return Some(build_dir);
        }
    }
    let sc = ContextSidecar {
        version: PROTOCOL_VERSION,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        toolchain: std::env::var("RUSTUP_TOOLCHAIN").ok(),
        first_seen: existing.map(|e| e.first_seen).unwrap_or(now),
        last_seen: now,
    };
    let tmp = build_dir.join(format!("{SIDECAR_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&sc).ok()?).ok()?;
    std::fs::rename(tmp, sidecar_path).ok()?;
    Some(build_dir)
}

fn workspace_root() -> Option<String> {
    let manifest = std::env::var_os("RGO_MANIFEST_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_MANIFEST_DIR").map(|p| PathBuf::from(p).join("Cargo.toml"))
        })?;
    manifest.parent().map(|p| p.to_string_lossy().into_owned())
}

fn request(message: Request) -> Result<Response, String> {
    let home = std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .map(|p| p.join(".rgo"))
        })
        .ok_or_else(|| "cannot determine RGO_HOME".to_owned())?;
    let socket = home.join("state").join("daemon.sock");
    let mut stream = PlatformStream::connect(socket).map_err(|e| e.to_string())?;
    let timeout = Some(Duration::from_millis(CLIENT_TIMEOUT_MILLIS));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| e.to_string())?;
    write_frame(
        &mut stream,
        &Request::Hello {
            version: PROTOCOL_VERSION,
            client: "rustc-wrapper".into(),
        },
    )?;
    match read_message::<Response>(&mut stream)? {
        Response::Hello { version } if version == PROTOCOL_VERSION => {}
        Response::Hello { version } => return Err(format!("protocol mismatch: {version}")),
        Response::Error { message, .. } => return Err(message),
        _ => return Err("invalid handshake response".into()),
    }
    write_frame(&mut stream, &message)?;
    read_message(&mut stream)
}

fn write_frame<T: serde::Serialize>(stream: &mut PlatformStream, value: &T) -> Result<(), String> {
    let frame = encode_frame(value).map_err(|e| e.to_string())?;
    stream.write_all(&frame).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

fn read_message<T: for<'de> serde::Deserialize<'de>>(
    stream: &mut PlatformStream,
) -> Result<T, String> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME_SIZE {
        return Err("invalid IPC frame length".into());
    }
    let mut frame = vec![0u8; len + 4];
    frame[..4].copy_from_slice(&header);
    stream
        .read_exact(&mut frame[4..])
        .map_err(|e| e.to_string())?;
    decode_frame(&frame).map_err(|e| e.to_string())
}

fn run_supervised(rustc: OsString, args: &[OsString], lease_id: u64) -> ! {
    let (stop, stop_thread) = mpsc::channel();
    let heartbeat = thread::spawn(move || {
        loop {
            match stop_thread.recv_timeout(Duration::from_secs(u64::from(DEFAULT_HEARTBEAT_SECS))) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = request(Request::Heartbeat { lease_id });
                }
            }
        }
    });
    let status = compiler_command(&rustc, args).status();
    let _ = stop.send(());
    let _ = heartbeat.join();
    let _ = request(Request::ReleaseLease { lease_id });
    match status {
        Ok(status) => std::process::exit(exit_code(status)),
        Err(error) => {
            eprintln!(
                "rgo-rustc-wrapper: failed to run {}: {error}",
                rustc.to_string_lossy()
            );
            std::process::exit(127);
        }
    }
}

fn arg_value(args: &[OsString], flag: &str) -> Option<OsString> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(rest) = a
            .to_str()
            .and_then(|s| s.strip_prefix(flag))
            .and_then(|s| s.strip_prefix('='))
        {
            return Some(rest.into());
        }
    }
    None
}

/// Walk up from `--out-dir` until the grandparent is `$RGO_HOME/builds`
/// (Cargo's `{workspace-path-hash}` expands to `xx/yyyy…`).
fn find_managed_build_dir(out_dir: &Path) -> Option<PathBuf> {
    let builds = match std::env::var_os(HOME_ENV) {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?)
            .join(".rgo"),
    }
    .join("builds");
    let mut p = out_dir;
    while let Some(parent) = p.parent() {
        if parent.parent() == Some(builds.as_path()) {
            return Some(p.to_path_buf());
        }
        p = parent;
    }
    None
}

#[cfg(unix)]
fn exec(rustc: OsString, args: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = compiler_command(&rustc, args).exec();
    eprintln!(
        "rgo-rustc-wrapper: failed to exec {}: {err}",
        rustc.to_string_lossy()
    );
    std::process::exit(127);
}

#[cfg(unix)]
fn exit_code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(1))
}

#[cfg(not(unix))]
fn exit_code(s: std::process::ExitStatus) -> i32 {
    s.code().unwrap_or(1)
}

#[cfg(not(unix))]
fn exec(rustc: OsString, args: &[OsString]) -> ! {
    match compiler_command(&rustc, args).status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(err) => {
            eprintln!(
                "rgo-rustc-wrapper: failed to run {}: {err}",
                rustc.to_string_lossy()
            );
            std::process::exit(127);
        }
    }
}
