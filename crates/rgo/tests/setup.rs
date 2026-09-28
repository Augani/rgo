use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn setup_migrates_legacy_pins_without_a_running_service() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let paths = rgo_core::paths::RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    let dir = paths.builds_dir().join("aa/legacy");
    std::fs::create_dir_all(&dir).unwrap();
    rgo_core::context::write_pin_marker(&dir).unwrap();

    let setup = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(rgo_core::context::is_pinned(&paths, &dir));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn failed_service_activation_rolls_back_a_new_setup() {
    use std::os::unix::fs::PermissionsExt;

    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let fake_bin = sandbox.projects.join("fake-service-manager");
    std::fs::create_dir(&fake_bin).unwrap();
    let (name, script) = if cfg!(target_os = "macos") {
        (
            "launchctl",
            "#!/bin/sh\ncase \"$1\" in print|bootstrap) exit 1;; esac\nexit 0\n",
        )
    } else {
        (
            "systemctl",
            "#!/bin/sh\ncase \"$2\" in is-active|enable) exit 1;; esac\nexit 0\n",
        )
    };
    let manager = fake_bin.join(name);
    std::fs::write(&manager, script).unwrap();
    std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let rgo = cargo_bin("rgo");
    let setup = sandbox
        .cmd(&rgo)
        .env("PATH", &path)
        .arg("setup")
        .output()
        .unwrap();
    assert!(!setup.status.success());
    assert!(
        String::from_utf8_lossy(&setup.stderr).contains("new activation was rolled back"),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let config = sandbox.cargo_home.join("config.toml");
    assert!(
        !config.exists(),
        "rollback left Cargo config {:?}",
        std::fs::read_to_string(&config)
    );
    assert!(!sandbox.cargo_home.join(".rgo-install.json").exists());
    assert!(!sandbox.rgo_home.join("state/storage-mode").exists());

    let undo = sandbox
        .cmd(&rgo)
        .env("PATH", &path)
        .args(["setup", "--undo"])
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(!config.exists());
}

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
fn setup_and_undo_refuse_unowned_or_edited_cargo_fences() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    let config_path = sandbox.cargo_home.join("config.toml");
    let record_path = sandbox.cargo_home.join(".rgo-install.json");

    let first = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let owned_config = std::fs::read_to_string(&config_path).unwrap();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    let owned_build_dir = record["managed_build_dir"].as_str().unwrap();
    let changed_config = owned_config.replace(
        owned_build_dir,
        "/tmp/another-rgo-home/builds/{workspace-path-hash}",
    );
    assert_ne!(changed_config, owned_config);
    std::fs::write(&config_path, &changed_config).unwrap();

    for args in [
        &["setup", "--no-service"][..],
        &["setup", "--undo", "--no-service"][..],
    ] {
        let result = sandbox.cmd(&rgo).args(args).output().unwrap();
        assert!(!result.status.success());
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            changed_config
        );
    }

    std::fs::write(&config_path, &owned_config).unwrap();
    std::fs::remove_file(&record_path).unwrap();
    let result = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), owned_config);
}

#[test]
fn one_storage_root_cannot_be_claimed_by_two_cargo_homes() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    let other_cargo_home = sandbox.home.join("other-cargo-home");
    std::fs::create_dir_all(&other_cargo_home).unwrap();

    let first = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let competing = sandbox
        .cmd(&rgo)
        .env("CARGO_HOME", &other_cargo_home)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(!competing.status.success());
    assert!(String::from_utf8_lossy(&competing.stderr).contains("already owned"));
    assert!(!other_cargo_home.join("config.toml").exists());

    let undo = sandbox
        .cmd(&rgo)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(undo.status.success());
    let second = sandbox
        .cmd(&rgo)
        .env("CARGO_HOME", &other_cargo_home)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(second.status.success());
}

