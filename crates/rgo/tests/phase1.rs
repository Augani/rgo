use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn setup_dry_run_includes_the_platform_service_without_mutating_the_sandbox() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "setup dry-run failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("would install service"), "{output}");
    assert!(output.contains("<string>daemon</string>"), "{output}");
    assert!(output.contains("<string>--foreground</string>"), "{output}");
    assert!(!sandbox.cargo_home.join("config.toml").exists());
}

#[test]
fn concurrent_builds_survive_aggressive_gc() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let setup = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--no-service"])
        .status()
        .unwrap();
    assert!(setup.success());
    let mut daemon = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if sandbox.rgo_home.join("state/daemon.sock").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let projects = (0..5)
        .map(|index| sandbox.simple_bin(&format!("worktree-{index}")))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let mut builds = Vec::new();
    for index in 0..20 {
        let project = &projects[index % projects.len()];
        builds.push(
            sandbox
                .cargo()
                .current_dir(project)
                .args(["build", "--offline"])
                .spawn()
                .unwrap(),
        );
    }

    let rgo = cargo_bin("rgo").to_path_buf();
    let home = sandbox.home.clone();
    let cargo_home = sandbox.cargo_home.clone();
    let rgo_home = sandbox.rgo_home.clone();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let gc = thread::spawn(move || {
        for _ in 0..12 {
            let status = Command::new(&rgo)
                .env_clear()
                .env("PATH", &path)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("CARGO_HOME", &cargo_home)
                .env("RGO_HOME", &rgo_home)
                .args(["gc", "--aggressive"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if status.is_ok_and(|status| status.success()) {
                continue;
            }
            thread::yield_now();
        }
    });

    for mut build in builds.drain(..) {
        let status = build.wait().unwrap();
        assert!(status.success(), "Cargo build failed under concurrent GC");
    }
    gc.join().unwrap();
    daemon.kill().unwrap();
    let _ = daemon.wait();
}
