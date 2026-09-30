#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rgo_core::context;
use rgo_core::gc;
use rgo_core::ipc;
use rgo_core::paths::RgoPaths;
use rgo_core::size;
use rgo_core::supervision;
use rgo_protocol::{Request, Response};
use rgo_testkit::Sandbox;

fn cargo_proxy() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("cargo"))
        .find(|path| path.is_file())
        .expect("Cargo proxy on PATH")
}

fn has_entries(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

fn age_finished_cargo_profile_locks(context: &Path) {
    let old = SystemTime::now() - Duration::from_secs(7200);
    for profile in std::fs::read_dir(context).unwrap().flatten() {
        if profile.path().is_dir() {
            let lock = profile.path().join(".cargo-build-lock");
            if lock.is_file() {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(lock)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
    }
}

#[test]
fn launcher_does_not_manage_another_cargo_home_or_storage_root() {
    let first = Sandbox::new().unwrap();
    let second = Sandbox::new().unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
    let first_setup = first
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        first_setup.status.success(),
        "{}",
        String::from_utf8_lossy(&first_setup.stderr)
    );
    let mut first_path = vec![first.cargo_home.join("rgo/shims")];
    first_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let first_path = std::env::join_paths(first_path).unwrap();
    let foreign_shim = second
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(first.cargo_home.join("rgo/shims/cargo"))
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(!foreign_shim.status.success());
    assert!(!second.cargo_home.join(".rgo-install.json").exists());
    let second_setup = second
        .cmd(rgo)
        .env("PATH", &first_path)
        .args(["setup", "--supervised", "--no-service"])
        .output()
        .unwrap();
    assert!(
        second_setup.status.success(),
        "{}",
        String::from_utf8_lossy(&second_setup.stderr)
    );
    let second_record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(second.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        PathBuf::from(
            second_record["supervised_cargo"]["real_cargo"]
                .as_str()
                .unwrap()
        )
        .canonicalize()
        .unwrap(),
        cargo.canonicalize().unwrap(),
    );
    let first_paths = RgoPaths {
        root: first.rgo_home.clone(),
    };
    let second_paths = RgoPaths {
        root: second.rgo_home.clone(),
    };
    let project = second.simple_bin("other-cargo-home").unwrap();
    let misplaced = second
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &first_path)
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        misplaced.status.success(),
        "{}",
        String::from_utf8_lossy(&misplaced.stderr)
    );
    assert!(String::from_utf8_lossy(&misplaced.stderr).contains("Cargo home differs"));
    assert!(project.join("target/debug").is_dir());
    assert!(first_paths.managed_build_dirs().is_empty());
    assert!(second_paths.managed_build_dirs().is_empty());

    let mut second_path = vec![second.cargo_home.join("rgo/shims")];
    second_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let second_path = std::env::join_paths(second_path).unwrap();
    let expected = second
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &second_path)
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert_eq!(second_paths.managed_build_dirs().len(), 1);

    let override_project = second.simple_bin("other-storage-root").unwrap();
    let overridden = second
        .cmd("cargo")
        .current_dir(&override_project)
        .env("PATH", &second_path)
        .env("RGO_HOME", &first.rgo_home)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        overridden.status.success(),
        "{}",
        String::from_utf8_lossy(&overridden.stderr)
    );
    assert!(String::from_utf8_lossy(&overridden.stderr).contains("storage root differs"));
    assert!(override_project.join("target/debug").is_dir());
    assert!(first_paths.managed_build_dirs().is_empty());
    assert_eq!(second_paths.managed_build_dirs().len(), 1);
}

#[test]
#[allow(unsafe_code)]
fn plain_cargo_starts_opted_in_maintenance_without_rgo_commands() {
    struct StopDaemon(u32);
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            unsafe { libc::kill(self.0 as i32, libc::SIGTERM) };
        }
    }

    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("lazy-maintenance").unwrap();
    let idle_workspace = sandbox.simple_bin("lazy-idle").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
    std::fs::write(
        sandbox.rgo_home.join("config.toml"),
        "[storage]\nmax_size = '1B'\nmin_free_space = '0B'\n[gc]\nauto = true\n",
    )
    .unwrap();
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    assert!(
        String::from_utf8_lossy(&setup.stdout)
            .contains("supervised Cargo starts maintenance only when [gc].auto = true")
    );
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let idle = paths.builds_dir().join("aa/idle");
    std::fs::create_dir_all(&idle).unwrap();
    std::fs::write(idle.join("unused"), vec![0u8; 8192]).unwrap();
    context::write_sidecar(
        &idle,
        &idle_workspace,
        &idle_workspace.join("Cargo.toml"),
        None,
    )
    .unwrap();
    assert!(!paths.socket_path().exists());

    let mut search_path = vec![sandbox.cargo_home.join("rgo/shims")];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", std::env::join_paths(search_path).unwrap())
        .env("RGO_DAEMON_POLL_SECS", "1")
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(status) = response else {
        panic!("daemon did not return status after plain Cargo use: {response:?}");
    };
    let _stop = StopDaemon(status.daemon_pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while idle.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !idle.exists(),
        "Cargo-started maintenance did not reclaim idle storage"
    );
}

