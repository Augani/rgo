#![cfg(all(windows, debug_assertions))]

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rgo_core::context;
use rgo_core::ipc;
use rgo_core::paths::RgoPaths;
use rgo_protocol::{Request, Response};
use rgo_testkit::Sandbox;

struct Processes {
    gc_release: PathBuf,
    job_release: PathBuf,
    daemon: Child,
    clean: Option<Child>,
    cargo: Option<Child>,
}

impl Drop for Processes {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gc_release, b"release");
        let _ = std::fs::write(&self.job_release, b"release");
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

fn age_finished_profile_locks(context: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(7200);
    for profile in std::fs::read_dir(context).unwrap().flatten() {
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

#[test]
fn cargo_waits_for_gc_to_remove_an_old_context_before_rebuilding_it() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("windows-gc-race").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let real_cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .expect("Cargo sets CARGO for integration tests");
    let setup = sandbox
        .cmd(rgo)
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
    let search_path =
        std::env::join_paths(std::iter::once(shim.parent().unwrap().to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();

    let gc_locked = sandbox.home.join("gc-locked");
    let gc_release = sandbox.home.join("gc-release");
    let job_assigned = sandbox.home.join("job-assigned");
    let job_release = sandbox.home.join("job-release");
    let daemon_log = sandbox.home.join("daemon.log");
    let daemon = sandbox
        .cmd(rgo)
        .args(["daemon", "--foreground", "--home"])
        .arg(&sandbox.rgo_home)
        .env("RGO_TEST_GC_LOCKED_MARKER", &gc_locked)
        .env("RGO_TEST_GC_LOCKED_RELEASE", &gc_release)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&daemon_log).unwrap())
        .spawn()
        .unwrap();
    let mut processes = Processes {
        gc_release,
        job_release,
        daemon,
        clean: None,
        cargo: None,
    };
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
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
        .cmd("cmd.exe")
        .current_dir(&project)
        .env("PATH", &search_path)
        .args(["/C", "cargo", "build", "--offline"])
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
    age_finished_profile_locks(context);

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
            &gc_locked,
            processes.clean.as_mut().unwrap(),
            Duration::from_secs(15)
        ),
        "GC did not reach its held-lock pause: {}",
        std::fs::read_to_string(&clean_log).unwrap_or_default()
    );
    assert!(old.is_file());

    let waiting = sandbox.home.join("session-lock-waiting");
    let build_log = sandbox.home.join("build.log");
    let job_release = processes.job_release.clone();
    processes.cargo = Some(
        sandbox
            .cmd("cmd.exe")
            .current_dir(&project)
            .env("PATH", &search_path)
            .env("RGO_TEST_SESSION_LOCK_MARKER", &waiting)
            .env("RGO_TEST_JOB_ASSIGNED_MARKER", &job_assigned)
            .env("RGO_TEST_JOB_ASSIGNED_RELEASE", &job_release)
            .args(["/C", "cargo", "build", "--offline"])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&build_log).unwrap())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_for_file(
            &waiting,
            processes.cargo.as_mut().unwrap(),
            Duration::from_secs(15)
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
        !job_assigned.exists(),
        "Cargo's job was assigned while GC held the context guard"
    );
    assert!(old.is_file());

    std::fs::write(&processes.gc_release, b"release").unwrap();
    assert!(wait_for_exit(
        processes.clean.as_mut().unwrap(),
        Duration::from_secs(15)
    ));
    assert!(
        processes.clean.as_mut().unwrap().wait().unwrap().success(),
        "clean failed: {}",
        std::fs::read_to_string(&clean_log).unwrap_or_default()
    );
    assert!(
        wait_for_file(
            &job_assigned,
            processes.cargo.as_mut().unwrap(),
            Duration::from_secs(15)
        ),
        "Cargo did not resume after GC: {}",
        std::fs::read_to_string(&build_log).unwrap_or_default()
    );
    assert!(!old.exists(), "GC left bytes from the old context");
    std::fs::write(&processes.job_release, b"release").unwrap();
    assert!(wait_for_exit(
        processes.cargo.as_mut().unwrap(),
        Duration::from_secs(30)
    ));
    assert!(
        processes.cargo.as_mut().unwrap().wait().unwrap().success(),
        "Cargo failed after GC: {}",
        std::fs::read_to_string(&build_log).unwrap_or_default()
    );
    assert!(context::read_sidecar(context).is_some());
    assert!(project.join("target/debug/windows-gc-race.exe").is_file());
}
