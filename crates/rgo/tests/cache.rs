//! Wrapper-level coverage for the opt-in Phase 3 cache.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Stdio};
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

    #[test]
    fn eligible_dependency_invocation_hits_across_build_contexts() {
        ensure_workspace_bins_built().unwrap();
        let sb = Sandbox::new().unwrap();
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

        let mut daemon = start_daemon(&sb);
        let wrapper = cargo_bin("rgo-rustc-wrapper");
        let invoke = |context: &str| {
            let out_dir = sb
                .rgo_home
                .join("builds/aa")
                .join(context)
                .join("debug/deps");
            fs::create_dir_all(&out_dir).unwrap();
            let mut command = sb.cmd(wrapper.clone());
            command
                .env("FAKE_RUSTC_LOG", &log)
                .env("CARGO_MANIFEST_DIR", &workspace)
                .args([
                    fake_rustc.as_os_str(),
                    "--crate-name".as_ref(),
                    "demo".as_ref(),
                    "--crate-type=lib".as_ref(),
                    "--emit=dep-info,metadata,link".as_ref(),
                    "--out-dir".as_ref(),
                    out_dir.as_os_str(),
                    source.as_os_str(),
                ]);
            command.output().unwrap()
        };

        let first = invoke("first");
        assert!(
            first.status.success(),
            "first failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let second = invoke("second");
        assert!(
            second.status.success(),
            "second failed: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        let manifests = fs::read_dir(sb.rgo_home.join("cas/manifests"))
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert_eq!(
            fs::read_to_string(&log).unwrap().lines().count(),
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
}

#[cfg(not(unix))]
#[test]
fn cache_wrapper_integration_is_platform_specific() {
    // The portable CAS and protocol tests cover Windows; this shell-backed compiler fixture is
    // intentionally limited to Unix hosts.
}
