use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn adopt_reports_target_storage_and_refuses_unsafe_selective_delete() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.projects.join("legacy");
    let target = project.join("target/debug");
    std::fs::create_dir_all(target.join("deps")).unwrap();
    std::fs::create_dir_all(target.join("build")).unwrap();
    std::fs::create_dir_all(target.join("incremental")).unwrap();
    std::fs::create_dir_all(target.join(".fingerprint")).unwrap();
    std::fs::create_dir_all(target.join("examples")).unwrap();
    std::fs::create_dir_all(target.join("doc")).unwrap();
    std::fs::create_dir_all(target.join("package")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='legacy'\nversion='0.1.0'\n",
    )
    .unwrap();
    let final_binary = target.join("legacy");
    std::fs::write(&final_binary, b"keep me").unwrap();
    std::fs::write(target.join("examples/example"), b"example").unwrap();
    std::fs::write(target.join("doc/index.html"), b"docs").unwrap();
    std::fs::write(target.join("package/legacy.crate"), b"package").unwrap();
    std::fs::write(target.join("user-file"), b"user").unwrap();
    std::fs::write(target.join("deps/liblegacy.rlib"), b"remove me").unwrap();

    let report = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["adopt", project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(report.status.success());
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert!(stdout.contains("Report only"), "{stdout}");
    assert!(
        stdout.contains("includes final outputs and user files"),
        "{stdout}"
    );
    assert!(target.join("deps/liblegacy.rlib").exists());

    let delete = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["adopt", "--delete", project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!delete.status.success());
    assert!(
        String::from_utf8_lossy(&delete.stderr).contains("deletion is unavailable"),
        "{}",
        String::from_utf8_lossy(&delete.stderr)
    );
    assert!(target.join("deps/liblegacy.rlib").exists());
    assert!(target.join("build").exists());
    assert!(final_binary.exists());
    assert_eq!(std::fs::read(final_binary).unwrap(), b"keep me");
    assert!(target.join("examples/example").exists());
    assert!(target.join("doc/index.html").exists());
    assert!(target.join("package/legacy.crate").exists());
    assert!(target.join("user-file").exists());
}

#[test]
fn adopt_skips_overrides_and_preserves_user_outputs() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.projects.join("overridden");
    let target = project.join("target/aarch64-unknown-linux-gnu/release");
    std::fs::create_dir_all(target.join("deps")).unwrap();
    std::fs::create_dir_all(project.join(".cargo")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='overridden'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".cargo/config.toml"),
        "[build]\ntarget-dir = \"../shared-target\"\n",
    )
    .unwrap();
    std::fs::write(target.join("deps/liboverridden.rlib"), b"keep").unwrap();
    std::fs::write(target.join("overridden"), b"final").unwrap();

    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["adopt", project.to_str().unwrap()])
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(result.status.success(), "{output}");
    assert!(output.contains("target-dir"), "{output}");
    assert!(target.join("deps/liboverridden.rlib").exists());
    assert!(target.join("overridden").exists());
}
