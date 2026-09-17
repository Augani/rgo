use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn adopt_reports_then_deletes_only_approved_intermediates() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.projects.join("legacy");
    let target = project.join("target/debug");
    std::fs::create_dir_all(target.join("deps")).unwrap();
    std::fs::create_dir_all(target.join("build")).unwrap();
    std::fs::create_dir_all(target.join("incremental")).unwrap();
    std::fs::create_dir_all(target.join(".fingerprint")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='legacy'\nversion='0.1.0'\n",
    )
    .unwrap();
    let final_binary = target.join("legacy");
    std::fs::write(&final_binary, b"keep me").unwrap();
    std::fs::write(target.join("deps/liblegacy.rlib"), b"remove me").unwrap();

    let report = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["adopt", project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(report.status.success());
    assert!(String::from_utf8_lossy(&report.stdout).contains("Report only"));
    assert!(target.join("deps/liblegacy.rlib").exists());

    let delete = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["adopt", "--delete", project.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        delete.status.success(),
        "adopt --delete failed: {}",
        String::from_utf8_lossy(&delete.stderr)
    );
    assert!(!target.join("deps").exists());
    assert!(!target.join("build").exists());
    assert!(final_binary.exists());
    assert_eq!(std::fs::read(final_binary).unwrap(), b"keep me");
}
