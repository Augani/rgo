#![cfg(any(unix, windows))]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
use rgo_core::ipc;
use rgo_core::paths::RgoPaths;
use rgo_core::{context, gc};
use rgo_protocol::SIDECAR_FILE;
#[cfg(debug_assertions)]
use rgo_protocol::{Request, Response};
use rgo_testkit::Sandbox;

struct RunningCargo {
    child: Option<Child>,
    release: PathBuf,
}

impl Drop for RunningCargo {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
        if let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(debug_assertions)]
struct RunningGcBatch {
    release: PathBuf,
    daemon: Child,
    clients: Vec<Child>,
}

#[cfg(debug_assertions)]
impl Drop for RunningGcBatch {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
        for child in &mut self.clients {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn wait_for_marker(ready: &Path, child: &mut Child, output: &Path, stage: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.is_file() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "Cargo exited before {stage} started ({status}): {}",
                std::fs::read_to_string(output).unwrap_or_default()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready.is_file(),
        "Cargo never reached {stage}: {}",
        std::fs::read_to_string(output).unwrap_or_default()
    );
}

#[test]
fn concurrent_gc_clients_preserve_a_running_cargo_test() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("test-lifetime").unwrap();
    let other_workspace = sandbox.simple_bin("idle-test-lifetime").unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        r#"fn main() {}
#[cfg(test)]
mod tests {
    #[test]
    fn held_test_process() {
        let ready = std::env::var("RGO_TEST_READY").unwrap();
        let release = std::env::var("RGO_TEST_RELEASE").unwrap();
        std::fs::write(ready, b"ready").unwrap();
        for _ in 0..1200 {
            if std::path::Path::new(&release).is_file() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("test process was never released");
    }
}
"#,
    )
    .unwrap();

    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo =
        PathBuf::from(std::env::var_os("CARGO").expect("Cargo test runner sets CARGO"));
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(real_cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    let shim = PathBuf::from(record["supervised_cargo"]["shim_path"].as_str().unwrap());
    let path = std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let idle = paths.builds_dir().join("bb/idle-test");
    std::fs::create_dir_all(&idle).unwrap();
    std::fs::write(idle.join("unused"), vec![b'x'; 8192]).unwrap();
    context::write_sidecar(
        &idle,
        &other_workspace,
        &other_workspace.join("Cargo.toml"),
        None,
    )
    .unwrap();

    let ready = sandbox.home.join("test-ready");
    let release = sandbox.home.join("test-release");
    let output = sandbox.home.join("cargo-test.log");
    let log = File::create(&output).unwrap();
    #[cfg(unix)]
    let mut command = sandbox.cmd("cargo");
    #[cfg(windows)]
    let mut command = {
        let mut command = sandbox.cmd("cmd.exe");
        command.args(["/C", "cargo"]);
        command
    };
    let child = command
        .current_dir(&project)
        .env("PATH", path)
        .env("RGO_TEST_READY", &ready)
        .env("RGO_TEST_RELEASE", &release)
        .args(["test", "--offline"])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let mut running = RunningCargo {
        child: Some(child),
        release: release.clone(),
    };
    wait_for_marker(
        &ready,
        running.child.as_mut().unwrap(),
        &output,
        "test process",
    );

    let active = paths
        .checked_managed_build_dirs()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate != &idle)
        .expect("supervised cargo test has a managed context");
    let blocked = gc::remove_atomically(&paths, &active).unwrap_err();
    assert!(
        blocked.to_string().contains("supervised Cargo"),
        "unexpected GC refusal: {blocked:#}"
    );
    #[cfg(debug_assertions)]
    {
        let locked = sandbox.home.join("gc-batch-locked");
        let release_gc = sandbox.home.join("gc-batch-release");
        let daemon_log = sandbox.home.join("gc-batch-daemon.log");
        let daemon = sandbox
            .cmd(rgo)
            .args(["daemon", "--foreground", "--home"])
            .arg(&sandbox.rgo_home)
            .env("RGO_TEST_GC_LOCKED_MARKER", &locked)
            .env("RGO_TEST_GC_LOCKED_RELEASE", &release_gc)
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&daemon_log).unwrap()))
            .spawn()
            .unwrap();
        let mut batch = RunningGcBatch {
            release: release_gc,
            daemon,
            clients: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut daemon_ready = false;
        while !daemon_ready && Instant::now() < deadline {
            daemon_ready = matches!(
                ipc::request_with_timeout(
                    &paths.socket_path(),
                    Request::QueryRemoteStatus,
                    Duration::from_millis(250)
                ),
                Ok(Response::RemoteStatus(_))
            );
            assert!(batch.daemon.try_wait().unwrap().is_none());
            if !daemon_ready {
                thread::sleep(Duration::from_millis(25));
            }
        }
        assert!(
            daemon_ready,
            "daemon did not become ready: stderr={} daemon_log={}",
            std::fs::read_to_string(&daemon_log).unwrap_or_default(),
            std::fs::read_to_string(paths.logs_dir().join("daemon.log")).unwrap_or_default()
        );

        let mut logs = Vec::new();
        for index in 0..4 {
            let log_path = sandbox.home.join(format!("gc-batch-{index}.log"));
            let log = File::create(&log_path).unwrap();
            batch.clients.push(
                sandbox
                    .cmd(rgo)
                    .args(["gc", "--target", "0"])
                    .stdout(Stdio::from(log.try_clone().unwrap()))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap(),
            );
            logs.push(log_path);
            if index == 0 {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !locked.is_file() && Instant::now() < deadline {
                    assert!(batch.clients[0].try_wait().unwrap().is_none());
                    thread::sleep(Duration::from_millis(25));
                }
                assert!(
                    locked.is_file(),
                    "first GC client did not reach the held-lock pause: {}",
                    std::fs::read_to_string(&logs[0]).unwrap_or_default()
                );
            }
        }
        thread::sleep(Duration::from_millis(200));
        assert!(
            batch
                .clients
                .iter_mut()
                .all(|child| child.try_wait().unwrap().is_none())
        );
        assert!(active.is_dir());
        assert!(idle.is_dir());
        std::fs::write(&batch.release, b"release").unwrap();
        for (client, log) in batch.clients.iter_mut().zip(&logs) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while client.try_wait().unwrap().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(25));
            }
            assert!(
                client.try_wait().unwrap().is_some(),
                "GC client timed out: {}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
            assert!(
                client.wait().unwrap().success(),
                "GC client failed: {}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let pass = sandbox
            .cmd(rgo)
            .args(["gc", "--target", "0"])
            .output()
            .unwrap();
        assert!(
            pass.status.success(),
            "{}",
            String::from_utf8_lossy(&pass.stderr)
        );
    }
    assert!(
        active.is_dir(),
        "GC removed a context during test execution"
    );
    assert!(
        !idle.exists(),
        "GC did not reclaim the unrelated idle context"
    );

    std::fs::write(&release, b"release").unwrap();
    let status = running.child.as_mut().unwrap().wait().unwrap();
    running.child.take();
    assert!(
        status.success(),
        "cargo test failed: {}",
        std::fs::read_to_string(&output).unwrap_or_default()
    );
    gc::remove_atomically(&paths, &active).unwrap();
}

