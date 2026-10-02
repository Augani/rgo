//! Pilot launcher for full-lifetime supervision of unchanged Cargo commands.
//! The real Cargo path is explicit so it cannot recurse through a PATH shim.

#[cfg(any(unix, windows))]
use std::ffi::{OsStr, OsString};
#[cfg(any(unix, windows))]
use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::process::Command;

#[cfg(any(unix, windows))]
use anyhow::{Context, Result, bail};
#[cfg(any(unix, windows))]
use rgo_core::cargo_config;
#[cfg(any(unix, windows))]
use rgo_core::config::Config;
#[cfg(any(unix, windows))]
use rgo_core::context;
#[cfg(any(unix, windows))]
use rgo_core::paths::{RgoPaths, cargo_home};
#[cfg(any(unix, windows))]
use rgo_core::supervision;
#[cfg(any(unix, windows))]
use rgo_protocol::BYPASS_ENV;
#[cfg(any(unix, windows))]
use serde::Deserialize;

#[cfg(any(unix, windows))]
#[derive(Deserialize)]
struct CargoProject {
    root: PathBuf,
}

#[cfg(any(unix, windows))]
pub fn run(
    real_cargo: &Path,
    expected_cargo_home: Option<&Path>,
    expected_rgo_home: Option<&Path>,
    daemon_exe: Option<&Path>,
    args: Vec<OsString>,
) -> Result<()> {
    let started = std::time::Instant::now();
    #[cfg(target_os = "macos")]
    let native = (std::env::var_os("RGO_MACOS_SUPERVISOR_PILOT").as_deref()
        == Some(OsStr::new("1")))
    .then(|| {
        let inherited = super::macos_cargo_job::InheritedDescriptors::capture()?;
        let attributes = super::macos_cargo_job::NativeAttributes::capture(&inherited.targets)?;
        Ok::<_, anyhow::Error>((inherited, attributes))
    });
    if !real_cargo.is_absolute() {
        bail!("--real-cargo must be an absolute path to the existing Cargo executable");
    }
    let resolved = real_cargo.canonicalize().context("locating real Cargo")?;
    if resolved == std::env::current_exe()?.canonicalize()? {
        bail!("--real-cargo points back to rgo; refusing recursive launch");
    }
    let active_cargo_home = cargo_home()?;
    if is_rgo_cargo_shim(&resolved) {
        bail!("--real-cargo points back to the owned Cargo launcher");
    }
    if expected_cargo_home.is_some_and(|expected| !same_directory(expected, &active_cargo_home)) {
        eprintln!("rgo: Cargo home differs from the owning launcher; using ordinary Cargo storage");
        return exec_real_cargo(real_cargo, &args);
    }
    let paths = match RgoPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!(
                "rgo: cannot locate managed storage ({error:#}); using ordinary Cargo storage"
            );
            return exec_real_cargo(real_cargo, &args);
        }
    };
    if expected_rgo_home.is_some_and(|expected| !same_directory(expected, &paths.root)) {
        eprintln!(
            "rgo: storage root differs from the owning launcher; using ordinary Cargo storage"
        );
        return exec_real_cargo(real_cargo, &args);
    }
    let (toolchain, cargo_args) = split_toolchain(&args);
    let mut command_args = Vec::new();
    if let Some(toolchain) = toolchain {
        command_args.push(toolchain.to_os_string());
    }
    let mut selection = if std::env::var_os(BYPASS_ENV).is_some() {
        None
    } else {
        select_context(real_cargo, toolchain, cargo_args, &paths, daemon_exe)?
    };
    let selected = started.elapsed();
    let mut session = match supervision::lock_cargo_session(
        &paths,
        selection.as_ref().map(|(dir, _)| dir.as_path()),
    ) {
        Ok(session) => session,
        Err(error) => {
            if let Some((_, root)) = &selection {
                eprintln!(
                    "rgo: managed lifecycle lock unavailable ({error:#}); using checkout storage"
                );
                return exec_real_cargo_in_checkout(real_cargo, &args, root);
            }
            if is_inert_query(cargo_args) {
                eprintln!("rgo: lifecycle lock unavailable ({error:#}); forwarding Cargo query");
                return exec_real_cargo(real_cargo, &args);
            }
            return Err(error);
        }
    };
    // The launcher may be the only process a no-service user runs. Start the
    // requested daemon after taking the session guard, before admitting this
    // invocation to managed storage. A missing or incompatible daemon leaves
    // the build in ordinary Cargo storage instead of entering a GC domain
    // without known coordination.
    let mut auto_gc_enabled = false;
    let maintenance_issue = match Config::load(&paths.config_file()) {
        Ok(config) if config.gc.auto => {
            auto_gc_enabled = true;
            (!daemon_exe.map_or_else(
                || super::daemon::ensure_running(&paths),
                |exe| super::daemon::ensure_running_from(&paths, exe),
            ))
            .then(|| "automatic maintenance is unavailable".to_owned())
        }
        Err(error) => Some(format!(
            "cannot read automatic maintenance configuration: {error:#}"
        )),
        _ => None,
    };
    if let Some(issue) = maintenance_issue {
        eprintln!("rgo: {issue}; using ordinary Cargo storage");
        if let Some((_, root)) = &selection {
            match supervision::lock_cargo_session(&paths, None) {
                Ok(global) => {
                    drop(session);
                    session = global;
                    selection = None;
                }
                Err(error) => {
                    eprintln!(
                        "rgo: fallback lifecycle lock unavailable ({error:#}); using checkout storage"
                    );
                    drop(session);
                    return exec_real_cargo_in_checkout(real_cargo, &args, root);
                }
            }
        }
    }
    if let Some((dir, root)) = &selection {
        let activate = (|| -> Result<()> {
            let newly_created = context::ensure_managed_context_dir(&paths, dir)?;
            if context::is_pinned(&paths, dir) && !context::is_pinned_dir(dir) {
                context::write_pin_marker(dir)?;
            }
            context::write_supervised_sidecar(dir, root, &root.join("Cargo.toml"), newly_created)?;
            if auto_gc_enabled {
                supervision::mark_pending_maintenance(&paths, dir)?;
            }
            Ok(())
        })();
        if let Err(error) = activate {
            tracing::warn!(%error, "managed context unavailable; using ordinary Cargo storage");
            // Acquire the conservative guard before releasing this context's
            // guard, so no GC pass can slip between the two modes.
            match supervision::lock_cargo_session(&paths, None) {
                Ok(global) => {
                    drop(session);
                    session = global;
                    selection = None;
                }
                Err(lock_error) => {
                    eprintln!(
                        "rgo: fallback lifecycle lock unavailable ({lock_error:#}); using checkout storage"
                    );
                    drop(session);
                    return exec_real_cargo_in_checkout(real_cargo, &args, root);
                }
            }
        }
    }
    if let Some((build_dir, _)) = &selection {
        let setting = format!(
            "build.build-dir={}",
            serde_json::to_string(&build_dir.to_string_lossy().as_ref())?
        );
        command_args.push(OsString::from("--config"));
        command_args.push(OsString::from(setting));
    }
    command_args.extend(cargo_args.iter().cloned());
    tracing::debug!(
        selection_ms = selected.as_secs_f64() * 1000.0,
        activation_ms = (started.elapsed() - selected).as_secs_f64() * 1000.0,
        "supervised Cargo launch preparation"
    );
    // Private pilot: exercise the installed unchanged-command launcher
    // before making launchd transport part of the supported activation mode.
    #[cfg(target_os = "macos")]
    if std::env::var_os("RGO_MACOS_SUPERVISOR_PILOT").as_deref() == Some(OsStr::new("1")) {
        if let Some((context, root)) = &selection {
            match native
                .context("Cargo native attribute capture is missing")
                .and_then(|capture| capture)
                .and_then(|(inherited, attributes)| {
                    super::macos_cargo_job::PreparedJob::prepare(
                        real_cargo,
                        &command_args,
                        &paths,
                        context,
                        &inherited,
                        attributes,
                    )
                })
                .and_then(|job| job.commit(&mut session))
            {
                Ok(job) => return job.run(session),
                Err(error) => {
                    super::macos_cargo_job::abort_cancelled();
                    eprintln!(
                        "rgo: macOS Cargo guardian unavailable ({error:#}); using checkout storage"
                    );
                    drop(session);
                    return exec_real_cargo_in_checkout(real_cargo, &args, root);
                }
            }
        }
    }
    #[cfg(unix)]
    {
        run_supervised(real_cargo, &command_args, session)
    }
    #[cfg(windows)]
    {
        run_supervised_windows(
            real_cargo,
            &command_args,
            &args,
            &paths,
            selection.as_ref().map(|(_, root)| root.as_path()),
            session,
        )
    }
}

