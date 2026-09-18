use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn setup_is_atomic_idempotent_and_preserves_user_config() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    sandbox
        .write_cargo_config(
            "[build]\nrustflags = [\"-C\", \"debuginfo=1\"]\n[net]\noffline = true\n",
        )
        .unwrap();
    let rgo = cargo_bin("rgo");
    let first = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let config_path = sandbox.cargo_home.join("config.toml");
    let configured = std::fs::read_to_string(&config_path).unwrap();
    assert!(configured.contains("rustflags"));
    assert!(configured.contains("offline"));
    assert!(configured.contains("rgo managed"));

    let second = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(configured, std::fs::read_to_string(&config_path).unwrap());

    let dry_run = sandbox
        .cmd(&rgo)
        .args(["setup", "--dry-run", "--no-service"])
        .output()
        .unwrap();
    assert!(dry_run.status.success());
    assert_eq!(configured, std::fs::read_to_string(&config_path).unwrap());

    let undo = sandbox
        .cmd(&rgo)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(undo.status.success());
    let undone = std::fs::read_to_string(&config_path).unwrap();
    assert!(undone.contains("rustflags"));
    assert!(undone.contains("offline"));
    assert!(!undone.contains("rgo managed"));

    let repeated_undo = sandbox
        .cmd(&rgo)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(repeated_undo.status.success());
    assert_eq!(undone, std::fs::read_to_string(config_path).unwrap());
}

#[test]
fn setup_composes_and_restores_an_existing_rustc_wrapper() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    sandbox
        .write_cargo_config("[build]\nrustc-wrapper = \"sccache\"\njobs = 2\n")
        .unwrap();
    let rgo = cargo_bin("rgo");
    let setup = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let config = std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap();
    assert!(config.contains("rgo-rustc-wrapper"), "{config}");
    assert!(!config.contains("rustc-wrapper = \"sccache\""), "{config}");
    assert_eq!(
        std::fs::read_to_string(sandbox.rgo_home.join("state/inner-wrapper")).unwrap(),
        "sccache"
    );

    let undo = sandbox
        .cmd(&rgo)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    let restored = std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap();
    assert!(
        restored.contains("rustc-wrapper = \"sccache\""),
        "{restored}"
    );
    assert!(restored.contains("jobs = 2"), "{restored}");
    assert!(!restored.contains("rgo-rustc-wrapper"), "{restored}");
}
