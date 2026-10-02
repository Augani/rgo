//! rgo-rustc-wrapper — invoked by Cargo as `<wrapper> <rustc> <args...>`.
//!
//! Phase 1 contract: be invisible. Do one cheap side effect (write/refresh the
//! build-dir sidecar so `rgo` can attribute the build-dir to a workspace) and then
//! `exec` the real compiler with identical args, env, stdio and exit code.
//! Any internal failure is swallowed: rustc always runs.
//!
//! Phase 3 adds the cacheability classifier + CAS lookup in front of the exec.
//! Everything here must stay fast: no heavy deps, no network, no blocking on a daemon.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use rgo_cas::{MANIFEST_VERSION, Manifest, ManifestOutput, ObjectRef, Store};
use rgo_key::{
    AllowedRoots, BypassReason, Candidate, Classification, classify, validate_file_inputs,
    validate_keyed_roots,
};
use rgo_materialize::materialize;
use rgo_protocol::{
    BYPASS_ENV, CACHE_EVENT_LOG_FILE, CACHE_EVENT_LOG_LOCK, CACHE_EVENT_LOG_MAX_BYTES,
    CACHE_EVENT_LOG_TRUNCATED, CLIENT_TIMEOUT_MILLIS, CacheEvent, CacheManifest, CacheObject,
    CacheOutput, ContextSidecar, DEFAULT_HEARTBEAT_SECS, DEFAULT_LEASE_TTL_SECS, HOME_ENV,
    LEASE_ENV, LeaseScope, MAX_FRAME_SIZE, PROTOCOL_VERSION, Request, Response, SIDECAR_FILE,
    decode_frame, encode_frame,
};

mod platform {
    use std::io;
    #[cfg(windows)]
    use std::io::{Read, Write};
    use std::path::Path;
    use std::time::Duration;
    #[cfg(windows)]
    use std::time::Instant;

    use interprocess::local_socket::{ConnectOptions, prelude::*};

    #[cfg(unix)]
    pub type Stream = interprocess::local_socket::Stream;
    #[cfg(windows)]
    pub struct Stream {
        inner: interprocess::local_socket::Stream,
        deadline: Instant,
    }

    #[cfg(unix)]
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
        use interprocess::local_socket::GenericFilePath;
        let name = path.to_fs_name::<GenericFilePath>()?;
        let stream = ConnectOptions::new().name(name).connect_sync()?;
        set_timeout(&stream, timeout)?;
        Ok(stream)
    }

    #[cfg(windows)]
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
        use interprocess::local_socket::GenericNamespaced;
        let name = path
            .to_string_lossy()
            .into_owned()
            .to_ns_name::<GenericNamespaced>()?;
        let inner = ConnectOptions::new()
            .name(name)
            .wait_mode(interprocess::ConnectWaitMode::Timeout(timeout))
            .connect_sync()?;
        Ok(Stream {
            inner,
            deadline: Instant::now() + timeout,
        })
    }

    #[cfg(unix)]
    fn set_timeout(stream: &Stream, timeout: Duration) -> io::Result<()> {
        use interprocess::local_socket::traits::Stream as _;
        stream.set_recv_timeout(Some(timeout))?;
        stream.set_send_timeout(Some(timeout))
    }

    #[cfg(windows)]
    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            use std::os::windows::io::AsHandle;
            let interprocess::local_socket::Stream::NamedPipe(pipe) = &self.inner;
            rgo_winpipe::read(pipe.inner().as_handle(), buf, self.deadline)
        }
    }

    #[cfg(windows)]
    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            use std::os::windows::io::AsHandle;
            let interprocess::local_socket::Stream::NamedPipe(pipe) = &self.inner;
            let written = rgo_winpipe::write(pipe.inner().as_handle(), buf, self.deadline)?;
            if written > 0 {
                pipe.inner().mark_dirty();
            }
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }
}

