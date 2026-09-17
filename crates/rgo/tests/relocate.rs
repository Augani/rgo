//! End-to-end: after `rgo setup`, a plain `cargo build` puts intermediates under
//! `$RGO_HOME/builds/...` while the final binary still lands in `<project>/target/debug/`.
//! Runs real cargo; offline; no registry needed.

use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn plain_cargo_build_is_relocated_and_binary_still_uplifted() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();

    let setup = sb
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let cfg = std::fs::read_to_string(sb.cargo_home.join("config.toml")).unwrap();
    assert!(cfg.contains("build-dir"), "config was not written:\n{cfg}");

    let proj = sb.simple_bin("hello").unwrap();
    let build = sb
        .cargo()
        .current_dir(&proj)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "cargo build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let bin = proj
        .join("target/debug")
        .join(if cfg!(windows) { "hello.exe" } else { "hello" });
    assert!(
        bin.exists(),
        "final binary must still be uplifted into the checkout's target/"
    );
    assert!(
        !proj.join("target/debug/deps").exists(),
        "intermediates must NOT be in the checkout"
    );

    // Cargo's {workspace-path-hash} expands to two components: builds/xx/yyyy
    let shards: Vec<_> = std::fs::read_dir(sb.rgo_home.join("builds"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(shards.len(), 1);
    let builds: Vec<_> = std::fs::read_dir(shards[0].path())
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(builds.len(), 1, "exactly one managed build-dir expected");
    assert!(
        builds[0].path().join("debug/deps").exists(),
        "intermediates live in the managed build-dir"
    );
    assert!(
        builds[0].path().join(".rgo-context.json").exists(),
        "wrapper wrote the sidecar"
    );

    let ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(ls.status.success());
    let out = String::from_utf8_lossy(&ls.stdout);
    assert!(
        out.contains(proj.to_str().unwrap()),
        "rgo ls should attribute the build-dir via the wrapper sidecar:\n{out}"
    );
}

#[test]
fn deleting_the_checkout_makes_the_context_an_orphan() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let proj = sb.simple_bin("gone").unwrap();
    assert!(
        sb.cargo()
            .current_dir(&proj)
            .args(["build", "--offline"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::remove_dir_all(&proj).unwrap();

    let out = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("(orphan)"));
}
