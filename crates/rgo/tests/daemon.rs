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