type PlatformStream = platform::Stream;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("rgo-rustc-wrapper: usage: <rustc> <args...>");
        std::process::exit(2);
    };
    if rustc == "--rgo-version" && args.len() == 0 {
        println!(
            "rgo-rustc-wrapper {} protocol {}",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        );
        return;
    }
    let args: Vec<OsString> = args.collect();

    let mut context_lease_id = None;
    if std::env::var_os(BYPASS_ENV).is_none() {
        if let Some(build_dir) = attribute(&args) {
            let workspace_root = workspace_root(&build_dir);
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

    let Some(build_dir) = attribute(&args) else {
        finish_passthrough(rustc, &args, context_lease_id);
    };
    let Some(candidate) = classify_invocation(Path::new(&rustc), &args, &build_dir) else {
        finish_passthrough(rustc, &args, context_lease_id);
    };
    if configured_inner_wrapper().is_some_and(|inner| {
        !is_sccache(Path::new(&inner)) || candidate.source_kind != rgo_key::SourceKind::Workspace
    }) {
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
            let restore_leases = [context_lease_id, lease_id]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let (restore_stop, restore_heartbeat) = start_lease_heartbeats(restore_leases);
            #[cfg(debug_assertions)]
            pause_at_cache_stage("RGO_TEST_CACHE_RESTORE_PAUSE");
            let restored = materialize_hit(&store, &candidate, &manifest).is_ok();
            if restored {
                replay_output(&store, manifest.stdout.as_ref(), true);
                replay_output(&store, manifest.stderr.as_ref(), false);
                record_event(Some(candidate.key.to_string()), "hit", 0, None);
            }
            stop_lease_heartbeats(restore_stop, restore_heartbeat);
            if let Some(id) = lease_id {
                let _ = request(Request::ReleaseLease { lease_id: id });
            }
            if restored {
                finish_success(context_lease_id);
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
        // A burst of parallel rustc invocations can stall responses past the
        // short socket timeout (commit writes serialize in the daemon); retry
        // for a bounded window before degrading to a plain compile so a hit is
        // not silently lost.
        let response = match request(message.clone()) {
            Ok(response) => response,
            Err(_error) => {
                let retry_deadline =
                    std::time::Instant::now() + Duration::from_secs(2).min(timeout);
                let mut retried = None;
                while std::time::Instant::now() < retry_deadline {
                    thread::sleep(Duration::from_millis(100));
                    if let Ok(response) = request(message.clone()) {
                        retried = Some(response);
                        break;
                    }
                }
                match retried {
                    Some(response) => response,
                    None => {
                        record_event(Some(key), "bypass", 0, Some("daemon_unreachable".into()));
                        return None;
                    }
                }
            }
        };
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
    // Compare the compiler's outputs against the state before launching it.
    // Filesystems can round mtimes below a high-resolution start timestamp;
    // that must not make a newly created output look absent.
    let before = snapshot_outputs(candidate);
    let result = run_captured_with_leases(
        &rustc,
        &candidate.compiler_args,
        context_lease_id,
        cache_lease_id,
        configured_inner_wrapper().is_some()
            && candidate.source_kind != rgo_key::SourceKind::Workspace,
    );
    // rustc has exited, but collecting its output and publishing the CAS
    // objects can still take longer than a lease TTL. Keep both protections
    // alive until publication and commit have finished.
    let publication_leases = if result.status.success() && cache_lease_id.is_some() {
        [context_lease_id, cache_lease_id]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (publication_stop, publication_heartbeat) = start_lease_heartbeats(publication_leases);
    #[cfg(debug_assertions)]
    if result.status.success() && cache_lease_id.is_some() {
        pause_at_cache_stage("RGO_TEST_CACHE_PUBLICATION_PAUSE");
    }
    if let Some(lease_id) = cache_lease_id {
        if result.status.success() {
            match before.and_then(|before| publish_result(store, candidate, &result, &before)) {
                Ok(manifest) => {
                    // Publication is outside the rustc hot path, and the daemon
                    // may be flushing the manifest under concurrent CI load.
                    // If the request does not reach the daemon, the next build
                    // must recompile instead of seeing an uncommitted entry.
                    let commit = request_with_timeout(
                        Request::CacheCommit {
                            key: candidate.key.to_string(),
                            lease_id,
                            manifest: wire_manifest(&manifest),
                        },
                        Duration::from_secs(2),
                    );
                    match commit {
                        Ok(Response::CacheCommitted { accepted: true }) => {}
                        Ok(other) => record_event(
                            Some(candidate.key.to_string()),
                            "bypass",
                            0,
                            Some(format!("cache_commit_rejected: {other:?}")),
                        ),
                        Err(error) => record_event(
                            Some(candidate.key.to_string()),
                            "bypass",
                            0,
                            Some(format!("cache_commit_failed: {error}")),
                        ),
                    }
                }
                Err(error) => {
                    record_event(
                        Some(candidate.key.to_string()),
                        "bypass",
                        0,
                        Some(format!("cache_publish_failed: {error:#}")),
                    );
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
    stop_lease_heartbeats(publication_stop, publication_heartbeat);
    if let Some(lease_id) = context_lease_id {
        let _ = request(Request::ReleaseLease { lease_id });
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
    std::env::var_os(HOME_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let cargo_home = std::env::var_os("CARGO_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .or_else(|| std::env::var_os("USERPROFILE"))
                        .map(|home| PathBuf::from(home).join(".cargo"))
                })?;
            let value = std::fs::read_to_string(cargo_home.join(".rgo-home")).ok()?;
            let root = PathBuf::from(value.trim_end_matches(['\r', '\n']));
            root.is_absolute().then_some(root)
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|path| PathBuf::from(path).join(".rgo"))
        })
}

fn classify_invocation(rustc: &Path, args: &[OsString], build_dir: &Path) -> Option<Candidate> {
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
            // Normalize paths relative to this invocation's validated context,
            // preserving the remaining profile/artifact suffix in the key.
            build_root: build_dir.to_path_buf(),
            source_roots: vec![
                cargo_home.join("registry").join("src"),
                cargo_home.join("git").join("checkouts"),
            ],
            workspace_roots: workspace_root(build_dir)
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            remap_workspace_paths: remap_workspace_paths_enabled(),
        },
    ) {
        Classification::Cacheable(candidate) => {
            if std::env::var_os("RGO_KEY_DEBUG").is_some() {
                let mut entry = Vec::new();
                let _ = writeln!(
                    entry,
                    "=== {} key={} src_digest={} out_dir_digest={:?}",
                    candidate.crate_name,
                    candidate.key,
                    candidate.source_digest,
                    candidate.out_dir_digest
                );
                for arg in &candidate.normalized_args {
                    let _ = writeln!(entry, "  {arg}");
                }
                let _ = append_bounded_log(
                    &home,
                    "key-debug.log",
                    "key-debug.lock",
                    "key-debug.truncated",
                    CACHE_EVENT_LOG_MAX_BYTES,
                    &entry,
                );
            }
            Some(*candidate)
        }
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
    let configured = std::env::var_os("RGO_INNER_RUSTC_WRAPPER").or_else(|| {
        rgo_home()
            .and_then(|home| std::fs::read_to_string(home.join("state/inner-wrapper")).ok())
            .map(|value| OsString::from(value.trim()))
            .filter(|value| !value.is_empty())
    })?;
    let path = Path::new(&configured);
    let is_self = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("rgo-rustc-wrapper"));
    let resolves_to_self = std::env::current_exe()
        .ok()
        .and_then(|current| {
            let configured = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir().ok()?.join(path)
            };
            Some((current, configured))
        })
        .is_some_and(|(current, configured)| {
            std::fs::canonicalize(current).ok() == std::fs::canonicalize(configured).ok()
        });
    if is_self || resolves_to_self {
        None
    } else {
        Some(configured)
    }
}

fn materialize_hit(
    store: &Store,
    candidate: &Candidate,
    manifest: &CacheManifest,
) -> Result<(), String> {
    if manifest.key != candidate.key.as_str() {
        return Err("cache manifest key mismatch".into());
    }
    validate_keyed_roots(candidate).map_err(|error| error.to_string())?;
    let mut used = vec![false; candidate.outputs.len()];
    let mut destinations = Vec::with_capacity(manifest.outputs.len());
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
        destinations.push((output.name.clone(), destination));
    }
    for (output, (_, destination)) in manifest.outputs.iter().zip(&destinations) {
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
            destination,
            object.mode,
            // A hardlink shares a mutable inode with the CAS object. Clone or
            // copy so edits to the build output cannot corrupt cached bytes.
            false,
        )
        .map_err(|error| error.to_string())?;
        if output.kind == "dep-info" {
            let bytes = store
                .read_object(&object)
                .map_err(|error| error.to_string())?;
            let relocated = relocate_dep_info_targets(&bytes, &destinations, false)?;
            std::fs::write(destination, relocated).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// rustc writes absolute producer output targets into its Makefile-style
/// dep-info. Cargo reads the dependencies, but external build tools also read
/// the targets, so a hit must name the consumer's materialized files.
fn relocate_dep_info_targets(
    bytes: &[u8],
    outputs: &[(String, PathBuf)],
    verify_producer: bool,
) -> Result<Vec<u8>, String> {
    let contents = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let mut by_name = HashMap::new();
    for (name, path) in outputs {
        if path.to_str().is_none() || by_name.insert(name.as_str(), path).is_some() {
            return Err("ambiguous or non-UTF-8 dep-info output path".into());
        }
    }
    let mut seen = HashSet::new();
    let mut relocated = String::with_capacity(contents.len());
    for line in contents.split_inclusive('\n') {
        let (body, ending) = line
            .strip_suffix('\n')
            .map_or((line, ""), |body| (body, "\n"));
        if let Some((target, dependencies)) = body.split_once(": ") {
            let name = Path::new(target)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| "invalid dep-info target".to_owned())?;
            let destination = by_name
                .get(name)
                .ok_or_else(|| format!("dep-info names an uncollected output: {name}"))?;
            if verify_producer {
                let actual = std::fs::canonicalize(target).map_err(|error| error.to_string())?;
                let expected =
                    std::fs::canonicalize(destination).map_err(|error| error.to_string())?;
                if actual != expected {
                    return Err(format!(
                        "dep-info output differs from collected file: {name}"
                    ));
                }
            }
            relocated.push_str(&destination.to_string_lossy());
            relocated.push_str(": ");
            relocated.push_str(dependencies);
            relocated.push_str(ending);
            seen.insert(name);
        } else {
            relocated.push_str(line);
        }
    }
    if seen.is_empty() {
        return Err("dep-info has no output rule".into());
    }
    Ok(relocated.into_bytes())
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
}

fn run_captured_with_leases(
    rustc: &OsString,
    args: &[OsString],
    context_lease_id: Option<u64>,
    cache_lease_id: Option<u64>,
    inner: bool,
) -> Captured {
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
    let (stop, heartbeat) = start_lease_heartbeats(lease_ids);
    let status = child.wait().unwrap_or_else(|_| exit_status(127));
    let stdout_bytes = stdout_thread.join().unwrap_or_default();
    let stderr_bytes = stderr_thread.join().unwrap_or_default();
    stop_lease_heartbeats(stop, heartbeat);
    let _ = std::io::stdout().write_all(&stdout_bytes);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(&stderr_bytes);
    let _ = std::io::stderr().flush();
    Captured {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }
}

type LeaseHeartbeat = (Option<mpsc::Sender<()>>, Option<thread::JoinHandle<()>>);

fn start_lease_heartbeats(lease_ids: Vec<u64>) -> LeaseHeartbeat {
    if lease_ids.is_empty() {
        return (None, None);
    }
    let (stop, stop_thread) = mpsc::channel();
    let heartbeat = thread::spawn(move || {
        loop {
            match stop_thread.recv_timeout(Duration::from_secs(u64::from(DEFAULT_HEARTBEAT_SECS))) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    for lease_id in &lease_ids {
                        // A busy daemon can miss one short IPC deadline. Keep
                        // refreshing the other lease and retry on the next tick.
                        let _ = request(Request::Heartbeat {
                            lease_id: *lease_id,
                        });
                    }
                }
            }
        }
    });
    (Some(stop), Some(heartbeat))
}

fn stop_lease_heartbeats(
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<thread::JoinHandle<()>>,
) {
    if let Some(stop) = stop {
        let _ = stop.send(());
    }
    if let Some(heartbeat) = heartbeat {
        let _ = heartbeat.join();
    }
}

#[cfg(debug_assertions)]
fn pause_at_cache_stage(env_var: &str) {
    let Some(dir) = std::env::var_os(env_var) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = std::fs::write(dir.join("ready"), b"");
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    while !dir.join("release").exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
}

fn publish_result(
    store: &Store,
    candidate: &Candidate,
    result: &Captured,
    before: &OutputSnapshot,
) -> anyhow::Result<Manifest> {
    let outputs = collect_outputs(candidate, before)?;
    if outputs.is_empty() {
        anyhow::bail!(
            "eligible rustc invocation produced no new or detectably changed cacheable outputs"
        );
    }
    let mut dep_infos = outputs.iter().filter(|(kind, _)| kind == "dep-info");
    let dep_info = dep_infos
        .next()
        .ok_or_else(|| anyhow::anyhow!("compiler result has no fresh dep-info"))?;
    anyhow::ensure!(
        dep_infos.next().is_none(),
        "compiler result has multiple dep-info files"
    );
    validate_file_inputs(candidate, &dep_info.1)?;
    let dep_info_bytes = std::fs::read(&dep_info.1)?;
    let named_outputs = outputs
        .iter()
        .map(|(_, path)| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 compiler output name"))?;
            Ok((name.to_owned(), path.clone()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    relocate_dep_info_targets(&dep_info_bytes, &named_outputs, true).map_err(anyhow::Error::msg)?;
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
    // The daemon publishes the manifest only after validating our live
    // producer lease. Writing it here would expose a cache hit even if
    // CacheCommit is later rejected or never reaches the daemon.
    Ok(manifest)
}

type OutputSnapshot = HashMap<PathBuf, (u64, Option<SystemTime>)>;

fn snapshot_outputs(candidate: &Candidate) -> anyhow::Result<OutputSnapshot> {
    listed_outputs(candidate)?
        .into_iter()
        .map(|(_, path)| {
            let metadata = std::fs::metadata(&path)?;
            Ok((path, (metadata.len(), metadata.modified().ok())))
        })
        .collect()
}

fn collect_outputs(
    candidate: &Candidate,
    before: &OutputSnapshot,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut outputs = Vec::new();
    for (kind, path) in listed_outputs(candidate)? {
        let metadata = std::fs::metadata(&path)?;
        let current = (metadata.len(), metadata.modified().ok());
        if before.get(&path) != Some(&current) {
            outputs.push((kind, path));
        }
    }
    Ok(outputs)
}

fn listed_outputs(candidate: &Candidate) -> anyhow::Result<Vec<(String, PathBuf)>> {
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
            if !path.is_file()
                || !is_output_for_kind(&path, &spec.kind)
                || !is_invocation_output(candidate, &path)
            {
                continue;
            }
            outputs.push((spec.kind.clone(), path));
        }
    }
    outputs.sort_by(|left, right| left.1.cmp(&right.1));
    outputs.dedup_by(|left, right| left.1 == right.1);
    Ok(outputs)
}

/// `--out-dir` is shared by every crate in the graph, so a directory scan can only claim
/// files whose stem names this invocation (`{lib?}{crate_name}{extra_filename}`). Anything
/// else is a concurrently-built sibling's output and must not enter this crate's manifest.
fn is_invocation_output(candidate: &Candidate, path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|v| v.to_str()) else {
        return false;
    };
    let base = format!(
        "{}{}",
        candidate.crate_name,
        candidate.extra_filename.as_deref().unwrap_or("")
    );
    stem == base || stem == format!("lib{base}")
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
    let event = CacheEvent {
        key,
        outcome: outcome.into(),
        bytes,
        reason,
    };
    if let Ok(mut entry) = serde_json::to_vec(&event) {
        entry.push(b'\n');
        let _ = append_bounded_log(
            &home,
            CACHE_EVENT_LOG_FILE,
            CACHE_EVENT_LOG_LOCK,
            CACHE_EVENT_LOG_TRUNCATED,
            CACHE_EVENT_LOG_MAX_BYTES,
            &entry,
        );
    }
}

/// A stable file lock serializes wrapper appends with daemon rotation. A full
/// log stops accepting events and leaves a durable marker so stats are not
/// presented as complete. The compiler path never fails because of logging.
fn append_bounded_log(
    home: &Path,
    filename: &str,
    lock_name: &str,
    marker_name: &str,
    max_bytes: u64,
    entry: &[u8],
) -> std::io::Result<bool> {
    let state = home.join("state");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join(lock_name))?;
    lock.lock_exclusive()?;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join(filename))?;
    if entry.len() as u64 > max_bytes.saturating_sub(log.metadata()?.len()) {
        let _ = std::fs::write(state.join(marker_name), b"1\n");
        return Ok(false);
    }
    log.write_all(entry)?;
    Ok(true)
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
    let explicit_manifest = std::env::var_os("RGO_MANIFEST_PATH");
    let sidecar_path = build_dir.join(SIDECAR_FILE);
    let existing: Option<ContextSidecar> = match std::fs::read_to_string(&sidecar_path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(sidecar) => Some(sidecar),
            Err(_) => return Some(build_dir),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Some(build_dir),
    };
    // The launcher writes the resolved root before rustc starts. Older native
    // wrapper sidecars can contain a member root and need one-time validation.
    if explicit_manifest.is_none() && existing.as_ref().is_some_and(|sc| sc.workspace_verified) {
        return Some(build_dir);
    }
    if std::env::var_os("CARGO_PRIMARY_PACKAGE").is_none() && explicit_manifest.is_none() {
        return Some(build_dir);
    }
    // CARGO_MANIFEST_DIR identifies the package, not necessarily its workspace.
    // Resolve only when a context is new or carries a legacy unverified sidecar.
    let manifest_path = match explicit_manifest {
        Some(path) => PathBuf::from(path),
        None => {
            let package_manifest =
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?).join("Cargo.toml");
            resolve_workspace_manifest(&package_manifest)?
        }
    };
    let workspace_root = manifest_path.parent()?.to_path_buf();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if let Some(e) = &existing {
        // Refresh at most once a day to avoid write churn on every rustc invocation.
        if now.saturating_sub(e.last_seen) < 86_400
            && e.workspace_verified
            && e.manifest_path == manifest_path.to_string_lossy()
            && (cfg!(not(unix)) || e.workspace_device.is_some())
        {
            return Some(build_dir);
        }
    }
    let supervised_origin = existing.as_ref().is_some_and(|sidecar| {
        sidecar.is_current_supervised()
            && sidecar.workspace_root == workspace_root.to_string_lossy()
            && sidecar.manifest_path == manifest_path.to_string_lossy()
    });
    let sc = ContextSidecar {
        version: PROTOCOL_VERSION,
        workspace_verified: true,
        supervised_origin,
        supervision_version: if supervised_origin {
            rgo_protocol::SUPERVISED_CONTEXT_VERSION
        } else {
            0
        },
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        workspace_device: {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(&workspace_root).ok().map(|m| m.dev())
            }
            #[cfg(windows)]
            {
                windows_volume_serial(&workspace_root)
            }
            #[cfg(not(any(unix, windows)))]
            {
                None
            }
        },
        workspace_mount_id: {
            #[cfg(target_os = "linux")]
            {
                use rustix::fs::{AtFlags, CWD, StatxFlags, statx};
                statx(CWD, &workspace_root, AtFlags::empty(), StatxFlags::MNT_ID)
                    .ok()
                    .and_then(|stat| {
                        (stat.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id)
                    })
            }
            #[cfg(not(target_os = "linux"))]
            {
                None
            }
        },
        toolchain: std::env::var("RUSTUP_TOOLCHAIN").ok(),
        first_seen: existing.map(|e| e.first_seen).unwrap_or(now),
        last_seen: now,
    };
    let tmp = build_dir.join(format!("{SIDECAR_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&sc).ok()?).ok()?;
    std::fs::rename(tmp, sidecar_path).ok()?;
    Some(build_dir)
}

fn resolve_workspace_manifest(package_manifest: &Path) -> Option<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format", "json"])
        .arg("--manifest-path")
        .arg(package_manifest)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let path = PathBuf::from(report.get("root")?.as_str()?);
    if !path.is_absolute() || !path.is_file() {
        return None;
    }
    let path = path.canonicalize().ok()?;
    #[cfg(windows)]
    let path = regular_windows_path(path)?;
    path.parent()?.to_str()?;
    Some(path)
}

