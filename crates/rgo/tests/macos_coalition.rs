#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use rgo_core::macos_coalition::{ResourceCoalition, ResourceState};
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

fn stop_private_daemon(paths: &RgoPaths) -> std::fs::File {
    assert!(matches!(
        rgo_core::ipc::request(&paths.socket_path(), Request::Shutdown).unwrap(),
        Response::Ok
    ));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.state_dir().join("daemon.lock"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !FileExt::try_lock_exclusive(&lock).unwrap() {
        assert!(Instant::now() < deadline, "private daemon did not stop");
        thread::sleep(Duration::from_millis(20));
    }
    lock
}

struct CancelProbe {
    child: std::process::Child,
    release: PathBuf,
}

fn publish_audit_release(path: &std::path::Path, action: &[u8]) -> std::io::Result<()> {
    let pending = path.with_extension("pending");
    std::fs::write(&pending, action)?;
    std::fs::rename(pending, path)
}

impl Drop for CancelProbe {
    fn drop(&mut self) {
        let _ = publish_audit_release(&self.release, b"resume");
        let _ = publish_audit_release(&self.release.with_file_name("delivery-release"), b"resume");
        let _ = publish_audit_release(&self.release.with_file_name("cut-release"), b"resume");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(unsafe_code)]
fn cancellation_before_commit(sandbox: &Sandbox, project: &std::path::Path, rgo: &str) {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let directory = paths.state_dir().join("macos-supervisor-audit");
    let path = std::env::join_paths(
        std::iter::once(sandbox.cargo_home.join("rgo/shims"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let source = project.join("src/main.rs");
    let original_source = std::fs::read(&source).unwrap();
    let mut failures = Vec::new();
    for (signal, action, ignored, blocked, cut) in [
        (libc::SIGINT, b"resume".as_slice(), false, false, false),
        (libc::SIGTERM, b"fail".as_slice(), false, false, false),
        (libc::SIGINT, b"resume".as_slice(), true, false, false),
        (libc::SIGTERM, b"resume".as_slice(), false, true, false),
        (libc::SIGTERM, b"fail".as_slice(), false, true, false),
        (libc::SIGTERM, b"resume".as_slice(), false, true, true),
        (libc::SIGINT, b"resume".as_slice(), false, false, true),
    ] {
        if blocked {
            // Check the inherited mask before changing the application action.
            // A queued TERM must kill the managed app or invoke the fallback
            // handler when unblocked; normal return is a lost notification.
            std::fs::write(
                &source,
                format!(
                    r#"unsafe extern "C" {{
    fn sigprocmask(how: i32, set: *const u32, old: *mut u32) -> i32;
    fn sigpending(set: *mut u32) -> i32;
    fn signal(sig: i32, action: usize) -> usize;
    fn _exit(code: i32) -> !;
}}
extern "C" fn terminated(_: i32) {{ unsafe {{ _exit(74) }} }}
fn main() {{ unsafe {{
    let mut mask = 0_u32;
    assert_eq!(sigprocmask({setmask}, std::ptr::null(), &mut mask), 0);
    let term = 1_u32 << ({term} - 1);
    assert_ne!(mask & term, 0, "Cargo lost its inherited signal mask");
    let mut pending = 0_u32;
    assert_eq!(sigpending(&mut pending), 0);
    let marker = std::path::PathBuf::from(std::env::var_os("RGO_PENDING_MARKER").unwrap());
    let temporary = marker.with_extension("pending");
    std::fs::write(&temporary, if pending & term != 0 {{ b"pending".as_slice() }} else {{ b"missing".as_slice() }}).unwrap();
    std::fs::rename(temporary, marker).unwrap();
    if {handled} {{ signal({term}, terminated as *const () as usize); }}
    assert_eq!(sigprocmask({unblock}, &term, std::ptr::null_mut()), 0);
}} }}
"#,
                    setmask = libc::SIG_SETMASK,
                    term = libc::SIGTERM,
                    unblock = libc::SIG_UNBLOCK,
                    handled = action == b"fail" || cut,
                ),
            )
            .unwrap();
        }
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let log = directory.join("stderr");
        let mut command = sandbox.cmd("cargo");
        command
            .current_dir(project)
            .env("PATH", &path)
            .env("RGO_MACOS_SUPERVISOR_PILOT", "1")
            .env("RGO_MACOS_SUPERVISOR_AUDIT", "1")
            .args([if blocked { "run" } else { "build" }, "--offline"])
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        if ignored {
            unsafe {
                command.pre_exec(|| {
                    if libc::signal(libc::SIGINT, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        if blocked {
            command.env("RGO_PENDING_MARKER", directory.join("pending-observed"));
            if action == b"resume" && !cut {
                command.env("RGO_MACOS_SUPERVISOR_COMMIT_AUDIT", "1");
            }
            unsafe {
                command.pre_exec(|| {
                    let mut mask = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGTERM);
                    if libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        if cut {
            command.env("RGO_MACOS_SUPERVISOR_COMMIT_CUT_AUDIT", "1");
        }
        let mut probe = CancelProbe {
            child: command.spawn().unwrap(),
            release: directory.join("release"),
        };
        wait_for_file(
            &directory.join("prepared"),
            Instant::now() + Duration::from_secs(10),
        );
        assert!(probe.child.try_wait().unwrap().is_none());
        if !cut || blocked {
            assert_eq!(unsafe { libc::kill(probe.child.id() as i32, signal) }, 0);
        }
        if !ignored && (!cut || blocked) {
            wait_for_file(
                &directory.join(if blocked {
                    "signal-observed"
                } else {
                    "cancel-observed"
                }),
                Instant::now() + Duration::from_secs(3),
            );
        }
        publish_audit_release(&probe.release, action).unwrap();
        if cut {
            wait_for_file(
                &directory.join("commit-cut"),
                Instant::now() + Duration::from_secs(3),
            );
            if !blocked {
                assert_eq!(unsafe { libc::kill(probe.child.id() as i32, signal) }, 0);
                wait_for_file(
                    &directory.join("commit-cancel-observed"),
                    Instant::now() + Duration::from_secs(3),
                );
            }
            publish_audit_release(&directory.join("cut-release"), b"resume").unwrap();
        }
        if blocked {
            if action == b"resume" && !cut {
                wait_for_file(
                    &directory.join("caller-committed"),
                    Instant::now() + Duration::from_secs(3),
                );
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while !directory.join("pending-observed").is_file()
                && probe.child.try_wait().unwrap().is_none()
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
            let observed = std::fs::read(directory.join("pending-observed")).unwrap_or_default();
            println!(
                "pending TERM before app unblocks: {:?}",
                String::from_utf8_lossy(&observed)
            );
            if observed != b"pending" {
                failures.push(format!("TERM was not pending before app code; release={action:?}, observed={observed:?}"));
            }
            if action == b"resume" && !cut {
                publish_audit_release(&directory.join("delivery-release"), b"resume").unwrap();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = probe.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "caller did not finish");
            thread::sleep(Duration::from_millis(10));
        };
        let committed = directory.join("committed").exists();
        println!(
            "startup cancellation: signal={signal}, ignored={ignored}, blocked={blocked}, cut={cut}, committed={committed}, status={status:?}"
        );
        let fallback = blocked && (action == b"fail" || cut);
        let expected_commit = ignored || (blocked && action == b"resume" && !cut);
        if committed != expected_commit
            || (ignored && !status.success())
            || (fallback && status.code() != Some(74))
            || (blocked && !fallback && status.signal() != Some(signal))
            || (!ignored && !blocked && status.signal() != Some(signal))
        {
            failures.push(format!("signal={signal}, ignored={ignored}, blocked={blocked}, cut={cut}, committed={committed}, status={status:?}; stderr={}", std::fs::read_to_string(&log).unwrap()));
        }
        // Wait for the owned one-use guardian to finish, then prove positive
        // reclamation before repeating in the same isolated workspace.
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_dir(paths.state_dir()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("macos-cargo-job-")
        }) {
            assert!(
                Instant::now() < deadline,
                "cancelled preparation was not retired"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let contexts = paths.managed_build_dirs();
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
            contexts.iter().all(|context| !context.exists()),
            "idle preparation context was not reclaimed"
        );
        for name in [
            "prepared",
            "cancel-observed",
            "signal-observed",
            "release",
            "committed",
            "caller-committed",
            "delivery-release",
            "pending-observed",
            "commit-cut",
            "commit-cancel-observed",
            "cut-release",
            "stderr",
        ] {
            match std::fs::remove_file(directory.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("audit cleanup: {error}"),
            }
        }
        std::fs::remove_dir(&directory).unwrap();
    }
    std::fs::write(source, original_source).unwrap();
    assert!(
        failures.is_empty(),
        "startup cancellation failures: {failures:#?}"
    );
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
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let _shutdown_daemon = ShutdownDaemon(paths.clone());
    if cfg!(debug_assertions) {
        cancellation_before_commit(&sandbox, &project, rgo);
    }
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
ready_pending = ready.with_name(f'{ready.name}.{os.getpid()}.pending')
ready_pending.write_text(json.dumps({'pid': os.getpid(), 'output': str(out)}))
os.replace(ready_pending, ready)
deadline = time.monotonic() + 90
while not release.exists() and time.monotonic() < deadline:
    time.sleep(0.02)
try:
    out.write_text('completed after Cargo exited')
    message = 'write succeeded'
except OSError as error:
    message = str(error)
# Existence is the reader's completion marker, so publish complete bytes.
result_pending = result.with_name(f'{result.name}.{os.getpid()}.pending')
result_pending.write_text(message)
os.replace(result_pending, result)
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
    std::fs::write(&receipt, &saved_receipt).unwrap();
    // Editing only the cleanup scope must invalidate the generated definition,
    // rather than redirect maintenance to a different, apparently idle context.
    let owner_path = job.directory.join("owner.json");
    let original_owner = std::fs::read(&owner_path).unwrap();
    let mut redirected: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
    redirected["context"] =
        serde_json::to_value(paths.builds_dir().join("aa/other-context")).unwrap();
    std::fs::write(&owner_path, serde_json::to_vec(&redirected).unwrap()).unwrap();
    assert!(rgo_core::macos_jobs::read_owner(&job.directory).is_err());
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
    assert!(contexts[0].is_dir() && job.directory.is_dir());
    assert_eq!(coalition.active_tasks().unwrap(), 1);
    std::fs::write(&owner_path, &original_owner).unwrap();
    // Earlier pilot schemas cannot establish this definition/scope binding.
    redirected = serde_json::from_slice(&original_owner).unwrap();
    redirected["version"] = serde_json::json!(2);
    let plist_path = job.directory.join("job.plist");
    let original_plist = std::fs::read(&plist_path).unwrap();
    let old_definition = redirected["definition"]
        .as_str()
        .unwrap()
        .replace("<key>LaunchOnlyOnce</key><true/>", "")
        .replace(
            &format!(
                "<string>--context</string><string>{}</string>",
                contexts[0].display()
            ),
            "",
        );
    redirected["definition"] = serde_json::json!(old_definition);
    std::fs::write(&plist_path, old_definition).unwrap();
    std::fs::write(&owner_path, serde_json::to_vec(&redirected).unwrap()).unwrap();
    assert!(rgo_core::macos_jobs::read_owner(&job.directory).is_err());
    // Schema 3 bound the context but still used label-based bootout. Its
    // exact historical definition cannot authorize the new retirement path.
    redirected = serde_json::from_slice(&original_owner).unwrap();
    redirected["version"] = serde_json::json!(3);
    let old_definition = redirected["definition"]
        .as_str()
        .unwrap()
        .replace("<key>LaunchOnlyOnce</key><true/>", "");
    redirected["definition"] = serde_json::json!(old_definition);
    std::fs::write(&plist_path, old_definition).unwrap();
    std::fs::write(&owner_path, serde_json::to_vec(&redirected).unwrap()).unwrap();
    assert!(rgo_core::macos_jobs::read_owner(&job.directory).is_err());
    std::fs::write(&owner_path, &original_owner).unwrap();
    std::fs::write(&plist_path, &original_plist).unwrap();
    // The killed one-use job removes its own registration while the detached
    // writer remains counted. A foreign replacement may now use this label.
    let deadline = Instant::now() + Duration::from_secs(10);
    while sandbox
        .cmd("launchctl")
        .args(["print", &job.target])
        .output()
        .unwrap()
        .status
        .success()
    {
        assert!(
            Instant::now() < deadline,
            "one-use registration remained after SIGKILL"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(coalition.active_tasks().unwrap(), 1);
    let foreign_pid = sandbox.home.join("foreign-job-pid");
    let foreign_plist = sandbox.home.join("foreign-job.plist");
    let foreign_program = sandbox.home.join("foreign-job.py");
    std::fs::write(&foreign_program, "import os, pathlib, sys, time\npathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\ntime.sleep(90)\n").unwrap();
    let foreign = format!(
        "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>/usr/bin/python3</string><string>{}</string><string>{}</string></array><key>RunAtLoad</key><true/></dict></plist>",
        owner["label"].as_str().unwrap(),
        foreign_program.display(),
        foreign_pid.display()
    );
    std::fs::write(&foreign_plist, foreign).unwrap();
    let loaded = sandbox
        .cmd("launchctl")
        .args(["bootstrap", owner["domain"].as_str().unwrap()])
        .arg(&foreign_plist)
        .output()
        .unwrap();
    assert!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    wait_for_file(&foreign_pid, Instant::now() + Duration::from_secs(10));
    let foreign_process = Process::observe(
        std::fs::read_to_string(&foreign_pid)
            .unwrap()
            .parse()
            .unwrap(),
    )
    .unwrap()
    .expect("foreign replacement exited before observation");
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
    while coalition.state().unwrap() != ResourceState::Reaped && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(coalition.state().unwrap(), ResourceState::Reaped);
    // Old receipt policies remain strict even for this positively observed,
    // now-reaped ID. Only the new policy can recognize kernel retirement.
    let mut legacy: serde_json::Value = serde_json::from_slice(&saved_receipt).unwrap();
    legacy["version"] = serde_json::json!(1);
    std::fs::write(&receipt, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert!(supervision::try_lock_gc(&paths, Some(&contexts[0])).is_err());
    assert!(contexts[0].is_dir());
    std::fs::write(&receipt, &saved_receipt).unwrap();
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
    let daemon_lock = stop_private_daemon(&paths);
    std::fs::write(&definition_path, &definition).unwrap();
    let (retiring_owner, retiring_bytes) =
        rgo_core::macos_jobs::read_owner(&job.directory).unwrap();
    let journal = job.directory.parent().unwrap().join(format!(
        ".retiring-{}.json",
        job.directory.file_name().unwrap().to_str().unwrap()
    ));
    let guard = supervision::try_lock_gc(&paths, Some(&contexts[0]))
        .unwrap()
        .unwrap();
    assert!(
        !rgo_core::macos_jobs::cleanup_step(&job.directory, &retiring_owner, &retiring_bytes, 2)
            .unwrap()
    );
    assert!(
        !job.directory.join("control.sock").exists() && !job.directory.join("owner.json").exists()
    );
    assert!(journal.is_file());
    let saved_journal = std::fs::read(&journal).unwrap();
    assert!(
        rgo_core::macos_jobs::read_owner(&job.directory)
            .err()
            .unwrap()
            .to_string()
            .contains("retirement is pending")
    );
    let mut changed_definition = definition.clone();
    changed_definition.extend_from_slice(b"<!-- later user edit -->");
    std::fs::write(&definition_path, changed_definition).unwrap();
    let edited = rgo_core::macos_jobs::cleanup(&job.directory, &retiring_owner, &retiring_bytes)
        .unwrap_err();
    assert!(edited.to_string().contains("content changed"), "{edited:#}");
    assert!(definition_path.is_file() && journal.is_file());
    std::fs::write(&definition_path, &definition).unwrap();
    drop(guard);
    drop(daemon_lock);
    let _first_daemon = PrivateDaemon(
        sandbox
            .cmd(rgo)
            .args(["daemon", "--foreground"])
            .env("RGO_DAEMON_POLL_SECS", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    // Metadata recovery also runs with automatic destructive GC disabled.
    let deadline = Instant::now() + Duration::from_secs(10);
    while (job.directory.exists() || journal.exists()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !job.directory.exists() && !journal.exists(),
        "idle crashed guardian was not recovered"
    );
    // Metadata cleanup never unloads a job by label: this replacement remains
    // alive after both the old directory and retirement journal disappear.
    assert!(foreign_process.state().unwrap() == ProcessState::Live);
    assert!(
        sandbox
            .cmd("launchctl")
            .args(["print", &job.target])
            .output()
            .unwrap()
            .status
            .success()
    );
    // Model interruption after directory removal but before journal unlink.
    // Replaying the exact journal also leaves the foreign job untouched.
    let guard = supervision::try_lock_gc(&paths, Some(&contexts[0]))
        .unwrap()
        .unwrap();
    std::fs::write(&journal, saved_journal).unwrap();
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
    rgo_core::macos_jobs::cleanup(&job.directory, &retiring_owner, &retiring_bytes).unwrap();
    assert!(!journal.exists());
    assert!(foreign_process.state().unwrap() == ProcessState::Live);
    drop(guard);
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
    let daemon_lock = stop_private_daemon(&paths);
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
