#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rgo_core::macos_coalition::ResourceCoalition;
use rgo_core::paths::RgoPaths;
use rgo_core::supervision;
use rgo_testkit::Sandbox;

struct ProbeJob<'a> {
    sandbox: &'a Sandbox,
    target: String,
    release: PathBuf,
    registered: bool,
}

impl Drop for ProbeJob<'_> {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"continue");
        if self.registered {
            let _ = self
                .sandbox
                .cmd("launchctl")
                .args(["bootout", &self.target])
                .output();
        }
    }
}

fn wait_for_file(path: &std::path::Path, deadline: Instant) {
    while !path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.is_file(), "probe did not publish {}", path.display());
}

#[test]
fn launchd_coalition_observes_closed_fd_writer_after_cargo_exits() {
    // Run this same test binary as the isolated launchd job. Register before
    // exec'ing the owned Cargo shim; the receipt must survive both parent exits
    // and the writer closing all inherited guard descriptors.
    if std::env::var_os("RGO_COALITION_PROBE_HELPER").is_some() {
        use std::os::unix::process::CommandExt;
        let paths = RgoPaths::discover().unwrap();
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        let context = supervision::context_for_workspace(&paths, &root).unwrap();
        let session = supervision::lock_cargo_session(&paths, Some(&context)).unwrap();
        session.record_macos_coalition().unwrap();
        session.retain_across_exec().unwrap();
        let error = std::process::Command::new(std::env::var_os("RGO_COALITION_CARGO").unwrap())
            .args(["build", "--offline"])
            .exec();
        panic!("could not exec private Cargo: {error}");
    }
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
    let python = sandbox
        .cmd("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let python = String::from_utf8(python.stdout).unwrap().trim().to_owned();
    assert!(std::path::Path::new(&python).is_absolute());
    let ready = sandbox.home.join("writer.json");
    let release = sandbox.home.join("release-writer");
    let result = sandbox.home.join("writer-result");
    let finished = sandbox.home.join("cargo-result");
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
    let runner = sandbox.home.join("job.py");
    std::fs::write(&runner, "import os, pathlib, subprocess\nresult = subprocess.run([os.environ['RGO_COALITION_HELPER'], '--exact', 'launchd_coalition_observes_closed_fd_writer_after_cargo_exits', '--nocapture'])\npathlib.Path(os.environ['RGO_COALITION_FINISHED']).write_text(str(result.returncode))\nraise SystemExit(result.returncode)\n").unwrap();
    let search_path = std::env::join_paths(
        std::iter::once(sandbox.cargo_home.join("rgo/shims"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let label = format!("com.rgo.probe.coalition.{}.{nonce}", std::process::id());
    let uid = sandbox.cmd("id").arg("-u").output().unwrap();
    assert!(uid.status.success());
    let domain = format!("gui/{}", String::from_utf8(uid.stdout).unwrap().trim());
    let definition = sandbox.home.join("job.plist");
    let payload = serde_json::json!({
        "Label": label,
        "ProgramArguments": [python, runner],
        "WorkingDirectory": project,
        "EnvironmentVariables": {
            "HOME": sandbox.home, "CARGO_HOME": sandbox.cargo_home, "RGO_HOME": sandbox.rgo_home,
            "PATH": search_path.to_str().unwrap(),
            "RUSTUP_HOME": std::env::var_os("RUSTUP_HOME").map(PathBuf::from)
                .unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap().join(".rustup")),
            "RUSTUP_TOOLCHAIN": "stable",
            "RGO_COALITION_CARGO": sandbox.cargo_home.join("rgo/shims/cargo"),
            "RGO_COALITION_HELPER": std::env::current_exe().unwrap(),
            "RGO_COALITION_PROBE_HELPER": "1",
            "RGO_COALITION_READY": ready, "RGO_COALITION_RELEASE": release,
            "RGO_COALITION_RESULT": result, "RGO_COALITION_FINISHED": finished,
        },
        "RunAtLoad": true,
        "AbandonProcessGroup": true,
        "StandardOutPath": sandbox.home.join("job.stdout"),
        "StandardErrorPath": sandbox.home.join("job.stderr"),
    });
    let plist = sandbox.cmd("python3").args(["-c", "import json, plistlib, sys; sys.stdout.buffer.write(plistlib.dumps(json.loads(sys.argv[1])))"])
        .arg(payload.to_string()).output().unwrap();
    assert!(
        plist.status.success(),
        "{}",
        String::from_utf8_lossy(&plist.stderr)
    );
    std::fs::write(&definition, plist.stdout).unwrap();
    let mut job = ProbeJob {
        sandbox: &sandbox,
        target: format!("{domain}/{label}"),
        release: release.clone(),
        registered: false,
    };
    let bootstrap = sandbox
        .cmd("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&definition)
        .output()
        .unwrap();
    assert!(
        bootstrap.status.success(),
        "{}",
        String::from_utf8_lossy(&bootstrap.stderr)
    );
    job.registered = true;
    let deadline = Instant::now() + Duration::from_secs(90);
    wait_for_file(&finished, deadline);
    assert_eq!(
        std::fs::read_to_string(&finished).unwrap(),
        "0",
        "{}",
        std::fs::read_to_string(sandbox.home.join("job.stderr")).unwrap_or_default()
    );
    assert!(
        ready.is_file(),
        "successful Cargo did not publish the detached writer"
    );
    let writer: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    let writer_pid = i32::try_from(writer["pid"].as_i64().unwrap()).unwrap();
    let coalition = ResourceCoalition::for_pid(writer_pid).unwrap();
    let host = ResourceCoalition::for_pid(std::process::id() as i32).unwrap();
    assert_ne!(
        coalition.id(),
        host.id(),
        "job inherited the host application's coalition"
    );
    let paths = RgoPaths {
        root: sandbox.rgo_home.clone(),
    };
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    assert!(std::path::Path::new(writer["output"].as_str().unwrap()).starts_with(&contexts[0]));
    let deadline = Instant::now() + Duration::from_secs(10);
    while coalition.active_tasks().unwrap() != 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        coalition.active_tasks().unwrap(),
        1,
        "exited Cargo/job parents did not leave an independently tracked writer"
    );
    assert!(
        supervision::try_lock_gc(&paths, Some(&contexts[0]))
            .unwrap()
            .is_none(),
        "closed descriptors released cleanup while the coalition still had a writer"
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
    assert!(contexts[0].is_dir());
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
            .is_some(),
        "zero-task coalition did not release cleanup"
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
        "isolated resource coalition {}: closed-FD writer protected from real GC after Cargo exit; successful write; idle context reclaimed after writer exit",
        coalition.id()
    );
}
