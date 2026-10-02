#![cfg(unix)]

use std::os::unix::process::CommandExt;
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

fn wait_for_pid(path: &Path, child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse() {
                return Some(pid);
            }
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

#[test]
#[allow(unsafe_code)]
fn terminal_ctrl_c_keeps_a_surviving_cargo_descendant_protected() {
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
    let mut command = sandbox.cmd("cargo");
    #[cfg(target_os = "macos")]
    command.env("RGO_MACOS_SUPERVISOR_PILOT", "1");
    let child = command
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_INTERRUPT_READY", &ready)
        .env("RGO_INTERRUPT_RELEASE", &release)
        .args(["run", "--offline"])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut running = RunningCargo { child, release };
    let child_pid = wait_for_pid(&ready, &mut running.child).unwrap_or_else(|| {
        panic!(
            "Cargo child did not start: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        )
    });
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

    let cargo_pid = running.child.id() as i32;
    #[cfg(not(target_os = "macos"))]
    assert_eq!(unsafe { libc::getpgid(child_pid) }, cargo_pid);
    #[cfg(target_os = "macos")]
    assert_ne!(
        unsafe { libc::getpgid(child_pid) },
        cargo_pid,
        "macOS did not isolate Cargo in its guardian's process group"
    );
    #[cfg(target_os = "macos")]
    {
        assert_eq!(unsafe { libc::kill(-cargo_pid, libc::SIGTSTP) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut status = 0;
            let changed =
                unsafe { libc::waitpid(cargo_pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
            if changed == cargo_pid {
                assert!(libc::WIFSTOPPED(status), "Cargo exited instead of stopping");
                break;
            }
            assert!(
                changed >= 0 && Instant::now() < deadline,
                "guardian did not reflect Cargo's stop to its launcher"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(unsafe { libc::kill(-cargo_pid, libc::SIGCONT) }, 0);
    }
    let interrupted = unsafe { libc::kill(-cargo_pid, libc::SIGINT) };
    let interrupt_error = std::io::Error::last_os_error();
    assert_eq!(
        interrupted,
        0,
        "Ctrl-C could not reach launcher group {cargo_pid}: {interrupt_error}; status {:?}; stderr {}",
        running.child.try_wait().unwrap(),
        std::fs::read_to_string(&log).unwrap_or_default()
    );
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
    context::write_supervised_sidecar(
        &idle,
        &other_project,
        &other_project.join("Cargo.toml"),
        true,
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
