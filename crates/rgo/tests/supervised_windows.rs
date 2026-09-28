#![cfg(windows)]

use std::path::PathBuf;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use rgo_core::context;
use rgo_core::gc;
use rgo_core::paths::RgoPaths;
use rgo_core::supervision;
use rgo_testkit::Sandbox;
use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

#[test]
fn setup_activates_unchanged_cargo_exe_and_undo_restores_direct_cargo() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("windows-setup-shim").unwrap();
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .expect("Cargo sets CARGO for integration tests");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_rgo"));
    let setup = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let shim = sandbox.cargo_home.join(format!(
        "rgo/shims/v{}/cargo.exe",
        env!("CARGO_PKG_VERSION")
    ));
    assert!(shim.is_file());
    let path = std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let build = sandbox
        .cmd("cmd.exe")
        .current_dir(&project)
        .env("PATH", &path)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(project.join("target/debug/windows-setup-shim.exe").exists());
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.checked_managed_build_dirs().unwrap();
    assert_eq!(contexts.len(), 1);
    assert!(context::read_sidecar(&contexts[0]).is_some());

    let doctor = sandbox
        .cmd(&cli)
        .env("PATH", &path)
        .args(["doctor", "--verify"])
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    std::fs::remove_file(&shim).unwrap();
    let repaired = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert!(shim.is_file());
    let managed_before_direct = paths.checked_managed_build_dirs().unwrap().len();
    let direct_project = sandbox.simple_bin("windows-direct-cargo").unwrap();
    let direct = sandbox
        .cmd(&real_cargo)
        .current_dir(&direct_project)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        direct.status.success(),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert!(direct_project.join("target/debug/deps").exists());
    assert_eq!(
        paths.checked_managed_build_dirs().unwrap().len(),
        managed_before_direct
    );
    let record_path = sandbox.cargo_home.join(".rgo-install.json");
    let original_record = std::fs::read(&record_path).unwrap();
    let mut newer_record: serde_json::Value = serde_json::from_slice(&original_record).unwrap();
    newer_record["binary_version"] = serde_json::Value::String("0.1.999".into());
    std::fs::write(
        &record_path,
        serde_json::to_vec_pretty(&newer_record).unwrap(),
    )
    .unwrap();
    let stale_project = sandbox.simple_bin("windows-stale-shim").unwrap();
    let stale = sandbox
        .cmd("cmd.exe")
        .current_dir(&stale_project)
        .env("PATH", &path)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(stale_project.join("target/debug/deps").is_dir());
    assert_eq!(
        paths.checked_managed_build_dirs().unwrap().len(),
        managed_before_direct
    );
    std::fs::write(&record_path, original_record).unwrap();
    let other_home = sandbox.home.join("other-cargo-home");
    std::fs::create_dir_all(&other_home).unwrap();
    let wrong_home_project = sandbox.simple_bin("windows-wrong-home").unwrap();
    let wrong_home = sandbox
        .cmd("cmd.exe")
        .current_dir(&wrong_home_project)
        .env("PATH", &path)
        .env("CARGO_HOME", &other_home)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        wrong_home.status.success(),
        "{}",
        String::from_utf8_lossy(&wrong_home.stderr)
    );
    assert!(wrong_home_project.join("target/debug/deps").exists());
    assert_eq!(
        paths.checked_managed_build_dirs().unwrap().len(),
        managed_before_direct
    );

    let fallback_path = shim.parent().unwrap().join(".rgo-cargo-fallback.json");
    let original_fallback = std::fs::read(&fallback_path).unwrap();
    let mut changed_fallback: serde_json::Value =
        serde_json::from_slice(&original_fallback).unwrap();
    changed_fallback["real_cargo"] =
        serde_json::Value::String(sandbox.home.join("other/cargo.exe").display().to_string());
    std::fs::write(
        &fallback_path,
        serde_json::to_vec_pretty(&changed_fallback).unwrap(),
    )
    .unwrap();
    let rejected_undo = sandbox
        .cmd(&cli)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(!rejected_undo.status.success());
    assert!(record_path.exists());
    std::fs::write(&fallback_path, original_fallback).unwrap();
    let undo = sandbox
        .cmd(&cli)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(!record_path.exists());
    assert!(shim.exists());
    let after_undo = sandbox.simple_bin("windows-after-undo").unwrap();
    let stale_shell = sandbox
        .cmd("cmd.exe")
        .current_dir(&after_undo)
        .env("PATH", &path)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        stale_shell.status.success(),
        "{}",
        String::from_utf8_lossy(&stale_shell.stderr)
    );
    assert!(after_undo.join("target/debug/deps").is_dir());
    assert_eq!(
        paths.checked_managed_build_dirs().unwrap().len(),
        managed_before_direct
    );
    let reinstall = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        reinstall.status.success(),
        "{}",
        String::from_utf8_lossy(&reinstall.stderr)
    );
}

