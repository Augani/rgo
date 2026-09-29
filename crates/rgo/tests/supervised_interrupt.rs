#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rgo_core::context;
use rgo_core::paths::RgoPaths;
use rgo_core::supervision;
use rgo_testkit::Sandbox;

fn cargo_proxy() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("cargo"))
        .find(|path| path.is_file())
        .expect("Cargo proxy on PATH")
}

struct RunningCargo {
    child: Child,
    release: PathBuf,
}

impl Drop for RunningCargo {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_for_file(path: &Path, child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.is_file() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    path.is_file()
}

#[test]
#[allow(unsafe_code)]
fn ctrl_c_keeps_a_surviving_cargo_descendant_protected() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("interrupt-parent").unwrap();
    let other_project = sandbox.simple_bin("interrupt-idle").unwrap();
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
    let mut search_path = vec![sandbox.cargo_home.join("rgo/shims")];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let search_path = std::env::join_paths(search_path).unwrap();
    let ready = sandbox.home.join("interrupt-child-ready");
    let release = sandbox.home.join("interrupt-child-release");
    let log = sandbox.home.join("interrupt-cargo.log");
    std::fs::write(
        project.join("src/main.rs"),
        r#"fn main() {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("trap '' INT; printf '%s' \"$$\" > \"$RGO_INTERRUPT_READY\"; i=0; while [ ! -f \"$RGO_INTERRUPT_RELEASE\" ] && [ \"$i\" -lt 30 ]; do sleep 1; i=$((i+1)); done")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn().unwrap();
    let _ = child.wait();
}
"#,
    )
    .unwrap();
    let child = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_INTERRUPT_READY", &ready)
        .env("RGO_INTERRUPT_RELEASE", &release)
        .args(["run", "--offline"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut running = RunningCargo { child, release };
    assert!(
        wait_for_file(&ready, &mut running.child),
        "Cargo child did not start: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let child_pid: i32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    let active = &contexts[0];
    assert!(
        supervision::try_lock_gc(&paths, Some(active))
            .unwrap()
            .is_none()
    );

    let interrupted = unsafe { libc::kill(running.child.id() as i32, libc::SIGINT) };
    assert_eq!(interrupted, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while running.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        running.child.try_wait().unwrap().is_some(),
        "Ctrl-C did not stop the Cargo launcher"
    );
    assert_eq!(unsafe { libc::kill(child_pid, 0) }, 0);
    assert!(
        supervision::try_lock_gc(&paths, Some(active))
            .unwrap()
            .is_none(),
        "GC acquired the interrupted context while its child was still running"
    );

    let idle = paths.builds_dir().join("bb/interrupt-idle");
    std::fs::create_dir_all(&idle).unwrap();
    std::fs::write(idle.join("unused"), b"reclaim").unwrap();
    context::write_sidecar(
        &idle,
        &other_project,
        &other_project.join("Cargo.toml"),
        None,
    )
    .unwrap();
    let gc = sandbox
        .cmd(rgo)
        .args(["gc", "--target", "0"])
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(active.exists(), "GC removed the interrupted active context");
    assert!(!idle.exists(), "GC did not reclaim unrelated idle storage");

    std::fs::write(&running.release, b"release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while supervision::try_lock_gc(&paths, Some(active))
        .unwrap()
        .is_none()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        supervision::try_lock_gc(&paths, Some(active))
            .unwrap()
            .is_some()
    );
}
