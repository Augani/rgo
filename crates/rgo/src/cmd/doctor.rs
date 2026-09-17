use std::path::Path;
use std::process::Command;

use anyhow::Result;
use rgo_core::adopt;
use rgo_core::cargo_config;
use rgo_core::config::volume_free_bytes;
use rgo_core::ipc;
use rgo_core::paths::cargo_home;
use rgo_core::service;
use rgo_protocol::{PROTOCOL_VERSION, Request, Response};

use super::{daemon, env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    let mut problems = 0;
    let mut check = |ok: bool, msg: String| {
        println!("{} {msg}", if ok { "ok  " } else { "WARN" });
        if !ok {
            problems += 1;
        }
    };

    let cfg_path = cargo_home()?.join("config.toml");
    let insp = cargo_config::inspect(&cargo_config::read_or_empty(&cfg_path)?)?;
    check(
        insp.has_fence,
        format!(
            "rgo fence present in {}; remediation: run `rgo setup`",
            cfg_path.display()
        ),
    );
    check(
        insp.build_dir_outside_fence.is_none(),
        format!(
            "no conflicting build.build-dir outside fence ({:?}); remediation: remove the override from {}",
            insp.build_dir_outside_fence,
            cfg_path.display()
        ),
    );
    check(
        insp.target_dir.is_none(),
        format!(
            "no global build.target-dir ({:?}); remediation: remove the override from {}",
            insp.target_dir,
            cfg_path.display()
        ),
    );
    if let Some(w) = &insp.rustc_wrapper {
        println!("info build.rustc-wrapper = {w:?}");
        let delegation = std::path::Path::new(w)
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("sccache"));
        println!(
            "info wrapper chain = rgo-rustc-wrapper -> {w} ({})",
            if delegation {
                "sccache overlap delegated; remapped workspace classes remain eligible"
            } else {
                "unknown inner wrapper; rgo cache bypasses conservatively"
            }
        );
    }
    for var in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        let v = std::env::var_os(var);
        check(
            v.is_none(),
            format!(
                "{var} not set in environment{}{}",
                v.as_ref()
                    .map(|v| format!(" (is {v:?}: overrides rgo)"))
                    .unwrap_or_default(),
                if v.is_some() {
                    "; remediation: unset it for rgo-managed builds"
                } else {
                    ""
                }
            ),
        );
    }
    check(
        e.paths.builds_dir().is_dir(),
        format!(
            "managed root exists: {}; remediation: run `rgo setup`",
            e.paths.builds_dir().display()
        ),
    );
    if e.cfg.remote.enabled {
        check(
            e.cfg.cache.enabled,
            "remote CAS requires cache.enabled = true; remediation: set `[cache].enabled = true` or disable remote CAS".into(),
        );
        check(
            !e.cfg.remote.endpoint.is_empty(),
            "remote endpoint is configured; remediation: set `[remote].endpoint`".into(),
        );
        check(
            !e.cfg.remote.namespace.is_empty(),
            "remote namespace is configured; remediation: set `[remote].namespace`".into(),
        );
        let token_available =
            std::env::var_os(&e.cfg.remote.token_env).is_some_and(|token| !token.is_empty());
        check(
            token_available,
            if token_available {
                format!(
                    "remote token is available through {}",
                    e.cfg.remote.token_env
                )
            } else {
                format!(
                    "remote token is unavailable through {}; remediation: export the configured token environment variable",
                    e.cfg.remote.token_env
                )
            },
        );
    }
    let daemon_ok = daemon::ensure_running(&e.paths);
    check(
        daemon_ok,
        format!(
            "daemon responds with protocol v{PROTOCOL_VERSION}; remediation: run `rgo daemon --foreground`"
        ),
    );
    if daemon_ok {
        if let Ok(Response::Status(status)) = ipc::request_with_timeout(
            &e.paths.socket_path(),
            Request::QueryStatus,
            std::time::Duration::from_secs(10),
        ) {
            println!(
                "info daemon pid {}: {} active lease(s), {} pinned context(s)",
                status.daemon_pid, status.active_leases, status.pinned_contexts
            );
            println!(
                "info last GC reclaimed {}",
                human(status.last_gc_reclaimed_bytes)
            );
            println!(
                "info cache {}: {} hit(s), {} miss(es), {} bypass(es), {} CAS",
                if status.cache.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.cache.hits,
                status.cache.misses,
                status.cache.bypasses,
                human(status.cache.cas_bytes)
            );
            println!(
                "info single-flight: {} active, {} producer(s), {} waiter(s), {} timeout(s), {} takeover(s)",
                status.cache.active_builds,
                status.cache.single_flight_producers,
                status.cache.single_flight_waiters,
                status.cache.single_flight_timeouts,
                status.cache.single_flight_takeovers
            );
            println!(
                "info workspace path remapping: {}",
                if e.cfg.cache.remap_workspace_paths {
                    "enabled (opt-in semantic change)"
                } else {
                    "disabled"
                }
            );
            println!(
                "info remote CAS: {} (healthy {}, queue {}, {} upload(s), {} download(s))",
                if status.remote.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.remote.healthy,
                status.remote.queue_depth,
                status.remote.uploads,
                status.remote.downloads
            );
            if let Some(error) = status.remote.last_error {
                let _ = error;
                println!("info remote last error: present (details omitted by doctor)");
            }
        }
    }
    let free = volume_free_bytes(&e.paths.root).unwrap_or(0);
    check(
        free >= e.cfg.min_free_space,
        format!(
            "volume free {} >= reserve {}{}",
            human(free),
            human(e.cfg.min_free_space),
            if free < e.cfg.min_free_space {
                "; remediation: free space or lower [storage].minimum-free-space"
            } else {
                ""
            }
        ),
    );
    println!(
        "info budget: hard {} / soft {}",
        human(e.cfg.max_size),
        human(e.cfg.soft_watermark)
    );
    check_toolchains(&mut check);
    check_filesystem(&e.paths.root, &mut check);
    match service::status(&std::env::current_exe()?) {
        Ok(status) if status.supported => check(
            status.installed && status.running,
            format!(
                "daemon service {} at {} (remediation: run `rgo setup`)",
                status.detail,
                status.location.display()
            ),
        ),
        Ok(status) => check(
            false,
            format!("daemon service unsupported: {}", status.detail),
        ),
        Err(error) => check(
            false,
            format!(
                "daemon service check failed: {error}; remediation: run `rgo setup --no-service`"
            ),
        ),
    }
    match adopt::scan(&adopt::default_roots()) {
        Ok(report) => {
            let candidates = report
                .candidates
                .iter()
                .filter(|candidate| candidate.eligible())
                .count();
            check(
                candidates == 0,
                format!(
                    "no reclaimable legacy target directories ({candidates} found; remediation: run `rgo adopt` then `rgo adopt --delete`)"
                ),
            );
        }
        Err(error) => check(
            false,
            format!(
                "legacy target scan failed: {error}; remediation: run `rgo adopt` with explicit roots"
            ),
        ),
    }

    if problems == 0 {
        println!("all good");
    } else {
        println!("{problems} warning(s)");
    }
    Ok(())
}