#[cfg(windows)]
fn regular_windows_path(path: PathBuf) -> Option<PathBuf> {
    let raw = path.to_str()?;
    let Some(rest) = raw.strip_prefix(r"\\?\") else {
        return Some(path);
    };
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        let regular = PathBuf::from(format!(r"\\{unc}"));
        return Some(if regular.is_file() { regular } else { path });
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        let regular = PathBuf::from(rest);
        return Some(if regular.is_file() { regular } else { path });
    }
    Some(path)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_volume_serial(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let success = unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) != 0 };
    let _ = unsafe { CloseHandle(handle) };
    success
        .then(|| u64::from(unsafe { info.assume_init().dwVolumeSerialNumber }))
        .filter(|serial| *serial != 0)
}

fn workspace_root(build_dir: &Path) -> Option<String> {
    std::fs::read_to_string(build_dir.join(SIDECAR_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<ContextSidecar>(&text).ok())
        .filter(|sidecar| sidecar.workspace_verified)
        .map(|sidecar| sidecar.workspace_root)
        .or_else(|| {
            std::env::var_os("RGO_MANIFEST_PATH")
                .and_then(|manifest| PathBuf::from(manifest).parent().map(Path::to_path_buf))
                .map(|root| root.to_string_lossy().into_owned())
        })
}

fn request(message: Request) -> Result<Response, String> {
    request_with_timeout(message, Duration::from_millis(CLIENT_TIMEOUT_MILLIS))
}

fn request_with_timeout(message: Request, timeout: Duration) -> Result<Response, String> {
    let home = rgo_home().ok_or_else(|| "cannot determine RGO_HOME".to_owned())?;
    let socket = home.join("state").join("daemon.sock");
    let mut stream = platform::connect(&socket, timeout).map_err(|e| e.to_string())?;
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
    let builds = rgo_home()?.join("builds");
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
    let bypass = std::env::var_os(BYPASS_ENV).is_some();
    let mut command = if bypass {
        let mut command = Command::new(&rustc);
        command.args(args);
        command
    } else {
        compiler_command(&rustc, args)
    };
    if bypass {
        strip_rgo_environment(&mut command);
    }
    let err = command.exec();
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
    let bypass = std::env::var_os(BYPASS_ENV).is_some();
    let mut command = if bypass {
        let mut command = Command::new(&rustc);
        command.args(args);
        command
    } else {
        compiler_command(&rustc, args)
    };
    if bypass {
        strip_rgo_environment(&mut command);
    }
    match command.status() {
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

fn strip_rgo_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RGO_") {
            command.env_remove(key);
        }
    }
}

#[cfg(test)]
mod bounded_log_tests {
    use super::append_bounded_log;

    #[test]
    fn full_log_stays_bounded_and_marks_observations_incomplete() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("state")).unwrap();
        assert!(
            append_bounded_log(home.path(), "events", "lock", "truncated", 8, b"1234").unwrap()
        );
        assert!(
            !append_bounded_log(home.path(), "events", "lock", "truncated", 8, b"56789").unwrap()
        );
        assert_eq!(
            std::fs::read(home.path().join("state/events")).unwrap(),
            b"1234"
        );
        assert!(home.path().join("state/truncated").is_file());
    }
}
