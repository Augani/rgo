//! Daemon/lease integration tests run real Cargo in a private Sandbox.

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use assert_cmd::cargo::cargo_bin;
use rgo_core::ipc;
#[cfg(unix)]
use rgo_protocol::PROTOCOL_VERSION;
use rgo_protocol::{Request, Response};
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(unsafe_code)]
#[test]
fn daemon_closes_inherited_nonstdio_descriptors() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    let (mut reader, writer) = UnixStream::pair().unwrap();
    assert_eq!(
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let mut daemon = sb
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground", "--home"])
        .arg(&sb.rgo_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(writer);
    reader.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match reader.read(&mut [0u8; 1]) {
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            result => break result,
        }
    };
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(
        result.unwrap(),
        0,
        "daemon retained an inherited pipe descriptor"
    );
}

fn start_daemon(sb: &Sandbox) -> Child {
    start_daemon_with_poll(sb, None)
}

fn start_daemon_with_poll(sb: &Sandbox, poll_secs: Option<&str>) -> Child {
    let mut command = sb.cmd(cargo_bin("rgo"));
    command
        .args(["daemon", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(poll_secs) = poll_secs {
        command.env("RGO_DAEMON_POLL_SECS", poll_secs);
    }
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if matches!(
            ipc::request_with_timeout(
                &sb.rgo_home.join("state/daemon.sock"),
                Request::QueryRemoteStatus,
                Duration::from_millis(100)
            ),
            Ok(Response::RemoteStatus(_))
        ) {
            return child;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "daemon exited during startup ({status}): {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    let diagnostics = std::fs::read_to_string(sb.rgo_home.join("logs/daemon.log"))
        .unwrap_or_else(|error| format!("daemon log unavailable: {error}"));
    panic!(
        "daemon did not answer IPC: {diagnostics}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(debug_assertions)]
#[test]
fn pressure_removal_admits_a_late_lease_and_preserves_the_next_context() {
    staged_context_removal_admits_a_late_lease("pressure");
}

#[cfg(debug_assertions)]
#[test]
fn stale_context_removal_admits_a_late_lease_and_preserves_the_next_context() {
    staged_context_removal_admits_a_late_lease("stale");
}

#[cfg(debug_assertions)]
#[test]
fn incremental_removal_admits_a_late_lease_and_preserves_the_next_context() {
    staged_context_removal_admits_a_late_lease("incremental");
}

#[cfg(debug_assertions)]
fn staged_context_removal_admits_a_late_lease(mode: &str) {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    let config = match mode {
        "stale" => {
            "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n[gc]\ncontext_retention = '0s'\n"
        }
        "incremental" => {
            "[storage]\nmax_size = '10MB'\nmin_free_space = '0B'\n[gc]\nincremental_retention = '0s'\n"
        }
        _ => "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n",
    };
    std::fs::write(paths.config_file(), config).unwrap();
    let workspaces = [
        sb.simple_bin("pressure-first").unwrap(),
        sb.simple_bin("pressure-second").unwrap(),
    ];
    let contexts = [
        paths.builds_dir().join("aa/first"),
        paths.builds_dir().join("bb/second"),
    ];
    for (workspace, context) in workspaces.iter().zip(&contexts) {
        std::fs::create_dir_all(context).unwrap();
        std::fs::write(context.join("payload"), vec![b'x'; 1024 * 1024]).unwrap();
        if mode == "incremental" {
            let incremental = context.join("debug/incremental/cache");
            std::fs::create_dir_all(&incremental).unwrap();
            std::fs::write(incremental.join("payload"), vec![b'y'; 1024 * 1024]).unwrap();
        }
        rgo_core::context::write_supervised_sidecar(
            context,
            workspace,
            &workspace.join("Cargo.toml"),
            true,
        )
        .unwrap();
    }

    let marker = sb.home.join("pressure-staged");
    let release = sb.home.join("pressure-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_CONTEXT_STAGED_MARKER", &marker)
            .env("RGO_TEST_CONTEXT_STAGED_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let gc_socket = socket.clone();
    let incremental = mode == "incremental";
    let gc = thread::spawn(move || {
        ipc::request_with_timeout(
            &gc_socket,
            Request::TriggerGc {
                dry_run: false,
                aggressive: false,
                auto: incremental,
                target_bytes: (!incremental).then_some(0),
            },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "GC did not stage a context");
        thread::sleep(Duration::from_millis(25));
    }
    let victim = PathBuf::from(std::fs::read_to_string(&marker).unwrap());
    let victim_context = contexts
        .iter()
        .find(|context| victim.starts_with(context))
        .unwrap();
    let protected = contexts
        .iter()
        .find(|context| *context != victim_context)
        .unwrap();
    assert!(protected.exists());
    assert!(
        rgo_core::supervision::try_lock_gc(&paths, Some(victim_context))
            .unwrap()
            .is_none(),
        "context removal released Cargo's lifecycle guard"
    );
    let lease = ipc::request_with_timeout(
        &socket,
        Request::AcquireLease {
            scope: rgo_protocol::LeaseScope::Context {
                build_dir: protected.to_string_lossy().into_owned(),
            },
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(matches!(lease, Response::Lease { .. }));
    std::fs::write(&daemon.release, b"release").unwrap();
    let response = gc.join().unwrap().unwrap();
    let Response::Gc(report) = response else {
        panic!("GC did not return a report: {response:?}");
    };
    assert!(!victim.exists());
    assert!(protected.exists());
    if mode == "incremental" {
        assert!(protected.join("debug/incremental/cache").exists());
    }
    assert!(report.skipped_execution_actions >= 1);
}

#[cfg(debug_assertions)]
#[test]
fn gc_plan_admits_a_late_context_lease_before_deletion() {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n[gc]\ncontext_retention = '0s'\n",
    )
    .unwrap();
    let workspace = sb.simple_bin("late-lease").unwrap();
    let context = paths.builds_dir().join("aa/late-lease");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(context.join("payload"), vec![b'x'; 1024 * 1024]).unwrap();
    rgo_core::context::write_supervised_sidecar(
        &context,
        &workspace,
        &workspace.join("Cargo.toml"),
        true,
    )
    .unwrap();
    let marker = sb.home.join("gc-planned");
    let release = sb.home.join("gc-plan-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_GC_PLANNED_MARKER", &marker)
            .env("RGO_TEST_GC_PLANNED_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let gc_socket = socket.clone();
    let gc = thread::spawn(move || {
        ipc::request_with_timeout(
            &gc_socket,
            Request::TriggerGc {
                dry_run: false,
                aggressive: false,
                auto: false,
                target_bytes: Some(0),
            },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "GC did not plan a context action"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        PathBuf::from(std::fs::read_to_string(&marker).unwrap()),
        context
    );
    let lease = ipc::request_with_timeout(
        &socket,
        Request::AcquireLease {
            scope: rgo_protocol::LeaseScope::Context {
                build_dir: context.to_string_lossy().into_owned(),
            },
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(matches!(lease, Response::Lease { .. }));
    std::fs::write(&daemon.release, b"release").unwrap();
    let Response::Gc(report) = gc.join().unwrap().unwrap() else {
        panic!("GC did not return a report");
    };
    assert!(context.exists());
    assert!(report.skipped_execution_actions >= 1);
}

#[cfg(debug_assertions)]
#[test]
fn clean_inventory_admits_a_late_lease_before_deletion() {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    let workspace = sb.simple_bin("clean-late-lease").unwrap();
    let context = paths.builds_dir().join("aa/clean-late-lease");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(context.join("payload"), b"keep").unwrap();
    rgo_core::context::write_supervised_sidecar(
        &context,
        &workspace,
        &workspace.join("Cargo.toml"),
        true,
    )
    .unwrap();
    let marker = sb.home.join("clean-inventoried");
    let release = sb.home.join("clean-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_CLEAN_INVENTORIED_MARKER", &marker)
            .env("RGO_TEST_CLEAN_INVENTORIED_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let clean_socket = socket.clone();
    let build_dir = context.to_string_lossy().into_owned();
    let clean = thread::spawn(move || {
        ipc::request_with_timeout(
            &clean_socket,
            Request::Clean { build_dir },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "clean did not inventory the context"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        PathBuf::from(std::fs::read_to_string(&marker).unwrap()),
        context
    );
    let lease = ipc::request_with_timeout(
        &socket,
        Request::AcquireLease {
            scope: rgo_protocol::LeaseScope::Context {
                build_dir: context.to_string_lossy().into_owned(),
            },
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(matches!(lease, Response::Lease { .. }));
    std::fs::write(&daemon.release, b"release").unwrap();
    let response = clean.join().unwrap().unwrap();
    assert!(
        matches!(response, Response::Error { ref message, .. } if message.contains("active lease")),
        "late lease did not protect clean victim: {response:?}"
    );
    assert!(context.join("payload").exists());
}

#[cfg(debug_assertions)]
#[test]
fn cas_selection_admits_a_late_lease_and_preserves_the_manifest() {
    cas_selection_lease_case(false);
    cas_selection_lease_case(true);
}

#[cfg(debug_assertions)]
fn cas_selection_lease_case(dry_run: bool) {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n",
    )
    .unwrap();
    let cas = rgo_cas::Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
    let object = cas.put_bytes(&vec![b'x'; 1024 * 1024], 0o444).unwrap();
    let key = "c".repeat(64);
    cas.write_manifest(&rgo_cas::Manifest {
        version: rgo_cas::MANIFEST_VERSION,
        key: key.clone(),
        outputs: vec![rgo_cas::ManifestOutput {
            kind: "rlib".into(),
            name: "libprobe.rlib".into(),
            object: object.clone(),
        }],
        stdout: None,
        stderr: None,
        created_at: rgo_core::context::unix_now(),
    })
    .unwrap();
    let manifest = cas.manifest_path(&key);
    let marker = sb.home.join("cas-selected");
    let release = sb.home.join("cas-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_CAS_SELECTED_MARKER", &marker)
            .env("RGO_TEST_CAS_SELECTED_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let gc_socket = socket.clone();
    let gc = thread::spawn(move || {
        ipc::request_with_timeout(
            &gc_socket,
            Request::TriggerGc {
                dry_run,
                aggressive: false,
                auto: false,
                target_bytes: Some(0),
            },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "GC did not select a CAS manifest"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        PathBuf::from(std::fs::read_to_string(&marker).unwrap()),
        manifest
    );
    let lease = ipc::request_with_timeout(
        &socket,
        Request::AcquireLease {
            scope: rgo_protocol::LeaseScope::Cache { key },
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(matches!(lease, Response::Lease { .. }));
    std::fs::write(&daemon.release, b"release").unwrap();
    let response = gc.join().unwrap().unwrap();
    let Response::Gc(report) = response else {
        panic!("GC did not return a report: {response:?}");
    };
    assert!(manifest.exists());
    assert!(cas.object_path(&object.digest).exists());
    if dry_run {
        assert!(report.dry_run);
    } else {
        assert!(report.skipped_execution_actions >= 1);
    }
}

#[cfg(debug_assertions)]
#[test]
fn cas_eviction_detects_a_completed_lease_between_manifests() {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n",
    )
    .unwrap();
    let cas = rgo_cas::Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
    let keys = ["a".repeat(64), "b".repeat(64)];
    let mut objects = Vec::new();
    for (key, fill) in keys.iter().zip(*b"ab") {
        let object = cas.put_bytes(&vec![fill; 1024 * 1024], 0o444).unwrap();
        cas.write_manifest(&rgo_cas::Manifest {
            version: rgo_cas::MANIFEST_VERSION,
            key: key.clone(),
            outputs: vec![rgo_cas::ManifestOutput {
                kind: "rlib".into(),
                name: "libprobe.rlib".into(),
                object: object.clone(),
            }],
            stdout: None,
            stderr: None,
            created_at: rgo_core::context::unix_now(),
        })
        .unwrap();
        objects.push(object);
    }
    let marker = sb.home.join("cas-staged");
    let release = sb.home.join("cas-stage-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_CAS_STAGED_MARKER", &marker)
            .env("RGO_TEST_CAS_STAGED_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let gc_socket = socket.clone();
    let gc = thread::spawn(move || {
        ipc::request_with_timeout(
            &gc_socket,
            Request::TriggerGc {
                dry_run: false,
                aggressive: false,
                auto: false,
                target_bytes: Some(0),
            },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "GC did not stage a CAS manifest");
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        PathBuf::from(std::fs::read_to_string(&marker).unwrap()),
        cas.manifest_path(&keys[0])
    );
    let lease = ipc::request_with_timeout(
        &socket,
        Request::AcquireLease {
            scope: rgo_protocol::LeaseScope::Cache {
                key: keys[1].clone(),
            },
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    let Response::Lease { lease_id, .. } = lease else {
        panic!("cache lease was not acquired");
    };
    assert!(matches!(
        ipc::request_with_timeout(
            &socket,
            Request::ReleaseLease { lease_id },
            Duration::from_secs(3)
        ),
        Ok(Response::Ok)
    ));
    std::fs::write(&daemon.release, b"release").unwrap();
    let Response::Gc(report) = gc.join().unwrap().unwrap() else {
        panic!("GC did not return a report");
    };
    assert!(!cas.manifest_path(&keys[0]).exists());
    assert!(cas.manifest_path(&keys[1]).exists());
    assert!(cas.object_path(&objects[1].digest).exists());
    assert!(report.skipped_execution_actions >= 1);
}

#[cfg(debug_assertions)]
#[test]
fn cas_object_sweep_rechecks_publication_between_staged_removals() {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        "[storage]\nmin_free_space = '0B'\n[cache]\nenabled = true\n",
    )
    .unwrap();
    let cas = rgo_cas::Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
    let objects = [
        cas.put_bytes(&vec![b'a'; 1024 * 1024], 0o444).unwrap(),
        cas.put_bytes(&vec![b'b'; 1024 * 1024], 0o444).unwrap(),
    ];
    let marker = sb.home.join("object-staged");
    let release = sb.home.join("object-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_CAS_OBJECT_STAGED_MARKER", &marker)
            .env("RGO_TEST_CAS_OBJECT_STAGED_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let gc_socket = socket.clone();
    let gc = thread::spawn(move || {
        ipc::request_with_timeout(
            &gc_socket,
            Request::TriggerGc {
                dry_run: false,
                aggressive: false,
                auto: false,
                target_bytes: Some(u64::MAX),
            },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "GC did not stage an orphan object"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let first = PathBuf::from(std::fs::read_to_string(&marker).unwrap());
    let protected = objects
        .iter()
        .find(|object| cas.object_path(&object.digest) != first)
        .unwrap();
    let key = "e".repeat(64);
    let acquired = ipc::request_with_timeout(
        &socket,
        Request::CacheAcquire {
            key: key.clone(),
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    let Response::CacheProducer { lease_id, .. } = acquired else {
        panic!("cache producer not admitted during staged removal: {acquired:?}");
    };
    let committed = ipc::request_with_timeout(
        &socket,
        Request::CacheCommit {
            key: key.clone(),
            lease_id,
            manifest: rgo_protocol::CacheManifest {
                version: rgo_cas::MANIFEST_VERSION,
                key: key.clone(),
                outputs: vec![rgo_protocol::CacheOutput {
                    kind: "rlib".into(),
                    name: "libprobe.rlib".into(),
                    object: rgo_protocol::CacheObject {
                        digest: protected.digest.clone(),
                        size: protected.size,
                        mode: protected.mode,
                    },
                }],
                stdout: None,
                stderr: None,
                created_at: rgo_core::context::unix_now(),
            },
        },
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(matches!(
        committed,
        Response::CacheCommitted { accepted: true }
    ));
    std::fs::write(&daemon.release, b"release").unwrap();
    let Response::Gc(report) = gc.join().unwrap().unwrap() else {
        panic!("GC did not return a report");
    };
    assert!(!first.exists());
    assert!(cas.manifest_path(&key).exists());
    assert!(cas.object_path(&protected.digest).exists());
    assert!(report.skipped_execution_actions >= 1);
}

#[cfg(debug_assertions)]
#[test]
fn cas_sweep_ignores_an_object_committed_during_its_inventory() {
    struct StopDaemon {
        child: Child,
        release: PathBuf,
    }
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        "[storage]\nmin_free_space = '0B'\n[cache]\nenabled = true\n",
    )
    .unwrap();
    let cas = rgo_cas::Store::new(paths.cas_dir(), paths.quarantine_dir()).unwrap();
    let object = cas.put_bytes(&vec![b'x'; 1024 * 1024], 0o444).unwrap();
    let key = "d".repeat(64);
    let marker = sb.home.join("cas-swept");
    let release = sb.home.join("cas-sweep-release");
    let mut daemon = StopDaemon {
        child: sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_TEST_CAS_SWEPT_MARKER", &marker)
            .env("RGO_TEST_CAS_SWEPT_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        release,
    };
    let socket = paths.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        ipc::request_with_timeout(
            &socket,
            Request::QueryRemoteStatus,
            Duration::from_millis(250)
        ),
        Ok(Response::RemoteStatus(_))
    ) {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not become ready");
        thread::sleep(Duration::from_millis(25));
    }
    let gc_socket = socket.clone();
    let gc = thread::spawn(move || {
        ipc::request_with_timeout(
            &gc_socket,
            Request::TriggerGc {
                dry_run: true,
                aggressive: false,
                auto: false,
                target_bytes: Some(u64::MAX),
            },
            Duration::from_secs(45),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.is_file() {
        assert!(daemon.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "GC did not inventory CAS");
        thread::sleep(Duration::from_millis(25));
    }
    let acquired = ipc::request_with_timeout(
        &socket,
        Request::CacheAcquire {
            key: key.clone(),
            pid: std::process::id(),
            ttl_secs: 30,
        },
        Duration::from_secs(3),
    )
    .unwrap();
    let Response::CacheProducer { lease_id, .. } = acquired else {
        panic!("cache producer not admitted during CAS scan: {acquired:?}");
    };
    let committed = ipc::request_with_timeout(
        &socket,
        Request::CacheCommit {
            key: key.clone(),
            lease_id,
            manifest: rgo_protocol::CacheManifest {
                version: rgo_cas::MANIFEST_VERSION,
                key: key.clone(),
                outputs: vec![rgo_protocol::CacheOutput {
                    kind: "rlib".into(),
                    name: "libprobe.rlib".into(),
                    object: rgo_protocol::CacheObject {
                        digest: object.digest.clone(),
                        size: object.size,
                        mode: object.mode,
                    },
                }],
                stdout: None,
                stderr: None,
                created_at: rgo_core::context::unix_now(),
            },
        },
        Duration::from_secs(3),
    )
    .unwrap();
    assert!(matches!(
        committed,
        Response::CacheCommitted { accepted: true }
    ));
    std::fs::write(&daemon.release, b"release").unwrap();
    let Response::Gc(report) = gc.join().unwrap().unwrap() else {
        panic!("GC did not return a report");
    };
    assert!(!report
        .actions
        .iter()
        .any(|action| action.path == cas.object_path(&object.digest).to_string_lossy().as_ref()));
    assert!(cas.manifest_path(&key).exists());
    assert!(cas.object_path(&object.digest).exists());
    let stats =
        ipc::request_with_timeout(&socket, Request::QueryCacheStats, Duration::from_secs(3))
            .unwrap();
    let Response::CacheStats(stats) = stats else {
        panic!("cache stats unavailable after concurrent publication: {stats:?}");
    };
    assert_eq!(stats.manifests, 1);
}

#[test]
fn manual_daemon_startup_preserves_legacy_pin_intent() {
    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    let context = paths.builds_dir().join("aa/pinned");
    std::fs::create_dir_all(&context).unwrap();
    let marker = context.join(rgo_core::context::PIN_MARKER);
    std::fs::write(&marker, b"").unwrap();

    let mut daemon = start_daemon(&sb);
    assert_eq!(
        std::fs::read(paths.pin_records_dir().join("aa/pinned.pin")).unwrap(),
        b"pin\n"
    );
    std::fs::remove_file(marker).unwrap();
    assert!(rgo_core::context::is_pinned(&paths, &context));
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn automatic_maintenance_reclaims_an_orphan_without_storage_pressure() {
    struct StopDaemon(Child);
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    let project = sb.simple_bin("age-orphan").unwrap();
    // A recent pressure/manual pass must not postpone scheduled retention.
    rgo_core::db::StateDb::open(&paths)
        .unwrap()
        .record_gc(false, false, false, &Default::default(), None)
        .unwrap();
    let context = paths.builds_dir().join("aa/orphan");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(context.join("intermediates"), vec![0u8; 1024 * 1024]).unwrap();
    rgo_core::context::write_supervised_sidecar(
        &context,
        &project,
        &project.join("Cargo.toml"),
        true,
    )
    .unwrap();
    std::fs::remove_file(project.join("Cargo.toml")).unwrap();
    assert!(rgo_core::context::list(&paths).unwrap()[0].is_orphan());

    let budget = 1_000_000_000_000u64;
    assert!(
        rgo_core::size::managed_snapshot(&paths)
            .unwrap()
            .total_bytes()
            < budget / 2
    );
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[storage]\nmax_size = '{budget}B'\nmin_free_space = '0B'\n[gc]\nauto = true\norphan_grace = '0s'\n"
        ),
    )
    .unwrap();

    let mut daemon = StopDaemon(start_daemon_with_poll(&sb, Some("1")));
    let deadline = Instant::now() + Duration::from_secs(15);
    while context.exists() && Instant::now() < deadline {
        assert!(daemon.0.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !context.exists(),
        "age maintenance did not reclaim the orphan"
    );
}

#[test]
fn opted_in_maintenance_reclaims_idle_bytes_and_reports_pinned_excess() {
    struct StopDaemon(Child);
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    let pinned_project = sb.simple_bin("budget-pinned").unwrap();
    let idle_project = sb.simple_bin("budget-idle").unwrap();
    let pinned = paths.builds_dir().join("aa/pinned");
    let idle = paths.builds_dir().join("bb/idle");
    for (dir, project) in [(&pinned, &pinned_project), (&idle, &idle_project)] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("intermediates"), vec![0u8; 2 * 1024 * 1024]).unwrap();
        rgo_core::context::write_supervised_sidecar(
            dir,
            project,
            &project.join("Cargo.toml"),
            true,
        )
        .unwrap();
    }
    rgo_core::context::write_durable_pin(&paths, &pinned).unwrap();

    // Establish the operational baseline before choosing a budget that even
    // removing the entire idle context cannot satisfy because of the pin.
    let baseline = StopDaemon(start_daemon(&sb));
    drop(baseline);
    let contexts = rgo_core::context::list(&paths).unwrap();
    let pinned_bytes = contexts
        .iter()
        .find(|context| context.dir == pinned)
        .unwrap()
        .usage
        .physical_bytes;
    let other_bytes = rgo_core::size::auxiliary_usage(&paths)
        .unwrap()
        .physical_bytes
        + rgo_core::size::Scanner::new()
            .measure_optional(&paths.cas_dir())
            .unwrap()
            .physical_bytes;
    let max_size = other_bytes + pinned_bytes / 2;
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    std::fs::write(
        sb.rgo_home.join("config.toml"),
        format!("[storage]\nmax_size = '{max_size}B'\nmin_free_space = '0B'\n[gc]\nauto = true\n"),
    )
    .unwrap();

    let mut daemon = StopDaemon(
        sb.cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .env("RGO_DAEMON_POLL_SECS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while idle.exists() && std::time::Instant::now() < deadline {
        assert!(daemon.0.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
    }
    assert!(!idle.exists(), "automatic GC did not reclaim eligible data");
    assert!(pinned.exists(), "automatic GC removed a pinned context");
    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(status) = response else {
        panic!("daemon did not return storage status: {response:?}");
    };
    assert_eq!(status.contexts, 1);
    assert!(status.last_gc_at > 0);
    assert!(status.protected_context_bytes >= pinned_bytes);
    assert!(status.unmet_budget_bytes.unwrap() > 0);
    assert!(
        status
            .unmet_budget_reason
            .as_deref()
            .unwrap()
            .contains("protected build contexts")
    );

    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::Unpin {
            build_dir: pinned.to_string_lossy().into_owned(),
        },
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(matches!(response, Response::Ok));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while pinned.exists() && std::time::Instant::now() < deadline {
        assert!(daemon.0.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !pinned.exists(),
        "unpin did not prompt automatic budget recovery"
    );
    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(status) = response else {
        panic!("daemon did not return storage status: {response:?}");
    };
    assert_eq!(status.unmet_budget_bytes, Some(0));
    assert_eq!(status.free_space_deficit_bytes, Some(0));
    assert_eq!(status.unmet_free_space_bytes, Some(0));
    assert_eq!(status.unmet_free_space_reason, None);

    // A reserve can be unmet independently of the managed-size budget. Use a
    // deliberately impossible reserve instead of filling the runner's volume.
    drop(daemon);
    std::fs::create_dir_all(&pinned).unwrap();
    std::fs::write(pinned.join("intermediates"), vec![0u8; 2 * 1024 * 1024]).unwrap();
    rgo_core::context::write_supervised_sidecar(
        &pinned,
        &pinned_project,
        &pinned_project.join("Cargo.toml"),
        true,
    )
    .unwrap();
    rgo_core::context::write_durable_pin(&paths, &pinned).unwrap();
    let reserve = rgo_core::config::volume_total_bytes_checked(&paths.root)
        .unwrap()
        .saturating_add(1024 * 1024 * 1024);
    std::fs::write(paths.config_file(), format!(
        "[storage]\nmax_size = '1000000000000B'\nmin_free_space = '{reserve}B'\n[gc]\nauto = false\n"
    )).unwrap();
    let _daemon = StopDaemon(start_daemon(&sb));
    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(status) = response else {
        panic!("daemon did not return reserve status: {response:?}");
    };
    assert!(status.managed_bytes < status.hard_limit_bytes);
    assert_eq!(status.unmet_budget_bytes, Some(0));
    let deficit = reserve.saturating_sub(status.volume_free_observed_bytes.unwrap());
    assert!(deficit > 0);
    assert_eq!(status.free_space_deficit_bytes, Some(deficit));
    assert_eq!(
        status.unmet_free_space_bytes,
        Some(deficit.saturating_sub(status.eligible_managed_bytes.unwrap()))
    );
    assert!(status.unmet_free_space_bytes.unwrap() > 0);
    let reason = status.unmet_free_space_reason.unwrap();
    assert!(reason.contains("protected build contexts"), "{reason}");
    assert!(reason.contains("free-space reserve"), "{reason}");
    let visible = sb.cmd(cargo_bin("rgo")).arg("status").output().unwrap();
    assert!(
        visible.status.success(),
        "{}",
        String::from_utf8_lossy(&visible.stderr)
    );
    let visible = String::from_utf8_lossy(&visible.stdout);
    assert!(visible.contains("Reserve deficit"), "{visible}");
    assert!(visible.contains("Reserve unmet est."), "{visible}");
    assert!(visible.contains("protected build contexts"), "{visible}");
    assert!(!visible.contains("Over budget"), "{visible}");
    assert!(!visible.contains("Budget unmet est."), "{visible}");
    assert!(pinned.is_dir());
}

#[test]
fn automatic_budget_reclaims_another_context_during_a_supervised_cargo_run() {
    struct StopDaemon(Child);
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct RunningCargo {
        child: Option<Child>,
        release: PathBuf,
    }
    impl RunningCargo {
        fn finish(mut self) -> Output {
            std::fs::write(&self.release, b"done").unwrap();
            self.child.take().unwrap().wait_with_output().unwrap()
        }
    }
    impl Drop for RunningCargo {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = std::fs::write(&self.release, b"done");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let sb = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join(if cfg!(windows) { "cargo.exe" } else { "cargo" }))
                .find(|path| path.is_file())
        })
        .expect("absolute Cargo executable for integration tests");
    let idle_project = sb.simple_bin("auto-running-idle").unwrap();
    let active_project = sb.simple_bin("auto-running-active").unwrap();
    std::fs::write(
        active_project.join("src/main.rs"),
        r#"fn main() {
    let ready = std::path::PathBuf::from(std::env::var_os("RGO_TEST_READY").unwrap());
    let release = std::path::PathBuf::from(std::env::var_os("RGO_TEST_RELEASE").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    for _ in 0..600 {
        if release.exists() { return; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("test did not release cargo run");
}
"#,
    )
    .unwrap();
    for project in [&idle_project, &active_project] {
        let build = sb
            .cmd(&rgo)
            .current_dir(project)
            .args(["cargo-shim", "--real-cargo"])
            .arg(&real_cargo)
            .args(["--", "build", "--offline"])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
    }
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    let contexts = rgo_core::context::list(&paths).unwrap();
    assert_eq!(contexts.len(), 2);
    let context_for = |project: &Path| {
        let root = project.canonicalize().unwrap();
        contexts
            .iter()
            .find(|context| {
                context.sidecar.as_ref().is_some_and(|sidecar| {
                    Path::new(&sidecar.workspace_root).canonicalize().ok() == Some(root.clone())
                })
            })
            .unwrap()
            .dir
            .clone()
    };
    let idle = context_for(&idle_project);
    let active = context_for(&active_project);
    let baseline = StopDaemon(start_daemon(&sb));
    drop(baseline);

    let ready = sb.home.join("active-run-ready");
    let release = sb.home.join("active-run-release");
    let mut running = RunningCargo {
        child: Some(
            sb.cmd(&rgo)
                .current_dir(&active_project)
                .env("RGO_TEST_READY", &ready)
                .env("RGO_TEST_RELEASE", &release)
                .args(["cargo-shim", "--real-cargo"])
                .arg(&real_cargo)
                .args(["--", "run", "--offline"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ),
        release,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() && Instant::now() < deadline {
        assert!(
            running
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready.exists(),
        "supervised cargo run never reached its program"
    );

    let active_bytes = rgo_core::context::list(&paths)
        .unwrap()
        .into_iter()
        .find(|context| context.dir == active)
        .unwrap()
        .usage
        .physical_bytes;
    let other_bytes = rgo_core::size::auxiliary_usage(&paths)
        .unwrap()
        .physical_bytes
        + rgo_core::size::Scanner::new()
            .measure_optional(&paths.cas_dir())
            .unwrap()
            .physical_bytes;
    let max_size = other_bytes + active_bytes / 2;
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    let old = SystemTime::now() - Duration::from_secs(7200);
    // Expire the profile-lock heuristic even for the running build. Its
    // supervised session guard, not a recent mtime, must prevent deletion.
    for context_dir in [&idle, &active] {
        for profile in std::fs::read_dir(context_dir).unwrap().flatten() {
            if profile.path().is_dir() {
                let lock = profile.path().join(".cargo-build-lock");
                if lock.is_file() {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(lock)
                        .unwrap()
                        .set_modified(old)
                        .unwrap();
                }
            }
        }
    }
    std::fs::write(
        sb.rgo_home.join("config.toml"),
        format!("[storage]\nmax_size = '{max_size}B'\nmin_free_space = '0B'\n[gc]\nauto = true\n"),
    )
    .unwrap();
    let mut daemon = StopDaemon(
        sb.cmd(&rgo)
            .args(["daemon", "--foreground"])
            .env("RGO_DAEMON_POLL_SECS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while idle.exists() && Instant::now() < deadline {
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(
            running
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(!idle.exists(), "maintenance did not reclaim the idle build");
    assert!(
        active.exists(),
        "maintenance removed a running Cargo context"
    );
    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(status) = response else {
        panic!("daemon did not return storage status: {response:?}");
    };
    assert_eq!(status.contexts, 1);
    assert!(status.unmet_budget_bytes.unwrap() > 0);
    assert!(
        status
            .unmet_budget_reason
            .as_deref()
            .unwrap()
            .contains("protected build contexts")
    );

    // Crash the daemon while the real Cargo program is still running. A new
    // daemon must inherit the same external lifecycle protection, including
    // when its first pass is an explicit zero-byte GC request.
    drop(daemon);
    let mut daemon = StopDaemon(start_daemon_with_poll(&sb, Some("1")));
    let restarted_gc = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::TriggerGc {
            dry_run: false,
            aggressive: false,
            auto: false,
            target_bytes: Some(0),
        },
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(matches!(restarted_gc, Response::Gc(_)), "{restarted_gc:?}");
    assert!(
        active.exists(),
        "restarted daemon removed a running Cargo context"
    );
    assert!(
        running
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none(),
        "running Cargo exited during daemon restart"
    );

    let output = running.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for profile in std::fs::read_dir(&active).unwrap().flatten() {
        if profile.path().is_dir() {
            let lock = profile.path().join(".cargo-build-lock");
            if lock.is_file() {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(lock)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while active.exists() && Instant::now() < deadline {
        assert!(daemon.0.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !active.exists(),
        "maintenance did not reclaim the exited build"
    );
    assert!(
        active_project
            .join(format!(
                "target/debug/auto-running-active{}",
                std::env::consts::EXE_SUFFIX
            ))
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn status_surfaces_a_daemon_measurement_error_without_starting_another_daemon() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    let mut daemon = start_daemon(&sb);
    let manifests = sb.rgo_home.join("cas/manifests");
    let moved = sb.rgo_home.join("cas/manifests.saved");
    std::fs::rename(&manifests, &moved).unwrap();
    std::fs::write(&manifests, b"not a directory").unwrap();

    let status = sb.cmd(cargo_bin("rgo")).arg("status").output().unwrap();
    assert!(!status.status.success());
    let error = String::from_utf8_lossy(&status.stderr);
    assert!(error.contains("daemon status failed"), "{error}");
    assert!(!String::from_utf8_lossy(&status.stdout).contains("Daemon               unavailable"));
    assert!(daemon.try_wait().unwrap().is_none());

    std::fs::remove_file(&manifests).unwrap();
    std::fs::rename(moved, manifests).unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn gc_reclaims_fresh_staging_and_quarantine_within_one_pass() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sb.cargo_home.display()),
    )
    .unwrap();
    let staged = paths.tmp_dir().join("gc-123-456-abandoned");
    std::fs::create_dir(&staged).unwrap();
    std::fs::write(staged.join("data"), vec![b'a'; 1024 * 1024]).unwrap();
    let quarantined = paths.quarantine_dir().join("corrupt.bad");
    std::fs::write(&quarantined, vec![b'b'; 1024 * 1024]).unwrap();
    let unknown = paths.tmp_dir().join("unknown-new-file");
    std::fs::write(&unknown, vec![b'c'; 1024 * 1024]).unwrap();
    let reclaimable = rgo_core::size::Scanner::new()
        .measure_checked(&staged)
        .unwrap()
        .physical_bytes
        + rgo_core::size::Scanner::new()
            .measure_checked(&quarantined)
            .unwrap()
            .physical_bytes;
    let mut daemon = start_daemon(&sb);
    let before = match ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(2),
    )
    .unwrap()
    {
        Response::Status(status) => status,
        response => panic!("expected status before cleanup, got {response:?}"),
    };
    let target = before
        .managed_bytes
        .saturating_sub(reclaimable)
        .saturating_add(512 * 1024);
    let gc = sb
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--target", &format!("{target}B")])
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(!staged.exists());
    assert!(!quarantined.exists());
    assert!(unknown.exists());
    let after = match ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(2),
    )
    .unwrap()
    {
        Response::Status(status) => status,
        response => panic!("expected status after cleanup, got {response:?}"),
    };
    assert!(
        after.managed_bytes <= target,
        "GC left {} managed bytes above target {target}",
        after.managed_bytes
    );
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn maintenance_recovers_unpin_decision_with_auto_gc_disabled() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    let absent = paths.builds_dir().join("aa/interrupted-unpin");
    rgo_core::context::remove_durable_pin(&paths, &absent).unwrap();
    let record = paths.pin_records_dir().join("aa/interrupted-unpin.pin");
    assert!(record.exists());
    std::fs::write(paths.config_file(), "[gc]\nauto = false\n").unwrap();

    let mut daemon = start_daemon_with_poll(&sb, Some("1"));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while record.exists() && std::time::Instant::now() < deadline {
        assert!(daemon.try_wait().unwrap().is_none(), "daemon exited early");
        thread::sleep(Duration::from_millis(100));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    let diagnostics = std::fs::read_to_string(paths.logs_dir().join("daemon.log"))
        .unwrap_or_else(|error| format!("daemon log unavailable: {error}"));
    assert!(
        !record.exists(),
        "maintenance kept an obsolete unpin decision: {diagnostics}"
    );
}

#[test]
fn explicit_daemon_home_survives_a_service_environment_without_cargo_home() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = sb
        .cmd(cargo_bin("rgo"))
        .env_remove("RGO_HOME")
        .env("CARGO_HOME", sb.projects.join("unrelated-cargo-home"))
        .args(["daemon", "--foreground", "--home"])
        .arg(&sb.rgo_home)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if matches!(
            ipc::request_with_timeout(
                &sb.rgo_home.join("state/daemon.sock"),
                Request::QueryRemoteStatus,
                Duration::from_millis(100)
            ),
            Ok(Response::RemoteStatus(_))
        ) {
            daemon.kill().unwrap();
            let _ = daemon.wait();
            return;
        }
        assert!(daemon.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(25));
    }
    let _ = daemon.kill();
    let output = daemon.wait_with_output().unwrap();
    let diagnostics = std::fs::read_to_string(sb.rgo_home.join("logs/daemon.log"))
        .unwrap_or_else(|error| format!("daemon log unavailable: {error}"));
    panic!(
        "explicit daemon home did not answer IPC: {diagnostics}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn daemon_coordinates_builds_and_pins_contexts() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(sb.rgo_home.join("state/daemon.pid"), "999999\n").unwrap();
    std::fs::write(sb.rgo_home.join("state/daemon.sock"), "stale socket").unwrap();
    let mut daemon = start_daemon(&sb);
    let project = sb.simple_bin("daemon-project").unwrap();
    let build = sb
        .cmd(cargo_bin("rgo"))
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let plain_project = sb.simple_bin("plain-cargo-project").unwrap();
    let plain_build = sb
        .cargo()
        .current_dir(&plain_project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        plain_build.status.success(),
        "{}",
        String::from_utf8_lossy(&plain_build.stderr)
    );

    let status = sb.cmd(cargo_bin("rgo")).arg("status").output().unwrap();
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success(),
        "stdout={} stderr={}",
        status_text,
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status_text.contains("Daemon               running"),
        "{status_text}"
    );
    assert!(
        status_text.contains("Active leases        "),
        "{status_text}"
    );

    let ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    let id = String::from_utf8_lossy(&ls.stdout)
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .expect("one context")
        .to_owned();
    let pin = sb
        .cmd(cargo_bin("rgo"))
        .args(["pin", &id])
        .output()
        .unwrap();
    assert!(
        pin.status.success(),
        "{}",
        String::from_utf8_lossy(&pin.stderr)
    );
    let pinned_ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(String::from_utf8_lossy(&pinned_ls.stdout).contains("PIN"));

    let unpin = sb
        .cmd(cargo_bin("rgo"))
        .args(["unpin", &id])
        .output()
        .unwrap();
    assert!(
        unpin.status.success(),
        "{}",
        String::from_utf8_lossy(&unpin.stderr)
    );

    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    let context = paths.builds_dir().join(&id);
    // These are durable filesystem writes. Match the CLI's pin deadline,
    // rather than the 150 ms timeout used by best-effort daemon probes.
    let pin_timeout = Duration::from_secs(5);
    // An unrelated malformed shard must not prevent pinning this valid
    // context; pin admission checks only the selected root/shard/context.
    let unrelated_shard = paths.builds_dir().join("zz");
    std::fs::write(&unrelated_shard, b"not a shard").unwrap();
    let repin = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::Pin {
            build_dir: context.display().to_string(),
        },
        pin_timeout,
    )
    .unwrap();
    assert!(matches!(repin, Response::Ok));
    let reunpin = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::Unpin {
            build_dir: context.display().to_string(),
        },
        pin_timeout,
    )
    .unwrap();
    assert!(matches!(reunpin, Response::Ok));
    std::fs::remove_file(&unrelated_shard).unwrap();
    std::fs::remove_dir_all(&context).unwrap();
    let stale_pin = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::Pin {
            build_dir: context.display().to_string(),
        },
        pin_timeout,
    )
    .unwrap();
    assert!(matches!(stale_pin, Response::Error { .. }));
    assert!(!rgo_core::context::is_pinned(&paths, &context));
    #[cfg(unix)]
    {
        let outside = sb.home.join("outside-pin-target");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &context).unwrap();
        let symlink_pin = ipc::request_with_timeout(
            &paths.socket_path(),
            Request::Pin {
                build_dir: context.display().to_string(),
            },
            pin_timeout,
        )
        .unwrap();
        assert!(matches!(symlink_pin, Response::Error { .. }));
        assert!(!outside.join(rgo_core::context::PIN_MARKER).exists());
    }

    daemon.kill().unwrap();
    let _ = daemon.wait();
}

#[test]
fn daemon_enforces_single_instance() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut first = start_daemon(&sb);
    let second = sb
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
    first.kill().unwrap();
    let _ = first.wait();
}

#[cfg(unix)]
#[test]
fn daemon_rejects_bad_handshakes_and_survives_malformed_clients() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = start_daemon(&sb);
    let socket = sb.rgo_home.join("state/daemon.sock");
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut connection = ipc::connect(&socket, Duration::from_secs(2)).unwrap();
    ipc::write_message(&mut connection, &Request::QueryStatus).unwrap();
    let response: Response = ipc::read_message(&mut connection).unwrap();
    assert!(matches!(
        response,
        Response::Error { code, .. } if code == "handshake_required"
    ));

    let mut connection = ipc::connect(&socket, Duration::from_secs(2)).unwrap();
    ipc::write_message(
        &mut connection,
        &Request::Hello {
            version: PROTOCOL_VERSION + 1,
            client: "test".into(),
        },
    )
    .unwrap();
    let response: Response = ipc::read_message(&mut connection).unwrap();
    assert!(matches!(
        response,
        Response::Error { code, .. } if code == "protocol_mismatch"
    ));

    let mut malformed = UnixStream::connect(&socket).unwrap();
    malformed.write_all(&(2u32.to_be_bytes())).unwrap();
    malformed.write_all(b"{}").unwrap();
    drop(malformed);
    let before =
        match ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2))
            .unwrap()
        {
            Response::Status(status) => status,
            response => panic!("expected status after malformed client, got {response:?}"),
        };
    let temp_file = sb.rgo_home.join("tmp/accounted-by-status");
    std::fs::write(
        &temp_file,
        (0..8192).map(|i| (i % 251) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    let allocated = rgo_core::size::Scanner::new()
        .measure(&temp_file)
        .physical_bytes;
    assert!(allocated > 0);
    let after =
        match ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2))
            .unwrap()
        {
            Response::Status(status) => status,
            response => panic!("expected status after adding temp file, got {response:?}"),
        };
    assert!(after.auxiliary_bytes >= before.auxiliary_bytes + allocated);
    assert!(after.managed_bytes >= before.managed_bytes + allocated);
    assert_eq!(
        after.managed_bytes,
        after
            .build_bytes
            .unwrap()
            .saturating_add(after.cas_bytes.unwrap())
            .saturating_add(after.auxiliary_bytes)
    );

    daemon.kill().unwrap();
    let _ = daemon.wait();
}

#[test]
fn daemon_loss_during_a_build_degrades_to_normal_cargo() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = start_daemon(&sb);
    let project = sb.simple_bin("daemon-loss").unwrap();
    let mut build = sb
        .cmd(cargo_bin("rgo"))
        .current_dir(&project)
        .args(["build", "--offline"])
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    let _ = daemon.kill();
    let _ = daemon.wait();
    let status = build.wait().unwrap();
    assert!(status.success(), "Cargo failed after daemon loss");
}

#[cfg(unix)]
fn only_context_dir(sb: &Sandbox) -> std::path::PathBuf {
    let builds = sb.rgo_home.join("builds");
    let mut dirs: Vec<_> = std::fs::read_dir(&builds)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .flat_map(|shard| {
            std::fs::read_dir(&shard)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .collect::<Vec<_>>()
        })
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "expected exactly one managed context");
    dirs.remove(0)
}

/// Make a managed context look like a long-idle orphan: remove the workspace so the
/// sidecar manifest is missing, age `last_seen`, drop Cargo's build locks so nothing
/// looks live, and age the build-dir mtime.
#[cfg(unix)]
fn age_context_into_orphan(project: &std::path::Path, context_dir: &std::path::Path) {
    use std::time::SystemTime;
    std::fs::remove_dir_all(project).unwrap();
    let sidecar_path = context_dir.join(".rgo-context.json");
    let mut sidecar: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    sidecar["last_seen"] = serde_json::json!(0);
    std::fs::write(&sidecar_path, sidecar.to_string()).unwrap();
    let mut stack = vec![context_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some(".cargo-build-lock")
            ) {
                std::fs::remove_file(&path).unwrap();
            }
        }
    }
    let old = SystemTime::now() - Duration::from_secs(7200);
    std::fs::File::open(context_dir)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

#[cfg(unix)]
#[test]
fn pin_survives_database_rebuild_and_protects_context_from_gc() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    std::fs::write(
        sb.rgo_home.join("config.toml"),
        "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n",
    )
    .unwrap();
    let real_cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--supervised", "--real-cargo"])
            .arg(&real_cargo)
            .arg("--no-service")
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = start_daemon(&sb);
    let project = sb.simple_bin("pinned-project").unwrap();
    let build = sb
        .cmd(sb.cargo_home.join("rgo/shims/cargo"))
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let context_dir = only_context_dir(&sb);
    let id = context_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|shard| {
            format!(
                "{shard}/{}",
                context_dir.file_name().unwrap().to_string_lossy()
            )
        })
        .unwrap();
    let pin = sb
        .cmd(cargo_bin("rgo"))
        .args(["pin", &id])
        .output()
        .unwrap();
    assert!(
        pin.status.success(),
        "{}",
        String::from_utf8_lossy(&pin.stderr)
    );
    assert!(context_dir.join(".rgo-pin").is_file());
    let paths = rgo_core::paths::RgoPaths {
        root: sb.rgo_home.clone(),
    };
    let decision = paths.pin_records_dir().join(format!("{id}.pin"));
    assert!(rgo_core::context::is_pinned(&paths, &context_dir));

    // Lose the database and the legacy marker: the stable pin record must
    // rebuild the index without depending on Cargo's build directory.
    rgo_core::context::remove_pin_marker(&context_dir).unwrap();
    daemon.kill().unwrap();
    let _ = daemon.wait();
    for entry in std::fs::read_dir(sb.rgo_home.join("state"))
        .unwrap()
        .flatten()
    {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("meta.sqlite") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    let mut daemon = start_daemon(&sb);
    let ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(
        String::from_utf8_lossy(&ls.stdout).contains("PIN"),
        "pin must survive a full database rebuild (status {}): stdout={}, stderr={}",
        ls.status,
        String::from_utf8_lossy(&ls.stdout),
        String::from_utf8_lossy(&ls.stderr)
    );

    // Even an aged orphan is untouchable while pinned.
    age_context_into_orphan(&project, &context_dir);
    let status = match ipc::request_with_timeout(
        &sb.rgo_home.join("state/daemon.sock"),
        Request::QueryStatus,
        Duration::from_secs(2),
    )
    .unwrap()
    {
        Response::Status(status) => status,
        response => panic!("expected status for pinned context, got {response:?}"),
    };
    assert!(status.protected_context_bytes > 0);
    assert!(status.eligible_managed_bytes.is_some());
    assert!(status.unmet_budget_bytes.unwrap() > 0);
    assert!(
        status
            .unmet_budget_reason
            .as_deref()
            .unwrap()
            .contains("protected build contexts")
    );
    let visible_status = sb.cmd(cargo_bin("rgo")).arg("status").output().unwrap();
    assert!(visible_status.status.success());
    let visible_status = String::from_utf8_lossy(&visible_status.stdout);
    assert!(
        visible_status.contains("Budget unmet est."),
        "{visible_status}"
    );
    assert!(
        visible_status.contains("protected build contexts"),
        "{visible_status}"
    );
    let gc = sb
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--target", "1B"])
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(
        context_dir.exists(),
        "pinned context was collected despite the pin"
    );
    let gc_output = String::from_utf8_lossy(&gc.stdout);
    assert!(gc_output.contains("Target still unmet"), "{gc_output}");
    assert!(gc_output.contains("Protected builds"), "{gc_output}");
    assert!(gc_output.contains("build contexts"), "{gc_output}");
    assert!(gc_output.contains("compiler cache"), "{gc_output}");
    assert!(gc_output.contains("other rgo state"), "{gc_output}");

    let unpin = sb
        .cmd(cargo_bin("rgo"))
        .args(["unpin", &id])
        .output()
        .unwrap();
    assert!(
        unpin.status.success(),
        "{}",
        String::from_utf8_lossy(&unpin.stderr)
    );
    assert!(!context_dir.join(".rgo-pin").exists());
    // Removing the marker bumped the build-dir mtime; age it again past the grace period.
    let old = std::time::SystemTime::now() - Duration::from_secs(7200);
    std::fs::File::open(&context_dir)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let gc = sb
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--aggressive"])
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(
        !context_dir.exists(),
        "unpinned aged orphan should have been collected"
    );
    assert!(!decision.exists(), "GC kept an obsolete unpin decision");
    // A context removed by Cargo can still have a durable pin intent. Its ID
    // must remain usable for explicit unpin even before the next build.
    rgo_core::context::write_durable_pin(&paths, &context_dir).unwrap();
    let listed = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(listed.status.success());
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains(&id));
    assert!(listed.contains("context absent; pin retained"));
    let unpin_absent = sb
        .cmd(cargo_bin("rgo"))
        .args(["unpin", &id])
        .output()
        .unwrap();
    assert!(
        unpin_absent.status.success(),
        "{}",
        String::from_utf8_lossy(&unpin_absent.stderr)
    );
    assert!(!rgo_core::context::is_pinned(&paths, &context_dir));
    assert!(
        !decision.exists(),
        "unpinning an absent context kept its tombstone"
    );
    daemon.kill().unwrap();
    let _ = daemon.wait();
}

#[cfg(unix)]
#[test]
fn connection_flood_and_oversized_frames_cannot_starve_the_daemon() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = start_daemon(&sb);
    let socket = sb.rgo_home.join("state/daemon.sock");

    // A frame claiming more than MAX_FRAME_SIZE is dropped without reading a body.
    let mut oversized = UnixStream::connect(&socket).unwrap();
    oversized
        .write_all(&((rgo_protocol::MAX_FRAME_SIZE as u32 + 1).to_be_bytes()))
        .unwrap();
    drop(oversized);
    assert!(matches!(
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)).unwrap(),
        Response::Status(_)
    ));

    // Send more idle clients than the cap. Some can be accepted after earlier
    // clients time out on a loaded host, so an additional health request may
    // either succeed or be refused; recovery after the timeout is the invariant.
    let mut held = Vec::new();
    for _ in 0..70 {
        held.push(UnixStream::connect(&socket).unwrap());
    }
    // The server-side read timeout frees stalled permits; the daemon recovers.
    drop(held);
    thread::sleep(Duration::from_millis(2_500));
    assert!(matches!(
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)).unwrap(),
        Response::Status(_)
    ));
    daemon.kill().unwrap();
    let _ = daemon.wait();
}

#[cfg(unix)]
#[test]
fn maintenance_reclaims_orphans_via_auto_gc() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    let real_cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--supervised", "--real-cargo"])
            .arg(&real_cargo)
            .arg("--no-service")
            .status()
            .unwrap()
            .success()
    );
    // Tiny storage budget so the watermark check fires for a single context.
    std::fs::write(
        sb.rgo_home.join("config.toml"),
        "[storage]\nmax_size = \"1KB\"\n[gc]\nauto = true\n",
    )
    .unwrap();
    let mut daemon = sb
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .env("RGO_DAEMON_POLL_SECS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = sb.rgo_home.join("state/daemon.sock");
    let mut ready = false;
    for _ in 0..80 {
        if matches!(
            ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_millis(100)),
            Ok(Response::Status(_))
        ) {
            ready = true;
            break;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert!(ready, "daemon did not answer IPC during startup");
    let project = sb.simple_bin("auto-gc-project").unwrap();
    let build = sb
        .cmd(sb.cargo_home.join("rgo/shims/cargo"))
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let context_dir = only_context_dir(&sb);
    age_context_into_orphan(&project, &context_dir);

    // A best-effort IPC release can be missed under runner load. The 30-second
    // lease TTL is the supported recovery path, followed by the 1-second poll.
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while context_dir.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(250));
    }
    assert!(
        !context_dir.exists(),
        "daemon maintenance did not auto-GC the aged orphan; status: {:?}; daemon: {}",
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)),
        std::fs::read_to_string(sb.rgo_home.join("logs/daemon.log")).unwrap_or_default(),
    );
    daemon.kill().unwrap();
    let _ = daemon.wait();
}

#[cfg(unix)]
#[test]
fn daemon_reclaims_killed_client_leases_after_the_ttl() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = start_daemon(&sb);
    let socket = sb.rgo_home.join("state/daemon.sock");
    let response = ipc::request_with_timeout(
        &socket,
        Request::AcquireLease {
            scope: rgo_protocol::LeaseScope::Workspace {
                workspace_root: "/tmp/killed-client".into(),
            },
            pid: 4242,
            ttl_secs: 3,
        },
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(matches!(response, Response::Lease { .. }));
    let active =
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)).unwrap();
    assert!(matches!(
        active,
        Response::Status(status) if status.active_leases >= 1
    ));
    // The lease uses whole-second wall-clock deadlines. One second can expire
    // before the immediate status request reaches a busy daemon.
    thread::sleep(Duration::from_millis(3_200));
    let expired =
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)).unwrap();
    assert!(matches!(
        expired,
        Response::Status(status) if status.active_leases == 0
    ));
    daemon.kill().unwrap();
    let _ = daemon.wait();
}
