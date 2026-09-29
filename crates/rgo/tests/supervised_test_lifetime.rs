#![cfg(any(unix, windows))]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rgo_core::paths::RgoPaths;
use rgo_core::{context, gc};
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

fn wait_for_test(ready: &Path, child: &mut Child, output: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.is_file() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "cargo test exited before its test process started ({status}): {}",
                std::fs::read_to_string(output).unwrap_or_default()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ready.is_file(),
        "cargo test never reached its test process: {}",
        std::fs::read_to_string(output).unwrap_or_default()
    );
}

#[test]
fn unchanged_cargo_test_holds_gc_guard_through_test_execution() {
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
    wait_for_test(&ready, running.child.as_mut().unwrap(), &output);

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
