#![cfg(unix)]
use std::time::{Duration, Instant};
use rgo_testkit::Sandbox;
use rgo_core::{paths::RgoPaths, supervision};

#[test]
fn audit_detached_build_writer_with_closed_descriptors() {
    let sandbox = Sandbox::new().unwrap();
    let project = sandbox.simple_bin("closed-fd-audit").unwrap();
    let rgo = env!("CARGO_BIN_EXE_rgo");
    let cargo = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("cargo")).find(|p| p.is_file()).unwrap();
    let setup = sandbox.cmd(rgo).args(["setup", "--supervised", "--no-service", "--real-cargo"])
        .arg(cargo).output().unwrap();
    assert!(setup.status.success(), "{}", String::from_utf8_lossy(&setup.stderr));
    std::fs::write(project.join("worker.py"), r#"import json, os, pathlib, time
ready = pathlib.Path(os.environ['RGO_AUDIT_READY'])
release = pathlib.Path(os.environ['RGO_AUDIT_RELEASE'])
result = pathlib.Path(os.environ['RGO_AUDIT_RESULT'])
out = pathlib.Path(os.environ['OUT_DIR']) / 'late-build-output'
ready.write_text(json.dumps({'pid': os.getpid(), 'output': str(out)}))
deadline = time.monotonic() + 30
while not release.exists() and time.monotonic() < deadline:
    time.sleep(0.02)
try:
    out.write_text('completed after Cargo exited')
    result.write_text('write succeeded')
except OSError as error:
    result.write_text(str(error))
"#).unwrap();
    std::fs::write(project.join("launch.py"), r#"import os, pathlib, subprocess, sys, time
subprocess.Popen([sys.executable, 'worker.py'], stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
deadline = time.monotonic() + 10
while not pathlib.Path(os.environ['RGO_AUDIT_READY']).exists():
    if time.monotonic() > deadline: raise RuntimeError('worker did not start')
    time.sleep(0.02)
"#).unwrap();
    std::fs::write(project.join("build.rs"), "fn main() { assert!(std::process::Command::new(\"python3\").arg(\"launch.py\").status().unwrap().success()); }\n").unwrap();
    let ready = sandbox.home.join("ready.json");
    let release = sandbox.home.join("release");
    let result = sandbox.home.join("result");
    let search_path = std::env::join_paths(std::iter::once(sandbox.cargo_home.join("rgo/shims"))
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap();
    let build = sandbox.cmd("cargo").current_dir(&project).env("PATH", search_path)
        .env("RGO_AUDIT_READY", &ready).env("RGO_AUDIT_RELEASE", &release)
        .env("RGO_AUDIT_RESULT", &result).args(["build", "--offline"]).output().unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    let paths = RgoPaths { root: sandbox.rgo_home.clone() };
    let contexts = paths.managed_build_dirs();
    assert_eq!(contexts.len(), 1);
    let context = &contexts[0];
    let marker: serde_json::Value = serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
    assert!(std::path::Path::new(marker["output"].as_str().unwrap()).starts_with(context));
    let available = supervision::try_lock_gc(&paths, Some(context)).unwrap().is_some();
    let gc = sandbox.cmd(rgo).args(["gc", "--target", "0"]).output().unwrap();
    let deleted = !context.exists();
    std::fs::write(release, b"continue").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !result.is_file() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(20)); }
    let write_result = std::fs::read_to_string(result).unwrap();
    println!("guard_available={available}; context_deleted={deleted}; gc_success={}; late_write={write_result}", gc.status.success());
    assert!(available && deleted && gc.status.success(), "audit did not reproduce the assumed gap: {}", String::from_utf8_lossy(&gc.stderr));
    assert!(write_result.contains("No such file or directory"));
}
