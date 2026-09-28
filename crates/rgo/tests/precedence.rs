use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn cargo_precedence_overrides_keep_user_selected_build_and_target_roots() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    assert!(
        sandbox
            .cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );

    let project = sandbox.simple_bin("project-override").unwrap();
    let project_build = project.join("project-build");
    let project_target = project.join("project-target");
    std::fs::create_dir_all(project.join(".cargo")).unwrap();
    std::fs::write(
        project.join(".cargo/config.toml"),
        format!(
            "[build]\nbuild-dir = {:?}\ntarget-dir = {:?}\n",
            project_build, project_target
        ),
    )
    .unwrap();
    let build = sandbox
        .cargo()
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(project_target.join("debug/project-override").exists());
    assert!(project_build.join("debug/deps").is_dir());

    let env_project = sandbox.simple_bin("environment-override").unwrap();
    let env_build = env_project.join("env-build");
    let env_target = env_project.join("env-target");
    let build = sandbox
        .cargo()
        .current_dir(&env_project)
        .env("CARGO_BUILD_BUILD_DIR", &env_build)
        .env("CARGO_TARGET_DIR", &env_target)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(env_target.join("debug/environment-override").exists());
    assert!(env_build.join("debug/deps").is_dir());

    let cli_project = sandbox.simple_bin("cli-override").unwrap();
    let cli_target = cli_project.join("cli-target");
    let build = sandbox
        .cargo()
        .current_dir(&cli_project)
        .args([
            "build",
            "--offline",
            "--target-dir",
            cli_target.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(cli_target.join("debug/cli-override").exists());
    let managed = rgo_core::paths::RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    assert!(
        managed
            .managed_build_dirs()
            .iter()
            .any(|dir| dir.join("debug/deps").is_dir()),
        "--target-dir must not cancel the configured build directory"
    );

    let bypass_project = sandbox.simple_bin("bypass-override").unwrap();
    let before = managed.managed_build_dirs().len();
    let bypass = sandbox
        .cargo()
        .current_dir(&bypass_project)
        .env("RGO_BYPASS", "1")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        bypass.status.success(),
        "{}",
        String::from_utf8_lossy(&bypass.stderr)
    );
    assert!(
        managed.managed_build_dirs().len() > before,
        "RGO_BYPASS must not imply that Cargo relocation is disabled"
    );

    let metadata = sandbox
        .cmd(cargo_bin("rgo"))
        .current_dir(&project)
        .args([
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        metadata.status.success(),
        "{}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    assert!(String::from_utf8_lossy(&metadata.stdout).contains("project-override"));

    let toolchain = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["+stable", "--version"])
        .output()
        .unwrap();
    assert!(
        toolchain.status.success(),
        "{}",
        String::from_utf8_lossy(&toolchain.stderr)
    );
    assert!(String::from_utf8_lossy(&toolchain.stdout).contains("cargo"));
}

#[test]
fn setup_uses_cargos_legacy_home_config_when_present() {
    ensure_workspace_bins_built().unwrap();
    for both_files in [false, true] {
        let sandbox = Sandbox::new().unwrap();
        let legacy = sandbox.cargo_home.join("config");
        let user_config = "[net]\noffline = true\n";
        std::fs::write(&legacy, user_config).unwrap();
        let modern = sandbox.cargo_home.join("config.toml");
        if both_files {
            std::fs::write(&modern, "[term]\ncolor = 'never'\n").unwrap();
        }

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
        assert!(
            std::fs::read_to_string(&legacy)
                .unwrap()
                .contains("rgo managed")
        );
        if both_files {
            assert_eq!(
                std::fs::read_to_string(&modern).unwrap(),
                "[term]\ncolor = 'never'\n"
            );
        } else {
            assert!(!modern.exists());
        }

        let project = sandbox.simple_bin("legacy-project").unwrap();
        let build = sandbox
            .cargo()
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
            root: sandbox.rgo_home.clone(),
        };
        assert_eq!(managed.managed_build_dirs().len(), 1);

        let undo = sandbox
            .cmd(cargo_bin("rgo"))
            .args(["setup", "--undo", "--no-service"])
            .output()
            .unwrap();
        assert!(
            undo.status.success(),
            "{}",
            String::from_utf8_lossy(&undo.stderr)
        );
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), user_config);
    }
}
