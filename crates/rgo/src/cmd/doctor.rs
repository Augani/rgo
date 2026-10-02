use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use rgo_core::adopt;
use rgo_core::cargo_config;
use rgo_core::config::volume_free_bytes;
use rgo_core::ipc;
use rgo_core::paths::cargo_home;
use rgo_core::service;
use rgo_protocol::{PROTOCOL_VERSION, Request, Response};
use serde::Serialize;

use super::{daemon, env, human};

#[derive(Default, Serialize)]
struct DoctorReport {
    schema_version: u32,
    warnings: usize,
    activation_verified: Option<bool>,
    profile_locks_observed: Option<bool>,
    entries: Vec<DoctorEntry>,
}

#[derive(Serialize)]
struct DoctorEntry {
    level: &'static str,
    message: String,
}

impl DoctorReport {
    fn check(&mut self, ok: bool, message: String) {
        if !ok {
            self.warnings += 1;
        }
        self.entries.push(DoctorEntry {
            level: if ok { "ok" } else { "warning" },
            message,
        });
    }

    fn info(&mut self, message: String) {
        self.entries.push(DoctorEntry {
            level: "info",
            message,
        });
    }

    fn print(&self, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(self)?);
        } else {
            for entry in &self.entries {
                println!(
                    "{} {}",
                    match entry.level {
                        "ok" => "ok  ",
                        "warning" => "WARN",
                        _ => "info",
                    },
                    entry.message
                );
            }
            if self.warnings == 0 {
                println!("all good");
            } else {
                println!("{} warning(s)", self.warnings);
            }
        }
        Ok(())
    }
}

