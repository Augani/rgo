//! Daemon/lease integration tests run real Cargo in a private Sandbox.

use std::process::{Child, Stdio};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
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
    let mut daemon = start_daemon(&sb);
    let project = sb.simple_bin("daemon-project").unwrap();
    let build = sb
        .cmd(cargo_bin("rgo"))
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
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
    assert!(status_text.contains("Daemon               running"), "{status_text}");
    assert!(status_text.contains("Active leases        "), "{status_text}");

    let ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    let id = String::from_utf8_lossy(&ls.stdout)
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .expect("one context")
        .to_owned();
    let pin = sb.cmd(cargo_bin("rgo")).args(["pin", &id]).output().unwrap();
    assert!(pin.status.success(), "{}", String::from_utf8_lossy(&pin.stderr));
    let pinned_ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(String::from_utf8_lossy(&pinned_ls.stdout).contains("PIN"));

    let unpin = sb.cmd(cargo_bin("rgo")).args(["unpin", &id]).output().unwrap();
    assert!(unpin.status.success(), "{}", String::from_utf8_lossy(&unpin.stderr));

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
