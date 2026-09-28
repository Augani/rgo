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
    args: Vec<OsString>,
) -> Result<()> {
    if !real_cargo.is_absolute() {
        bail!("--real-cargo must be an absolute path to the existing Cargo executable");
    }
    let resolved = real_cargo.canonicalize().context("locating real Cargo")?;
    if resolved == std::env::current_exe()?.canonicalize()? {
        bail!("--real-cargo points back to rgo; refusing recursive launch");
    }
    let active_cargo_home = cargo_home()?;
    let owned_shim =
        active_cargo_home
            .join("rgo/shims")
            .join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
    if owned_shim.canonicalize().ok().as_ref() == Some(&resolved) {
        bail!("--real-cargo points back to the owned Cargo launcher");
    }
    if expected_cargo_home.is_some_and(|expected| !same_directory(expected, &active_cargo_home)) {
        eprintln!("rgo: Cargo home differs from the owning launcher; using ordinary Cargo storage");
        return exec_real_cargo(real_cargo, &args);
    }
    let paths = RgoPaths::discover()?;
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
        select_context(real_cargo, toolchain, cargo_args, &paths)?
    };
    let mut session =
        supervision::lock_cargo_session(&paths, selection.as_ref().map(|(dir, _)| dir.as_path()))?;
    // The launcher may be the only process a no-service user runs. Start the
    // requested daemon after taking the session guard, before admitting this
    // invocation to managed storage. A missing or incompatible daemon leaves
    // the build in ordinary Cargo storage instead of entering a GC domain
    // without known coordination.
    let maintenance_issue = match Config::load(&paths.config_file()) {
        Ok(config) if config.gc.auto && !super::daemon::ensure_running(&paths) => {
            Some("automatic maintenance is unavailable".to_owned())
        }
        Err(error) => Some(format!(
            "cannot read automatic maintenance configuration: {error:#}"
        )),
        _ => None,
    };
    if let Some(issue) = maintenance_issue {
        eprintln!("rgo: {issue}; using ordinary Cargo storage");
        if selection.is_some() {
            let global = supervision::lock_cargo_session(&paths, None)?;
            drop(session);
            session = global;
            selection = None;
        }
    }
    if let Some((dir, root)) = &selection {
        let activate = (|| -> Result<()> {
            context::ensure_managed_context_dir(&paths, dir)?;
            if context::is_pinned(&paths, dir) && !context::is_pinned_dir(dir) {
                context::write_pin_marker(dir)?;
            }
            context::write_sidecar(dir, root, &root.join("Cargo.toml"), None)
        })();
        if let Err(error) = activate {
            tracing::warn!(%error, "managed context unavailable; using ordinary Cargo storage");
            // Acquire the conservative guard before releasing this context's
            // guard, so no GC pass can slip between the two modes.
            let global = supervision::lock_cargo_session(&paths, None)?;
            drop(session);
            session = global;
            selection = None;
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
            selection.is_some(),
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

#[cfg(unix)]
fn exec_real_cargo(real_cargo: &Path, args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(real_cargo).args(args).exec();
    Err(error).with_context(|| format!("executing {}", real_cargo.display()))
}

#[cfg(windows)]
fn exec_real_cargo(real_cargo: &Path, args: &[OsString]) -> Result<()> {
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
    managed: bool,
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
            if managed {
                let global = supervision::lock_cargo_session(paths, None)?;
                drop(session);
                let _guard = global;
                return exec_real_cargo(real_cargo, fallback_args);
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
fn select_context(
    real_cargo: &Path,
    toolchain: Option<&OsStr>,
    args: &[OsString],
    paths: &RgoPaths,
) -> Result<Option<(PathBuf, PathBuf)>> {
    if workspace_command(args).is_none()
        || args.iter().any(|arg| {
            arg.to_str().is_some_and(|text| {
                text == "--config"
                    || text.starts_with("--config=")
                    || text == "-C"
                    || text.starts_with("-C")
                    || text == "-Z"
                    || text.starts_with("-Z")
            })
        })
    {
        return Ok(None);
    }
    if config_may_override_build_dir()? {
        return Ok(None);
    }

    let mut version = Command::new(real_cargo);
    if let Some(toolchain) = toolchain {
        version.arg(toolchain);
    }
    let Ok(version) = version.arg("--version").output() else {
        return Ok(None);
    };
    if !version.status.success()
        || !cargo_config::supports_build_dir(&String::from_utf8_lossy(&version.stdout))
    {
        eprintln!(
            "rgo: active Cargo does not support managed build storage (requires 1.91+); using ordinary Cargo storage"
        );
        return Ok(None);
    }

    let mut locate = Command::new(real_cargo);
    if let Some(toolchain) = toolchain {
        locate.arg(toolchain);
    }
    locate.args(["locate-project", "--workspace", "--message-format", "json"]);
    if let Some(manifest) = argument_value(args, "--manifest-path") {
        locate.arg("--manifest-path").arg(manifest);
    }
    let output = locate.output().context("locating Cargo workspace")?;
    if !output.status.success() {
        return Ok(None);
    }
    let Ok(report) = serde_json::from_slice::<CargoProject>(&output.stdout) else {
        return Ok(None);
    };
    if !report.root.is_absolute() || !report.root.is_file() {
        return Ok(None);
    }
    let manifest = report.root.canonicalize()?;
    let root = manifest
        .parent()
        .context("Cargo workspace manifest has no parent")?;
    // Sidecars store workspace paths as UTF-8. Keep an unrepresentable root
    // outside the managed namespace until that format can preserve OS bytes.
    if root.to_str().is_none() {
        return Ok(None);
    }
    let dir = supervision::context_for_workspace(paths, root)?;
    Ok(Some((dir, root.to_path_buf())))
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
    let legacy = directory.join("config");
    let modern = directory.join("config.toml");
    let path = match std::fs::symlink_metadata(&legacy) {
        Ok(_) => legacy,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(&modern) {
                Ok(_) => modern,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
                Err(_) => return true,
            }
        }
        Err(_) => return true,
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| cargo_config::may_set_build_dir(&text).ok())
        .unwrap_or(true)
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
