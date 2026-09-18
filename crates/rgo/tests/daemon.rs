//! Daemon/lease integration tests run real Cargo in a private Sandbox.

use std::process::{Child, Stdio};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
#[cfg(unix)]
use rgo_core::ipc;
#[cfg(unix)]
use rgo_protocol::{PROTOCOL_VERSION, Request, Response};
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

fn start_daemon(sb: &Sandbox) -> Child {
    let mut child = sb
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if sb.rgo_home.join("state/daemon.sock").exists() {
            return child;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon did not create its socket");
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
    assert!(matches!(
        ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(2)).unwrap(),
        Response::Status(_)
    ));

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
                Some(".cargo-build-lock") | Some(".cargo-lock")
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

    // Lose the entire database: the pin marker inside the build dir is the durable
    // record, and a restarted daemon must rebuild the pins table from it.
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
        "pin must survive a full database rebuild: {}",
        String::from_utf8_lossy(&ls.stdout)
    );

    // Even an aged orphan is untouchable while pinned.
    age_context_into_orphan(&project, &context_dir);
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
        context_dir.exists(),
        "pinned context was collected despite the pin"
    );

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
        "[storage]\nmax_size = \"1KB\"\n",
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
