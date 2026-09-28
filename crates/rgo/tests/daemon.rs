//! Daemon/lease integration tests run real Cargo in a private Sandbox.

use std::process::{Child, Stdio};
use std::thread;
use std::time::Duration;

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
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let result = reader.read(&mut [0u8; 1]);
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(
        result.unwrap(),
        0,
        "daemon retained an inherited pipe descriptor"
    );
}

fn start_daemon(sb: &Sandbox) -> Child {
    let mut child = sb
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if matches!(
            ipc::request_with_timeout(
                &sb.rgo_home.join("state/daemon.sock"),
                Request::QueryStatus,
                Duration::from_millis(100)
            ),
            Ok(Response::Status(_))
        ) {
            return child;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon did not create its socket");
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

    let mut daemon = sb
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .env("RGO_DAEMON_POLL_SECS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while record.exists() && std::time::Instant::now() < deadline {
        assert!(daemon.try_wait().unwrap().is_none(), "daemon exited early");
        thread::sleep(Duration::from_millis(100));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(
        !record.exists(),
        "maintenance kept an obsolete unpin decision"
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
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if matches!(
            ipc::request_with_timeout(
                &sb.rgo_home.join("state/daemon.sock"),
                Request::QueryStatus,
                Duration::from_millis(100)
            ),
            Ok(Response::Status(_))
        ) {
            daemon.kill().unwrap();
            let _ = daemon.wait();
            return;
        }
        assert!(daemon.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(25));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    panic!("explicit daemon home did not create the expected socket");
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
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let mut daemon = start_daemon(&sb);
    let project = sb.simple_bin("pinned-project").unwrap();
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

    // Saturate the connection cap with clients that connect then never send.
    let mut held = Vec::new();
    for _ in 0..70 {
        held.push(UnixStream::connect(&socket).unwrap());
    }
    thread::sleep(Duration::from_millis(400));
    // While saturated, requests are refused fast instead of queueing unboundedly.
    assert!(
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(1)).is_err(),
        "daemon should refuse connections beyond the cap"
    );

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
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
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
    for _ in 0..80 {
        if sb.rgo_home.join("state/daemon.sock").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let project = sb.simple_bin("auto-gc-project").unwrap();
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
    let context_dir = only_context_dir(&sb);
    age_context_into_orphan(&project, &context_dir);

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while context_dir.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(250));
    }
    assert!(
        !context_dir.exists(),
        "daemon maintenance did not auto-GC the aged orphan"
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
            ttl_secs: 1,
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
    thread::sleep(Duration::from_millis(1_200));
    let expired =
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)).unwrap();
    assert!(matches!(
        expired,
        Response::Status(status) if status.active_leases == 0
    ));
    daemon.kill().unwrap();
    let _ = daemon.wait();
}