#[test]
fn opted_in_maintenance_reclaims_an_idle_real_cargo_build() {
    struct StopDaemon(Child);
    impl Drop for StopDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("supervised-budget").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(cargo_proxy())
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let path = std::env::join_paths(
        std::iter::once(sandbox.cargo_home.join("rgo/shims"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &path)
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let final_binary = project.join("target/debug/supervised-budget");
    assert!(final_binary.is_file());
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.checked_managed_build_dirs().unwrap();
    assert_eq!(contexts.len(), 1);
    let context = &contexts[0];
    let listed = context::list(&paths).unwrap();
    assert_eq!(listed.len(), 1);
    let context_bytes = listed[0].usage.physical_bytes;
    assert!(context_bytes > 0);
    // Initialize the daemon's SQLite and operational files with automatic GC
    // still disabled. The configured budget must include this real baseline.
    let mut baseline_daemon = StopDaemon(
        sandbox
            .cmd(rgo)
            .args(["daemon", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let startup_deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while Instant::now() < startup_deadline {
        if matches!(
            ipc::request_with_timeout(
                &paths.socket_path(),
                Request::QueryRemoteStatus,
                Duration::from_millis(100)
            ),
            Ok(Response::RemoteStatus(_))
        ) {
            ready = true;
            break;
        }
        assert!(baseline_daemon.0.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(25));
    }
    assert!(ready, "baseline daemon did not start");
    drop(baseline_daemon);
    assert!(context.exists());
    let other_bytes = size::auxiliary_usage(&paths).unwrap().physical_bytes
        + size::Scanner::new()
            .measure_optional(&paths.cas_dir())
            .unwrap()
            .physical_bytes;
    let max_size = other_bytes + context_bytes / 2;

    // The Cargo session has exited. Age only its documented profile-lock
    // heuristic; the external supervised lifecycle guard remains authoritative.
    age_finished_cargo_profile_locks(context);
    std::fs::write(
        sandbox.rgo_home.join("config.toml"),
        format!("[storage]\nmax_size = '{max_size}B'\nmin_free_space = '0B'\n[gc]\nauto = true\n"),
    )
    .unwrap();
    let mut daemon = StopDaemon(
        sandbox
            .cmd(rgo)
            .args(["daemon", "--foreground"])
            .env("RGO_DAEMON_POLL_SECS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while context.exists() && Instant::now() < deadline {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited during maintenance"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !context.exists(),
        "automatic maintenance did not reclaim the idle build; daemon: {}",
        std::fs::read_to_string(paths.logs_dir().join("daemon.log")).unwrap_or_default()
    );
    assert!(
        final_binary.is_file(),
        "maintenance removed Cargo's final output"
    );
    let response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(status) = response else {
        panic!("daemon did not return storage status: {response:?}");
    };
    assert_eq!(status.contexts, 0);
    assert!(status.last_gc_at > 0);
    assert!(
        status.managed_bytes <= status.hard_limit_bytes,
        "maintenance removed the build but stayed above its budget: managed={} limit={} unmet={:?} reason={:?}",
        status.managed_bytes,
        status.hard_limit_bytes,
        status.unmet_budget_bytes,
        status.unmet_budget_reason
    );

    // Rebuild through the unchanged Cargo command and require maintenance to
    // recover the same budget again. The first successful pass must not be a
    // one-time effect of daemon initialization or a stale initial snapshot.
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"cycle 2\"); }\n",
    )
    .unwrap();
    let second_build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &path)
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        second_build.status.success(),
        "{}",
        String::from_utf8_lossy(&second_build.stderr)
    );
    assert!(context.exists());
    // The first post-build pass must retain its signal while Cargo's recent
    // profile lock makes this otherwise idle context temporarily ineligible.
    let retry_deadline = Instant::now() + Duration::from_secs(15);
    let deferred = loop {
        let record = supervision::pending_maintenance(&paths)
            .unwrap()
            .into_iter()
            .find(|record| record.context == *context);
        if let Some(record) = record.filter(|record| record.retry_after.is_some()) {
            break record;
        }
        assert!(
            Instant::now() < retry_deadline,
            "maintenance cleared the launch signal before the profile-lock grace expired"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(deferred.retry_after.unwrap() > context::unix_now());
    age_finished_cargo_profile_locks(context);
    // Advance only the persisted retry deadline; the daemon must still take
    // its normal authoritative snapshot and lifecycle guard before deleting.
    assert!(
        supervision::defer_pending_maintenance(&paths, &deferred, context::unix_now()).unwrap()
    );
    let second_deadline = Instant::now() + Duration::from_secs(15);
    while context.exists() && Instant::now() < second_deadline {
        assert!(daemon.0.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !context.exists(),
        "maintenance did not reclaim the rebuilt context"
    );
    assert!(final_binary.is_file());
    let second_response = ipc::request_with_timeout(
        &paths.socket_path(),
        Request::QueryStatus,
        Duration::from_secs(5),
    )
    .unwrap();
    let Response::Status(second_status) = second_response else {
        panic!("daemon did not return status after the second cycle: {second_response:?}");
    };
    assert_eq!(second_status.contexts, 0);
    assert!(second_status.managed_bytes <= second_status.hard_limit_bytes);
}

#[test]
fn unavailable_automatic_maintenance_uses_ordinary_cargo_storage() {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;

    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("maintenance-unavailable").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
    std::fs::write(sandbox.rgo_home.join("config.toml"), "[gc]\nauto = true\n").unwrap();
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let daemon_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.state_dir().join("daemon.lock"))
        .unwrap();
    daemon_lock.lock_exclusive().unwrap();
    let mut search_path = vec![sandbox.cargo_home.join("rgo/shims")];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", std::env::join_paths(search_path).unwrap())
        .env_remove("RGO_HOME")
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(String::from_utf8_lossy(&build.stderr).contains("using ordinary Cargo storage"));
    assert!(project.join("target/debug").is_dir());
    assert!(paths.managed_build_dirs().is_empty());
}

#[test]
fn unsupported_cargo_is_rejected_at_setup_and_left_unmanaged_by_the_launcher() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("old-cargo").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
    let fake_dir = sandbox.home.join("old-cargo-proxy");
    std::fs::create_dir_all(&fake_dir).unwrap();
    let fake = fake_dir.join("cargo");
    let quoted_cargo = cargo.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nif [ \"$1\" = '--version' ] || {{ [ \"$1\" = '+stable' ] && [ \"$2\" = '--version' ]; }}; then\n  echo 'cargo 1.90.0 (fake)'\n  exit 0\nfi\nexec '{quoted_cargo}' \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut search_path = vec![fake_dir];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let search_path = std::env::join_paths(search_path).unwrap();

    let native = sandbox
        .cmd(rgo)
        .env("PATH", &search_path)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(!native.status.success());
    assert!(String::from_utf8_lossy(&native.stderr).contains("requires Cargo 1.91"));
    let supervised = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&fake)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(!supervised.status.success());
    assert!(String::from_utf8_lossy(&supervised.stderr).contains("requires Cargo 1.91"));
    assert!(!sandbox.cargo_home.join(".rgo-install.json").exists());

    let build = sandbox
        .cmd(rgo)
        .current_dir(&project)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&fake)
        .args(["--", "+stable", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(String::from_utf8_lossy(&build.stderr).contains("using ordinary Cargo storage"));
    assert!(project.join("target/debug").is_dir());
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    assert!(paths.managed_build_dirs().is_empty());
}

#[test]
fn concurrent_cargo_clean_protects_its_context_but_allows_other_gc() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("clean-during-gc").unwrap();
    let other_project = sandbox.simple_bin("other-during-gc").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
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
    let build = sandbox
        .cmd(rgo)
        .current_dir(&project)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&cargo)
        .args(["--", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let context = paths.managed_build_dirs().pop().unwrap();
    let other = paths.builds_dir().join("bb/other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("unused"), vec![b'x'; 8192]).unwrap();
    context::write_sidecar(
        &other,
        &other_project,
        &other_project.join("Cargo.toml"),
        None,
    )
    .unwrap();

    let fake_dir = sandbox.home.join("paused-cargo");
    std::fs::create_dir(&fake_dir).unwrap();
    let fake = fake_dir.join("cargo");
    let quoted_cargo = cargo.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = clean ]; then\n    : > \"$RGO_CLEAN_READY\"\n    n=0\n    while [ ! -f \"$RGO_CLEAN_RELEASE\" ]; do\n      n=$((n + 1))\n      [ \"$n\" -lt 600 ] || exit 99\n      sleep 0.05\n    done\n    break\n  fi\ndone\nexec '{quoted_cargo}' \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ready = sandbox.home.join("clean-ready");
    let release = sandbox.home.join("clean-release");
    let mut cleaning = sandbox
        .cmd(rgo)
        .current_dir(&project)
        .env("RGO_CLEAN_READY", &ready)
        .env("RGO_CLEAN_RELEASE", &release)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&fake)
        .args(["--", "clean", "--offline"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.is_file() && Instant::now() < deadline {
        if cleaning.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready.is_file(),
        "supervised cargo clean did not reach the pause point"
    );
    assert!(
        supervision::try_lock_gc(&paths, Some(&context))
            .unwrap()
            .is_none()
    );
    assert!(gc::remove_atomically(&paths, &context).is_err());

    let gc = sandbox
        .cmd(rgo)
        .args(["gc", "--target", "0"])
        .output()
        .unwrap();
    std::fs::write(&release, b"continue").unwrap();
    let cleaned = cleaning.wait().unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(
        cleaned.success(),
        "supervised cargo clean failed: {cleaned}"
    );
    assert!(
        !other.exists(),
        "GC did not remove an eligible idle context"
    );
    assert!(
        !context.exists(),
        "cargo clean did not remove its managed context"
    );
}

#[test]
fn nested_cargo_build_keeps_both_contexts_safe_while_gc_reclaims_an_idle_one() {
    let sandbox = Sandbox::new().unwrap();
    let parent = sandbox.simple_bin("nested-parent").unwrap();
    let nested = sandbox.simple_bin("nested-child").unwrap();
    let idle_workspace = sandbox.simple_bin("nested-idle").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo = cargo_proxy();
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&real_cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    std::fs::write(
        parent.join("src/main.rs"),
        r#"fn main() {
    let manifest = std::env::var("RGO_NESTED_MANIFEST").unwrap();
    let status = std::process::Command::new("cargo")
        .args(["build", "--offline", "--manifest-path", &manifest])
        .status()
        .unwrap();
    if !status.success() { std::process::exit(13); }
}
"#,
    )
    .unwrap();
    std::fs::write(
        nested.join("build.rs"),
        r#"fn main() {
    let ready = std::env::var("RGO_NESTED_READY").unwrap();
    let release = std::env::var("RGO_NESTED_RELEASE").unwrap();
    std::fs::write(&ready, b"ready").unwrap();
    for _ in 0..600 {
        if std::path::Path::new(&release).is_file() { return; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("nested Cargo build was never released");
}
"#,
    )
    .unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let parent_context =
        supervision::context_for_workspace(&paths, &parent.canonicalize().unwrap()).unwrap();
    let nested_context =
        supervision::context_for_workspace(&paths, &nested.canonicalize().unwrap()).unwrap();
    let idle = paths.builds_dir().join("cc/idle");
    std::fs::create_dir_all(&idle).unwrap();
    std::fs::write(idle.join("unused"), vec![b'i'; 8192]).unwrap();
    context::write_sidecar(
        &idle,
        &idle_workspace,
        &idle_workspace.join("Cargo.toml"),
        None,
    )
    .unwrap();

    let shim_dir = sandbox.cargo_home.join("rgo/shims");
    let mut path = vec![shim_dir];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(path).unwrap();
    let ready = sandbox.home.join("nested-ready");
    let release = sandbox.home.join("nested-release");
    let mut running = sandbox
        .cmd("cargo")
        .current_dir(&parent)
        .env("PATH", &path)
        .env("RGO_NESTED_MANIFEST", nested.join("Cargo.toml"))
        .env("RGO_NESTED_READY", &ready)
        .env("RGO_NESTED_RELEASE", &release)
        .args(["run", "--offline"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.is_file() && Instant::now() < deadline {
        if running.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let nested_started = ready.is_file();
    let both_guarded = nested_started
        && parent_context.is_dir()
        && nested_context.is_dir()
        && supervision::try_lock_gc(&paths, Some(&parent_context))
            .unwrap()
            .is_none()
        && supervision::try_lock_gc(&paths, Some(&nested_context))
            .unwrap()
            .is_none();
    let gc = nested_started.then(|| {
        sandbox
            .cmd(rgo)
            .args(["gc", "--target", "0"])
            .output()
            .unwrap()
    });
    let protected = parent_context.is_dir() && nested_context.is_dir();
    let unrelated_reclaimed = !idle.exists();
    std::fs::write(&release, b"continue").unwrap();
    let finished = running.wait_with_output().unwrap();
    assert!(
        nested_started && finished.status.success(),
        "nested Cargo did not finish: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(
        both_guarded,
        "nested Cargo did not retain both lifecycle guards"
    );
    assert!(
        gc.as_ref().is_some_and(|output| output.status.success()),
        "GC failed while nested Cargo was running: {}",
        gc.as_ref()
            .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
            .unwrap_or_default()
    );
    assert!(protected, "GC removed a running nested Cargo context");
    assert!(unrelated_reclaimed, "GC did not reclaim an idle context");
}

#[test]
fn setup_and_launcher_reject_an_alias_to_the_owned_cargo_shim() {
    let sandbox = Sandbox::new().unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
    let installed = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let shim = sandbox.cargo_home.join("rgo/shims/cargo");
    let original = std::fs::read(&shim).unwrap();
    let alias_dir = sandbox.home.join("cargo-alias");
    std::fs::create_dir(&alias_dir).unwrap();
    let alias = alias_dir.join("cargo");
    std::os::unix::fs::symlink(&shim, &alias).unwrap();

    let requested = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&alias)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(!requested.status.success());
    assert!(String::from_utf8_lossy(&requested.stderr).contains("points to the rgo launcher"));
    assert_eq!(std::fs::read(&shim).unwrap(), original);

    let launcher = sandbox
        .cmd(rgo)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&alias)
        .args(["--", "--version"])
        .output()
        .unwrap();
    assert!(!launcher.status.success());
    assert!(String::from_utf8_lossy(&launcher.stderr).contains("owned Cargo launcher"));

    let mut search_path = vec![alias_dir];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let search_path = std::env::join_paths(search_path).unwrap();
    let automatic = sandbox
        .cmd(rgo)
        .env("PATH", search_path)
        .args(["setup", "--supervised", "--no-service"])
        .output()
        .unwrap();
    assert!(
        automatic.status.success(),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        record["supervised_cargo"]["real_cargo"],
        cargo.to_str().unwrap()
    );
}

#[test]
#[allow(unsafe_code)]
fn shimmed_cargo_isolated_from_direct_cargo_and_holds_gc_lock_through_run() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("supervised-pilot").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = cargo_proxy();
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let shim_dir = sandbox.cargo_home.join("rgo/shims");
    let shim = shim_dir.join("cargo");
    assert!(shim.is_file());
    assert!(!sandbox.cargo_home.join("config.toml").exists());
    let incompatible = sandbox
        .cmd(rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(!incompatible.status.success());
    let preview = sandbox
        .cmd(rgo)
        .args(["setup", "--undo", "--dry-run", "--no-service"])
        .output()
        .unwrap();
    assert!(preview.status.success());
    assert!(
        String::from_utf8_lossy(&preview.stdout).contains("would remove supervised Cargo shim")
    );
    assert!(shim.is_file());
    let inactive = sandbox
        .cmd(rgo)
        .args(["doctor", "--verify", "--json"])
        .output()
        .unwrap();
    assert!(!inactive.status.success());
    let inactive_report: serde_json::Value = serde_json::from_slice(&inactive.stdout).unwrap();
    assert_eq!(inactive_report["activation_verified"], false);
    let mut search_path = vec![shim_dir];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let search_path = std::env::join_paths(search_path).unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };

    let build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    let context = &contexts[0];
    assert!(context.join(".rgo-context.json").is_file());
    let sidecar_path = context.join(".rgo-context.json");
    let mut stale: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
    stale["last_seen"] = serde_json::json!(1);
    std::fs::write(&sidecar_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    let warm = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(warm.status.success());
    assert!(
        !String::from_utf8_lossy(&warm.stderr).contains("Compiling "),
        "warm Cargo build unexpectedly compiled: {}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert!(context::read_sidecar(context).unwrap().last_seen > 1);
    stale["last_seen"] = serde_json::json!(1);
    std::fs::write(&sidecar_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    let application_flags = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["run", "--offline", "--", "--config", "app.toml", "-C", "-Z"])
        .output()
        .unwrap();
    assert!(
        application_flags.status.success(),
        "{}",
        String::from_utf8_lossy(&application_flags.stderr)
    );
    assert!(context::read_sidecar(context).unwrap().last_seen > 1);
    context::write_durable_pin(&paths, context).unwrap();

    let package_clean = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["clean", "-p", "supervised-pilot", "--offline"])
        .output()
        .unwrap();
    assert!(
        package_clean.status.success(),
        "{}",
        String::from_utf8_lossy(&package_clean.stderr)
    );
    assert!(context.join(".rgo-context.json").is_file());

    let full_clean = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["clean", "--offline"])
        .output()
        .unwrap();
    assert!(
        full_clean.status.success(),
        "{}",
        String::from_utf8_lossy(&full_clean.stderr)
    );
    assert!(!context.exists());
    assert!(context::is_pinned(&paths, context));
    let rebuilt = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        rebuilt.status.success(),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    assert!(context.join(".rgo-context.json").is_file());
    assert!(context.join(".rgo-pin").is_file());
    assert!(gc::remove_atomically(&paths, context).is_err());
    context::remove_durable_pin(&paths, context).unwrap();

    let flagged = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["--offline", "-v", "check"])
        .output()
        .unwrap();
    assert!(
        flagged.status.success(),
        "{}",
        String::from_utf8_lossy(&flagged.stderr)
    );
    let alias_build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["b", "--offline"])
        .output()
        .unwrap();
    assert!(
        alias_build.status.success(),
        "{}",
        String::from_utf8_lossy(&alias_build.stderr)
    );
    let alias = sandbox.home.join("linked-project");
    std::os::unix::fs::symlink(&project, &alias).unwrap();
    let linked = sandbox
        .cmd("cargo")
        .current_dir(&alias)
        .env("PATH", &search_path)
        .args(["check", "--offline"])
        .output()
        .unwrap();
    assert!(
        linked.status.success(),
        "{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    assert_eq!(paths.managed_build_dirs().len(), 1);
    let informational = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .output()
        .unwrap();
    assert!(informational.status.success());
    assert_eq!(paths.managed_build_dirs().len(), 1);
    let custom_build = sandbox.home.join("custom-intermediates");
    let override_setting = format!(
        "build.build-dir={}",
        serde_json::to_string(custom_build.to_str().unwrap()).unwrap()
    );
    let overridden = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline", "--config", &override_setting])
        .output()
        .unwrap();
    assert!(
        overridden.status.success(),
        "{}",
        String::from_utf8_lossy(&overridden.stderr)
    );
    assert!(has_entries(&custom_build));

    let project_build = sandbox.home.join("project-selected-build-dir");
    let project_config = project.join(".cargo/config.toml");
    std::fs::create_dir_all(project_config.parent().unwrap()).unwrap();
    std::fs::write(
        &project_config,
        format!(
            "[build]\nbuild-dir = {}\n",
            serde_json::to_string(project_build.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let project_override = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        project_override.status.success(),
        "{}",
        String::from_utf8_lossy(&project_override.stderr)
    );
    assert!(has_entries(&project_build));
    std::fs::remove_file(&project_config).unwrap();

    let env_build = sandbox.home.join("environment-selected-build-dir");
    let environment_override = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("CARGO_BUILD_BUILD_DIR", &env_build)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        environment_override.status.success(),
        "{}",
        String::from_utf8_lossy(&environment_override.stderr)
    );
    assert!(has_entries(&env_build));

    let plugin = sandbox.cargo_home.join("rgo/shims/cargo-hold");
    std::fs::write(
        &plugin,
        "#!/bin/sh\nprintf ready > \"$RGO_HOLD_READY\"\nsleep 2\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hold_ready = sandbox.home.join("plugin-ready");
    let unknown_victim = paths.builds_dir().join("bb/unknown-victim");
    std::fs::create_dir_all(&unknown_victim).unwrap();
    std::fs::write(unknown_victim.join("unused"), b"data").unwrap();
    context::write_sidecar(&unknown_victim, &project, &project.join("Cargo.toml"), None).unwrap();
    let mut unknown = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_HOLD_READY", &hold_ready)
        .arg("hold")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hold_ready.is_file() && Instant::now() < deadline {
        if unknown.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(hold_ready.is_file(), "external Cargo command did not start");
    assert!(gc::remove_atomically(&paths, &unknown_victim).is_err());
    assert!(unknown.wait().unwrap().success());
    gc::remove_atomically(&paths, &unknown_victim).unwrap();

    std::fs::remove_file(&hold_ready).unwrap();
    std::fs::create_dir_all(&unknown_victim).unwrap();
    std::fs::write(unknown_victim.join("unused"), b"data").unwrap();
    context::write_sidecar(&unknown_victim, &project, &project.join("Cargo.toml"), None).unwrap();
    let mut bypassed = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_BYPASS", "1")
        .env("RGO_HOLD_READY", &hold_ready)
        .arg("hold")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hold_ready.is_file() && Instant::now() < deadline {
        if bypassed.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(hold_ready.is_file(), "bypassed Cargo command did not start");
    assert!(gc::remove_atomically(&paths, &unknown_victim).is_err());
    assert!(bypassed.wait().unwrap().success());
    gc::remove_atomically(&paths, &unknown_victim).unwrap();

    let direct = sandbox
        .cmd(&cargo)
        .current_dir(&project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        direct.status.success(),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert!(project.join("target/debug").is_dir());

    let ready = sandbox.home.join("cargo-run-ready");
    let release = sandbox.home.join("cargo-run-release");
    let run_log = sandbox.home.join("cargo-run-stderr.log");
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() { std::fs::write(std::env::var(\"RGO_TEST_READY\").unwrap(), b\"ready\").unwrap(); let release = std::env::var(\"RGO_TEST_RELEASE\").unwrap(); for _ in 0..600 { if std::path::Path::new(&release).is_file() { return; } std::thread::sleep(std::time::Duration::from_millis(50)); } panic!(\"release marker never appeared\"); }\n",
    )
    .unwrap();
    let mut child = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("RGO_TEST_READY", &ready)
        .env("RGO_TEST_RELEASE", &release)
        .env("PATH", &search_path)
        .args(["+stable", "run", "--offline"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&run_log).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.is_file() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let started = ready.is_file();
    let old = SystemTime::now() - Duration::from_secs(12 * 60);
    if started {
        let sidecar_path = context.join(".rgo-context.json");
        let mut sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
        sidecar["last_seen"] = serde_json::json!(1);
        std::fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        for profile in std::fs::read_dir(context).unwrap() {
            let profile = profile.unwrap();
            if profile.file_type().unwrap().is_dir() {
                let lock = profile.path().join(".cargo-build-lock");
                if lock.is_file() {
                    std::fs::File::open(&lock)
                        .unwrap()
                        .set_modified(old)
                        .unwrap();
                }
            }
        }
        std::fs::File::open(context)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let heuristic_expired = started
        && context::list(&paths).unwrap().iter().any(|candidate| {
            candidate.dir == *context
                && candidate.idle_for(SystemTime::now()) > gc::LIVE_WINDOW
                && !candidate.recently_locked(gc::LIVE_WINDOW, SystemTime::now())
        });
    let held = started
        && supervision::try_lock_gc(&paths, Some(context))
            .unwrap()
            .is_none();
    let other = paths.builds_dir().join("bb/other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("unused"), b"reclaim").unwrap();
    context::write_sidecar(&other, &project, &project.join("Cargo.toml"), None).unwrap();
    let gc_pass = started.then(|| {
        sandbox
            .cmd(rgo)
            .args(["gc", "--target", "0"])
            .output()
            .unwrap()
    });
    let gc_skipped_active = gc_pass.as_ref().is_some_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("a supervised Cargo invocation is using managed storage")
    });
    let protected = started && context.exists();
    let unrelated_reclaimed = started && !other.exists();
    if started {
        std::fs::write(&release, b"continue").unwrap();
    }
    if !started {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();
    assert!(
        started && status.success(),
        "supervised cargo run did not start and finish (status {status}); stderr: {}",
        std::fs::read_to_string(&run_log).unwrap_or_default()
    );
    assert!(
        held,
        "GC acquired the context lock while cargo run was active"
    );
    assert!(
        heuristic_expired,
        "the active context still looked recent to the heuristic"
    );
    assert!(
        gc_skipped_active,
        "daemon GC did not report the supervised lifecycle guard: {}",
        gc_pass
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default()
    );
    assert!(
        protected && context.exists(),
        "GC removed the active context"
    );
    assert!(
        unrelated_reclaimed && !other.exists(),
        "GC could not reclaim an idle context"
    );
    assert!(
        supervision::try_lock_gc(&paths, Some(context))
            .unwrap()
            .is_some()
    );

    let background_ready = sandbox.home.join("background-child-pid");
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() { let marker = std::env::var(\"RGO_BACKGROUND_READY\").unwrap(); let _child = std::process::Command::new(\"sh\").arg(\"-c\").arg(\"printf '%s' \\\"$$\\\" > \\\"$RGO_BACKGROUND_READY\\\"; sleep 4\").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap(); for _ in 0..100 { if std::path::Path::new(&marker).is_file() { return; } std::thread::sleep(std::time::Duration::from_millis(10)); } panic!(\"background child did not start\"); }\n",
    )
    .unwrap();
    let detached = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_BACKGROUND_READY", &background_ready)
        .args(["run", "--offline"])
        .output()
        .unwrap();
    assert!(
        detached.status.success(),
        "{}",
        String::from_utf8_lossy(&detached.stderr)
    );
    let child_pid: i32 = std::fs::read_to_string(&background_ready)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(child_pid, 0) }, 0);
    assert!(
        supervision::try_lock_gc(&paths, Some(context))
            .unwrap()
            .is_none(),
        "GC ignored a detached child that inherited Cargo's build context"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while supervision::try_lock_gc(&paths, Some(context))
        .unwrap()
        .is_none()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        supervision::try_lock_gc(&paths, Some(context))
            .unwrap()
            .is_some()
    );
    let killed_ready = sandbox.home.join("killed-cargo-wrapper-pid");
    let slow_wrapper = sandbox.home.join("slow-rustc-wrapper");
    std::fs::write(
        &slow_wrapper,
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$RGO_KILLED_READY\"\nsleep 4\nexec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&slow_wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"after kill\"); }\n",
    )
    .unwrap();
    let mut killed_cargo = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_KILLED_READY", &killed_ready)
        .env("RUSTC_WRAPPER", &slow_wrapper)
        .args(["build", "--offline"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !killed_ready.is_file() && Instant::now() < deadline {
        if killed_cargo.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(killed_ready.is_file(), "rustc wrapper did not start");
    let wrapper_pid: i32 = std::fs::read_to_string(&killed_ready)
        .unwrap()
        .parse()
        .unwrap();
    killed_cargo.kill().unwrap();
    let _ = killed_cargo.wait();
    assert_eq!(unsafe { libc::kill(wrapper_pid, 0) }, 0);
    assert!(
        supervision::try_lock_gc(&paths, Some(context))
            .unwrap()
            .is_none(),
        "GC ignored a rustc wrapper still running after Cargo was killed"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while supervision::try_lock_gc(&paths, Some(context))
        .unwrap()
        .is_none()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        supervision::try_lock_gc(&paths, Some(context))
            .unwrap()
            .is_some()
    );
    gc::remove_atomically(&paths, context).unwrap();
    assert!(!context.exists());

    let shard = context.parent().unwrap();
    std::fs::remove_dir(shard).unwrap();
    let external = sandbox.home.join("external-shard-target");
    std::fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, shard).unwrap();
    std::fs::remove_dir_all(project.join("target")).unwrap();
    let unsafe_shard = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        unsafe_shard.status.success(),
        "{}",
        String::from_utf8_lossy(&unsafe_shard.stderr)
    );
    assert!(project.join("target/debug").is_dir());
    assert!(!external.join(context.file_name().unwrap()).exists());
    std::fs::remove_file(shard).unwrap();

    let workspace = sandbox.workspace("virtual-workspace", &["member"]).unwrap();
    let member = workspace.join("member/Cargo.toml");
    let virtual_build = sandbox
        .cmd("cargo")
        .current_dir(&sandbox.home)
        .env("PATH", &search_path)
        .args(["build", "--offline", "--manifest-path"])
        .arg(&member)
        .output()
        .unwrap();
    assert!(
        virtual_build.status.success(),
        "{}",
        String::from_utf8_lossy(&virtual_build.stderr)
    );
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    let sidecar = context::read_sidecar(&contexts[0]).unwrap();
    assert_eq!(
        PathBuf::from(sidecar.workspace_root)
            .canonicalize()
            .unwrap(),
        workspace.canonicalize().unwrap()
    );

    let unresolved = sandbox.simple_bin("unresolved-dependency").unwrap();
    std::fs::write(
        unresolved.join("Cargo.toml"),
        "[package]\nname = \"unresolved-dependency\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nrgo_missing_registry_package = \"9999\"\n",
    )
    .unwrap();
    let missing = sandbox
        .cmd("cargo")
        .current_dir(&unresolved)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(paths.managed_build_dirs().iter().any(|dir| {
        context::read_sidecar(dir).is_some_and(|sidecar| {
            PathBuf::from(sidecar.workspace_root)
                .canonicalize()
                .is_ok_and(|root| root == unresolved.canonicalize().unwrap())
        })
    }));

    let doctor = sandbox
        .cmd(rgo)
        .env("PATH", &search_path)
        .args(["doctor", "--verify", "--json"])
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["activation_verified"], true);

    let tampered = b"#!/bin/sh\nexit 1\n";
    std::fs::write(&shim, tampered).unwrap();
    let refused = sandbox
        .cmd(rgo)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert_eq!(std::fs::read(&shim).unwrap(), tampered);
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    std::fs::write(
        &shim,
        record["supervised_cargo"]["shim_contents"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let undone = sandbox
        .cmd(rgo)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(
        undone.status.success(),
        "{}",
        String::from_utf8_lossy(&undone.stderr)
    );
    assert!(!shim.exists());
    assert!(!sandbox.cargo_home.join(".rgo-install.json").exists());
    let unsafe_switch = sandbox
        .cmd(rgo)
        .args(["setup", "--no-service"])
        .output()
        .unwrap();
    assert!(!unsafe_switch.status.success());
    assert!(
        String::from_utf8_lossy(&unsafe_switch.stderr)
            .contains("different or unverified storage mode")
    );
    assert!(!sandbox.cargo_home.join("config.toml").exists());
}