pub fn run(json: bool, verify: bool) -> Result<()> {
    let e = env()?;
    let report = RefCell::new(DoctorReport {
        schema_version: 2,
        ..DoctorReport::default()
    });
    let mut check = |ok: bool, msg: String| report.borrow_mut().check(ok, msg);
    let mut info = |msg: String| report.borrow_mut().info(msg);
    let mut supervised_ready = true;

    let cargo_home = cargo_home()?;
    let cfg_path = cargo_config::effective_home_config(&cargo_home);
    let (insp, config_ok) = match cargo_config::read_or_empty(&cfg_path)
        .and_then(|text| cargo_config::inspect(&text))
    {
        Ok(inspection) => (inspection, true),
        Err(error) => {
            check(
                false,
                format!(
                    "cannot inspect Cargo home configuration {}: {error:#}; repair the file before activation",
                    cfg_path.display()
                ),
            );
            supervised_ready = false;
            (cargo_config::Inspection::default(), false)
        }
    };
    let supervised = std::fs::read(cargo_home.join(".rgo-install.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|record| record.get("supervised_cargo").cloned())
        .filter(|value| value.is_object());
    if insp.has_include {
        info("Cargo home config includes other files; rgo leaves any included wrapper settings in place and cannot infer their effective chain from the home file alone".into());
    }
    if let Some(mode) = &supervised {
        let included_build_dir = if insp.has_include {
            match cargo_config::may_set_build_dir_in_file(&cfg_path) {
                Ok(false) => {
                    info("included Cargo configuration has no build.build-dir; supervised storage can remain active".into());
                    false
                }
                Ok(true) => {
                    check(
                        false,
                        format!(
                            "included Cargo configuration from {} sets build.build-dir; direct Cargo may enter managed storage",
                            cfg_path.display()
                        ),
                    );
                    true
                }
                Err(error) => {
                    check(
                        false,
                        format!(
                            "cannot verify included Cargo configuration from {}: {error:#}",
                            cfg_path.display()
                        ),
                    );
                    true
                }
            }
        } else {
            false
        };
        check(
            config_ok && !insp.has_fence,
            format!(
                "no global rgo build-directory fence in {} (configuration must be readable)",
                cfg_path.display()
            ),
        );
        let shim = mode["shim_path"].as_str().map(PathBuf::from);
        let owned = shim.as_ref().is_some_and(|path| {
            let Ok(metadata) = std::fs::symlink_metadata(path) else {
                return false;
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return false;
            }
            let Some(contents) = mode["shim_contents"].as_str() else {
                return false;
            };
            let Ok(bytes) = std::fs::read(path) else {
                return false;
            };
            if let Some(digest) = contents.strip_prefix("binary-blake3:") {
                blake3::hash(&bytes).to_hex().as_str() == digest
            } else {
                bytes == contents.as_bytes()
            }
        });
        check(owned, "supervised Cargo launcher matches its installation record; remediation: rerun `rgo setup --supervised`".into());
        let active = shim.as_ref().is_some_and(|shim| {
            let Ok(owned_path) = shim.canonicalize() else {
                return false;
            };
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join(if cfg!(windows) { "cargo.exe" } else { "cargo" }))
                .find(|path| path.is_file())
                .and_then(|path| path.canonicalize().ok())
                .is_some_and(|path| path == owned_path)
        });
        supervised_ready = owned
            && active
            && config_ok
            && !insp.has_fence
            && !included_build_dir
            && insp.build_dir_outside_fence.is_none();
        check(
            active,
            format!(
                "Cargo on PATH resolves to the supervised launcher {}; remediation: put its directory before the real Cargo proxy on PATH",
                shim.as_deref().unwrap_or(Path::new("<missing>")).display()
            ),
        );
        info(format!(
            "real Cargo proxy: {}",
            mode["real_cargo"].as_str().unwrap_or("<missing>")
        ));
        if let Some(build_dir) = &insp.build_dir_outside_fence {
            check(
                false,
                format!(
                    "global build.build-dir = {build_dir:?} could let direct Cargo enter the managed namespace; remove it before supervised activation"
                ),
            );
        }
    } else {
        check(
            config_ok && insp.has_fence,
            format!(
                "rgo fence present in {}; remediation: run `rgo setup`",
                cfg_path.display()
            ),
        );
        check(
            config_ok
                && insp.configured_build_dir.as_deref()
                    == Some(e.paths.build_dir_template().as_str()),
            format!(
                "global build.build-dir = {:?} (expected {:?}); remediation: run `rgo setup`",
                insp.configured_build_dir,
                e.paths.build_dir_template()
            ),
        );
        check(
            config_ok && insp.build_dir_outside_fence.is_none(),
            format!(
                "no conflicting build.build-dir outside fence ({:?}); remediation: remove the override from {}",
                insp.build_dir_outside_fence,
                cfg_path.display()
            ),
        );
    }
    if let Some(target) = &insp.target_dir {
        info(format!(
            "global build.target-dir = {target:?}; this changes final outputs, not the configured build directory"
        ));
    }
    if insp.has_fence {
        let inner = e
            .paths
            .state_dir()
            .join("inner-wrapper")
            .canonicalize()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if let Some(outer) = &insp.configured_rustc_wrapper {
            info(format!(
                "wrapper chain: {} -> {}",
                outer,
                inner.as_deref().unwrap_or("rustc")
            ));
            check(
                std::path::Path::new(outer).is_file(),
                format!(
                    "configured wrapper exists at {outer:?}; remediation: reinstall the matched binaries, or run `rgo setup --undo` using any working rgo executable"
                ),
            );
        } else {
            info("wrapper chain: storage-only (no global compiler wrapper)".into());
        }
    }
    if let Some(w) = &insp.rustc_wrapper {
        info(format!("build.rustc-wrapper = {w:?}"));
        check(
            false,
            "existing build.rustc-wrapper takes precedence; rgo dependency caching is disconnected; remediation: remove it and run `rgo setup`, or keep it and use rgo for storage management only".into(),
        );
    }
    if let Some(w) = &insp.rustc_workspace_wrapper_outside_fence {
        check(
            false,
            format!(
                "existing build.rustc-workspace-wrapper = {w:?} prevents safe rgo wrapper installation; remediation: remove it and run `rgo setup`, or keep it and use rgo for storage management only"
            ),
        );
    }
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        info(format!(
            "CARGO_TARGET_DIR = {target:?}; this changes final outputs, not the configured build directory"
        ));
    }
    for var in [
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
    check_project_configs(&cargo_home, &mut check, &mut info);
    #[cfg(any(unix, windows))]
    if supervised.is_some() {
        match super::cargo_shim::compiler_override_issue(&e.paths, None) {
            Ok(Some(issue)) => check(
                false,
                format!(
                    "{issue}; supervised Cargo preserves the selected producer and uses ordinary Cargo storage"
                ),
            ),
            Ok(None) => {}
            Err(error) => check(
                false,
                format!(
                    "compiler settings cannot be verified: {error:#}; supervised Cargo uses ordinary Cargo storage"
                ),
            ),
        }
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
    let daemon_ok = daemon::is_available(&e.paths);
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
            info(format!(
                "daemon pid {}: {} active lease(s), {} pinned context(s)",
                status.daemon_pid, status.active_leases, status.pinned_contexts
            ));
            info(format!(
                "last GC reclaimed {}",
                human(status.last_gc_reclaimed_bytes)
            ));
            info(format!(
                "cache {}: {} hit(s), {} miss(es), {} bypass(es), {} CAS",
                if status.cache.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.cache.hits,
                status.cache.misses,
                status.cache.bypasses,
                human(status.cache.cas_bytes)
            ));
            info(format!(
                "single-flight: {} active, {} producer(s), {} waiter(s), {} timeout(s), {} takeover(s)",
                status.cache.active_builds,
                status.cache.single_flight_producers,
                status.cache.single_flight_waiters,
                status.cache.single_flight_timeouts,
                status.cache.single_flight_takeovers
            ));
            info(format!(
                "workspace path remapping: {}",
                if e.cfg.cache.remap_workspace_paths {
                    "enabled (opt-in semantic change)"
                } else {
                    "disabled"
                }
            ));
            info(format!(
                "remote CAS: {} (healthy {}, queue {}, {} upload(s), {} download(s))",
                if status.remote.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.remote.healthy,
                status.remote.queue_depth,
                status.remote.uploads,
                status.remote.downloads
            ));
            if let Some(error) = status.remote.last_error {
                let _ = error;
                info("remote last error: present (details omitted by doctor)".into());
            }
        }
    }
    if let Some(free) = volume_free_bytes(&e.paths.root) {
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
    } else {
        check(
            false,
            format!(
                "volume free unknown at {}; cannot verify the reserve",
                e.paths.root.display()
            ),
        );
    }
    info(format!(
        "budget: hard {} / soft {}",
        human(e.cfg.max_size),
        human(e.cfg.soft_watermark)
    ));
    match rgo_core::paths::check_local_cleanup_volume(&e.paths.root) {
        Ok(()) => info("cleanup volume: no known network-mount exclusion detected".into()),
        Err(error) => check(false, format!("cleanup volume is unsupported: {error:#}")),
    }
    let deletion_problem = e
        .paths
        .require_supervised_deletion()
        .err()
        .map(|error| format!("{error:#}"));
    let supervised_deletion = deletion_problem.is_none();
    check(
        e.cfg.gc.auto && supervised_deletion,
        if e.cfg.gc.auto && supervised_deletion {
            "automatic GC enabled (experimental lifecycle safety; active builds and pins can delay reclamation)".into()
        } else if e.cfg.gc.auto {
            format!(
                "automatic GC is configured but destructive cleanup is unavailable: {}",
                deletion_problem.as_deref().unwrap_or("unknown cause")
            )
        } else if supervised_deletion {
            "automatic GC disabled while the Cargo lifecycle safety gate remains open; manual `rgo gc` is available in supervised mode".into()
        } else {
            format!(
                "automatic GC disabled; destructive `rgo gc` and `rgo clean` are unavailable: {}",
                deletion_problem.as_deref().unwrap_or("unknown cause")
            )
        },
    );
    check_toolchains(
        supervised
            .as_ref()
            .and_then(|mode| mode["real_cargo"].as_str()),
        &mut check,
        &mut info,
    );
    if verify {
        check_filesystem(&e.paths.root, &mut check, &mut info);
    } else {
        info("filesystem hardlink, clone, and case-sensitivity probe deferred to `rgo doctor --verify`".into());
    }
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
                .filter(|candidate| candidate.has_storage())
                .count();
            info(format!(
                "legacy target directories with allocated files: {candidates} (inspect with `rgo adopt`; totals include final outputs)"
            ));
            check(
                report.skipped.is_empty(),
                format!(
                    "legacy target scan read all paths ({} skipped; remediation: run `rgo adopt` with explicit readable roots)",
                    report.skipped.len()
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

    let mut report = report.into_inner();
    if verify {
        let result = if supervised_ready {
            verify_plain_cargo(&e.paths, insp.configured_rustc_wrapper.as_deref())
        } else {
            Err(anyhow::anyhow!(
                "supervised Cargo launcher is not active and owned"
            ))
        };
        report.activation_verified = Some(result.is_ok());
        match &result {
            Ok(probe) => {
                report.profile_locks_observed = Some(probe.profile_locks_observed);
                report.check(
                    true,
                    "plain Cargo debug/release builds use one discoverable managed context; configured wrapper attribution was also verified when present".into(),
                );
                report.check(
                    probe.profile_locks_observed,
                    if probe.profile_locks_observed {
                        "active Cargo created documented debug/release profile build locks in the disposable project".into()
                    } else {
                        "active Cargo did not create both documented profile build locks in the disposable project; native lock-based cleanup is unverified for this toolchain".into()
                    },
                );
            }
            Err(error) => report.check(
                false,
                format!("plain Cargo activation probe failed: {error:#}"),
            ),
        }
        report.print(json)?;
        if let Err(error) = result {
            bail!("activation verification failed: {error:#}");
        }
        return Ok(());
    }
    report.print(json)
}

fn check_project_configs(
    cargo_home: &Path,
    check: &mut impl FnMut(bool, String),
    info: &mut impl FnMut(String),
) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            check(
                false,
                format!("cannot inspect Cargo project configuration: {error}"),
            );
            return;
        }
    };
    let cargo_home_identity = cargo_home.canonicalize().ok();
    let mut found = false;
    for directory in cwd.ancestors() {
        // Cargo searches `.cargo/config` before `.cargo/config.toml` at each
        // level, starting at the process working directory. This is also the
        // search used by the supervised launcher before it admits a context.
        let config_dir = directory.join(".cargo");
        if config_dir == cargo_home
            || cargo_home_identity
                .as_ref()
                .is_some_and(|home| config_dir.canonicalize().is_ok_and(|path| &path == home))
        {
            continue;
        }
        let path = cargo_config::effective_home_config(&config_dir);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                check(
                    false,
                    format!(
                        "cannot inspect Cargo project configuration {}: {error}",
                        path.display()
                    ),
                );
                continue;
            }
            Ok(_) => found = true,
        }
        match cargo_config::may_set_build_dir_in_file(&path) {
            Ok(true) => check(
                false,
                format!(
                    "project configuration {} may set build.build-dir; this working directory may use a different intermediate location than the Cargo-home setting (supervised Cargo leaves it unmanaged)",
                    path.display()
                ),
            ),
            Ok(false) => {}
            Err(error) => check(
                false,
                format!(
                    "cannot prove project configuration {} leaves build.build-dir unchanged: {error:#}; supervised Cargo leaves this working directory unmanaged",
                    path.display()
                ),
            ),
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(inspection) = cargo_config::inspect(&text) {
                if let Some(target) = inspection.target_dir {
                    info(format!(
                        "project configuration {} sets build.target-dir = {target:?}; this changes final outputs, not the build directory",
                        path.display()
                    ));
                }
                if let Some(wrapper) = inspection.rustc_wrapper {
                    info(format!(
                        "project configuration {} sets build.rustc-wrapper = {wrapper:?}; wrapper composition may differ here",
                        path.display()
                    ));
                }
                if let Some(wrapper) = inspection.rustc_workspace_wrapper_outside_fence {
                    info(format!(
                        "project configuration {} sets build.rustc-workspace-wrapper = {wrapper:?}; wrapper composition may differ here",
                        path.display()
                    ));
                }
            }
        }
    }
    if !found {
        info(format!(
            "no project Cargo configuration found from {} through its ancestors",
            cwd.display()
        ));
    }
}

