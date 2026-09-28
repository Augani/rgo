#![cfg(windows)]

use std::path::PathBuf;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use rgo_core::context;
use rgo_core::gc;
use rgo_core::paths::RgoPaths;
use rgo_core::supervision;
use rgo_testkit::Sandbox;

#[test]
fn suspended_job_launcher_guards_running_cargo_and_allows_unrelated_gc() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("windows-job-guard").unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        r#"
fn main() {
    let ready = std::path::PathBuf::from(std::env::var_os("RGO_TEST_READY").unwrap());
    let release = std::path::PathBuf::from(std::env::var_os("RGO_TEST_RELEASE").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    while !release.exists() {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
"#,
    )
    .unwrap();
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .expect("Cargo sets CARGO for integration tests");
    assert!(real_cargo.is_absolute());
    let ready = sandbox.home.join("running-ready");
    let release = sandbox.home.join("running-release");

    let mut child = sandbox
        .cmd(env!("CARGO_BIN_EXE_rgo"))
        .current_dir(&project)
        .env("RGO_TEST_READY", &ready)
        .env("RGO_TEST_RELEASE", &release)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&real_cargo)
        .args(["--", "run", "--offline"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "Cargo exited before its program started: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !ready.exists() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "Cargo did not start its program: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let checks = std::panic::catch_unwind(|| {
        assert!(project.join("target/debug/windows-job-guard.exe").exists());
        let contexts = paths.checked_managed_build_dirs().unwrap();
        assert_eq!(contexts.len(), 1);
        assert!(context::read_sidecar(&contexts[0]).is_some());
        assert!(
            supervision::try_lock_gc(&paths, Some(&contexts[0]))
                .unwrap()
                .is_none()
        );

        let idle_project = sandbox.simple_bin("windows-idle").unwrap();
        let idle = paths.builds_dir().join("bb/idle");
        std::fs::create_dir_all(&idle).unwrap();
        std::fs::write(idle.join("output"), b"reclaim").unwrap();
        context::write_sidecar(&idle, &idle_project, &idle_project.join("Cargo.toml"), None)
            .unwrap();
        gc::remove_atomically(&paths, &idle).unwrap();
        assert!(!idle.exists());
        assert!(contexts[0].exists());
        contexts[0].clone()
    });
    std::fs::write(&release, b"done").unwrap();
    let output = child.wait_with_output().unwrap();
    let context = match checks {
        Ok(context) => context,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        supervision::try_lock_gc(&paths, Some(&context))
            .unwrap()
            .is_some()
    );
}