#[test]
fn running_build_script_keeps_its_context_while_gc_reclaims_an_idle_one() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("build-script-lifetime").unwrap();
    let idle_workspace = sandbox.simple_bin("idle-build-script-lifetime").unwrap();
    std::fs::write(
        project.join("build.rs"),
        r#"fn main() {
    let ready = std::env::var("RGO_BUILD_READY").unwrap();
    let release = std::env::var("RGO_BUILD_RELEASE").unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    for _ in 0..1200 {
        if std::path::Path::new(&release).is_file() {
            std::fs::write(output.join("after-gc"), b"survived").unwrap();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("build script was never released");
}
"#,
    )
    .unwrap();

    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo =
        PathBuf::from(std::env::var_os("CARGO").expect("Cargo test runner sets CARGO"));
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(real_cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    let shim = PathBuf::from(record["supervised_cargo"]["shim_path"].as_str().unwrap());
    let path = std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let idle = paths.builds_dir().join("bb/idle-build-script");
    std::fs::create_dir_all(&idle).unwrap();
    std::fs::write(idle.join("unused"), vec![b'x'; 8192]).unwrap();
    context::write_sidecar(
        &idle,
        &idle_workspace,
        &idle_workspace.join("Cargo.toml"),
        None,
    )
    .unwrap();

    let ready = sandbox.home.join("build-ready");
    let release = sandbox.home.join("build-release");
    let output = sandbox.home.join("cargo-build.log");
    let log = File::create(&output).unwrap();
    #[cfg(unix)]
    let mut command = sandbox.cmd("cargo");
    #[cfg(windows)]
    let mut command = {
        let mut command = sandbox.cmd("cmd.exe");
        command.args(["/C", "cargo"]);
        command
    };
    let child = command
        .current_dir(&project)
        .env("PATH", path)
        .env("RGO_BUILD_READY", &ready)
        .env("RGO_BUILD_RELEASE", &release)
        .args(["build", "--offline"])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let mut running = RunningCargo {
        child: Some(child),
        release: release.clone(),
    };
    wait_for_marker(
        &ready,
        running.child.as_mut().unwrap(),
        &output,
        "build script",
    );

    let active = paths
        .checked_managed_build_dirs()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate != &idle)
        .expect("supervised Cargo build has a managed context");
    let blocked = gc::remove_atomically(&paths, &active).unwrap_err();
    assert!(
        blocked.to_string().contains("supervised Cargo"),
        "unexpected GC refusal: {blocked:#}"
    );
    let pass = sandbox
        .cmd(rgo)
        .args(["gc", "--target", "0"])
        .output()
        .unwrap();
    assert!(
        pass.status.success(),
        "{}; daemon_log={}",
        String::from_utf8_lossy(&pass.stderr),
        std::fs::read_to_string(paths.logs_dir().join("daemon.log")).unwrap_or_default()
    );
    assert!(
        active.is_dir(),
        "GC removed a context during a build script"
    );
    assert!(!idle.exists(), "GC did not reclaim the idle context");

    std::fs::write(&release, b"release").unwrap();
    let status = running.child.as_mut().unwrap().wait().unwrap();
    running.child.take();
    assert!(
        status.success(),
        "cargo build failed: {}",
        std::fs::read_to_string(&output).unwrap_or_default()
    );
    gc::remove_atomically(&paths, &active).unwrap();
}

