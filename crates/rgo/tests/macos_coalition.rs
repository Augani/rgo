#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use rgo_core::macos_coalition::ResourceCoalition;
use rgo_core::macos_terminal_hosts::{self as terminal_hosts, Host, Process, ProcessState, Saved};
use rgo_core::paths::RgoPaths;
use rgo_core::supervision;
use rgo_protocol::{Request, Response};
use rgo_testkit::Sandbox;

struct ReleaseWorker(PathBuf);

impl Drop for ReleaseWorker {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"continue");
        if self.0.with_file_name("shell-worker-pid").is_file() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !self.0.with_file_name("shell-worker-exited").is_file()
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

struct PrivateDaemon(std::process::Child);

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct ShutdownDaemon(RgoPaths);

impl Drop for ShutdownDaemon {
    fn drop(&mut self) {
        let _ = rgo_core::ipc::request_with_timeout(
            &self.0.socket_path(),
            Request::Shutdown,
            Duration::from_secs(1),
        );
    }
}

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
        .env("RGO_DAEMON_POLL_SECS", "1")
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
        .env("RGO_DAEMON_POLL_SECS", "1")
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
    let _shutdown_daemon = ShutdownDaemon(paths.clone());
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
        .env("RGO_DAEMON_POLL_SECS", "1")
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
    // An edited definition must survive housekeeping even after its context
    // becomes idle. Restoring the exact owned definition permits recovery.
    let definition_path = job.directory.join("job.plist");
    let definition = std::fs::read(&definition_path).unwrap();
    let mut edited = definition.clone();
    edited.extend_from_slice(b"<!-- user edit -->");
    std::fs::write(&definition_path, edited).unwrap();
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
    assert!(job.directory.is_dir(), "recovery removed an edited job");
    assert!(
        sandbox
            .cmd("launchctl")
            .args(["print", &job.target])
            .output()
            .unwrap()
            .status
            .success()
    );
    std::fs::write(&definition_path, definition).unwrap();
    // Metadata recovery also runs with automatic destructive GC disabled.
    let deadline = Instant::now() + Duration::from_secs(10);
    while job.directory.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !job.directory.exists(),
        "idle crashed guardian was not recovered"
    );
    // Recovery removes the rendezvous before bootout can reap the identity.
    // Directory disappearance precedes completion of that external helper.
    let deadline = Instant::now() + Duration::from_secs(10);
    while sandbox
        .cmd("launchctl")
        .args(["print", &job.target])
        .output()
        .unwrap()
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "recovered job was not unloaded");
        thread::sleep(Duration::from_millis(20));
    }
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
    if std::env::var_os("RGO_TERMINAL_PROBE").is_some() {
        let status = std::process::Command::new("python3").arg("terminal-child.py").status().unwrap();
        std::process::exit(status.code().unwrap_or(130));
    }
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

    // Extend the same real-Cargo fixture with an interactive zsh job, rather
    // than adding a second copy of the guardian setup and crash checks.
    std::fs::write(
        project.join("terminal-child.py"),
        include_str!("fixtures/macos-terminal-child.py"),
    )
    .unwrap();
    let worker_release = ReleaseWorker(project.join("release-shell-worker"));
    let terminal = sandbox
        .cmd("python3")
        .arg("-c")
        .arg(include_str!("fixtures/macos-terminal-shell.py"))
        .current_dir(&project)
        .env("PATH", &search_path)
        .env("RGO_MACOS_SUPERVISOR_PILOT", "1")
        .env("RGO_TERMINAL_PROBE", "1")
        .env("RGO_TERMINAL_HELPER", rgo)
        .env("RGO_COALITION_READY", &ready)
        .env("RGO_COALITION_RELEASE", &release)
        .env("RGO_COALITION_RESULT", &result)
        .output()
        .unwrap();
    assert!(
        terminal.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&terminal.stdout),
        String::from_utf8_lossy(&terminal.stderr)
    );
    println!("{}", String::from_utf8_lossy(&terminal.stdout));

    let token = std::fs::read_to_string(project.join("retiring-terminal-token")).unwrap();
    let host_dir = paths.state_dir().join("terminal-hosts").join(&token);
    let journal = host_dir
        .parent()
        .unwrap()
        .join(format!(".retiring-{token}.json"));
    let host: Host = terminal_hosts::read(&host_dir.join("host.json")).unwrap();
    assert!(host.shell.state().unwrap() == ProcessState::Gone);
    let unknown = terminal_hosts::retire(&paths, &token, None).unwrap_err();
    assert!(
        unknown.to_string().contains("unknown content"),
        "{unknown:#}"
    );
    assert!(host_dir.join("host.json").is_file() && !journal.exists());

    let worker_pid: i32 = std::fs::read_to_string(project.join("shell-worker-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let worker = Process::observe(worker_pid).unwrap().unwrap();
    assert_eq!(worker.session, host.shell.session);
    let mut lease: Saved = serde_json::from_slice(
        &std::fs::read(project.join("retiring-terminal-lease.json")).unwrap(),
    )
    .unwrap();
    lease.caller = worker.clone();
    terminal_hosts::write(&host_dir, "lease.json", &lease, false).unwrap();
    std::fs::remove_file(host_dir.join("keep-me")).unwrap();
    let live = terminal_hosts::retire(&paths, &token, None).unwrap_err();
    assert!(live.to_string().contains("live caller"), "{live:#}");
    assert!(host_dir.join("host.json").is_file() && !journal.exists());

    // fs4 try_lock returns Ok(false) for contention. A second descriptor must
    // refuse recovery rather than continuing as though it held the lock.
    let host_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(host_dir.join("host.lock"))
        .unwrap();
    FileExt::lock_exclusive(&host_lock).unwrap();
    assert!(
        terminal_hosts::lock(&host_dir)
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    drop(host_lock);
    let retirement_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            paths
                .state_dir()
                .join("locks/macos-terminal-retirement.lock"),
        )
        .unwrap();
    FileExt::lock_exclusive(&retirement_lock).unwrap();
    assert!(
        terminal_hosts::retire(&paths, &token, None)
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    drop(retirement_lock);

    // Stop the actual daemon before interrupting retirement. Acquiring its
    // singleton lock proves exit; a stale socket is not evidence of liveness.
    assert!(matches!(
        rgo_core::ipc::request(&paths.socket_path(), Request::Shutdown).unwrap(),
        Response::Ok
    ));
    let daemon_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.state_dir().join("daemon.lock"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !FileExt::try_lock_exclusive(&daemon_lock).unwrap() {
        assert!(Instant::now() < deadline, "private daemon did not stop");
        thread::sleep(Duration::from_millis(20));
    }
    drop(worker_release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while worker.state().unwrap() == ProcessState::Live {
        assert!(
            Instant::now() < deadline,
            "private shell worker did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!terminal_hosts::retire_step(&paths, &token, None, 1).unwrap());
    assert!(!host_dir.join("host.json").exists() && journal.is_file());
    let generation = host_dir.join("generation");
    let original = std::fs::read(&generation).unwrap();
    std::fs::write(&generation, b"user edit").unwrap();
    let remaining = std::fs::read_dir(&host_dir).unwrap().count();
    let edited = terminal_hosts::retire(&paths, &token, None).unwrap_err();
    assert!(edited.to_string().contains("content changed"), "{edited:#}");
    assert_eq!(std::fs::read_dir(&host_dir).unwrap().count(), remaining);
    assert!(journal.is_file());
    std::fs::write(&generation, original).unwrap();
    drop(daemon_lock);
    let _daemon = PrivateDaemon(
        sandbox
            .cmd(rgo)
            .args(["daemon", "--foreground"])
            .env("RGO_DAEMON_POLL_SECS", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while host_dir.exists() || journal.exists() {
        assert!(
            Instant::now() < deadline,
            "restarted daemon did not resume owned terminal retirement"
        );
        thread::sleep(Duration::from_millis(20));
    }
    println!(
        "terminal metadata: live caller, added/edited content, busy locks, and interrupted retirement across real daemon restart passed"
    );
}