#[cfg(any(unix, windows))]
fn same_directory(expected: &Path, active: &Path) -> bool {
    expected.is_absolute()
        && expected.is_dir()
        && expected
            .canonicalize()
            .ok()
            .is_some_and(|expected| active.canonicalize().ok() == Some(expected))
}

#[cfg(any(unix, windows))]
fn is_rgo_cargo_shim(path: &Path) -> bool {
    path.ancestors().any(|directory| {
        directory
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("shims"))
            && directory
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("rgo"))
    })
}

/// A selected workspace has no caller-supplied build-dir override. If rgo's
/// guard fails before Cargo starts, explicitly keep this invocation's build
/// state in its checkout even if a Cargo config changes concurrently.
#[cfg(any(unix, windows))]
fn exec_real_cargo_in_checkout(real_cargo: &Path, args: &[OsString], root: &Path) -> Result<()> {
    let local_build_dir = root.join("target");
    let path = local_build_dir
        .to_str()
        .context("checkout path is not UTF-8")?;
    let setting = format!("build.build-dir={}", serde_json::to_string(path)?);
    let (toolchain, cargo_args) = split_toolchain(args);
    let mut local_args = Vec::with_capacity(args.len() + 2);
    if let Some(toolchain) = toolchain {
        local_args.push(toolchain.to_os_string());
    }
    local_args.push(OsString::from("--config"));
    local_args.push(OsString::from(setting));
    local_args.extend(cargo_args.iter().cloned());
    exec_real_cargo(real_cargo, &local_args)
}

