//! Wrapper-level coverage for the opt-in Phase 3 cache.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, MutexGuard};
    use std::thread;
    use std::time::{Duration, Instant};

    use assert_cmd::cargo::cargo_bin;
    use rgo_core::db::StateDb;
    use rgo_core::ipc;
    use rgo_core::paths::RgoPaths;
    use rgo_protocol::{Request, Response};
    use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

    fn wrapper_bin() -> PathBuf {
        cargo_bin("rgo")
            .with_file_name(format!("rgo-rustc-wrapper{}", std::env::consts::EXE_SUFFIX))
    }

    // These end-to-end cases each launch a daemon and compiler processes. Running
    // separate sandboxes simultaneously can starve the short IPC timeout and
    // turn a correct single-flight hit into a load-dependent fallback compile.
    static E2E_LOCK: Mutex<()> = Mutex::new(());

    fn serial_e2e() -> MutexGuard<'static, ()> {
        E2E_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn start_daemon(sb: &Sandbox) -> Child {
        let mut child = sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if matches!(
                ipc::request_with_timeout(
                    &sb.rgo_home.join("state/daemon.sock"),
                    Request::QueryRemoteStatus,
                    Duration::from_millis(250),
                ),
                Ok(Response::RemoteStatus(_))
            ) {
                return child;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        let diagnostics = fs::read_to_string(sb.rgo_home.join("logs/daemon.log"))
            .unwrap_or_else(|error| format!("daemon log unavailable: {error}"));
        panic!("daemon did not answer IPC: {diagnostics}");
    }

    struct Fixture {
        fake_rustc: PathBuf,
        log: PathBuf,
        source: PathBuf,
        workspace: PathBuf,
    }

    /// Deterministic shell "rustc": logs each invocation to FAKE_RUSTC_LOG and writes
    /// predictable rlib/rmeta/dep-info outputs so hits can be byte-compared.
    fn fixture(sb: &Sandbox) -> Fixture {
        fs::write(sb.rgo_home.join("config.toml"), "[cache]\nenabled = true\n").unwrap();
        let source_root = sb.cargo_home.join("registry/src/index/demo-1.0.0");
        fs::create_dir_all(source_root.join("src")).unwrap();
        fs::write(source_root.join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        let source = source_root.join("src/lib.rs");
        fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();

        let workspace = sb.projects.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let fake_rustc = sb.projects.join("fake-rustc");
        let log = sb.projects.join("fake-rustc.log");
        fs::write(
            &fake_rustc,
            r#"#!/bin/sh
if [ "$1" = "-vV" ]; then
  printf 'rustc 1.85.0 (rgo fake)\n'
  exit 0
fi
if [ "$1" = "--print" ] && [ "$2" = "sysroot" ]; then
  printf '%s/fake-sysroot\n' "$(dirname "$0")"
  exit 0
fi
printf 'compile\n' >> "$FAKE_RUSTC_LOG"
fail_once=
if [ -n "$FAKE_RUSTC_FAIL_ONCE_FILE" ] && [ ! -e "$FAKE_RUSTC_FAIL_ONCE_FILE" ]; then
  : > "$FAKE_RUSTC_FAIL_ONCE_FILE"
  fail_once=1
fi
if [ -n "$FAKE_RUSTC_SLEEP" ]; then sleep "$FAKE_RUSTC_SLEEP"; fi
if [ -n "$fail_once" ]; then exit 1; fi
if [ -n "$FAKE_RUSTC_NO_OUTPUT" ]; then exit 0; fi
out=
name=demo
source=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --crate-name) name="$2"; shift 2 ;;
    --out-dir) out="$2"; shift 2 ;;
    *.rs) source="$1"; shift ;;
    *) shift ;;
  esac
