use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use rgo_core::{context, paths::RgoPaths, supervision};
use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

#[test]
fn setup_dry_run_includes_the_platform_service_without_mutating_the_sandbox() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "setup dry-run failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("would install service"), "{output}");
    #[cfg(target_os = "macos")]
    assert!(
        output.contains("<string>daemon</string><string>--foreground</string>"),
        "{output}"
    );
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    assert!(output.contains("daemon --foreground"), "{output}");
    assert!(!sandbox.cargo_home.join("config.toml").exists());

    let undo = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--undo", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(String::from_utf8_lossy(&undo.stdout).contains("would remove service"));
    assert!(!sandbox.cargo_home.join("config.toml").exists());
}

#[test]
fn automatic_gc_can_be_disabled_without_starting_coordination() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    std::fs::write(sandbox.rgo_home.join("config.toml"), "[gc]\nauto = false\n").unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--auto"])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
    assert!(!sandbox.rgo_home.join("state/daemon.sock").exists());
}

#[test]
fn automatic_gc_without_pressure_is_quiet_and_does_not_start_a_daemon() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--auto"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    assert!(!sandbox.rgo_home.join("state/daemon.sock").exists());
}

#[test]
fn doctor_does_not_start_daemon_and_emits_structured_output() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .arg("doctor")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(!sandbox.rgo_home.join("state/daemon.sock").exists());
    assert!(!sandbox.rgo_home.join("state/daemon.pid").exists());
    let json = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert!(report["profile_locks_observed"].is_null());
    assert!(
        report["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
    );
    assert!(
        report["entries"]
            .as_array()
            .is_some_and(
                |entries| entries.iter().any(|entry| entry["level"] == "warning"
                    && entry["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("automatic GC disabled")))
            )
    );
    let entries = report["entries"].as_array().unwrap();
    assert!(entries.iter().any(|entry| {
        entry["level"] == "ok"
            && entry["message"]
                .as_str()
                .is_some_and(|message| message.contains("active Cargo from this working directory"))
    }));
    assert!(entries.iter().all(|entry| {
        entry["message"].as_str().is_none_or(|message| {
            !message.starts_with("installed toolchain") || entry["level"] == "info"
        })
    }));
}

#[test]
fn doctor_reports_project_build_override_and_legacy_config_precedence() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("doctor-project-config").unwrap();
    let config_dir = project.join(".cargo");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "include = [\"override.toml\"]\n[build]\ntarget-dir = \"finals\"\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("override.toml"),
        "[build]\nbuild-dir = \"intermediates\"\n",
    )
    .unwrap();
    let doctor = |cwd: &std::path::Path| {
        let output = sandbox
            .cmd(cargo_bin("rgo"))
            .current_dir(cwd)
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let report = doctor(&project);
    let messages: Vec<&str> = report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["message"].as_str())
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| { message.contains("config.toml may set build.build-dir") })
    );
    assert!(
        messages
            .iter()
            .any(|message| { message.contains("config.toml sets build.target-dir = \"finals\"") })
    );

    // Cargo ignores config.toml when the extensionless file exists here.
    std::fs::write(
        config_dir.join("config"),
        "[build]\ntarget-dir = \"legacy\"\n",
    )
    .unwrap();
    let report = doctor(&project);
    let messages: Vec<&str> = report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["message"].as_str())
        .collect();
    assert!(!messages.iter().any(|message| {
        message.contains("project configuration") && message.contains("may set build.build-dir")
    }));
    assert!(
        messages
            .iter()
            .any(|message| { message.contains("config sets build.target-dir = \"legacy\"") })
    );

    let global_config = sandbox.cargo_home.join("config.toml");
    std::fs::write(&global_config, "[build]\nbuild-dir = \"global-build\"\n").unwrap();
    let report = doctor(&sandbox.home);
    assert!(!report["entries"].as_array().unwrap().iter().any(|entry| {
        entry["message"].as_str().is_some_and(|message| {
            message.contains("project configuration")
                && message.contains(&global_config.display().to_string())
        })
    }));
}

#[test]
fn gc_target_rejects_non_concrete_sizes_before_startup() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--target", "not-a-size"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("invalid size"));
    assert!(!sandbox.rgo_home.join("state/daemon.sock").exists());
}