#[cfg(unix)]
#[test]
fn running_rustdoc_keeps_its_context_while_gc_reclaims_an_idle_one() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("rustdoc-lifetime").unwrap();
    let idle_workspace = sandbox.simple_bin("idle-rustdoc-lifetime").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo =
        PathBuf::from(std::env::var_os("CARGO").expect("Cargo test runner sets CARGO"));
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(real_cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    let shim = PathBuf::from(record["supervised_cargo"]["shim_path"].as_str().unwrap());
    let path = std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let rustdoc = sandbox
        .cmd("rustup")
        .args(["which", "rustdoc"])
        .output()
        .unwrap();
    assert!(rustdoc.status.success());
    let real_rustdoc = PathBuf::from(String::from_utf8(rustdoc.stdout).unwrap().trim());
    let rustdoc_wrapper = sandbox.home.join("held-rustdoc");
    std::fs::write(
        &rustdoc_wrapper,
        b"#!/bin/sh\n: > \"$RGO_RUSTDOC_READY\"\nwhile [ ! -f \"$RGO_RUSTDOC_RELEASE\" ]; do sleep 0.05; done\nexec \"$RGO_REAL_RUSTDOC\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&rustdoc_wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let idle = paths.builds_dir().join("bb/idle-rustdoc");
    std::fs::create_dir_all(&idle).unwrap();
    std::fs::write(idle.join("unused"), vec![b'x'; 8192]).unwrap();
    context::write_sidecar(
        &idle,
        &idle_workspace,
        &idle_workspace.join("Cargo.toml"),
        None,
    )
    .unwrap();

    let ready = sandbox.home.join("rustdoc-ready");
    let release = sandbox.home.join("rustdoc-release");
    let output = sandbox.home.join("cargo-doc.log");
    let log = File::create(&output).unwrap();
    let child = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", path)
        .env("RUSTDOC", &rustdoc_wrapper)
        .env("RGO_REAL_RUSTDOC", real_rustdoc)
        .env("RGO_RUSTDOC_READY", &ready)
        .env("RGO_RUSTDOC_RELEASE", &release)
        .args(["doc", "--offline", "--no-deps"])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let mut running = RunningCargo {
        child: Some(child),
        release: release.clone(),
    };
    wait_for_marker(&ready, running.child.as_mut().unwrap(), &output, "rustdoc");

    let active = paths
        .checked_managed_build_dirs()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate != &idle)
        .expect("supervised cargo doc has a managed context");
    let blocked = gc::remove_atomically(&paths, &active).unwrap_err();
    assert!(
        blocked.to_string().contains("supervised Cargo"),
        "unexpected GC refusal: {blocked:#}"
    );
    let pass = sandbox
        .cmd(rgo)
        .args(["gc", "--target", "0"])
        .output()
        .unwrap();
    assert!(
        pass.status.success(),
        "{}",
        String::from_utf8_lossy(&pass.stderr)
    );
    assert!(active.is_dir(), "GC removed a context during rustdoc");
    assert!(!idle.exists(), "GC did not reclaim the idle context");

    std::fs::write(&release, b"release").unwrap();
    let status = running.child.as_mut().unwrap().wait().unwrap();
    running.child.take();
    assert!(
        status.success(),
        "cargo doc failed: {}",
        std::fs::read_to_string(&output).unwrap_or_default()
    );
    assert!(
        project
            .join("target/doc/rustdoc_lifetime/index.html")
            .is_file()
    );
    gc::remove_atomically(&paths, &active).unwrap();
}