done
mkdir -p "$out"
printf 'rlib:%s\n' "$name" > "$out/lib$name.rlib"
printf 'rmeta:%s\n' "$name" > "$out/lib$name.rmeta"
printf '%s: %s\n' "$name" "$source" > "$out/$name.d"
if [ -n "$FAKE_RUSTC_OLD_MTIME" ]; then touch -t 202001010000 "$out"/*; fi
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_rustc, fs::Permissions::from_mode(0o755)).unwrap();
        let fake_sysroot = sb.projects.join("fake-sysroot");
        fs::create_dir_all(fake_sysroot.join("bin")).unwrap();
        fs::create_dir_all(fake_sysroot.join("lib/rustlib")).unwrap();
        fs::write(
            fake_sysroot.join("lib/rustlib/multirust-channel-manifest.toml"),
            "fake test toolchain\n",
        )
        .unwrap();
        fs::copy(&fake_rustc, fake_sysroot.join("bin/rustc")).unwrap();
        Fixture {
            fake_rustc,
            log,
            source,
            workspace,
        }
    }

    fn out_dir(sb: &Sandbox, context: &str) -> PathBuf {
        let dir = sb
            .rgo_home
            .join("builds/aa")
            .join(context)
            .join("debug/deps");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn command_for(sb: &Sandbox, fixture: &Fixture, context: &str, extra_args: &[&str]) -> Command {
        let mut command = sb.cmd(wrapper_bin());
        command
            .env("FAKE_RUSTC_LOG", &fixture.log)
            .env("CARGO_MANIFEST_DIR", &fixture.workspace)
            .args([
                fixture.fake_rustc.as_os_str(),
                "--crate-name".as_ref(),
                "demo".as_ref(),
                "--crate-type=lib".as_ref(),
                "--emit=dep-info,metadata,link".as_ref(),
                "--out-dir".as_ref(),
                out_dir(sb, context).as_os_str(),
                fixture.source.as_os_str(),
            ])
            .args(extra_args);
        command
    }

    fn compile_count(fixture: &Fixture) -> usize {
        fs::read_to_string(&fixture.log)
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    fn outputs(sb: &Sandbox, context: &str) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = fs::read_dir(out_dir(sb, context))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect()
    }

    fn cache_events(sb: &Sandbox) -> String {
        fs::read_to_string(sb.rgo_home.join("state/cache-events.log")).unwrap_or_default()
    }

    /// Durable hit counter from the daemon DB (`rgo cache stats`), unlike the
    /// event log file which the daemon drains at maintenance ticks.
    fn cache_hits(sb: &Sandbox) -> u64 {
        let output = sb
            .cmd(cargo_bin("rgo"))
            .args(["cache", "stats"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| {
                line.strip_prefix("Cache hits")
                    .and_then(|rest| rest.trim().parse().ok())
            })
            .unwrap_or(0)
    }

    /// Block until the fake rustc has been invoked `count` times — i.e. the
    /// wrapper has secured the producer role and started compiling.
    fn wait_for_compiles(fixture: &Fixture, count: usize) {
        for _ in 0..100 {
            if compile_count(fixture) >= count {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "expected {count} compiles, log: {:?}",
            compile_count(fixture)
        );
    }

    /// A script that logs its own invocation then execs its arguments — a stand-in
    /// for third-party rustc wrappers such as sccache.
    fn inner_wrapper_fixture(sb: &Sandbox, name: &str) -> (PathBuf, PathBuf) {
        let script = sb.projects.join(name);
        let log = sb.projects.join(format!("{name}.log"));
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'inner\\n' >> \"$FAKE_INNER_LOG\"\nexec \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }

    /// Two identical single-crate package trees used as equivalent worktrees.
    fn worktree_sources(sb: &Sandbox, name: &str) -> PathBuf {
        let root = sb.projects.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"wsdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn ws() -> u32 { 7 }\n").unwrap();
        root
    }

    /// Every managed build context under `builds/<shard>/<ctx>` — used to
    /// attribute a new context to the build that just ran.
    fn contexts(sb: &Sandbox) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for shard in fs::read_dir(sb.rgo_home.join("builds"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for entry in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                if entry.path().is_dir() {
                    dirs.push(entry.path());
                }
            }
        }
        dirs
    }

    /// Loadable dependency artifacts under a context's `release/deps/`
    /// (rlib/rmeta/shared-library outputs, dep-info excluded since it embeds
    /// per-context paths by design).
    fn dep_artifacts(context: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        artifacts_prefixed(context, "libdep_")
    }

    /// Directories cargo may place unit artifacts in. Stable uses
    /// `release/deps/`; newer nightlies use the per-unit layout
    /// `release/build/<pkg>/<hash>/out/`. A one-level `build/<pkg>-<hash>/out/`
    /// is a build-script OUT_DIR on every toolchain, so only the two-level
    /// form is collected.
    fn artifact_dirs(context: &std::path::Path) -> Vec<PathBuf> {
        let release = context.join("release");
        let mut dirs = vec![release.join("deps")];
        for pkg in fs::read_dir(release.join("build"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for unit in fs::read_dir(pkg.path()).into_iter().flatten().flatten() {
                let out = unit.path().join("out");
                if out.is_dir() {
                    dirs.push(out);
                }
            }
        }
        dirs
    }

    /// `(name, bytes)` for every `{file_prefix}*` artifact in the context,
    /// sorted — dep-info `.d` files excluded (they embed paths).
    fn artifacts_prefixed(context: &std::path::Path, file_prefix: &str) -> Vec<(String, Vec<u8>)> {
        let mut files = std::collections::BTreeMap::new();
        for dir in artifact_dirs(context) {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = entry.path();
                if !p.is_file()
                    || !p
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(file_prefix)
                    || p.extension().is_some_and(|ext| ext == "d")
                {
                    continue;
                }
                files.insert(
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read(&p).unwrap(),
                );
            }
        }
        files.into_iter().collect()
    }

    /// The context dir created by the build that ran after `before` was taken.
    fn new_context(sb: &Sandbox, before: &[PathBuf]) -> PathBuf {
        contexts(sb)
            .into_iter()
            .find(|d| !before.contains(d))
            .expect("build created no build context")
    }

    fn workspace_command_for(
        sb: &Sandbox,
        fixture: &Fixture,
        worktree: &std::path::Path,
        context: &str,
    ) -> Command {
        let mut command = sb.cmd(wrapper_bin());
        command
            .env("FAKE_RUSTC_LOG", &fixture.log)
            .env("CARGO_MANIFEST_DIR", worktree)
            .env("RGO_MANIFEST_PATH", worktree.join("Cargo.toml"))
            .args([
                fixture.fake_rustc.as_os_str(),
                "--crate-name".as_ref(),
                "wsdemo".as_ref(),
                "--crate-type=lib".as_ref(),
                "--emit=dep-info,metadata,link".as_ref(),
                "--out-dir".as_ref(),
                out_dir(sb, context).as_os_str(),
                worktree.join("src/lib.rs").as_os_str(),
            ]);
        command
    }

    #[test]
    fn eligible_dependency_invocation_hits_across_build_contexts() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);
        let command_for = |context: &str, sleep: Option<&str>| {
            let mut command = command_for(&sb, &fixture, context, &[]);
            if let Some(sleep) = sleep {
                command.env("FAKE_RUSTC_SLEEP", sleep);
            }
            command
        };

        let first_child = command_for("flight-first", Some("1")).spawn().unwrap();
        let second_child = command_for("flight-second", Some("1")).spawn().unwrap();
        let first = first_child.wait_with_output().unwrap();
        let second = second_child.wait_with_output().unwrap();
        assert!(first.status.success());
        assert!(second.status.success());
        assert_eq!(
            compile_count(&fixture),
            1,
            "concurrent identical keys should have one producer; events: {}",
            cache_events(&sb)
        );

        let first = command_for("first", Some("1")).output().unwrap();
        assert!(
            first.status.success(),
            "first failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let second = command_for("second", Some("1")).output().unwrap();
        assert!(
            second.status.success(),
            "second failed: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        let manifests = fs::read_dir(sb.rgo_home.join("cas/manifests"))
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert_eq!(
            compile_count(&fixture),
            1,
            "second invocation should hit CAS: manifests={manifests} first={} second={}",
            String::from_utf8_lossy(&first.stderr),
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(
            fs::read(sb.rgo_home.join("builds/aa/second/debug/deps/libdemo.rlib")).unwrap(),
            b"rlib:demo\n"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn real_rustc_external_include_never_publishes_a_hit() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let external = sb.projects.join("outside-package.txt");
        fs::write(&external, "first").unwrap();
        fs::write(
            &fixture.source,
            format!(
                "pub const VALUE: &str = include_str!({:?});\n",
                external.to_string_lossy()
            ),
        )
        .unwrap();
        let mut daemon = start_daemon(&sb);
        let compile = |context: &str| {
            let output = sb
                .cmd(wrapper_bin())
                .env("CARGO_MANIFEST_DIR", &fixture.workspace)
                .arg("rustc")
                .args(["--crate-name", "demo", "--crate-type=lib"])
                .arg("--emit=dep-info,metadata,link")
                .arg("--out-dir")
                .arg(out_dir(&sb, context))
                .arg(&fixture.source)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "real rustc compile failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            fs::read(out_dir(&sb, context).join("libdemo.rlib")).unwrap()
        };
        let first = compile("external-first");
        fs::write(&external, "second").unwrap();
        let second = compile("external-second");
        assert_ne!(first, second, "external include edit must recompile");
        assert_eq!(cache_hits(&sb), 0);
        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn hits_are_byte_identical_materially_faster_and_explained() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);

        // A 3-second "compile" produces the CAS entry.
        let mut producer = command_for(&sb, &fixture, "producer", &[]);
        producer.env("FAKE_RUSTC_SLEEP", "3");
        let produced = producer.output().unwrap();
        assert!(
            produced.status.success(),
            "{}",
            String::from_utf8_lossy(&produced.stderr)
        );
        let expected = outputs(&sb, "producer");
        assert_eq!(expected.len(), 3, "rlib + rmeta + dep-info expected");

        // The hit must return all outputs byte-identical and faster than compiling.
        let started = std::time::Instant::now();
        let mut hit_command = command_for(&sb, &fixture, "consumer", &[]);
        hit_command.env("FAKE_RUSTC_SLEEP", "3");
        let hit = hit_command.output().unwrap();
        let elapsed = started.elapsed();
        assert!(
            hit.status.success(),
            "{}",
            String::from_utf8_lossy(&hit.stderr)
        );
        assert_eq!(compile_count(&fixture), 1, "consumer should have hit");
        assert_eq!(
            outputs(&sb, "consumer"),
            expected,
            "cache hit outputs differ from the produced outputs"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "cache hit took {elapsed:?}, not materially faster than a 3s compile"
        );
        assert!(
            cache_events(&sb).contains("\"outcome\":\"hit\""),
            "hit not recorded in cache-events.log: {}",
            cache_events(&sb)
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn publication_keeps_cache_and_context_leases_past_the_ttl() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let paths = RgoPaths {
            root: sb.rgo_home.clone(),
        };
        paths.ensure_layout().unwrap();
        fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
        fs::write(
            paths.state_dir().join("owner-cargo-home"),
            format!("{}\n", sb.cargo_home.display()),
        )
        .unwrap();
        let mut daemon = start_daemon(&sb);
        let pause = sb.projects.join("publication-pause");
        fs::create_dir_all(&pause).unwrap();
        let mut command = command_for(&sb, &fixture, "producer", &[]);
        command
            .env("RGO_TEST_CACHE_PUBLICATION_PAUSE", &pause)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !pause.join("ready").exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if !pause.join("ready").exists() {
            fs::write(pause.join("release"), b"").unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "wrapper never entered publication: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let lease_count = || {
            StateDb::open_read_only(&paths)?
                .stats()
                .map(|s| s.active_leases)
        };
        let mut lease_samples = vec![(0, lease_count())];
        for second in [12, 24, 35] {
            let previous = lease_samples.last().unwrap().0;
            thread::sleep(Duration::from_secs(second - previous));
            lease_samples.push((second, lease_count()));
        }
        let socket = sb.rgo_home.join("state/daemon.sock");
        let status =
            ipc::request_with_timeout(&socket, Request::QueryStatus, Duration::from_secs(5));
        let gc = sb
            .cmd(cargo_bin("rgo"))
            .args(["gc", "--target", "0"])
            .output()
            .unwrap();
        let context_survived = sb.rgo_home.join("builds/aa/producer").is_dir();
        fs::write(pause.join("release"), b"").unwrap();
        let output = child.wait_with_output().unwrap();
        let _ = daemon.kill();
        let _ = daemon.wait();

        let Response::Status(status) = status.unwrap() else {
            panic!("daemon did not return status during publication");
        };
        assert!(
            status.active_leases >= 2,
            "lease samples: {lease_samples:?}; status: {status:?}; wrapper stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            gc.status.success(),
            "{}",
            String::from_utf8_lossy(&gc.stderr)
        );
        assert!(context_survived, "GC removed an active compiler context");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_dir(sb.rgo_home.join("cas/manifests"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn restore_keeps_cache_and_context_leases_past_the_ttl() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let paths = RgoPaths {
            root: sb.rgo_home.clone(),
        };
        paths.ensure_layout().unwrap();
        fs::write(paths.state_dir().join("storage-mode"), b"supervised\n").unwrap();
        fs::write(
            paths.state_dir().join("owner-cargo-home"),
            format!("{}\n", sb.cargo_home.display()),
        )
        .unwrap();
        let mut daemon = start_daemon(&sb);
        let pause = sb.projects.join("restore-pause");
        fs::create_dir_all(&pause).unwrap();
        let produced = command_for(&sb, &fixture, "producer", &[])
            .env("RGO_TEST_CACHE_RESTORE_PAUSE", &pause)
            .output()
            .unwrap();
        assert!(
            produced.status.success(),
            "{}",
            String::from_utf8_lossy(&produced.stderr)
        );
        let mut command = command_for(&sb, &fixture, "consumer", &[]);
        command
            .env("RGO_TEST_CACHE_RESTORE_PAUSE", &pause)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !pause.join("ready").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if !pause.join("ready").exists() {
            fs::write(pause.join("release"), b"").unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "wrapper never entered restore: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        thread::sleep(Duration::from_secs(35));
        let active_leases = StateDb::open_read_only(&paths)
            .unwrap()
            .stats()
            .unwrap()
            .active_leases;
        let gc = sb
            .cmd(cargo_bin("rgo"))
            .args(["gc", "--target", "0"])
            .output()
            .unwrap();
        let context_survived = sb.rgo_home.join("builds/aa/consumer").is_dir();
        fs::write(pause.join("release"), b"").unwrap();
        let output = child.wait_with_output().unwrap();
        let _ = daemon.kill();
        let _ = daemon.wait();

        assert!(
            active_leases >= 2,
            "restore leases expired: {active_leases}"
        );
        assert!(
            gc.status.success(),
            "{}",
            String::from_utf8_lossy(&gc.stderr)
        );
        assert!(context_survived, "GC removed the active restore context");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(compile_count(&fixture), 1, "consumer should have hit");
        assert_eq!(outputs(&sb, "consumer"), outputs(&sb, "producer"));
    }

    #[test]
    fn daemon_loss_before_commit_does_not_publish_a_cache_hit() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);
        let pause = sb.projects.join("uncommitted-pause");
        fs::create_dir_all(&pause).unwrap();
        let mut command = command_for(&sb, &fixture, "producer", &[]);
        command
            .env("RGO_TEST_CACHE_PUBLICATION_PAUSE", &pause)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !pause.join("ready").exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if !pause.join("ready").exists() {
            fs::write(pause.join("release"), b"").unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "wrapper never entered publication: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        daemon.kill().unwrap();
        daemon.wait().unwrap();
        fs::write(pause.join("release"), b"").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(compile_count(&fixture), 1);
        assert_eq!(
            fs::read_dir(sb.rgo_home.join("cas/manifests"))
                .unwrap()
                .count(),
            0,
            "an unacknowledged producer must not expose a cache manifest"
        );

        let mut recovered = start_daemon(&sb);
        let second = command_for(&sb, &fixture, "consumer", &[])
            .output()
            .unwrap();
        let _ = recovered.kill();
        let _ = recovered.wait();
        assert!(
            second.status.success(),
            "{}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(compile_count(&fixture), 2, "the second build must compile");
    }

    #[test]
    fn corrupt_cas_objects_quarantine_and_recover_by_recompiling() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);

        let produced = command_for(&sb, &fixture, "producer", &[])
            .output()
            .unwrap();
        assert!(
            produced.status.success(),
            "{}",
            String::from_utf8_lossy(&produced.stderr)
        );
        assert_eq!(compile_count(&fixture), 1);

        // Corrupt every published object (not the manifests): the lookup still hits,
        // but integrity verification must reject the bytes before materializing.
        let objects = sb.rgo_home.join("cas/objects");
        let mut corrupted = 0;
        for shard in fs::read_dir(&objects).unwrap().flatten() {
            for entry in fs::read_dir(shard.path()).unwrap().flatten() {
                if entry.path().is_file() {
                    // Objects are published read-only; corruption arrives via the
                    // filesystem, so clear the mode before scribbling.
                    fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o644)).unwrap();
                    fs::write(entry.path(), b"corrupted-bytes").unwrap();
                    corrupted += 1;
                }
            }
        }
        assert!(
            corrupted > 0,
            "no CAS objects to corrupt; events: {}; daemon: {}",
            cache_events(&sb),
            fs::read_to_string(sb.rgo_home.join("logs/daemon.log")).unwrap_or_default(),
        );

        let recovered = command_for(&sb, &fixture, "consumer", &[])
            .output()
            .unwrap();
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            compile_count(&fixture),
            2,
            "corrupt objects must degrade to recompiling, not serve bad bytes"
        );
        assert_eq!(
            fs::read(out_dir(&sb, "consumer").join("libdemo.rlib")).unwrap(),
            b"rlib:demo\n"
        );
        let quarantined = fs::read_dir(sb.rgo_home.join("quarantine"))
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert!(quarantined > 0, "corrupt objects were not quarantined");

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn unwritable_cas_degrades_to_plain_compiles() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);

        // ENOSPC and EACCES reach the same write path: making the whole CAS tree
        // read-only forces every object/manifest store to fail like a full disk.
        let cas = sb.rgo_home.join("cas");
        for entry in fs::read_dir(&cas).unwrap().flatten() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o555)).unwrap();
        }
        fs::set_permissions(&cas, fs::Permissions::from_mode(0o555)).unwrap();

        let first = command_for(&sb, &fixture, "full-first", &[])
            .output()
            .unwrap();
        assert!(
            first.status.success(),
            "compile must succeed even when publishing fails: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(
            fs::read(out_dir(&sb, "full-first").join("libdemo.rlib")).unwrap(),
            b"rlib:demo\n"
        );
        let manifests = fs::read_dir(cas.join("manifests"))
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert_eq!(manifests, 0, "nothing may publish into a read-only CAS");

        // A second invocation still misses and still compiles — never hits bad
        // state and never fails the build.
        let second = command_for(&sb, &fixture, "full-second", &[])
            .output()
            .unwrap();
        assert!(second.status.success());
        assert_eq!(compile_count(&fixture), 2);

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn unsafe_invocations_always_compile_and_explain_the_bypass() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);

        // (extra args, expected bypass reason in cache-events.log)
        let cases: [(&[&str], &str); 4] = [
            (&["-Cincremental=yes"], "incremental"),
            (&["-Lnative=/tmp/rgo-native"], "native_input"),
            (&["-Ztime-passes"], "unstable_flag"),
            (&["--emit=asm"], "unsupported_emit"),
        ];
        for (index, (args, reason)) in cases.iter().enumerate() {
            for attempt in 0..2 {
                let context = format!("unsafe-{index}-{attempt}");
                let output = command_for(&sb, &fixture, &context, args).output().unwrap();
                assert!(
                    output.status.success(),
                    "{args:?} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                cache_events(&sb).contains(&format!("\"reason\":\"{reason}\"")),
                "bypass reason {reason} not explained in cache-events.log: {}",
                cache_events(&sb)
            );
        }
        assert_eq!(
            compile_count(&fixture),
            8,
            "unsafe invocations must compile every time, never hit"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn single_flight_timeout_and_producer_failure_fall_back_to_compiling() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        fs::write(
            sb.rgo_home.join("config.toml"),
            "[cache]\nenabled = true\nsingle_flight_timeout = \"1s\"\n",
        )
        .unwrap();
        let mut daemon = start_daemon(&sb);

        // A slow producer holds the key; the waiter gives up after the configured
        // timeout and compiles itself rather than hanging.
        let mut producer_cmd = command_for(&sb, &fixture, "slow-producer", &[]);
        producer_cmd.env("FAKE_RUSTC_SLEEP", "3");
        let mut producer = producer_cmd.spawn().unwrap();
        wait_for_compiles(&fixture, 1);
        let mut waiter_command = command_for(&sb, &fixture, "waiter", &[]);
        waiter_command.env("FAKE_RUSTC_SLEEP", "3");
        let waiter = waiter_command.output().unwrap();
        assert!(
            waiter.status.success(),
            "{}",
            String::from_utf8_lossy(&waiter.stderr)
        );
        let _ = producer.wait();
        assert!(
            cache_events(&sb).contains("\"reason\":\"single_flight_timeout\""),
            "waiter timeout not explained: {}",
            cache_events(&sb)
        );
        assert_eq!(compile_count(&fixture), 2);

        // A producer that fails must release the key so the waiter compiles.
        let mut failing = command_for(
            &sb,
            &fixture,
            "fail-producer",
            &["--crate-name", "failcrate"],
        );
        let failure_marker = sb.projects.join("failed-once");
        failing
            .env("FAKE_RUSTC_SLEEP", "1")
            .env("FAKE_RUSTC_FAIL_ONCE_FILE", &failure_marker);
        let mut failing = failing.spawn().unwrap();
        wait_for_compiles(&fixture, 3);
        let mut waiter_command =
            command_for(&sb, &fixture, "fail-waiter", &["--crate-name", "failcrate"]);
        waiter_command
            .env("FAKE_RUSTC_SLEEP", "1")
            .env("FAKE_RUSTC_FAIL_ONCE_FILE", &failure_marker);
        let waiter = waiter_command.output().unwrap();
        assert!(
            waiter.status.success(),
            "waiter must compile after producer failure: {}",
            String::from_utf8_lossy(&waiter.stderr)
        );
        let _ = failing.wait();
        assert_eq!(
            compile_count(&fixture),
            4,
            "failed producer plus recovered waiter should be two more compiles"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn inner_wrapper_composition_and_sccache_workspace_caching() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        fs::write(
            sb.rgo_home.join("config.toml"),
            "[cache]\nenabled = true\nremap_workspace_paths = true\n",
        )
        .unwrap();
        let mut daemon = start_daemon(&sb);

        // A non-sccache inner wrapper composes by passthrough: rgo records the
        // bypass and the inner wrapper still wraps every compile.
        let (inner, inner_log) = inner_wrapper_fixture(&sb, "third-party-wrapper");
        for attempt in 0..2 {
            let mut command = command_for(&sb, &fixture, &format!("inner-{attempt}"), &[]);
            command
                .env("RGO_INNER_RUSTC_WRAPPER", &inner)
                .env("FAKE_INNER_LOG", &inner_log);
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(compile_count(&fixture), 2, "inner-wrapped builds never hit");
        assert_eq!(
            fs::read_to_string(&inner_log).unwrap().lines().count(),
            2,
            "inner wrapper must wrap each passthrough compile"
        );
        assert!(
            cache_events(&sb).contains("\"reason\":\"inner_wrapper\""),
            "inner wrapper bypass not explained: {}",
            cache_events(&sb)
        );

        // sccache + registry source: also a passthrough (sccache owns dep caching).
        let (sccache, sccache_log) = inner_wrapper_fixture(&sb, "sccache");
        let mut command = command_for(&sb, &fixture, "sccache-registry", &[]);
        command
            .env("RGO_INNER_RUSTC_WRAPPER", &sccache)
            .env("FAKE_INNER_LOG", &sccache_log);
        assert!(command.output().unwrap().status.success());
        assert_eq!(compile_count(&fixture), 3);
        assert_eq!(fs::read_to_string(&sccache_log).unwrap().lines().count(), 1);

        // sccache + workspace source: rgo owns workspace caching, so the inner
        // wrapper is skipped. Different CARGO_MANIFEST_DIR values stay distinct
        // because user code can embed them through env!.
        let ws_a = worktree_sources(&sb, "ws-a");
        let ws_b = worktree_sources(&sb, "ws-b");
        let first = workspace_command_for(&sb, &fixture, &ws_a, "ws-a-ctx");
        let mut first = first;
        first
            .env("RGO_INNER_RUSTC_WRAPPER", &sccache)
            .env("FAKE_INNER_LOG", &sccache_log);
        let first = first.output().unwrap();
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(compile_count(&fixture), 4);

        let second = workspace_command_for(&sb, &fixture, &ws_b, "ws-b-ctx");
        let mut second = second;
        second
            .env("RGO_INNER_RUSTC_WRAPPER", &sccache)
            .env("FAKE_INNER_LOG", &sccache_log);
        let second = second.output().unwrap();
        assert!(
            second.status.success(),
            "{}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(
            compile_count(&fixture),
            5,
            "worktrees with different compile-time environments must not collide"
        );
        assert_eq!(
            fs::read_to_string(&sccache_log).unwrap().lines().count(),
            1,
            "sccache must not see workspace compiles rgo caches itself"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn workspace_remap_keeps_environment_dependent_worktrees_distinct() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);
        let ws_a = worktree_sources(&sb, "remap-a");
        let ws_b = worktree_sources(&sb, "remap-b");

        // Without remapping, the workspace path is part of the key: two checkouts
        // of identical content compile separately.
        let first = workspace_command_for(&sb, &fixture, &ws_a, "plain-a")
            .output()
            .unwrap();
        assert!(first.status.success());
        let second = workspace_command_for(&sb, &fixture, &ws_b, "plain-b")
            .output()
            .unwrap();
        assert!(second.status.success());
        assert_eq!(
            compile_count(&fixture),
            2,
            "un-remapped worktrees must miss"
        );

        // Remapping changes the key, so the first remapped build misses.
        fs::write(
            sb.rgo_home.join("config.toml"),
            "[cache]\nenabled = true\nremap_workspace_paths = true\n",
        )
        .unwrap();
        let third = workspace_command_for(&sb, &fixture, &ws_a, "remapped-a")
            .output()
            .unwrap();
        assert!(
            third.status.success(),
            "{}",
            String::from_utf8_lossy(&third.stderr)
        );
        assert_eq!(compile_count(&fixture), 3, "first remapped key is a miss");

        // CARGO_MANIFEST_DIR can still be embedded by env!, so the other
        // worktree needs a separate compile even with source remapping.
        let fourth = workspace_command_for(&sb, &fixture, &ws_b, "remapped-b")
            .output()
            .unwrap();
        assert!(
            fourth.status.success(),
            "{}",
            String::from_utf8_lossy(&fourth.stderr)
        );
        assert_eq!(
            compile_count(&fixture),
            4,
            "remapped worktrees with different environments must not collide"
        );
        let artifacts = |context| {
            outputs(&sb, context)
                .into_iter()
                .filter(|(name, _)| !name.ends_with(".d"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            artifacts("remapped-b"),
            artifacts("remapped-a"),
            "equivalent fixture compiles should produce the same artifacts"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    /// Performance gate: the bypassed wrapper must add only a small constant
    /// overhead over invoking the compiler directly — no daemon round trips,
    /// hashing, or classification work may run on the bypass path.
    #[test]
    fn bypassed_wrapper_overhead_stays_bounded() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        const RUNS: u32 = 20;

        let direct = (0..RUNS)
            .map(|_| {
                let started = std::time::Instant::now();
                let status = Command::new(&fixture.fake_rustc)
                    .arg("-vV")
                    .env("FAKE_RUSTC_LOG", &fixture.log)
                    .status()
                    .unwrap();
                assert!(status.success());
                started.elapsed()
            })
            .collect::<Vec<_>>();
        let wrapped = (0..RUNS)
            .map(|_| {
                let started = std::time::Instant::now();
                let status = sb
                    .cmd(wrapper_bin())
                    .env("RGO_BYPASS", "1")
                    .env("FAKE_RUSTC_LOG", &fixture.log)
                    .arg(&fixture.fake_rustc)
                    .arg("-vV")
                    .status()
                    .unwrap();
                assert!(status.success());
                started.elapsed()
            })
            .collect::<Vec<_>>();
        let avg = |runs: &[std::time::Duration]| {
            runs.iter().sum::<std::time::Duration>() / runs.len() as u32
        };
        let (direct, wrapped) = (avg(&direct), avg(&wrapped));
        assert!(
            wrapped <= direct + Duration::from_millis(100),
            "bypassed wrapper avg {wrapped:?} exceeds direct avg {direct:?} by >100ms"
        );
    }

    /// Real-Cargo differential corpus (scaled-down): three chained git
    /// dependencies compiled cold, cold-while-publishing, and via cache hits
    /// must produce byte-identical artifacts, and the hit-produced binary
    /// must behave identically.
    #[test]
    fn real_cargo_git_dependency_builds_match_bypass_control() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git unavailable; skipping real-cargo differential test");
            return;
        }
        let sb = Sandbox::new().unwrap();
        fixture(&sb); // enables [cache] in the sandbox config; fake rustc unused here
        let setup = sb
            .cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .output()
            .unwrap();
        assert!(
            setup.status.success(),
            "{}",
            String::from_utf8_lossy(&setup.stderr)
        );
        let mut daemon = start_daemon(&sb);

        let git = |dir: &std::path::Path, args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "rgo")
                .env("GIT_AUTHOR_EMAIL", "rgo@test")
                .env("GIT_COMMITTER_NAME", "rgo")
                .env("GIT_COMMITTER_EMAIL", "rgo@test")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        let dep_repo = |name: &str, manifest: &str, files: &[(&str, &str)]| {
            let dir = sb.projects.join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("Cargo.toml"), manifest).unwrap();
            for (path, body) in files {
                fs::write(dir.join(path), body).unwrap();
            }
            git(&dir, &["init", "-q"]);
            git(&dir, &["add", "-A"]);
            git(&dir, &["commit", "-qm", "init"]);
            format!("file://{}", dir.display())
        };
        let lib_manifest = |name: &str, deps: &str| {
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}"
            )
        };
        let url_a = dep_repo(
            "dep_a",
            &lib_manifest("dep_a", ""),
            &[("src/lib.rs", "pub fn a() -> u32 { 1 }\n")],
        );
        let url_b = dep_repo(
            "dep_b",
            &lib_manifest("dep_b", &format!("dep_a = {{ git = \"{url_a}\" }}\n")),
            &[("src/lib.rs", "pub fn b() -> u32 { dep_a::a() + 1 }\n")],
        );
        let url_c = dep_repo(
            "dep_c",
            &lib_manifest("dep_c", &format!("dep_b = {{ git = \"{url_b}\" }}\n")),
            &[("src/lib.rs", "pub fn c() -> u32 { dep_b::b() + 1 }\n")],
        );
        // Build-script consumer: its lib compile carries an OUT_DIR env whose
        // contents are digested into the key — the widened class exercised
        // end-to-end by real cargo.
        let url_build = dep_repo(
            "dep_build",
            &lib_manifest("dep_build", ""),
            &[
                (
                    "build.rs",
                    "fn main() {\n  let out = std::env::var(\"OUT_DIR\").unwrap();\n  std::fs::write(std::path::Path::new(&out).join(\"generated.rs\"), \"pub fn g() -> u32 { 40 }\\n\").unwrap();\n}\n",
                ),
                (
                    "src/lib.rs",
                    "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n",
                ),
            ],
        );
        // proc-macro crate: widened class compiled for the host toolchain.
        let url_proc = dep_repo(
            "dep_proc",
            "[package]\nname = \"dep_proc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n\n[dependencies]\n",
            &[(
                "src/lib.rs",
                "extern crate proc_macro;\nuse proc_macro::TokenStream;\n\n#[proc_macro]\npub fn m(_input: TokenStream) -> TokenStream {\n    \"fn hi() -> u32 { 9 }\".parse().unwrap()\n}\n",
            )],
        );
        let app = |name: &str| {
            let dir = sb.projects.join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep_c = {{ git = \"{url_c}\" }}\ndep_build = {{ git = \"{url_build}\" }}\ndep_proc = {{ git = \"{url_proc}\" }}\n"
                ),
            )
            .unwrap();
            fs::write(
                dir.join("src/main.rs"),
                "dep_proc::m!();\nfn main() { println!(\"answer={}\", dep_c::c() + dep_build::g() + hi()); }\n",
            )
            .unwrap();
            dir
        };

        // Cold reference: RGO_BYPASS compiles through the wrapper without
        // caching, still inside managed build-dirs.
        let app_a = app("diff-a");
        let before = contexts(&sb);
        let cold = sb
            .cargo()
            .env("RGO_BYPASS", "1")
            .current_dir(&app_a)
            .args(["build", "--release"])
            .output()
            .unwrap();
        assert!(
            cold.status.success(),
            "cold build failed: {}",
            String::from_utf8_lossy(&cold.stderr)
        );
        let ctx_a = new_context(&sb, &before);
        let cold_artifacts = dep_artifacts(&ctx_a);
        for expected in [
            "libdep_a",
            "libdep_b",
            "libdep_c",
            "libdep_build",
            "libdep_proc",
        ] {
            assert!(
                cold_artifacts
                    .iter()
                    .any(|(name, _)| name.starts_with(expected)),
                "{expected} artifact missing: {cold_artifacts:?}"
            );
        }

        // Publisher: identical dependencies in a second checkout compile cold
        // and populate the CAS.
        let app_b = app("diff-b");
        let before = contexts(&sb);
        let published = sb
            .cargo()
            .current_dir(&app_b)
            .args(["build", "--release"])
            .output()
            .unwrap();
        assert!(
            published.status.success(),
            "publisher build failed: {}",
            String::from_utf8_lossy(&published.stderr)
        );
        let ctx_b = new_context(&sb, &before);
        let published_artifacts = dep_artifacts(&ctx_b);

        // Path-free dependencies must be byte-identical across build contexts.
        // Two classes are legitimately context-dependent and excluded here:
        //   - dep_build embeds the per-context OUT_DIR path (include! of
        //     generated code) — its key digests OUT_DIR *contents* instead.
        //   - dep_proc's proc-macro dylib carries a linker-generated field
        //     (Mach-O UUID / per-build hash) so dylib bytes are never
        //     identical across compiles; a cache hit returns the producer's
        //     verified bytes verbatim, which is the correct equivalence.
        let path_free = |artifacts: &[(String, Vec<u8>)]| -> Vec<(String, Vec<u8>)> {
            artifacts
                .iter()
                .filter(|(name, _)| {
                    !name.starts_with("libdep_build") && !name.starts_with("libdep_proc")
                })
                .cloned()
                .collect()
        };
        assert_eq!(
            path_free(&cold_artifacts),
            path_free(&published_artifacts),
            "cold and published dependency artifacts diverged (nondeterminism or key drift)"
        );

        // Consumer: a third checkout may compile when its environment differs.
        // Path-free output remains comparable to the bypass control, and the
        // resulting binary must behave the same. Cache hits are deliberately
        // not required until dynamic input validation can recover safe reuse.
        let app_c = app("diff-c");
        let before = contexts(&sb);
        let hit = sb
            .cargo()
            .current_dir(&app_c)
            .args(["build", "--release"])
            .output()
            .unwrap();
        assert!(
            hit.status.success(),
            "consumer build failed: {}",
            String::from_utf8_lossy(&hit.stderr)
        );
        let ctx_c = new_context(&sb, &before);
        assert_eq!(
            path_free(&published_artifacts),
            path_free(&dep_artifacts(&ctx_c)),
            "path-free consumer artifacts diverged from the bypass control"
        );
        let binary = app_c.join("target/release/app");
        let run = sb.cmd(&binary).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            "answer=52",
            "hit-built binary behaved differently"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn real_cargo_cache_respects_arbitrary_compile_time_environment() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        fixture(&sb); // enable the optional cache only inside this private home
        let setup = sb
            .cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .output()
            .unwrap();
        assert!(
            setup.status.success(),
            "{}",
            String::from_utf8_lossy(&setup.stderr)
        );
        let mut daemon = start_daemon(&sb);

        let dependency = sb.projects.join("environment-dependency");
        fs::create_dir_all(dependency.join("src")).unwrap();
        fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname='environment_dependency'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            dependency.join("src/lib.rs"),
            "pub fn value() -> &'static str { env!(\"APP_BUILD_FLAVOR\") }\n",
        )
        .unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=rgo test",
                "-c",
                "user.email=rgo@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            let output = sb
                .cmd("git")
                .current_dir(&dependency)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        for (name, expected, bypass) in [
            ("publisher", "alpha", false),
            ("consumer", "beta", false),
            ("uncached-control", "beta", true),
        ] {
            let project = sb.projects.join(name);
            fs::create_dir_all(project.join("src")).unwrap();
            fs::write(
                project.join("Cargo.toml"),
                format!(
                    "[package]\nname='environment_probe'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nenvironment_dependency={{git='file://{}'}}\n",
                    dependency.display()
                ),
            )
            .unwrap();
            fs::write(
                project.join("src/main.rs"),
                "fn main() { println!(\"{}\", environment_dependency::value()); }\n",
            )
            .unwrap();
            let fetched = sb
                .cmd("cargo")
                .current_dir(&project)
                .arg("fetch")
                .output()
                .unwrap();
            assert!(
                fetched.status.success(),
                "{}",
                String::from_utf8_lossy(&fetched.stderr)
            );
            let mut build = sb.cargo();
            build
                .current_dir(&project)
                .env("APP_BUILD_FLAVOR", expected)
                .args(["build", "--offline"]);
            if bypass {
                build.env("RGO_BYPASS", "1");
            }
            let built = build.output().unwrap();
            assert!(
                built.status.success(),
                "{}",
                String::from_utf8_lossy(&built.stderr)
            );
            let executable = project.join(if cfg!(windows) {
                "target/debug/environment_probe.exe"
            } else {
                "target/debug/environment_probe"
            });
            let result = sb.cmd(&executable).output().unwrap();
            assert!(result.status.success());
            assert_eq!(
                String::from_utf8_lossy(&result.stdout).trim(),
                expected,
                "{name}"
            );
        }

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    /// Networked differential corpus — the scalable half of the Phase 3 gate.
    /// Pulls real registry crates through plain cargo in three contexts
    /// (bypassed cold reference, publisher, consumer) and asserts the hit
    /// reproduces the publisher's bytes plus a working binary.
    ///
    /// Disabled unless `RGO_CORPUS_ONLINE=1`. `RGO_CORPUS_CRATES="pkg=req,…"`
    /// overrides the crate list (env crates get build/hit/artifact checks but
    /// no behavioral probe), `RGO_CORPUS_TOOLCHAINS="stable,beta"` overrides
    /// the toolchain axis; the OS axis is wherever CI runs it.
    #[test]
    fn online_registry_corpus_hits_reproduce_publisher_outputs() {
        let _serial = serial_e2e();
        if std::env::var_os("RGO_CORPUS_ONLINE").is_none() {
            eprintln!("skipping networked corpus; set RGO_CORPUS_ONLINE=1 to run");
            return;
        }
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        fixture(&sb); // enables [cache] in the sandbox config; fake rustc unused here
        let setup = sb
            .cmd(cargo_bin("rgo"))
            .args(["setup", "--no-service"])
            .output()
            .unwrap();
        assert!(
            setup.status.success(),
            "{}",
            String::from_utf8_lossy(&setup.stderr)
        );
        let mut daemon = start_daemon(&sb);

        // (slug, dependency TOML lines, src/main.rs content, expected stdout)
        // Every entry's whole dep closure is asserted: all `lib*` outputs in
        // the consumer context must equal the publisher's bytes.
        let defaults: Vec<(&str, &str, &str, &str)> = vec![
            (
                "itoa",
                "itoa = \"1\"",
                "fn main() { print!(\"{}\", itoa::Buffer::new().format(421)); }\n",
                "421",
            ),
            (
                "memchr",
                "memchr = \"2\"",
                "fn main() { print!(\"{}\", memchr::memchr(b'z', b\"azb\").unwrap()); }\n",
                "1",
            ),
            (
                "semver",
                "semver = \"1\"",
                "fn main() { print!(\"{}\", semver::Version::parse(\"3.7.9\").unwrap().patch); }\n",
                "9",
            ),
            (
                "hex",
                "hex = \"0.4\"",
                "fn main() { print!(\"{}\", hex::encode([222u8, 173])); }\n",
                "dead",
            ),
            (
                "ryu",
                "ryu = \"1\"",
                "fn main() { print!(\"{}\", ryu::Buffer::new().format(1.25)); }\n",
                "1.25",
            ),
            (
                "unicode-width",
                "unicode-width = \"0.1\"",
                "fn main() { print!(\"{}\", unicode_width::UnicodeWidthStr::width(\"hello\")); }\n",
                "5",
            ),
            (
                "either",
                "either = \"1\"",
                "fn main() { print!(\"{}\", either::Either::<u8, u8>::Left(3).is_left()); }\n",
                "true",
            ),
            (
                "smallvec",
                "smallvec = \"1\"",
                "fn main() { print!(\"{}\", smallvec::SmallVec::<[u8; 4]>::from_slice(&[1, 2, 3]).len()); }\n",
                "3",
            ),
            (
                "percent-encoding",
                "percent-encoding = \"2\"",
                "fn main() { print!(\"{}\", percent_encoding::utf8_percent_encode(\"a b\", percent_encoding::NON_ALPHANUMERIC)); }\n",
                "a%20b",
            ),
            (
                "anyhow",
                "anyhow = \"1\"",
                "fn main() { print!(\"{}\", anyhow::anyhow!(\"x\")); }\n",
                "x",
            ),
            (
                "crc32fast",
                "crc32fast = \"1\"",
                "fn main() { print!(\"{}\", crc32fast::hash(b\"abc\")); }\n",
                "891568578",
            ),
            // Real proc-macro dependency closures — the classes fixed by the
            // bare-extern and output-ownership work, at registry scale.
            (
                "thiserror",
                "thiserror = \"2\"",
                "#[derive(thiserror::Error, Debug)]\n#[error(\"oops {0}\")]\nstruct E(u8);\nfn main() { print!(\"{}\", E(4)); }\n",
                "oops 4",
            ),
            (
                "serde",
                "serde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"",
                "#[derive(serde::Serialize)]\nstruct S { v: u8 }\nfn main() { print!(\"{}\", serde_json::to_string(&S { v: 3 }).unwrap()); }\n",
                "{\"v\":3}",
            ),
            // Breadth tier toward the 50-crate gate: small pure-Rust libs.
            (
                "cfg-if",
                "cfg-if = \"1\"",
                "cfg_if::cfg_if! { if #[cfg(any(unix, windows))] { fn main() { print!(\"y\"); } } else { fn main() { print!(\"n\"); } } }\n",
                "y",
            ),
            (
                "scopeguard",
                "scopeguard = \"1\"",
                "fn main() { let _g = scopeguard::guard((), |_| print!(\"x\")); }\n",
                "x",
            ),
            (
                "bitflags",
                "bitflags = \"2\"",
                "bitflags::bitflags! { struct F: u8 { const A = 1; } }\nfn main() { print!(\"{}\", F::A.bits()); }\n",
                "1",
            ),
            (
                "once_cell",
                "once_cell = \"1\"",
                "static C: once_cell::sync::Lazy<u8> = once_cell::sync::Lazy::new(|| 7);\nfn main() { print!(\"{}\", *C); }\n",
                "7",
            ),
            (
                "pin-project-lite",
                "pin-project-lite = \"0.2\"",
                "pin_project_lite::pin_project! { struct P { v: u8 } }\nfn main() { let _ = P { v: 5 }; print!(\"5\"); }\n",
                "5",
            ),
            (
                "log",
                "log = \"0.4\"",
                "fn main() { print!(\"{}\", log::Level::Info); }\n",
                "INFO",
            ),
            (
                "aho-corasick",
                "aho-corasick = \"1\"",
                "fn main() { let ac = aho_corasick::AhoCorasick::new([\"ab\"]).unwrap(); print!(\"{}\", ac.is_match(\"xabz\")); }\n",
                "true",
            ),
            (
                "base64",
                "base64 = \"0.22\"",
                "use base64::Engine;\nfn main() { print!(\"{}\", base64::engine::general_purpose::STANDARD.encode(b\"hi\")); }\n",
                "aGk=",
            ),
            (
                "itertools",
                "itertools = \"0.13\"",
                "fn main() { print!(\"{}\", itertools::join([1, 2], \",\")); }\n",
                "1,2",
            ),
            (
                "slab",
                "slab = \"0.4\"",
                "fn main() { let mut s = slab::Slab::new(); let k = s.insert(9u8); print!(\"{}\", s[k]); }\n",
                "9",
            ),
            (
                "bytes",
                "bytes = \"1\"",
                "fn main() { print!(\"{}\", bytes::Bytes::from_static(b\"hi\").len()); }\n",
                "2",
            ),
            (
                "byteorder",
                "byteorder = \"1\"",
                "use byteorder::{ByteOrder, LittleEndian};\nfn main() { let mut b = [0u8; 2]; LittleEndian::write_u16(&mut b, 258); print!(\"{}\", b[0]); }\n",
                "2",
            ),
            (
                "unicode-normalization",
                "unicode-normalization = \"0.1\"",
                "use unicode_normalization::UnicodeNormalization;\nfn main() { print!(\"{}\", \"\\u{e9}\".nfd().count()); }\n",
                "2",
            ),
            (
                "unicode-segmentation",
                "unicode-segmentation = \"1\"",
                "use unicode_segmentation::UnicodeSegmentation;\nfn main() { print!(\"{}\", \"ab\".graphemes(true).count()); }\n",
                "2",
            ),
            (
                "ordered-float",
                "ordered-float = \"4\"",
                "fn main() { print!(\"{}\", ordered_float::OrderedFloat(2.5) > ordered_float::OrderedFloat(1.0)); }\n",
                "true",
            ),
            (
                "strsim",
                "strsim = \"0.11\"",
                "fn main() { print!(\"{}\", strsim::levenshtein(\"ab\", \"ac\")); }\n",
                "1",
            ),
            (
                "heck",
                "heck = \"0.5\"",
                "use heck::ToSnakeCase;\nfn main() { print!(\"{}\", \"FooBar\".to_snake_case()); }\n",
                "foo_bar",
            ),
            (
                "termcolor",
                "termcolor = \"1\"",
                "fn main() { print!(\"{:?}\", termcolor::Color::Red); }\n",
                "Red",
            ),
            (
                "lazy_static",
                "lazy_static = \"1\"",
                "lazy_static::lazy_static! { static ref N: u8 = 4; }\nfn main() { print!(\"{}\", *N); }\n",
                "4",
            ),
            (
                "linked-hash-map",
                "linked-hash-map = \"0.5\"",
                "fn main() { let mut m = linked_hash_map::LinkedHashMap::new(); m.insert(1, 2); print!(\"{}\", m[&1]); }\n",
                "2",
            ),
            (
                "getrandom",
                "getrandom = \"0.3\"",
                "fn main() { let mut b = [0u8; 1]; getrandom::fill(&mut b).unwrap(); print!(\"ok\"); }\n",
                "ok",
            ),
            (
                "indexmap",
                "indexmap = \"2\"",
                "fn main() { let mut m = indexmap::IndexMap::new(); m.insert(\"a\", 1); print!(\"{}\", *m.get_index(0).unwrap().0); }\n",
                "a",
            ),
            (
                "uuid",
                "uuid = \"1\"",
                "fn main() { print!(\"{}\", uuid::Uuid::nil()); }\n",
                "00000000-0000-0000-0000-000000000000",
            ),
            (
                "parking_lot",
                "parking_lot = \"0.12\"",
                "fn main() { let m = parking_lot::Mutex::new(3u8); print!(\"{}\", *m.lock()); }\n",
                "3",
            ),
            // Build-script consumers and bigger real closures.
            (
                "libc",
                "libc = \"0.2\"",
                "fn main() { print!(\"{}\", libc::EXIT_SUCCESS); }\n",
                "0",
            ),
            (
                "num-traits",
                "num-traits = \"0.2\"",
                "fn main() { print!(\"{}\", num_traits::signum(-3)); }\n",
                "-1",
            ),
            (
                "chrono",
                "chrono = \"0.4\"",
                "use chrono::Datelike;\nfn main() { print!(\"{}\", chrono::NaiveDate::from_ymd_opt(2020, 1, 2).unwrap().day()); }\n",
                "2",
            ),
            (
                "flate2",
                "flate2 = \"1\"",
                "use std::io::Write;\nfn main() { let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6)); e.write_all(b\"ab\").unwrap(); print!(\"{}\", e.finish().unwrap().len() > 0); }\n",
                "true",
            ),
            (
                "rand",
                "rand = \"0.8\"",
                "use rand::{Rng, SeedableRng};\nfn main() { let mut r = rand::rngs::StdRng::seed_from_u64(1); print!(\"{}\", r.gen_range(0u8..10) < 10); }\n",
                "true",
            ),
            (
                "tracing",
                "tracing = \"0.1\"",
                "fn main() { print!(\"{}\", tracing::Level::INFO.as_str()); }\n",
                "INFO",
            ),
            (
                "pin-project",
                "pin-project = \"1\"",
                "use pin_project::pin_project;\n#[pin_project]\nstruct P { v: u8 }\nfn main() { let _ = P { v: 5 }; print!(\"5\"); }\n",
                "5",
            ),
            (
                "async-trait",
                "async-trait = \"0.1\"",
                "#[async_trait::async_trait]\ntrait T { async fn f(&self) -> u8; }\nstruct S;\n#[async_trait::async_trait]\nimpl T for S { async fn f(&self) -> u8 { 3 } }\nfn main() { print!(\"ok\"); }\n",
                "ok",
            ),
            (
                "regex",
                "regex = \"1\"",
                "fn main() { print!(\"{}\", regex::Regex::new(r\"^\\d+$\").unwrap().is_match(\"123\")); }\n",
                "true",
            ),
            (
                "toml",
                "toml = \"0.8\"",
                "fn main() { print!(\"{}\", toml::from_str::<toml::Value>(\"a=1\").unwrap()[\"a\"].as_integer().unwrap()); }\n",
                "1",
            ),
            (
                "rayon",
                "rayon = \"1\"",
                "use rayon::prelude::*;\nfn main() { print!(\"{}\", (0u32..4).into_par_iter().sum::<u32>()); }\n",
                "6",
            ),
            (
                "futures",
                "futures = \"0.3\"",
                "fn main() { print!(\"{}\", futures::executor::block_on(async { 6u8 })); }\n",
                "6",
            ),
            (
                "tokio",
                "tokio = { version = \"1\", features = [\"macros\", \"rt\"] }",
                "#[tokio::main(flavor = \"current_thread\")]\nasync fn main() { let v = async { 5u8 }.await; print!(\"{v}\"); }\n",
                "5",
            ),
            (
                "clap",
                "clap = { version = \"4\", features = [\"derive\"] }",
                "use clap::Parser;\n#[derive(clap::Parser)]\nstruct A { #[arg(long)] n: u8 }\nfn main() { let a = A::parse_from([\"a\", \"--n\", \"3\"]); print!(\"{}\", a.n); }\n",
                "3",
            ),
        ];
        struct CorpusEntry {
            slug: String,
            deps: String,
            main: String,
            expected: Option<String>,
        }
        let crates: Vec<CorpusEntry> = match std::env::var("RGO_CORPUS_CRATES") {
            Ok(list) => list
                .split(',')
                .map(|entry| {
                    let (pkg, req) = entry.split_once('=').unwrap_or_else(|| {
                        panic!("RGO_CORPUS_CRATES entry {entry:?} is not `pkg=version`")
                    });
                    CorpusEntry {
                        slug: pkg.to_owned(),
                        deps: format!("{pkg} = \"{req}\""),
                        main: "fn main() {}\n".to_owned(),
                        expected: None,
                    }
                })
                .collect(),
            Err(_) => defaults
                .into_iter()
                .map(|(slug, deps, main, expected)| CorpusEntry {
                    slug: slug.to_owned(),
                    deps: deps.to_owned(),
                    main: main.to_owned(),
                    expected: Some(expected.to_owned()),
                })
                .collect(),
        };
        let toolchains: Vec<String> = match std::env::var("RGO_CORPUS_TOOLCHAINS") {
            Ok(list) => list.split(',').map(str::to_owned).collect(),
            Err(_) => vec![std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "stable".into())],
        };

        for toolchain in &toolchains {
            for entry in &crates {
                let slug = format!("{toolchain}-{}", entry.slug).replace('.', "_");
                eprintln!("corpus: {slug}");
                let manifest = format!(
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{}\n",
                    entry.deps
                );
                let main = &entry.main;
                let app = |suffix: &str| {
                    let dir = sb.projects.join(format!("{slug}-{suffix}"));
                    fs::create_dir_all(dir.join("src")).unwrap();
                    fs::write(dir.join("Cargo.toml"), &manifest).unwrap();
                    fs::write(dir.join("src/main.rs"), main).unwrap();
                    dir
                };
                let build = |dir: &std::path::Path, bypass: bool| {
                    let mut command = sb.cargo();
                    command
                        .env("RUSTUP_TOOLCHAIN", toolchain)
                        .current_dir(dir)
                        .args(["build", "--release"]);
                    if bypass {
                        command.env("RGO_BYPASS", "1");
                    }
                    let output = command.output().unwrap();
                    assert!(
                        output.status.success(),
                        "{slug} build failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    output
                };

                // Whole dep closure: every `lib*` artifact the consumer's deps
                // dir holds must equal the publisher's bytes.
                let before = contexts(&sb);
                build(&app("cold"), true);
                let ctx_cold = new_context(&sb, &before);
                assert!(
                    !artifacts_prefixed(&ctx_cold, "lib").is_empty(),
                    "{slug}: cold build produced no dep artifacts"
                );

                let before = contexts(&sb);
                build(&app("pub"), false);
                let ctx_pub = new_context(&sb, &before);
                let published = artifacts_prefixed(&ctx_pub, "lib");

                // Hit fidelity: every artifact materialized via a cache hit must
                // reproduce the publisher's bytes verbatim. Missed crates may
                // legitimately recompile to different bytes (build-script
                // consumers embed per-context OUT_DIR paths; proc-macro dylibs
                // carry linker-generated ids), so whole-closure equality is not
                // a valid gate — the hit manifest names the outputs to check.
                let hits_before = cache_hits(&sb);
                let consumer = app("hit");
                build(&consumer, false);
                let ctx_hit = new_context(&sb, &before);
                assert!(
                    cache_hits(&sb) > hits_before,
                    "{slug}: consumer recorded no cache hit; events: {}",
                    cache_events(&sb)
                );
                // The event log is drained into the daemon DB at maintenance
                // ticks, so whatever hit keys remain in the file are a valid
                // (possibly partial) sample to check fidelity against.
                let hit_keys: Vec<String> = cache_events(&sb)
                    .lines()
                    .filter_map(|line| {
                        let v: serde_json::Value = serde_json::from_str(line).ok()?;
                        (v["outcome"] == "hit").then(|| v["key"].as_str().unwrap_or("").to_owned())
                    })
                    .collect();
                let find_output = |ctx: &std::path::Path, name: &str| {
                    artifact_dirs(ctx)
                        .into_iter()
                        .map(|dir| dir.join(name))
                        .find(|p| p.is_file())
                };
                for key in &hit_keys {
                    let manifest: serde_json::Value = serde_json::from_str(
                        &std::fs::read_to_string(
                            sb.rgo_home.join(format!("cas/manifests/{key}.json")),
                        )
                        .unwrap_or_else(|_| panic!("{slug}: manifest for hit key {key} missing")),
                    )
                    .unwrap();
                    for output in manifest["outputs"].as_array().unwrap() {
                        let name = output["name"].as_str().unwrap();
                        let (p, h) = (find_output(&ctx_pub, name), find_output(&ctx_hit, name));
                        if let (Some(p), Some(h)) = (p, h) {
                            assert_eq!(
                                std::fs::read(&p).unwrap(),
                                std::fs::read(&h).unwrap(),
                                "{slug}: hit output {name} diverged from publisher bytes"
                            );
                        }
                    }
                }
                // The consumer's dep-closure file set must match the publisher's.
                assert_eq!(
                    published.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                    artifacts_prefixed(&ctx_hit, "lib")
                        .iter()
                        .map(|(n, _)| n)
                        .collect::<Vec<_>>(),
                    "{slug}: hit consumer's dep closure differs from the publisher"
                );

                if let Some(expected) = &entry.expected {
                    let run = sb
                        .cmd(consumer.join("target/release/app"))
                        .output()
                        .unwrap();
                    assert_eq!(
                        String::from_utf8_lossy(&run.stdout).trim(),
                        *expected,
                        "{slug}: hit-built binary behaved differently"
                    );
                }

                // No-op rebuild must stay no-op: cargo prints no Compiling lines.
                let rebuild = build(&consumer, false);
                assert!(
                    !String::from_utf8_lossy(&rebuild.stderr).contains("Compiling"),
                    "{slug}: rebuild recompiled instead of staying fresh: {}",
                    String::from_utf8_lossy(&rebuild.stderr)
                );
            }
        }

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn widened_classes_proc_macro_and_metadata_only_bin_are_cacheable() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);

        let invoke = |context: &str, crate_type: &str, emit: &str| {
            let mut command = sb.cmd(wrapper_bin());
            command
                .env("FAKE_RUSTC_LOG", &fixture.log)
                .env("FAKE_RUSTC_OLD_MTIME", "1")
                .env("CARGO_MANIFEST_DIR", &fixture.workspace)
                .args([
                    fixture.fake_rustc.as_os_str(),
                    "--crate-name".as_ref(),
                    "demo".as_ref(),
                    format!("--crate-type={crate_type}").as_ref(),
                    format!("--emit={emit}").as_ref(),
                    "--out-dir".as_ref(),
                    out_dir(&sb, context).as_os_str(),
                    fixture.source.as_os_str(),
                ]);
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{crate_type}/{emit} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        // proc-macro crates are inside the v1 cacheable boundary.
        invoke("pm-first", "proc-macro", "dep-info,metadata,link");
        invoke("pm-second", "proc-macro", "dep-info,metadata,link");
        // metadata-only bins (cargo check on binaries) are too.
        invoke("bin-first", "bin", "dep-info,metadata");
        invoke("bin-second", "bin", "dep-info,metadata");

        let compiles = compile_count(&fixture);
        if compiles != 2 {
            let events = fs::read_to_string(sb.rgo_home.join("state/cache-events.log"))
                .unwrap_or_else(|error| format!("cache event log unavailable: {error}"));
            let status = sb.cmd(cargo_bin("rgo")).arg("status").output().unwrap();
            panic!(
                "proc-macro and metadata-only bin second builds should hit; compiled {compiles} times; cache events: {events}; status: {} {}",
                String::from_utf8_lossy(&status.stdout),
                String::from_utf8_lossy(&status.stderr)
            );
        }

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    #[test]
    fn unchanged_stale_output_is_never_published_as_a_compiler_result() {
        let _serial = serial_e2e();
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);
        let output_dir = out_dir(&sb, "stale-output");
        fs::write(output_dir.join("libdemo.rmeta"), b"stale bytes").unwrap();

        for _ in 0..2 {
            let result = command_for(&sb, &fixture, "stale-output", &[])
                .env("FAKE_RUSTC_NO_OUTPUT", "1")
                .output()
                .unwrap();
            assert!(result.status.success());
        }
        assert_eq!(compile_count(&fixture), 2);
        assert_eq!(
            fs::read_dir(sb.rgo_home.join("cas/manifests"))
                .unwrap()
                .count(),
            0,
            "rgo cached a pre-existing output that rustc did not write"
        );
        daemon.kill().unwrap();
        let _ = daemon.wait();
    }
}

#[cfg(not(unix))]
#[test]
fn cache_wrapper_integration_is_platform_specific() {
    // The portable CAS and protocol tests cover Windows; this shell-backed compiler fixture is
    // intentionally limited to Unix hosts.
}