#[test]
fn pressure_gc_reclaims_idle_unpinned_only_contexts_in_private_home() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sandbox.cargo_home.display()),
    )
    .unwrap();
    for index in 0..3 {
        let workspace = sandbox.projects.join(format!("pressure-{index}"));
        std::fs::create_dir_all(&workspace).unwrap();
        let manifest = workspace.join("Cargo.toml");
        std::fs::write(&manifest, "[workspace]\n").unwrap();
        let dir = paths.builds_dir().join(format!("{index:02x}/context"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data"), vec![index as u8 + 1; 2 * 1024 * 1024]).unwrap();
        context::write_supervised_sidecar(&dir, &workspace, &manifest, true).unwrap();
    }

    let automatic = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--auto"])
        .output()
        .unwrap();
    assert!(automatic.status.success());
    assert_eq!(
        paths.managed_build_dirs().len(),
        3,
        "unattended deletion must stay disabled until Cargo lifecycle safety is proven"
    );
    assert!(!paths.socket_path().exists());

    let result = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["gc", "--target", "1MB"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\ndaemon log:\n{}",
        String::from_utf8_lossy(&result.stderr),
        std::fs::read_to_string(paths.logs_dir().join("daemon.log")).unwrap_or_default()
    );
    assert!(
        paths.managed_build_dirs().is_empty(),
        "every safely idle context must remain eligible under pressure: {}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn explicit_clean_reports_a_locked_context_instead_of_claiming_removal() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    paths.ensure_layout().unwrap();
    std::fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
    std::fs::write(
        paths.state_dir().join("owner-cargo-home"),
        format!("{}\n", sandbox.cargo_home.display()),
    )
    .unwrap();
    let project = sandbox.simple_bin("clean-locked").unwrap();
    let context_dir = paths.builds_dir().join("aa/context");
    std::fs::create_dir_all(&context_dir).unwrap();
    std::fs::write(context_dir.join("output"), b"keep").unwrap();
    context::write_supervised_sidecar(&context_dir, &project, &project.join("Cargo.toml"), true)
        .unwrap();

    let guard = supervision::lock_cargo_session(&paths, Some(&context_dir)).unwrap();
    let blocked = sandbox
        .cmd(cargo_bin("rgo"))
        .env("RGO_LOG", "debug")
        .args(["clean", "aa/context"])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("supervised Cargo invocation"),
        "{}\ndaemon log:\n{}",
        String::from_utf8_lossy(&blocked.stderr),
        std::fs::read_to_string(paths.logs_dir().join("daemon.log")).unwrap_or_default()
    );
    assert!(context_dir.join("output").is_file());

    drop(guard);
    // A daemon maintenance pass can briefly acquire the same nonblocking
    // guard between a probe and the command. Retry only that transient
    // refusal; every other clean error remains a failure.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let cleaned = loop {
        let result = sandbox
            .cmd(cargo_bin("rgo"))
            .args(["clean", "aa/context"])
            .output()
            .unwrap();
        if result.status.success() {
            break result;
        }
        let stderr = String::from_utf8_lossy(&result.stderr);
        if !stderr.contains("a supervised Cargo invocation is using managed storage")
            || std::time::Instant::now() >= deadline
        {
            break result;
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert!(
        cleaned.status.success(),
        "{}\ndaemon log:\n{}",
        String::from_utf8_lossy(&cleaned.stderr),
        std::fs::read_to_string(paths.logs_dir().join("daemon.log")).unwrap_or_default()
    );
    assert!(String::from_utf8_lossy(&cleaned.stdout).contains("removed aa/context"));
    assert!(!context_dir.exists());
}

#[test]
fn concurrent_builds_survive_aggressive_gc() {
    ensure_workspace_bins_built().unwrap();
    let sandbox = Sandbox::new().unwrap();
    let real_cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
    let setup = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&real_cargo)
        .arg("--no-service")
        .status()
        .unwrap();
    assert!(setup.success());
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    let shim = PathBuf::from(record["supervised_cargo"]["shim_path"].as_str().unwrap());
    let mut daemon = sandbox
        .cmd(cargo_bin("rgo"))
        .args(["daemon", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if sandbox.rgo_home.join("state/daemon.sock").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let projects = (0..5)
        .map(|index| sandbox.simple_bin(&format!("worktree-{index}")))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let mut builds = Vec::new();
    for index in 0..20 {
        let project = &projects[index % projects.len()];
        builds.push(
            sandbox
                .cmd(&shim)
                .current_dir(project)
                .args(["build", "--offline"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }

    let rgo = cargo_bin("rgo").to_path_buf();
    let home = sandbox.home.clone();
    let cargo_home = sandbox.cargo_home.clone();
    let rgo_home = sandbox.rgo_home.clone();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let gc = thread::spawn(move || {
        for _ in 0..12 {
            let status = Command::new(&rgo)
                .env_clear()
                .env("PATH", &path)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("CARGO_HOME", &cargo_home)
                .env("RGO_HOME", &rgo_home)
                .args(["gc", "--aggressive"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if status.is_ok_and(|status| status.success()) {
                continue;
            }
            thread::yield_now();
        }
    });

    let outcomes: Vec<_> = builds
        .drain(..)
        .map(|build| build.wait_with_output().unwrap())
        .collect();
    gc.join().unwrap();
    daemon.kill().unwrap();
    let _ = daemon.wait();
    for (index, outcome) in outcomes.iter().enumerate() {
        assert!(
            outcome.status.success(),
            "Cargo build {index} failed under concurrent GC: {}\ndaemon log:\n{}",
            String::from_utf8_lossy(&outcome.stderr),
            std::fs::read_to_string(sandbox.rgo_home.join("logs/daemon.log")).unwrap_or_default()
        );
    }
}