#[test]
fn unchanged_cargo_build_refreshes_context_use_without_rustc() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("no-op-use").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo =
        PathBuf::from(std::env::var_os("CARGO").expect("Cargo test runner sets CARGO"));
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(real_cargo)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.cargo_home.join(".rgo-install.json")).unwrap(),
    )
    .unwrap();
    let shim = PathBuf::from(record["supervised_cargo"]["shim_path"].as_str().unwrap());
    let path = std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let build = || {
        #[cfg(unix)]
        let mut command = sandbox.cmd("cargo");
        #[cfg(windows)]
        let mut command = {
            let mut command = sandbox.cmd("cmd.exe");
            command.args(["/C", "cargo"]);
            command
        };
        command
            .current_dir(&project)
            .env("PATH", &path)
            .args(["build", "--offline"])
            .output()
            .unwrap()
    };
    let first = build();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.checked_managed_build_dirs().unwrap();
    assert_eq!(contexts.len(), 1);
    let dir = &contexts[0];
    let mut sidecar = context::read_sidecar(dir).unwrap();
    let first_seen = sidecar.first_seen;
    sidecar.last_seen = 1;
    std::fs::write(
        dir.join(SIDECAR_FILE),
        serde_json::to_vec(&sidecar).unwrap(),
    )
    .unwrap();

    let second = build();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&second.stderr).contains("Compiling"),
        "second build unexpectedly compiled: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let refreshed = context::read_sidecar(dir).unwrap();
    assert_eq!(refreshed.first_seen, first_seen);
    assert!(refreshed.last_seen > 1, "no-op use was not recorded");
}
