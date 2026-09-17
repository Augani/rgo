use anyhow::Result;
use rgo_core::cargo_config;
use rgo_core::config::volume_free_bytes;
use rgo_core::ipc;
use rgo_core::paths::cargo_home;
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
        format!("rgo fence present in {}", cfg_path.display()),
    );
    check(
        insp.build_dir_outside_fence.is_none(),
        format!(
            "no conflicting build.build-dir outside fence ({:?})",
            insp.build_dir_outside_fence
        ),
    );
    check(
        insp.target_dir.is_none(),
        format!("no global build.target-dir ({:?})", insp.target_dir),
    );
    if let Some(w) = &insp.rustc_wrapper {
        println!("info build.rustc-wrapper = {w:?}");
        println!("info wrapper chain = rgo-rustc-wrapper -> {w}");
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
                "{var} not set in environment{}",
                v.map(|v| format!(" (is {v:?}: overrides rgo)"))
                    .unwrap_or_default()
            ),
        );
    }
    check(
        e.paths.builds_dir().is_dir(),
        format!("managed root exists: {}", e.paths.builds_dir().display()),
    );
    let daemon_ok = daemon::ensure_running(&e.paths);
    check(
        daemon_ok,
        format!("daemon responds with protocol v{PROTOCOL_VERSION}"),
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
        }
    }
    let free = volume_free_bytes(&e.paths.root).unwrap_or(0);
    check(
        free >= e.cfg.min_free_space,
        format!(
            "volume free {} >= reserve {}",
            human(free),
            human(e.cfg.min_free_space)
        ),
    );
    println!(
        "info budget: hard {} / soft {}",
        human(e.cfg.max_size),
        human(e.cfg.soft_watermark)
    );
    // TODO(phase 1): per-toolchain `cargo --version` >= build-dir stabilization; fs reflink capability probe;
    //                service health; stray target/ scan via `rgo adopt`.

    if problems == 0 {
        println!("all good");
    } else {
        println!("{problems} warning(s)");
    }
    Ok(())
}
