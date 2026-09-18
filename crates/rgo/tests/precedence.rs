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
