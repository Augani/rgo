use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::cargo::cargo_bin;
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

fn build_script_out_dir(output: &Output) -> PathBuf {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| message["reason"] == "build-script-executed")
        .and_then(|message| message["out_dir"].as_str().map(PathBuf::from))
        .expect("Cargo did not report a build-script output directory")
}

fn add_build_script(project: &Path) {
    std::fs::write(project.join("build.rs"), "fn main() {}\n").unwrap();
}

fn final_binary(target: &Path, name: &str) -> PathBuf {
    target
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

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
    add_build_script(&project);
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
        .args(["build", "--offline", "--message-format=json"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(final_binary(&project_target, "project-override").exists());
    assert!(build_script_out_dir(&build).starts_with(&project_build));

    let env_project = sandbox.simple_bin("environment-override").unwrap();
    add_build_script(&env_project);
    let env_build = env_project.join("env-build");
    let env_target = env_project.join("env-target");
    let build = sandbox
        .cargo()
        .current_dir(&env_project)
        .env("CARGO_BUILD_BUILD_DIR", &env_build)
        .env("CARGO_TARGET_DIR", &env_target)
        .args(["build", "--offline", "--message-format=json"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(final_binary(&env_target, "environment-override").exists());
    assert!(build_script_out_dir(&build).starts_with(&env_build));

    let cli_project = sandbox.simple_bin("cli-override").unwrap();
    add_build_script(&cli_project);
    let cli_target = cli_project.join("cli-target");
    let build = sandbox
        .cargo()
        .current_dir(&cli_project)
        .args([
            "build",
            "--offline",
            "--target-dir",
            cli_target.to_str().unwrap(),
            "--message-format=json",
        ])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(final_binary(&cli_target, "cli-override").exists());
    let managed = rgo_core::paths::RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    assert!(
        build_script_out_dir(&build).starts_with(managed.builds_dir()),
        "--target-dir must not cancel the configured build directory"
    );

    let bypass_project = sandbox.simple_bin("bypass-override").unwrap();
    add_build_script(&bypass_project);
    let bypass = sandbox
        .cargo()
        .current_dir(&bypass_project)
        .env("RGO_BYPASS", "1")
        .args(["build", "--offline", "--message-format=json"])
        .output()
        .unwrap();
    assert!(
        bypass.status.success(),
        "{}",
        String::from_utf8_lossy(&bypass.stderr)
    );
    assert!(
        build_script_out_dir(&bypass).starts_with(managed.builds_dir()),
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
        let manifest = std::fs::canonicalize(project.join("Cargo.toml")).unwrap();
        assert!(managed.managed_build_dirs().iter().any(|dir| {
            rgo_core::context::read_sidecar(dir).is_some_and(|sidecar| {
                std::fs::canonicalize(sidecar.manifest_path).is_ok_and(|path| path == manifest)
            })
        }));

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

#[test]
fn native_cargo_attributes_member_build_to_workspace_root() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
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

    let workspace = sandbox.workspace("native-member", &["member"]).unwrap();
    let member_manifest = workspace.join("member/Cargo.toml");
    let build = sandbox
        .cargo()
        .current_dir(&sandbox.home)
        .args(["build", "--offline", "--manifest-path"])
        .arg(&member_manifest)
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
    let contexts = managed.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    let sidecar = rgo_core::context::read_sidecar(&contexts[0]).unwrap();
    assert!(sidecar.workspace_verified);
    assert_eq!(
        PathBuf::from(sidecar.workspace_root)
            .canonicalize()
            .unwrap(),
        workspace.canonicalize().unwrap()
    );
    assert_eq!(
        PathBuf::from(sidecar.manifest_path).canonicalize().unwrap(),
        workspace.join("Cargo.toml").canonicalize().unwrap()
    );

    // Sidecars from earlier native installs lack verification and can contain
    // the member path. The next real compile must correct that identity.
    let sidecar_path = contexts[0].join(rgo_protocol::SIDECAR_FILE);
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("workspace_verified");
    legacy["workspace_root"] = serde_json::json!(workspace.join("member"));
    legacy["manifest_path"] = serde_json::json!(member_manifest);
    std::fs::write(&sidecar_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    std::fs::write(
        workspace.join("member/src/main.rs"),
        "fn main() { println!(\"rebuilt\"); }\n",
    )
    .unwrap();
    let rebuild = sandbox
        .cargo()
        .current_dir(&sandbox.home)
        .args(["build", "--offline", "--manifest-path"])
        .arg(&member_manifest)
        .output()
        .unwrap();
    assert!(
        rebuild.status.success(),
        "{}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
    let repaired = rgo_core::context::read_sidecar(&contexts[0]).unwrap();
    assert!(repaired.workspace_verified);
    assert_eq!(
        PathBuf::from(repaired.workspace_root)
            .canonicalize()
            .unwrap(),
        workspace.canonicalize().unwrap()
    );
}
