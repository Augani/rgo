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
    assert!(
        cfg.contains("rustc-wrapper") && !cfg.contains("rustc-workspace-wrapper"),
        "all compiler invocations must pass through rgo so dependency caching is reachable:\n{cfg}"
    );

    let proj = sb.simple_bin("hello").unwrap();
    std::fs::write(proj.join("build.rs"), "fn main() {}\n").unwrap();
    let build = sb
        .cargo()
        .current_dir(&proj)
        .args(["build", "--offline", "--message-format=json"])
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
    let out_dir = String::from_utf8_lossy(&build.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| message["reason"] == "build-script-executed")
        .and_then(|message| message["out_dir"].as_str().map(std::path::PathBuf::from))
        .expect("Cargo did not report the build-script output directory");
    let builds_root = sb.rgo_home.join("builds");
    assert!(
        out_dir.starts_with(&builds_root),
        "Cargo reported intermediate output outside the managed root: {}",
        out_dir.display()
    );
    let context = out_dir
        .ancestors()
        .take_while(|directory| directory.starts_with(&builds_root))
        .find(|directory| directory.join(".rgo-context.json").is_file());
    assert!(
        context.is_some(),
        "wrapper did not write the context sidecar"
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
fn worktrees_of_one_repo_group_under_a_header_in_ls() {
    ensure_workspace_bins_built().unwrap();
    let sb = Sandbox::new().unwrap();
    assert!(
        sb.cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .status()
            .unwrap()
            .success()
    );

    // Main checkout: a plain `.git` directory.
    let main = sb.simple_bin("main-repo").unwrap();
    std::fs::create_dir_all(main.join(".git/worktrees/linked")).unwrap();
    std::fs::write(main.join(".git/worktrees/linked/commondir"), "../..\n").unwrap();

    // Linked worktree: `.git` file pointing at its per-worktree gitdir.
    let linked = sb.simple_bin("linked-wt").unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", main.join(".git/worktrees/linked").display()),
    )
    .unwrap();

    // An unrelated solo project stays outside any group.
    let solo = sb.simple_bin("solo").unwrap();
    std::fs::create_dir(solo.join(".git")).unwrap();

    for project in [&main, &linked, &solo] {
        assert!(
            sb.cargo()
                .current_dir(project)
                .args(["build", "--offline"])
                .status()
                .unwrap()
                .success()
        );
    }

    let ls = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    assert!(ls.status.success());
    let out = String::from_utf8_lossy(&ls.stdout);
    let repo = std::fs::canonicalize(&main).unwrap();
    let header = format!("── {} (2 contexts)", repo.display());
    assert!(
        out.contains(&header),
        "expected worktree group header {header:?}:\n{out}"
    );
    assert!(
        out.contains(main.to_str().unwrap()),
        "main row missing:\n{out}"
    );
    assert!(
        out.contains(linked.to_str().unwrap()),
        "linked worktree row missing:\n{out}"
    );
    assert!(
        out.contains(solo.to_str().unwrap()),
        "solo row missing:\n{out}"
    );
    // The solo repo must not get a group header of its own.
    assert_eq!(
        out.matches("── ").count(),
        1,
        "expected exactly one group:\n{out}"
    );
}

#[test]
fn deleting_the_checkout_reports_its_workspace_state() {
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
    let orphan_supported = ((cfg!(target_os = "macos") || cfg!(windows))
        && rgo_core::context::workspace_device(&proj).is_some())
        || rgo_core::context::workspace_mount_id(&proj).is_some();
    std::fs::remove_dir_all(&proj).unwrap();

    let out = sb.cmd(cargo_bin("rgo")).arg("ls").output().unwrap();
    let expected = if orphan_supported {
        "(orphan)"
    } else {
        "(workspace unavailable; protected)"
    };
    assert!(String::from_utf8_lossy(&out.stdout).contains(expected));
}