pub(super) struct ActivationProbe {
    profile_locks_observed: bool,
}

pub(super) fn verify_plain_cargo(
    paths: &rgo_core::paths::RgoPaths,
    wrapper: Option<&str>,
) -> Result<ActivationProbe> {
    if let Some(wrapper) = wrapper {
        anyhow::ensure!(
            Path::new(wrapper).is_file(),
            "configured wrapper {wrapper:?} is missing"
        );
    }
    let probe = tempfile::tempdir()?;
    let root = probe.path();
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"rgo_activation_probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n")?;
    std::fs::write(root.join("build.rs"), "fn main() {}\n")?;
    let build_script_out = |release: bool| -> Result<std::path::PathBuf> {
        let mut command = Command::new("cargo");
        command.args(["build", "--offline", "--message-format=json"]);
        if release {
            command.arg("--release");
        }
        let output = command.current_dir(root).output()?;
        anyhow::ensure!(
            output.status.success(),
            "cargo build{} failed: {}",
            if release { " --release" } else { "" },
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let out_dir = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .find_map(|event| {
                (event["reason"] == "build-script-executed")
                    .then(|| event["out_dir"].as_str().map(str::to_owned))
                    .flatten()
            })
            .ok_or_else(|| anyhow::anyhow!("Cargo emitted no build-script output location"))?;
        Ok(std::fs::canonicalize(out_dir)?)
    };
    let debug_out = build_script_out(false)?;
    let release_out = build_script_out(true)?;
    let expected = std::fs::canonicalize(paths.builds_dir())?;
    anyhow::ensure!(
        debug_out.starts_with(&expected) && release_out.starts_with(&expected),
        "Cargo placed build-script output outside managed storage: debug={}, release={}, expected root={}",
        debug_out.display(),
        release_out.display(),
        expected.display()
    );
    // Two profiles must meet at the same build-dir. This uses Cargo's
    // documented profile split and emitted paths, without parsing Cargo's
    // private build/ or deps/ subdirectories or reimplementing its hash.
    let context = debug_out
        .ancestors()
        .find(|ancestor| release_out.starts_with(ancestor))
        .ok_or_else(|| anyhow::anyhow!("Cargo debug/release outputs have no common directory"))?;
    anyhow::ensure!(
        context != expected
            && paths
                .checked_managed_build_dirs()?
                .iter()
                .any(|dir| dir.canonicalize().is_ok_and(|dir| dir == context)),
        "Cargo's build-directory layout is not a discoverable rgo context: {}",
        context.display()
    );
    if wrapper.is_some() {
        let manifest = std::fs::canonicalize(root.join("Cargo.toml"))?;
        anyhow::ensure!(
            rgo_core::context::read_sidecar(context).is_some_and(|sidecar| {
                std::fs::canonicalize(&sidecar.manifest_path).is_ok_and(|path| path == manifest)
            }),
            "Cargo reached the managed build root but rgo's configured wrapper did not attribute this context"
        );
    }
    let profile_locks_observed = ["debug", "release"].iter().all(|profile| {
        std::fs::symlink_metadata(context.join(profile).join(".cargo-build-lock"))
            .is_ok_and(|metadata| metadata.file_type().is_file())
    });
    // This private, randomly named workspace has no remaining Cargo process.
    // Its checked context belongs only to the probe and should not consume
    // the user's budget or appear as an orphan after setup/doctor.
    if std::fs::remove_dir_all(context).is_ok() {
        // Cargo's workspace hash can create a parent shard. Remove it only
        // when empty, leaving any other managed context untouched.
        if let Some(shard) = context.parent().filter(|shard| *shard != expected) {
            let _ = std::fs::remove_dir(shard);
        }
    }
    Ok(ActivationProbe {
        profile_locks_observed,
    })
}

