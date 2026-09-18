#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;

use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn bypass_passthrough_strips_rgo_environment_and_preserves_exit_status() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let cargo = sandbox.projects.join("fake-cargo");
    let log = sandbox.projects.join("cargo-env.log");
    std::fs::write(
        &cargo,
        format!("#!/bin/sh\nenv > '{}'\nexit 23\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .env("CARGO", &cargo)
        .env("RGO_BYPASS", "1")
        .env("RGO_SENTINEL", "must-not-leak")
        .args(["unknown-subcommand", "--exact-argument"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(23));
    let environment = std::fs::read_to_string(log).unwrap();
    assert!(!environment.lines().any(|line| line.starts_with("RGO_")));
}

#[test]
fn rustc_wrapper_bypass_executes_the_inner_compiler_directly() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rustc = sandbox.projects.join("fake-rustc");
    let log = sandbox.projects.join("rustc-env.log");
    std::fs::write(
        &rustc,
        format!("#!/bin/sh\nenv > '{}'\nexit 19\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&rustc, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo-rustc-wrapper"))
        .env("RGO_BYPASS", "1")
        .env("RGO_SENTINEL", "must-not-leak")
        .args([rustc.to_str().unwrap(), "--version"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(19));
    let environment = std::fs::read_to_string(log).unwrap();
    assert!(!environment.contains("RGO_BYPASS"));
    assert!(!environment.contains("RGO_SENTINEL"));
}

#[test]
fn passthrough_preserves_non_utf8_arguments_and_signal_exit_status() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let cargo = sandbox.projects.join("fake-cargo-non-utf8");
    std::fs::write(&cargo, "#!/bin/sh\nexit 23\n").unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .env("CARGO", &cargo)
        .arg(OsString::from_vec(vec![b'c', 0xff, b'a']))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(23));

    let signal_cargo = sandbox.projects.join("fake-cargo-signal");
    std::fs::write(&signal_cargo, "#!/bin/sh\nkill -TERM $$\n").unwrap();
    std::fs::set_permissions(&signal_cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .env("CARGO", &signal_cargo)
        .arg("version")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(128 + 15));
}
