#![cfg(windows)]

use std::path::PathBuf;

use rgo_core::context;
use rgo_core::paths::RgoPaths;
use rgo_testkit::Sandbox;

#[test]
fn suspended_job_launcher_builds_with_unchanged_cargo_arguments() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("windows-job-guard").unwrap();
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .expect("Cargo sets CARGO for integration tests");
    assert!(real_cargo.is_absolute());

    let output = sandbox
        .cmd(env!("CARGO_BIN_EXE_rgo"))
        .current_dir(&project)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&real_cargo)
        .args(["--", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.join("target/debug/windows-job-guard.exe").exists());
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.checked_managed_build_dirs().unwrap();
    assert_eq!(contexts.len(), 1);
    assert!(context::read_sidecar(&contexts[0]).is_some());
}