fn check_toolchains(check: &mut impl FnMut(bool, String)) {
    let names = Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|names| !names.is_empty())
        .unwrap_or_else(|| vec!["current".into()]);
    for name in names {
        let output = if name == "current" {
            Command::new("cargo").arg("--version").output()
        } else {
            Command::new("rustup")
                .args(["run", &name, "cargo", "--version"])
                .output()
        };
        let Ok(output) = output else {
            check(
                false,
                format!(
                    "toolchain {name} could not run cargo; remediation: install it with `rustup toolchain install {name}`"
                ),
            );
            continue;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let compatible = parse_version(&text).is_some_and(|version| version >= (1, 85, 0));
        check(
            compatible,
            format!(
                "toolchain {name}: {}build-dir compatible (requires Cargo >= 1.85.0){}",
                text.trim(),
                if compatible {
                    ""
                } else {
                    "; remediation: upgrade the toolchain or use `rgo setup --no-wrapper`"
                }
            ),
        );
    }
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))?
        .split('.')
        .take(3)
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u32>>>()
        .and_then(|parts| (parts.len() == 3).then(|| (parts[0], parts[1], parts[2])))
}

fn check_filesystem(root: &Path, check: &mut impl FnMut(bool, String)) {
    let base = if root.is_dir() {
        root.to_path_buf()
    } else {
        root.parent().unwrap_or(root).to_path_buf()
    };
    let probe = base.join(format!(".rgo-doctor-probe-{}", std::process::id()));
    let result = (|| -> anyhow::Result<(bool, bool, bool)> {
        std::fs::create_dir_all(&probe)?;
        let source = probe.join("source");
        let hard = probe.join("hardlink");
        let clone = probe.join("clone");
        std::fs::write(&source, b"rgo")?;
        let hard_links = std::fs::hard_link(&source, &hard).is_ok();
        let case_sensitive = {
            let lower = probe.join("case-probe");
            std::fs::write(&lower, b"case")?;
            !probe.join("CASE-PROBE").exists()
        };
        let clone_support = matches!(
            rgo_materialize::materialize(&source, &clone, 0o600, false),
            Ok(rgo_materialize::Strategy::CloneFile)
        );
        Ok((hard_links, clone_support, case_sensitive))
    })();
    let _ = std::fs::remove_dir_all(&probe);
    match result {
        Ok((hard_links, clone_support, case_sensitive)) => {
            check(
                hard_links,
                format!(
                    "hardlinks supported{}",
                    if hard_links {
                        ""
                    } else {
                        "; remediation: keep build and target roots on a filesystem supporting hardlinks"
                    }
                ),
            );
            check(
                clone_support || cfg!(not(target_os = "macos")),
                format!(
                    "reflink/clonefile capability probed: {}",
                    if clone_support {
                        "available"
                    } else {
                        "not available; copy fallback will be used"
                    }
                ),
            );
            println!(
                "info filesystem case sensitivity: {}",
                if case_sensitive {
                    "sensitive"
                } else {
                    "insensitive"
                }
            );
            println!(
                "info allocation accounting: {}",
                if cfg!(windows) {
                    "Windows allocation metadata"
                } else {
                    "filesystem allocated blocks"
                }
            );
        }
        Err(error) => check(
            false,
            format!(
                "filesystem capability probe failed: {error}; remediation: make {} writable",
                root.display()
            ),
        ),
    }
}
