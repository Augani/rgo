//! Wrapper-level coverage for the opt-in Phase 3 cache.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use assert_cmd::cargo::cargo_bin;
    use rgo_testkit::{Sandbox, ensure_workspace_bins_built};

    fn start_daemon(sb: &Sandbox) -> Child {
        let mut child = sb
            .cmd(cargo_bin("rgo"))
            .args(["daemon", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..80 {
            if sb.rgo_home.join("state/daemon.sock").exists() {
                return child;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not create its socket");
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
printf 'compile\n' >> "$FAKE_RUSTC_LOG"
if [ -n "$FAKE_RUSTC_SLEEP" ]; then sleep "$FAKE_RUSTC_SLEEP"; fi
if [ -n "$FAKE_RUSTC_FAIL" ]; then exit 1; fi
out=
name=demo
while [ "$#" -gt 0 ]; do
  case "$1" in
    --crate-name) name="$2"; shift 2 ;;
    --out-dir) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p "$out"
printf 'rlib:%s\n' "$name" > "$out/lib$name.rlib"
printf 'rmeta:%s\n' "$name" > "$out/lib$name.rmeta"
printf 'dep:%s\n' "$name" > "$out/$name.d"
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_rustc, fs::Permissions::from_mode(0o755)).unwrap();
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
        let mut command = sb.cmd(cargo_bin("rgo-rustc-wrapper"));
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
        let deps = context.join("release/deps");
        let mut files: Vec<_> = fs::read_dir(&deps)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("libdep_")
                    && !p.extension().is_some_and(|ext| ext == "d")
            })
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
        let mut command = sb.cmd(cargo_bin("rgo-rustc-wrapper"));
        command
            .env("FAKE_RUSTC_LOG", &fixture.log)
            .env("CARGO_MANIFEST_DIR", worktree)
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
            "concurrent identical keys should have one producer"
        );

        let first = command_for("first", None).output().unwrap();
        assert!(
            first.status.success(),
            "first failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let second = command_for("second", None).output().unwrap();
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
    fn hits_are_byte_identical_materially_faster_and_explained() {
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
        let hit = command_for(&sb, &fixture, "consumer", &[])
            .output()
            .unwrap();
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
    fn corrupt_cas_objects_quarantine_and_recover_by_recompiling() {
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
        assert!(corrupted > 0, "no CAS objects to corrupt");

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
        let waiter = command_for(&sb, &fixture, "waiter", &[]).output().unwrap();
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
        failing
            .env("FAKE_RUSTC_SLEEP", "1")
            .env("FAKE_RUSTC_FAIL", "1");
        let mut failing = failing.spawn().unwrap();
        wait_for_compiles(&fixture, 3);
        let waiter = command_for(&sb, &fixture, "fail-waiter", &["--crate-name", "failcrate"])
            .output()
            .unwrap();
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
        // wrapper is skipped entirely and equivalent worktrees share one compile.
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
            4,
            "equivalent worktree should hit rgo's workspace cache under sccache"
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
    fn workspace_remap_shares_hits_across_equivalent_worktrees() {
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

        // With remap_workspace_paths, the checkout location leaves the key. The key
        // itself changes when remap is enabled, so the first remapped build misses…
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

        // …and the equivalent worktree then hits that shared workspace key.
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
            3,
            "remapped worktree should hit the shared workspace key"
        );
        assert_eq!(
            outputs(&sb, "remapped-b"),
            outputs(&sb, "remapped-a"),
            "remapped hit outputs differ from a direct compile"
        );

        daemon.kill().unwrap();
        let _ = daemon.wait();
    }

    /// Performance gate: the bypassed wrapper must add only a small constant
    /// overhead over invoking the compiler directly — no daemon round trips,
    /// hashing, or classification work may run on the bypass path.
    #[test]
    fn bypassed_wrapper_overhead_stays_bounded() {
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
                    .cmd(cargo_bin("rgo-rustc-wrapper"))
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
    fn real_cargo_git_dependency_hits_reproduce_cold_compiles() {
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

        // Consumer: a third checkout must serve every dependency from the CAS —
        // including the build-script consumer and the proc-macro — reproducing the
        // publisher's exact bytes plus a working binary.
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
            published_artifacts,
            dep_artifacts(&ctx_c),
            "materialized hit outputs differ from the cold/published artifacts"
        );
        let hits = cache_events(&sb).matches("\"outcome\":\"hit\"").count();
        assert!(
            hits >= 5,
            "expected cache hits for all five dependencies; events: {}",
            cache_events(&sb)
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
    fn widened_classes_proc_macro_and_metadata_only_bin_are_cacheable() {
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
        let fixture = fixture(&sb);
        let mut daemon = start_daemon(&sb);

        let invoke = |context: &str, crate_type: &str, emit: &str| {
            let mut command = sb.cmd(cargo_bin("rgo-rustc-wrapper"));
            command
                .env("FAKE_RUSTC_LOG", &fixture.log)
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

        assert_eq!(
            compile_count(&fixture),
            2,
            "proc-macro and metadata-only bin second builds should hit"
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