#[test]
fn a_previously_owned_flat_shim_remains_repairable() {
    let sandbox = Sandbox::new().unwrap();
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_rgo"));
    let real_cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
    let flat = sandbox.cargo_home.join("rgo/shims/cargo.exe");
    std::fs::create_dir_all(flat.parent().unwrap()).unwrap();
    std::fs::copy(&cli, &flat).unwrap();
    let unowned = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(!unowned.status.success());
    std::fs::remove_file(&flat).unwrap();
    let setup = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let versioned = sandbox.cargo_home.join(format!(
        "rgo/shims/v{}/cargo.exe",
        env!("CARGO_PKG_VERSION")
    ));
    let record_path = sandbox.cargo_home.join(".rgo-install.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    let outside = sandbox.home.join("outside/cargo.exe");
    record["supervised_cargo"]["shim_path"] =
        serde_json::Value::String(outside.display().to_string());
    std::fs::write(&record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    let rejected = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!outside.exists());

    std::fs::copy(&versioned, &flat).unwrap();
    record["supervised_cargo"]["shim_path"] = serde_json::Value::String(flat.display().to_string());
    std::fs::write(&record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    std::fs::remove_file(&versioned).unwrap();

    let repeated = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(flat.is_file());
    assert!(!versioned.exists());
    let path = std::env::join_paths(std::iter::once(flat.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let doctor = sandbox
        .cmd(&cli)
        .env("PATH", path)
        .args(["doctor", "--verify"])
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let undo = sandbox
        .cmd(&cli)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(!flat.exists());
}

#[test]
fn undo_with_a_running_versioned_shim_keeps_old_shells_on_ordinary_cargo() {
    let sandbox = Sandbox::new().unwrap();
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_rgo"));
    let real_cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
    let project = sandbox.simple_bin("windows-running-undo").unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        r#"
fn main() {
    let ready = std::path::PathBuf::from(std::env::var_os("RGO_TEST_READY").unwrap());
    let release = std::path::PathBuf::from(std::env::var_os("RGO_TEST_RELEASE").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    while !release.exists() {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
"#,
    )
    .unwrap();
    let setup = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let shim = sandbox.cargo_home.join(format!(
        "rgo/shims/v{}/cargo.exe",
        env!("CARGO_PKG_VERSION")
    ));
    let path = std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let ready = sandbox.home.join("running-undo-ready");
    let release = sandbox.home.join("running-undo-release");
    let mut child = sandbox
        .cmd("cmd.exe")
        .current_dir(&project)
        .env("PATH", &path)
        .env("RGO_TEST_READY", &ready)
        .env("RGO_TEST_RELEASE", &release)
        .args(["/C", "cargo", "run", "--offline"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !ready.exists() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "Cargo did not start: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let undo = sandbox
        .cmd(&cli)
        .args(["setup", "--undo", "--no-service"])
        .output()
        .unwrap();
    std::fs::write(&release, b"done").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(shim.is_file());
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let managed_before = paths.checked_managed_build_dirs().unwrap().len();
    let after = sandbox
        .simple_bin("windows-stale-shell-after-undo")
        .unwrap();
    let stale = sandbox
        .cmd("cmd.exe")
        .current_dir(&after)
        .env("PATH", &path)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(after.join("target/debug/deps").is_dir());
    assert_eq!(
        paths.checked_managed_build_dirs().unwrap().len(),
        managed_before
    );
}

#[test]
fn interrupted_versioned_setup_keeps_the_old_shim_usable_and_repairs_the_new_one() {
    let sandbox = Sandbox::new().unwrap();
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_rgo"));
    let real_cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
    let first = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let new_shim = sandbox.cargo_home.join(format!(
        "rgo/shims/v{}/cargo.exe",
        env!("CARGO_PKG_VERSION")
    ));
    let old_shim = sandbox.cargo_home.join("rgo/shims/v0.1.999/cargo.exe");
    std::fs::create_dir_all(old_shim.parent().unwrap()).unwrap();
    std::fs::rename(&new_shim, &old_shim).unwrap();
    std::fs::rename(
        new_shim.parent().unwrap().join(".rgo-cargo-fallback.json"),
        old_shim.parent().unwrap().join(".rgo-cargo-fallback.json"),
    )
    .unwrap();
    let old_bytes = std::fs::read(&old_shim).unwrap();
    let record_path = sandbox.cargo_home.join(".rgo-install.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["binary_version"] = serde_json::Value::String("0.1.999".into());
    record["supervised_cargo"]["shim_path"] =
        serde_json::Value::String(old_shim.display().to_string());
    std::fs::write(&record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let old_fallback = old_shim.parent().unwrap().join(".rgo-cargo-fallback.json");
    let fallback_bytes = std::fs::read(&old_fallback).unwrap();
    std::fs::remove_file(&old_fallback).unwrap();
    let rejected = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!new_shim.exists());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&record_path).unwrap()).unwrap()
            ["binary_version"],
        "0.1.999"
    );
    std::fs::write(&old_fallback, fallback_bytes).unwrap();

    let interrupted = sandbox
        .cmd(&cli)
        .env("RGO_SETUP_TEST_EXIT_AFTER_RECORD", "1")
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert_eq!(interrupted.status.code(), Some(88));
    assert!(!new_shim.exists());
    assert_eq!(std::fs::read(&old_shim).unwrap(), old_bytes);
    let old_path = std::env::join_paths(
        std::iter::once(old_shim.parent().unwrap().to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();
    let local_project = sandbox.simple_bin("windows-upgrade-old-shell").unwrap();
    let local = sandbox
        .cmd("cmd.exe")
        .current_dir(&local_project)
        .env("PATH", &old_path)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    assert!(local_project.join("target/debug/deps").is_dir());
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    assert!(paths.checked_managed_build_dirs().unwrap().is_empty());

    let repaired = sandbox
        .cmd(&cli)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(&real_cargo)
        .output()
        .unwrap();
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert!(new_shim.is_file());
    assert_eq!(std::fs::read(&old_shim).unwrap(), old_bytes);
    let new_path = std::env::join_paths(
        std::iter::once(new_shim.parent().unwrap().to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();
    let managed_project = sandbox.simple_bin("windows-upgrade-new-shell").unwrap();
    let managed = sandbox
        .cmd("cmd.exe")
        .current_dir(&managed_project)
        .env("PATH", &new_path)
        .args(["/C", "cargo", "build", "--offline"])
        .output()
        .unwrap();
    assert!(
        managed.status.success(),
        "{}",
        String::from_utf8_lossy(&managed.stderr)
    );
    assert_eq!(paths.checked_managed_build_dirs().unwrap().len(), 1);
}

#[test]
fn suspended_job_launcher_guards_running_cargo_and_allows_unrelated_gc() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("windows-job-guard").unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        r#"
fn main() {
    let ready = std::path::PathBuf::from(std::env::var_os("RGO_TEST_READY").unwrap());
    let release = std::path::PathBuf::from(std::env::var_os("RGO_TEST_RELEASE").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    while !release.exists() {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
"#,
    )
    .unwrap();
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .expect("Cargo sets CARGO for integration tests");
    assert!(real_cargo.is_absolute());
    let ready = sandbox.home.join("running-ready");
    let release = sandbox.home.join("running-release");

    let mut child = sandbox
        .cmd(env!("CARGO_BIN_EXE_rgo"))
        .current_dir(&project)
        .env("RGO_TEST_READY", &ready)
        .env("RGO_TEST_RELEASE", &release)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&real_cargo)
        .args(["--", "run", "--offline"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "Cargo exited before its program started: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !ready.exists() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "Cargo did not start its program: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let checks = std::panic::catch_unwind(|| {
        assert!(project.join("target/debug/windows-job-guard.exe").exists());
        let contexts = paths.checked_managed_build_dirs().unwrap();
        assert_eq!(contexts.len(), 1);
        assert!(context::read_sidecar(&contexts[0]).is_some());
        assert!(
            supervision::try_lock_gc(&paths, Some(&contexts[0]))
                .unwrap()
                .is_none()
        );

        let idle_project = sandbox.simple_bin("windows-idle").unwrap();
        let idle = paths.builds_dir().join("bb/idle");
        std::fs::create_dir_all(&idle).unwrap();
        std::fs::write(idle.join("output"), b"reclaim").unwrap();
        context::write_sidecar(&idle, &idle_project, &idle_project.join("Cargo.toml"), None)
            .unwrap();
        gc::remove_atomically(&paths, &idle).unwrap();
        assert!(!idle.exists());
        assert!(contexts[0].exists());
        contexts[0].clone()
    });
    std::fs::write(&release, b"done").unwrap();
    let output = child.wait_with_output().unwrap();
    let context = match checks {
        Ok(context) => context,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        supervision::try_lock_gc(&paths, Some(&context))
            .unwrap()
            .is_some()
    );
}

#[test]
#[allow(unsafe_code)] // Process wait proves Job Object teardown killed the child.
fn killing_launcher_terminates_cargo_run_child_before_gc_guard_releases() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("windows-killed-launcher").unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        r#"
fn main() {
    let ready = std::path::PathBuf::from(std::env::var_os("RGO_TEST_READY").unwrap());
    let staging = ready.with_extension("tmp");
    std::fs::write(&staging, std::process::id().to_string()).unwrap();
    std::fs::rename(staging, ready).unwrap();
    loop { std::thread::sleep(std::time::Duration::from_millis(100)); }
}
"#,
    )
    .unwrap();
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .expect("Cargo sets CARGO for integration tests");
    let ready = sandbox.home.join("running-pid");
    let mut launcher = sandbox
        .cmd(env!("CARGO_BIN_EXE_rgo"))
        .current_dir(&project)
        .env("RGO_TEST_READY", &ready)
        .args(["cargo-shim", "--real-cargo"])
        .arg(&real_cargo)
        .args(["--", "run", "--offline"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        if launcher.try_wait().unwrap().is_some() {
            let output = launcher.wait_with_output().unwrap();
            panic!(
                "Cargo exited before its program started: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !ready.exists() {
        launcher.kill().unwrap();
        let output = launcher.wait_with_output().unwrap();
        panic!(
            "Cargo did not start its program: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let pid: u32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    let program = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if program.is_null() {
        launcher.kill().unwrap();
        launcher.wait().unwrap();
        panic!("cannot observe supervised program");
    }
    let before_kill = unsafe { WaitForSingleObject(program, 0) };
    if before_kill != WAIT_TIMEOUT {
        launcher.kill().unwrap();
        launcher.wait().unwrap();
        unsafe { CloseHandle(program) };
        panic!("supervised program was not alive before guardian termination");
    }

    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let termination = unsafe { WaitForSingleObject(program, 5_000) };
    unsafe { CloseHandle(program) };
    assert_eq!(
        termination, WAIT_OBJECT_0,
        "Cargo child survived its guardian"
    );

    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.checked_managed_build_dirs().unwrap();
    assert_eq!(contexts.len(), 1);
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0]))
            .unwrap()
            .is_some()
    );
    gc::remove_atomically(&paths, &contexts[0]).unwrap();
    assert!(!contexts[0].exists());
}