fn check_toolchains(
    real_cargo: Option<&str>,
    check: &mut impl FnMut(bool, String),
    info: &mut impl FnMut(String),
) {
    // In supervised mode, ask the recorded rustup proxy directly. Invoking
    // the shim just to print a version would create lifecycle lock files in
    // what is otherwise a read-only diagnostic command.
    let cargo = real_cargo.unwrap_or("cargo");
    match Command::new(cargo).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let banner = String::from_utf8_lossy(&output.stdout);
            let compatible = cargo_config::supports_build_dir(&banner);
            check(
                compatible,
                format!(
                    "active Cargo from this working directory: {} (build-dir {}supported; requires Cargo >= 1.91.0){}",
                    banner.trim(),
                    if compatible { "" } else { "not " },
                    if compatible {
                        ""
                    } else {
                        "; this toolchain uses ordinary local intermediates"
                    }
                ),
            );
        }
        Ok(output) => check(
            false,
            format!(
                "active Cargo at {cargo:?} could not report its version (exit {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(error) => check(
            false,
            format!("active Cargo at {cargo:?} cannot run: {error}"),
        ),
    }
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
        .unwrap_or_default();
    for name in names {
        let output = Command::new("rustup")
            .args(["run", &name, "cargo", "--version"])
            .output();
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => {
                info(format!(
                    "installed toolchain {name}: Cargo version unavailable"
                ));
                continue;
            }
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let compatible = cargo_config::supports_build_dir(&text);
        info(format!(
            "installed toolchain {name}: {}build-dir {}supported (requires Cargo >= 1.91.0)",
            text.trim(),
            if compatible { "" } else { "not " },
        ));
    }
}

fn check_filesystem(
    root: &Path,
    check: &mut impl FnMut(bool, String),
    info: &mut impl FnMut(String),
) {
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
            info(format!(
                "filesystem case sensitivity: {}",
                if case_sensitive {
                    "sensitive"
                } else {
                    "insensitive"
                }
            ));
            info(format!(
                "allocation accounting: {}",
                if cfg!(windows) {
                    "Windows allocation metadata"
                } else {
                    "filesystem allocated blocks"
                }
            ));
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

#[cfg(test)]
mod tests {
    use rgo_core::cargo_config;

    #[test]
    fn cargo_build_dir_boundary_includes_prerelease_banners() {
        for (banner, compatible) in [
            ("cargo 1.90.0 (abc 2025-01-01)", false),
            ("cargo 1.91.0 (abc 2025-01-01)", true),
            ("cargo 1.91.0-beta.1 (abc 2025-01-01)", true),
            ("cargo 1.100.0-nightly (abc 2026-09-01)", true),
        ] {
            assert_eq!(
                cargo_config::supports_build_dir(banner),
                compatible,
                "{banner}"
            );
        }
    }
}
