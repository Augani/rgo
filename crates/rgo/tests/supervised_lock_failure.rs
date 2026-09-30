#![cfg(any(unix, windows))]

use std::path::PathBuf;

use rgo_core::paths::RgoPaths;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn damaged_lifecycle_lock_keeps_plain_cargo_build_available() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX)))
                .find(|path| path.is_file())
        })
        .expect("real Cargo executable");
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&real_cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    let shim = PathBuf::from(record["supervised_cargo"]["shim_path"].as_str().unwrap());
    let managed_project = sandbox.simple_bin("lock-probe-managed").unwrap();
    let managed = sandbox
        .cmd(&shim)
        .current_dir(&managed_project)
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        managed.status.success(),
        "{}",
        String::from_utf8_lossy(&managed.stderr)
    );
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    assert_eq!(paths.checked_managed_build_dirs().unwrap().len(), 1);

    let locks = paths.state_dir().join("locks");
    std::fs::rename(&locks, paths.state_dir().join("locks-disabled")).unwrap();
    std::fs::write(&locks, b"unavailable").unwrap();
    let fallback_project = sandbox.simple_bin("lock-probe-fallback").unwrap();
    let fallback = sandbox
        .cmd(&shim)
        .current_dir(&fallback_project)
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        fallback.status.success(),
        "{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    assert!(
        String::from_utf8_lossy(&fallback.stderr).contains("lifecycle lock unavailable"),
        "{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    assert!(
        fallback_project
            .join("target/debug")
            .join(format!(
                "lock-probe-fallback{}",
                std::env::consts::EXE_SUFFIX
            ))
            .is_file()
    );
    assert_eq!(paths.checked_managed_build_dirs().unwrap().len(), 1);
}