#[cfg(unix)]
#[test]
fn dry_run_rejects_an_unsafe_storage_root_without_changing_it() {
    use std::os::unix::fs::PermissionsExt;

    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    std::fs::set_permissions(&sandbox.rgo_home, std::fs::Permissions::from_mode(0o777)).unwrap();

    let result = sandbox
        .cmd(&rgo)
        .args(["setup", "--dry-run", "--no-service"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("writable by other users"));
    assert_eq!(
        std::fs::metadata(&sandbox.rgo_home)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o777
    );
    assert!(!sandbox.cargo_home.join(".rgo-install.json").exists());
}

#[test]
fn different_storage_roots_render_different_service_names() {
    ensure_workspace_bins_built().unwrap();
    let rgo = cargo_bin("rgo");
    let first = Sandbox::new().unwrap();
    let second = Sandbox::new().unwrap();
    let rendered = |sandbox: &Sandbox| {
        let output = sandbox
            .cmd(&rgo)
            .args(["setup", "--dry-run"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find(|line| line.starts_with("would install service "))
            .unwrap()
            .strip_prefix("would install service ")
            .unwrap()
            .split_once(" at ")
            .unwrap()
            .0
            .to_owned()
    };
    assert_ne!(rendered(&first), rendered(&second));
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
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["original_rustc_wrapper"], "sccache");
    assert_eq!(record["protocol_version"], rgo_protocol::PROTOCOL_VERSION);
    // The external activation record recovers the original wrapper even if
    // the managed state file was lost before undo.
    std::fs::remove_file(sandbox.rgo_home.join("state/inner-wrapper")).unwrap();
    let repair_dir = sandbox.projects.join("repair-bin");
    std::fs::create_dir_all(&repair_dir).unwrap();
    let repair_rgo = repair_dir.join(if cfg!(windows) { "rgo.exe" } else { "rgo" });
    std::fs::copy(&rgo, &repair_rgo).unwrap();
    std::fs::set_permissions(&repair_rgo, std::fs::metadata(&rgo).unwrap().permissions()).unwrap();

    let mut busy_retries = 0;
    let undo = loop {
        let attempt = sandbox
            .cmd(&repair_rgo)
            .args(["setup", "--undo", "--no-service"])
            .output();
        match attempt {
            Ok(output) => break output,
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && busy_retries < 10 =>
            {
                // A freshly copied executable can briefly be ETXTBSY on the
                // Linux CI filesystem. Retry only this launch error.
                busy_retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => panic!("starting the repair binary: {error}"),
        }
    };
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
    assert!(!sandbox.cargo_home.join(".rgo-install.json").exists());
}

#[test]
fn custom_rgo_home_survives_a_fresh_cargo_process() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let custom_home = sandbox.projects.join("custom-rgo-home");
    let rgo = cargo_bin("rgo");
    let setup = sandbox
        .cmd(&rgo)
        .env("RGO_HOME", &custom_home)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.cargo_home.join(".rgo-home")).unwrap(),
        format!("{}\n", custom_home.display())
    );

    let project = sandbox.simple_bin("fresh-cargo").unwrap();
    let build = sandbox
        .cargo()
        .env_remove("RGO_HOME")
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let managed = rgo_core::paths::RgoPaths {
        root: custom_home.clone(),
    };
    assert_eq!(managed.managed_build_dirs().len(), 1);
    assert!(
        managed.managed_build_dirs()[0]
            .join(rgo_protocol::SIDECAR_FILE)
            .is_file(),
        "the wrapper must attribute the custom context without RGO_HOME"
    );
    assert!(
        sandbox.rgo_home.join("builds").read_dir().is_err(),
        "the default home must remain unused"
    );

    let verify = sandbox
        .cmd(&rgo)
        .env_remove("RGO_HOME")
        .args(["doctor", "--verify", "--json"])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&verify.stdout).unwrap();
    assert_eq!(report["activation_verified"], true);
    let overridden = sandbox
        .cmd(&rgo)
        .env_remove("RGO_HOME")
        .env(
            "CARGO_BUILD_BUILD_DIR",
            sandbox.projects.join("override-build"),
        )
        .args(["doctor", "--verify", "--json"])
        .output()
        .unwrap();
    assert!(!overridden.status.success());
    let report: serde_json::Value = serde_json::from_slice(&overridden.stdout).unwrap();
    assert_eq!(report["activation_verified"], false);

    let undo = sandbox
        .cmd(&rgo)
        .env_remove("RGO_HOME")
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(!sandbox.cargo_home.join(".rgo-home").exists());
}

#[test]
fn storage_only_activation_is_verified_from_cargos_build_script_location() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    let setup = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-wrapper", "--no-service"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let config = std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap();
    assert!(config.contains("build-dir"));
    assert!(!config.contains("rustc-wrapper"));

    let verify = sandbox
        .cmd(&rgo)
        .args(["doctor", "--verify", "--json"])
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&verify.stdout).unwrap();
    assert_eq!(report["activation_verified"], true);

    let overridden = sandbox
        .cmd(&rgo)
        .env(
            "CARGO_BUILD_BUILD_DIR",
            sandbox.projects.join("override-build"),
        )
        .args(["doctor", "--verify", "--json"])
        .output()
        .unwrap();
    assert!(!overridden.status.success());
    let report: serde_json::Value = serde_json::from_slice(&overridden.stdout).unwrap();
    assert_eq!(report["activation_verified"], false);
}

