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

struct ShutdownDaemon(RgoPaths);

impl Drop for ShutdownDaemon {
    fn drop(&mut self) {
        let _ = rgo_core::ipc::request_with_timeout(
            &self.0.socket_path(),
            rgo_protocol::Request::Shutdown,
            Duration::from_secs(1),
        );
    }
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
    #[cfg(target_os = "macos")]
    let user_ready = sandbox.home.join("interrupt-user-signal");
    let release = sandbox.home.join("interrupt-child-release");
    let log = sandbox.home.join("interrupt-cargo.log");
    std::fs::write(
        project.join("src/main.rs"),
        r#"#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn signal(number: i32, action: usize) -> usize;
    fn _exit(code: i32) -> !;
}
#[cfg(target_os = "macos")]
extern "C" fn interrupted(_: i32) { unsafe { _exit(73) } }
#[cfg(target_os = "macos")]
static USER_SIGNAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "macos")]
extern "C" fn user_signal(_: i32) { USER_SIGNAL.store(true, std::sync::atomic::Ordering::Relaxed); }
fn main() {
    // A running application may replace the SIGINT action it inherited from
    // a background launch. Direct job-group signals must then reach it.
    #[cfg(target_os = "macos")]
    unsafe {
        assert_ne!(signal(2, interrupted as *const () as usize), usize::MAX);
        assert_ne!(signal(30, user_signal as *const () as usize), usize::MAX);
    }
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("trap '' INT USR1; printf '%s' \"$$\" > \"$RGO_INTERRUPT_READY\"; i=0; while [ ! -f \"$RGO_INTERRUPT_RELEASE\" ] && [ \"$i\" -lt 30 ]; do sleep 1; i=$((i+1)); done")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn().unwrap();
    #[cfg(target_os = "macos")]
    loop {
        if USER_SIGNAL.swap(false, std::sync::atomic::Ordering::Relaxed) {
            let marker = std::path::PathBuf::from(std::env::var_os("RGO_INTERRUPT_USER_READY").unwrap());
            let staging = marker.with_extension("pending");
            std::fs::write(&staging, b"handled").unwrap();
            std::fs::rename(staging, marker).unwrap();
        }
        if child.try_wait().unwrap().is_some() { break; }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = child.wait();
}
"#,
    )
    .unwrap();
    let mut command = sandbox.cmd("cargo");
    #[cfg(target_os = "macos")]
    {
        command.env("RGO_MACOS_SUPERVISOR_PILOT", "1");
        command.env("RGO_INTERRUPT_USER_READY", &user_ready);
        // Preserve ignored SIGINT during startup, then let the running
        // application change its action. SIGCONT is also ignored and blocked;
        // the separate Cargo group must still receive its kernel effect.
        unsafe {
            command.pre_exec(|| {
                let mut mask = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGCONT);
                if libc::signal(libc::SIGINT, libc::SIG_IGN) == libc::SIG_ERR
                    || libc::signal(libc::SIGUSR1, libc::SIG_IGN) == libc::SIG_ERR
                    || libc::signal(libc::SIGCONT, libc::SIG_IGN) == libc::SIG_ERR
                    || libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
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
    let _shutdown_daemon = ShutdownDaemon(paths.clone());
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    let active = &contexts[0];
    assert!(
        supervision::try_lock_gc(&paths, Some(active))
            .unwrap()
            .is_none()
    );

    let cargo_pid = running.child.id() as i32;
    #[cfg(target_os = "macos")]
    let user_forwarded = {
        assert_eq!(unsafe { libc::kill(-cargo_pid, libc::SIGUSR1) }, 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !user_ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        std::fs::read(&user_ready).unwrap_or_default() == b"handled"
    };
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
    let status = running.child.try_wait().unwrap();
    assert!(status.is_some(), "Ctrl-C did not stop the Cargo launcher");
    #[cfg(target_os = "macos")]
    assert_eq!(
        status.unwrap().code(),
        Some(73),
        "the running application's replacement SIGINT handler did not determine the result"
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
    assert!(
        !active.exists(),
        "idle interrupted context was not reclaimed"
    );
    #[cfg(target_os = "macos")]
    assert!(
        user_forwarded,
        "the running application's replacement SIGUSR1 handler was not reached"
    );
}