#[cfg(unix)]
fn exec_real_cargo(real_cargo: &Path, args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(real_cargo).args(args).exec();
    Err(error).with_context(|| format!("executing {}", real_cargo.display()))
}

#[cfg(windows)]
pub(super) fn exec_real_cargo(real_cargo: &Path, args: &[OsString]) -> Result<()> {
    let status = Command::new(real_cargo)
        .args(args)
        .status()
        .with_context(|| format!("executing {}", real_cargo.display()))?;
    std::process::exit(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn run_supervised(
    real_cargo: &Path,
    args: &[OsString],
    session: supervision::SessionGuard,
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    session.retain_across_exec()?;
    let error = Command::new(real_cargo).args(args).exec();
    Err(error).with_context(|| format!("executing {}", real_cargo.display()))
}

#[cfg(windows)]
fn run_supervised_windows(
    real_cargo: &Path,
    args: &[OsString],
    fallback_args: &[OsString],
    paths: &RgoPaths,
    managed_root: Option<&Path>,
    session: supervision::SessionGuard,
) -> Result<()> {
    let mut job = match super::windows_job::JobGuard::spawn(real_cargo, args) {
        Ok(job) => job,
        Err(error) => {
            eprintln!(
                "rgo: supervised Cargo job unavailable ({error:#}); using ordinary Cargo storage"
            );
            // Creation failed before Cargo could execute. If this invocation
            // had selected a context, take the conservative global guard
            // before releasing that context's guard and dropping its override.
            if let Some(root) = managed_root {
                match supervision::lock_cargo_session(paths, None) {
                    Ok(global) => {
                        drop(session);
                        let _guard = global;
                        return exec_real_cargo(real_cargo, fallback_args);
                    }
                    Err(lock_error) => {
                        eprintln!(
                            "rgo: fallback lifecycle lock unavailable ({lock_error:#}); using checkout storage"
                        );
                        drop(session);
                        return exec_real_cargo_in_checkout(real_cargo, fallback_args, root);
                    }
                }
            }
            let _guard = session;
            return exec_real_cargo(real_cargo, fallback_args);
        }
    };
    let code = job.wait_primary()?;
    // The primary Cargo process can exit before a compiler or build-script
    // descendant. Keep both the job and the filesystem guard through the
    // complete process tree, then return Cargo's own exit code.
    job.wait_empty()?;
    drop(job);
    drop(session);
    std::process::exit(code as i32)
}

#[cfg(any(unix, windows))]
fn split_toolchain(args: &[OsString]) -> (Option<&OsStr>, &[OsString]) {
    if args
        .first()
        .is_some_and(|arg| arg.to_str().is_some_and(|text| text.starts_with('+')))
    {
        (Some(args[0].as_os_str()), &args[1..])
    } else {
        (None, args)
    }
}

#[cfg(any(unix, windows))]
fn is_inert_query(args: &[OsString]) -> bool {
    args.len() == 1
        && args[0]
            .to_str()
            .is_some_and(|arg| matches!(arg, "--version" | "-V" | "--help" | "-h" | "--list"))
}

#[cfg(any(unix, windows))]
fn select_context(
    real_cargo: &Path,
    toolchain: Option<&OsStr>,
    args: &[OsString],
    paths: &RgoPaths,
    daemon_exe: Option<&Path>,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let started = std::time::Instant::now();
    if workspace_command(args).is_none() || has_unmanaged_global_option(args) {
        return Ok(None);
    }
    // An unreadable or vanished working directory cannot prove where Cargo
    // would build. Keep the command available under the global session guard.
    if config_may_override_build_dir().unwrap_or(true) {
        return Ok(None);
    }
    match compiler_override_issue(paths, daemon_exe) {
        Ok(None) => {}
        Ok(Some(issue)) => {
            eprintln!("rgo: {issue}; using ordinary Cargo storage");
            return Ok(None);
        }
        Err(error) => {
            eprintln!(
                "rgo: compiler settings cannot be verified ({error:#}); using ordinary Cargo storage"
            );
            return Ok(None);
        }
    }

    let validated = started.elapsed();
    let mut version = Command::new(real_cargo);
    if let Some(toolchain) = toolchain {
        version.arg(toolchain);
    }
    version.arg("--version");
    let mut locate = Command::new(real_cargo);
    if let Some(toolchain) = toolchain {
        locate.arg(toolchain);
    }
    locate.args(["locate-project", "--workspace", "--message-format", "json"]);
    if let Some(manifest) = argument_value(args, "--manifest-path") {
        locate.arg("--manifest-path").arg(manifest);
    }
    // Both are independent queries against the exact same Cargo selection.
    // Verify fresh results every time; a rustup proxy's bytes alone do not
    // identify the active toolchain or workspace. Thread creation/query failure
    // keeps the invocation outside managed storage.
    let queries = std::thread::scope(|scope| -> Result<_> {
        let version = std::thread::Builder::new()
            .name("rgo-cargo-version".to_owned())
            .spawn_scoped(scope, move || version.output())?;
        let workspace = locate.output();
        let version = version
            .join()
            .map_err(|_| anyhow::anyhow!("Cargo version query panicked"))?;
        Ok((version, workspace))
    });
    let Ok((Ok(version), output)) = queries else {
        return Ok(None);
    };
    let queried = started.elapsed();
    if !version.status.success()
        || !cargo_config::supports_build_dir(&String::from_utf8_lossy(&version.stdout))
    {
        eprintln!(
            "rgo: active Cargo does not support managed build storage (requires 1.91+); using ordinary Cargo storage"
        );
        return Ok(None);
    }
    let Ok(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let Ok(report) = serde_json::from_slice::<CargoProject>(&output.stdout) else {
        return Ok(None);
    };
    if !report.root.is_absolute() {
        return Ok(None);
    }
    let Ok(manifest) = report.root.canonicalize() else {
        return Ok(None);
    };
    if !manifest.is_file() {
        return Ok(None);
    }
    let Some(root) = manifest.parent() else {
        return Ok(None);
    };
    // Sidecars store workspace paths as UTF-8. Keep an unrepresentable root
    // outside the managed namespace until that format can preserve OS bytes.
    if root.to_str().is_none() {
        return Ok(None);
    }
    let Ok(dir) = supervision::context_for_workspace(paths, root) else {
        return Ok(None);
    };
    tracing::debug!(
        validation_ms = validated.as_secs_f64() * 1000.0,
        queries_ms = (queried - validated).as_secs_f64() * 1000.0,
        identity_ms = (started.elapsed() - queried).as_secs_f64() * 1000.0,
        "supervised Cargo context selection"
    );
    Ok(Some((dir, root.to_path_buf())))
}

/// The nightly layout flag changes Cargo's private contents inside a build
/// directory, not its selected directory. Keep all other unstable options
/// unmanaged because their effect on build-path selection is not verified.
#[cfg(any(unix, windows))]
fn has_unmanaged_global_option(args: &[OsString]) -> bool {
    let mut args = args.iter().map(OsString::as_os_str);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "-Z" {
            if args.next() != Some(OsStr::new("build-dir-new-layout")) {
                return true;
            }
            continue;
        }
        if arg == "-Zbuild-dir-new-layout" {
            continue;
        }
        if arg.to_str().is_some_and(|text| {
            text == "--config"
                || text.starts_with("--config=")
                || text == "-C"
                || text.starts_with("-C")
                || text.starts_with("-Z")
        }) {
            return true;
        }
    }
    false
}

#[cfg(any(unix, windows))]
fn config_may_override_build_dir() -> Result<bool> {
    if std::env::var_os("CARGO_BUILD_BUILD_DIR").is_some() {
        return Ok(true);
    }
    let cwd = std::env::current_dir()?;
    for ancestor in cwd.ancestors() {
        if config_directory_may_override(&ancestor.join(".cargo")) {
            return Ok(true);
        }
    }
    Ok(config_directory_may_override(&cargo_home()?))
}

#[cfg(any(unix, windows))]
fn config_directory_may_override(directory: &Path) -> bool {
    config_directory_matches(directory, |path| {
        cargo_config::may_set_build_dir_in_file(path)
    })
    .unwrap_or(true)
}

#[cfg(any(unix, windows))]
fn config_directory_matches(
    directory: &Path,
    inspect: impl FnOnce(&Path) -> Result<bool>,
) -> Result<bool> {
    let legacy = directory.join("config");
    let modern = directory.join("config.toml");
    let path = match std::fs::symlink_metadata(&legacy) {
        Ok(_) => legacy,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(&modern) {
                Ok(_) => modern,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    };
    inspect(&path)
}

/// Share the admission reason with doctor. The common no-wrapper case needs
/// no extra process probe; only an explicitly selected rgo wrapper is checked.
#[cfg(any(unix, windows))]
pub(super) fn compiler_override_issue(
    paths: &RgoPaths,
    daemon_exe: Option<&Path>,
) -> Result<Option<String>> {
    fn unverified(approved: Option<&Path>) -> Result<bool> {
        for key in cargo_config::COMPILER_ENV_KEYS {
            if let Some(value) = std::env::var_os(key) {
                if key.ends_with("WRAPPER")
                    && *key != "RGO_INNER_RUSTC_WRAPPER"
                    && value
                        .to_str()
                        .is_some_and(|value| cargo_config::wrapper_is_approved(value, approved))
                {
                    continue;
                }
                return Ok(true);
            }
        }
        let cwd = std::env::current_dir()?;
        for directory in cwd
            .ancestors()
            .map(|path| path.join(".cargo"))
            .chain(std::iter::once(cargo_home()?))
        {
            if config_directory_matches(&directory, |path| {
                cargo_config::may_set_unverified_compiler_in_file(path, approved)
            })? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    if !unverified(None)? {
        return Ok(None);
    }
    let issue =
        "custom compiler or wrapper has no verified full-session writer protection".to_owned();
    if std::env::var_os("RGO_INNER_RUSTC_WRAPPER").is_some() {
        return Ok(Some(issue));
    }
    match std::fs::read_to_string(paths.state_dir().join("inner-wrapper")) {
        Ok(value) if !value.trim().is_empty() => return Ok(Some(issue)),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let current = std::env::current_exe()?;
    let approved = super::setup::matching_wrapper_path(daemon_exe.unwrap_or(&current))
        .ok()
        .map(PathBuf::from);
    if approved.is_some() && !unverified(approved.as_deref())? {
        Ok(None)
    } else {
        Ok(Some(issue))
    }
}

#[cfg(any(unix, windows))]
fn workspace_command(args: &[OsString]) -> Option<&str> {
    const WORKSPACE_COMMANDS: &[&str] = &[
        "build", "b", "check", "c", "run", "r", "test", "t", "bench", "doc", "d", "clean", "rustc",
        "rustdoc", "fix", "clippy",
    ];
    let mut index = 0;
    while let Some(argument) = args.get(index).and_then(|value| value.to_str()) {
        match argument {
            "-Z" if args
                .get(index + 1)
                .is_some_and(|arg| arg == "build-dir-new-layout") =>
            {
                index += 2
            }
            "-Zbuild-dir-new-layout" => index += 1,
            "--locked" | "--offline" | "--frozen" | "-q" | "--quiet" | "-v" | "--verbose" => {
                index += 1
            }
            "--color" => index += 2,
            value if value.starts_with("--color=") => index += 1,
            value
                if value.starts_with('-')
                    && value.len() > 1
                    && value[1..].chars().all(|c| c == 'v') =>
            {
                index += 1
            }
            value if WORKSPACE_COMMANDS.contains(&value) => return Some(value),
            _ => return None,
        }
    }
    None
}

#[cfg(any(unix, windows))]
fn argument_value<'a>(args: &'a [OsString], flag: &str) -> Option<&'a OsStr> {
    let mut iter = args.iter();
    while let Some(argument) = iter.next() {
        if argument == "--" {
            break;
        }
        if argument == flag {
            return iter.next().map(OsString::as_os_str);
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|text| text.strip_prefix(flag))
            .and_then(|text| text.strip_prefix('='))
        {
            return Some(OsStr::new(value));
        }
    }
    None
}

#[cfg(not(any(unix, windows)))]
pub fn run(
    _real_cargo: &std::path::Path,
    _expected_cargo_home: Option<&std::path::Path>,
    _expected_rgo_home: Option<&std::path::Path>,
    _args: Vec<std::ffi::OsString>,
) -> anyhow::Result<()> {
    anyhow::bail!("the supervised Cargo pilot is not available on this platform")
}
