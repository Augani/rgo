#![cfg(all(unix, debug_assertions))]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rgo_core::context;
use rgo_core::ipc;
use rgo_core::paths::RgoPaths;
use rgo_protocol::{Request, Response};
use rgo_testkit::Sandbox;

fn cargo_proxy() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("cargo"))
        .find(|path| path.is_file())
        .expect("Cargo proxy on PATH")
}

struct Processes {
    release: PathBuf,
    daemon: Child,
    clean: Option<Child>,
    cargo: Option<Child>,
}

impl Drop for Processes {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
        for process in [&mut self.cargo, &mut self.clean] {
            if let Some(child) = process.as_mut() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn wait_for_file(path: &Path, child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !path.is_file() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    path.is_file()
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    child.try_wait().unwrap().is_some()
}

#[test]
fn cargo_waits_for_gc_to_remove_an_old_context_before_rebuilding_it() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("gc-race").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo = cargo_proxy();
    let fake_dir = sandbox.home.join("observed-cargo");
    std::fs::create_dir(&fake_dir).unwrap();
    let fake = fake_dir.join("cargo");
    let quoted_cargo = real_cargo.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = build ] && [ -n \"${{RGO_RACE_CARGO_ENTERED:-}}\" ]; then\n    : > \"$RGO_RACE_CARGO_ENTERED\"\n    break\n  fi\ndone\nexec '{quoted_cargo}' \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--real-cargo"])
        .arg(&fake)
        .arg("--no-service")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let mut search_path = vec![sandbox.cargo_home.join("rgo/shims")];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let search_path = std::env::join_paths(search_path).unwrap();
    let locked = sandbox.home.join("gc-locked");
    let release = sandbox.home.join("gc-release");
    let daemon_log = sandbox.home.join("daemon.log");
    let daemon = sandbox
        .cmd(rgo)
        .args(["daemon", "--foreground", "--home"])
        .arg(&sandbox.rgo_home)
        .env("RGO_TEST_GC_LOCKED_MARKER", &locked)
        .env("RGO_TEST_GC_LOCKED_RELEASE", &release)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&daemon_log).unwrap())
        .spawn()
        .unwrap();
    let mut processes = Processes {
        release,
        daemon,
        clean: None,
        cargo: None,
    };
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while !ready && Instant::now() < deadline {
        ready = matches!(
            ipc::request_with_timeout(
                &paths.socket_path(),
                Request::QueryRemoteStatus,
                Duration::from_millis(250)
            ),
            Ok(Response::RemoteStatus(_))
        );
        assert!(processes.daemon.try_wait().unwrap().is_none());
        if !ready {
            thread::sleep(Duration::from_millis(25));
        }
    }
    assert!(
        ready,
        "daemon did not become ready: {}",
        std::fs::read_to_string(&daemon_log).unwrap_or_default()
    );

    let initial = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let listed = context::list(&paths).unwrap();
    assert_eq!(listed.len(), 1);
    let context = &listed[0].dir;
    let old = context.join("old-generation");
    std::fs::write(&old, b"must be removed").unwrap();
    let old_time = SystemTime::now() - Duration::from_secs(7200);
    for profile in std::fs::read_dir(context).unwrap().flatten() {
        let lock = profile.path().join(".cargo-build-lock");
        if lock.is_file() {
            std::fs::OpenOptions::new()
                .write(true)
                .open(lock)
                .unwrap()
                .set_modified(old_time)
                .unwrap();
        }
    }

    let clean_log = sandbox.home.join("clean.log");
    processes.clean = Some(
        sandbox
            .cmd(rgo)
            .arg("clean")
            .arg(listed[0].id())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&clean_log).unwrap())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_for_file(
            &locked,
            processes.clean.as_mut().unwrap(),
            Duration::from_secs(15)
        ),
        "GC did not reach its held-lock pause: {}",
        std::fs::read_to_string(&clean_log).unwrap_or_default()
    );
    assert!(old.is_file());

    let entered = sandbox.home.join("real-cargo-entered");
    let waiting = sandbox.home.join("session-lock-waiting");
    let build_log = sandbox.home.join("build.log");
    processes.cargo = Some(
        sandbox
            .cmd("cargo")
            .current_dir(&project)
            .env("PATH", &search_path)
            .env("RGO_RACE_CARGO_ENTERED", &entered)
            .env("RGO_TEST_SESSION_LOCK_MARKER", &waiting)
            .args(["build", "--offline"])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&build_log).unwrap())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_for_file(
            &waiting,
            processes.cargo.as_mut().unwrap(),
            Duration::from_secs(10)
        ),
        "Cargo did not reach its lifecycle lock: {}",
        std::fs::read_to_string(&build_log).unwrap_or_default()
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        processes
            .cargo
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert!(
        !entered.exists(),
        "real Cargo entered the context while GC held its guard"
    );
    assert!(old.is_file());

    std::fs::write(&processes.release, b"release").unwrap();
    assert!(wait_for_exit(
        processes.clean.as_mut().unwrap(),
        Duration::from_secs(15)
    ));
    assert!(
        processes.clean.as_mut().unwrap().wait().unwrap().success(),
        "clean failed: {}",
        std::fs::read_to_string(&clean_log).unwrap_or_default()
    );
    assert!(wait_for_exit(
        processes.cargo.as_mut().unwrap(),
        Duration::from_secs(30)
    ));
    assert!(
        processes.cargo.as_mut().unwrap().wait().unwrap().success(),
        "Cargo failed after GC: {}",
        std::fs::read_to_string(&build_log).unwrap_or_default()
    );
    assert!(entered.is_file());
    assert!(!old.exists(), "GC left bytes from the old context");
    assert!(context::read_sidecar(context).is_some());
    assert!(project.join("target/debug/gc-race").is_file());
}
