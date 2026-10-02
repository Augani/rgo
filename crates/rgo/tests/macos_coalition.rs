#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use rgo_core::macos_coalition::ResourceCoalition;
use rgo_core::paths::RgoPaths;
use rgo_core::supervision;
use rgo_testkit::Sandbox;

struct ProbeJob<'a> {
    sandbox: &'a Sandbox,
    target: String,
    directory: PathBuf,
    release: PathBuf,
}

impl Drop for ProbeJob<'_> {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"continue");
        let _ = self
            .sandbox
            .cmd("launchctl")
            .args(["bootout", &self.target])
            .output();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn wait_for_file(path: &std::path::Path, deadline: Instant) {
    while !path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.is_file(), "probe did not publish {}", path.display());
}

#[test]
#[allow(unsafe_code)]
fn launchd_coalition_observes_closed_fd_writer_after_cargo_exits() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("coalition-writer").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("cargo"))
        .find(|path| path.is_file())
        .unwrap();
    let setup = sandbox
        .cmd(rgo)
        .args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(cargo)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let ready = sandbox.home.join("writer.json");
    let release = sandbox.home.join("release-writer");
    let result = sandbox.home.join("writer-result");
    std::fs::write(
        project.join("worker.py"),
        r#"import json, os, pathlib, time
ready = pathlib.Path(os.environ['RGO_COALITION_READY'])
release = pathlib.Path(os.environ['RGO_COALITION_RELEASE'])
result = pathlib.Path(os.environ['RGO_COALITION_RESULT'])
out = pathlib.Path(os.environ['OUT_DIR']) / 'late-build-output'
ready.write_text(json.dumps({'pid': os.getpid(), 'output': str(out)}))
deadline = time.monotonic() + 90
while not release.exists() and time.monotonic() < deadline:
    time.sleep(0.02)
try:
    out.write_text('completed after Cargo exited')
    result.write_text('write succeeded')
except OSError as error:
    result.write_text(str(error))
"#,
    )
    .unwrap();
    std::fs::write(
        project.join("launch.py"),
        r#"import os, pathlib, subprocess, sys, time
subprocess.Popen([sys.executable, 'worker.py'], stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
deadline = time.monotonic() + 10
while not pathlib.Path(os.environ['RGO_COALITION_READY']).exists():
    if time.monotonic() > deadline: raise RuntimeError('worker did not start')
    time.sleep(0.02)
"#,
    )
    .unwrap();
    std::fs::write(project.join("build.rs"), "fn main() { assert!(std::process::Command::new(\"python3\").arg(\"launch.py\").status().unwrap().success()); }\n").unwrap();
    let search_path = std::env::join_paths(
        std::iter::once(sandbox.cargo_home.join("rgo/shims"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let build = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_MACOS_SUPERVISOR_PILOT", "1")
        .env("RGO_COALITION_READY", &ready)
        .env("RGO_COALITION_RELEASE", &release)
        .env("RGO_COALITION_RESULT", &result)
        .args(["build", "--offline"])
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&build.stderr).contains("guardian unavailable"),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        ready.is_file(),
        "successful Cargo did not publish the detached writer"
    );
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let directory = std::fs::read_dir(paths.state_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("macos-cargo-job-")
        })
        .expect("installed Cargo launcher did not create its own guardian job");
    let owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("owner.json")).unwrap()).unwrap();
    let job = ProbeJob {
        sandbox: &sandbox,
        target: format!(
            "{}/{}",
            owner["domain"].as_str().unwrap(),
            owner["label"].as_str().unwrap()
        ),
        directory,
        release: release.clone(),
    };
    let status = sandbox
        .cmd("launchctl")
        .args(["print", &job.target])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).unwrap();
    let guardian_pid: i32 = status
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .expect("Cargo guardian was not alive after its primary exited")
        .parse()
        .unwrap();
    let writer: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    let writer_pid = i32::try_from(writer["pid"].as_i64().unwrap()).unwrap();
    let coalition = ResourceCoalition::for_pid(writer_pid).unwrap();
    assert_eq!(coalition, ResourceCoalition::for_pid(guardian_pid).unwrap());
    let host = ResourceCoalition::for_pid(std::process::id() as i32).unwrap();
    assert_ne!(
        coalition.id(),
        host.id(),
        "job inherited the host application's coalition"
    );
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    assert!(std::path::Path::new(writer["output"].as_str().unwrap()).starts_with(&contexts[0]));
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0]))
            .unwrap()
            .is_none()
    );

    // Crash the independent guardian as well: inherited descriptors are gone,
    // but its durable kernel receipt must still protect the detached writer.
    assert_eq!(unsafe { libc::kill(guardian_pid, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while coalition.active_tasks().unwrap() != 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(coalition.active_tasks().unwrap(), 1);
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0]))
            .unwrap()
            .is_none(),
        "guardian crash released cleanup while the closed-FD writer was alive"
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
        contexts[0].is_dir(),
        "GC removed the detached writer's context"
    );
    let receipt = std::fs::read_dir(paths.state_dir().join("locks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("macos-coalition-context-")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let saved_receipt = std::fs::read(&receipt).unwrap();
    std::fs::write(&receipt, b"invalid receipt").unwrap();
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0])).is_err(),
        "invalid durable protection was treated as an idle context"
    );
    std::fs::write(&receipt, saved_receipt).unwrap();
    std::fs::write(&release, b"continue").unwrap();
    wait_for_file(&result, Instant::now() + Duration::from_secs(10));
    assert_eq!(std::fs::read_to_string(&result).unwrap(), "write succeeded");
    let deadline = Instant::now() + Duration::from_secs(10);
    while coalition.active_tasks().unwrap() != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(coalition.active_tasks().unwrap(), 0);
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0]))
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
    assert!(!contexts[0].exists(), "idle context was not reclaimed");
    println!(
        "installed Cargo pilot: real detached writer protected through Cargo exit and guardian SIGKILL; late write succeeded; idle context reclaimed"
    );
    drop(job);

    // Check real launcher transport and healthy completion with this same
    // project: stdin/EOF, separate streams, native bytes, and nonzero status.
    std::fs::write(
        project.join("src/main.rs"),
        r#"use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
fn main() {
    let mut input = Vec::new(); std::io::stdin().read_to_end(&mut input).unwrap();
    let mut out = std::io::stdout(); out.write_all(&input).unwrap();
    out.write_all(std::env::args_os().nth(1).unwrap().as_bytes()).unwrap();
    out.write_all(std::env::var_os("RGO_NATIVE_BYTES").unwrap().as_bytes()).unwrap();
    eprintln!("application stderr preserved");
    std::process::exit(17);
}
"#,
    )
    .unwrap();
    use std::io::Write;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Stdio;
    let mut child = sandbox
        .cmd("cargo")
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_MACOS_SUPERVISOR_PILOT", "1")
        .env("RGO_COALITION_READY", &ready)
        .env("RGO_COALITION_RELEASE", &release)
        .env("RGO_COALITION_RESULT", &result)
        .env(
            "RGO_NATIVE_BYTES",
            std::ffi::OsString::from_vec(vec![b'e', 0xfe]),
        )
        .args(["run", "--offline", "--"])
        .arg(std::ffi::OsString::from_vec(vec![b'a', 0xff]))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin through launchd\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(17),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"stdin through launchd\na\xffe\xfe");
    assert!(String::from_utf8_lossy(&output.stderr).contains("application stderr preserved"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = std::fs::read_dir(paths.state_dir())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("macos-cargo-job-")
            })
            .count();
        if remaining == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "healthy Cargo guardian did not clean its job directory"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0]))
            .unwrap()
            .is_some(),
        "healthy guardian did not retire its kernel receipt"
    );
    println!(
        "installed Cargo pilot: stdin/EOF, stdout/stderr, non-UTF-8 args/environment, exit 17, and healthy job retirement passed"
    );
}