#[test]
fn setup_and_undo_preserve_inline_and_dotted_build_settings() {
    ensure_workspace_bins_built().unwrap();
    let rgo = cargo_bin("rgo");
    for config in [
        "build = { jobs = 2, rustflags = [\"-C\", \"debuginfo=1\"] }\n",
        "build.jobs = 2\nbuild.rustflags = [\"-C\", \"debuginfo=1\"]\n",
    ] {
        let sandbox = Sandbox::new().unwrap();
        sandbox.write_cargo_config(config).unwrap();
        let setup = sandbox
            .cmd(&rgo)
            .args(["setup", "--no-wrapper", "--no-service"])
            .output()
            .unwrap();
        assert!(
            setup.status.success(),
            "{config:?}: {}",
            String::from_utf8_lossy(&setup.stderr)
        );
        let verify = sandbox
            .cmd(&rgo)
            .args(["doctor", "--verify", "--json"])
            .output()
            .unwrap();
        assert!(
            verify.status.success(),
            "{config:?}: {}",
            String::from_utf8_lossy(&verify.stderr)
        );
        let undo = sandbox
            .cmd(&rgo)
            .args(["setup", "--undo", "--no-service"])
            .output()
            .unwrap();
        assert!(
            undo.status.success(),
            "{config:?}: {}",
            String::from_utf8_lossy(&undo.stderr)
        );
        let restored = std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap();
        assert!(restored.contains("jobs = 2"));
        assert!(
            rgo_core::cargo_config::inspect(&restored)
                .unwrap()
                .configured_build_dir
                .is_none()
        );
    }
}

#[test]
fn setup_keeps_included_wrapper_settings_and_uses_storage_only() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    sandbox
        .write_cargo_config("include = [\"extra.toml\"]\n")
        .unwrap();
    let included = sandbox.cargo_home.join("extra.toml");
    std::fs::write(&included, "[build]\nrustc-wrapper = \"sccache\"\n").unwrap();
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
    let configured = std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap();
    assert!(configured.contains("include = [\"extra.toml\"]"));
    assert!(configured.contains("build-dir"));
    assert!(!configured.contains("rustc-wrapper"));
    assert_eq!(
        std::fs::read_to_string(&included).unwrap(),
        "[build]\nrustc-wrapper = \"sccache\"\n"
    );
    assert!(
        sandbox
            .cmd(&rgo)
            .args(["setup", "--undo", "--no-service"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap(),
        "include = [\"extra.toml\"]\n"
    );
}

#[test]
fn concurrent_setup_serializes_config_and_install_record() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    let mut first = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .spawn()
        .unwrap();
    let mut second = sandbox
        .cmd(&rgo)
        .args(["setup", "--no-service"])
        .spawn()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    let config = std::fs::read_to_string(sandbox.cargo_home.join("config.toml")).unwrap();
    assert!(rgo_core::cargo_config::inspect(&config).unwrap().has_fence);
    assert_eq!(
        config.matches(rgo_core::cargo_config::FENCE_START).count(),
        1
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["schema_version"], 2);
}

#[test]
fn undo_uses_the_owned_config_when_legacy_precedence_changes() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let rgo = cargo_bin("rgo");
    assert!(
        sandbox
            .cmd(&rgo)
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );
    let legacy = sandbox.cargo_home.join("config");
    std::fs::write(&legacy, "[net]\noffline = true\n").unwrap();
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
    assert_eq!(
        std::fs::read_to_string(&legacy).unwrap(),
        "[net]\noffline = true\n"
    );
    assert!(
        !std::fs::read_to_string(sandbox.cargo_home.join("config.toml"))
            .unwrap()
            .contains("rgo managed")
    );
}

#[cfg(unix)]
#[test]
fn setup_rejects_a_mismatched_wrapper_before_activation() {
    use std::os::unix::fs::PermissionsExt;

    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let bin_dir = sandbox.projects.join("bad-bundle");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let rgo = bin_dir.join("rgo");
    std::fs::copy(cargo_bin("rgo"), &rgo).unwrap();
    std::fs::set_permissions(&rgo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wrapper = bin_dir.join("rgo-rustc-wrapper");
    std::fs::write(&wrapper, "#!/bin/sh\necho 'wrong wrapper'\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    // A just-copied executable can briefly report ETXTBSY on Linux runners.
    // Retry only that transient exec error; all other launch failures remain
    // immediate test failures.
    let result = (0..10)
        .find_map(
            |_| match sandbox.cmd(&rgo).args(["setup", "--no-service"]).output() {
                Ok(result) => Some(result),
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    None
                }
                Err(error) => panic!("launching copied rgo: {error}"),
            },
        )
        .expect("copied rgo remained busy for one second");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not a matching rgo wrapper"));
    assert!(!sandbox.cargo_home.join("config.toml").exists());
    assert!(!sandbox.cargo_home.join(".rgo-home").exists());
    assert!(!sandbox.cargo_home.join(".rgo-install.json").exists());
}
