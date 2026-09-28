use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rgo_core::cargo_config::{self, Desired};
use rgo_core::context;
use rgo_core::ipc;
use rgo_core::paths::{RgoPaths, activation_pointer, cargo_home};
use rgo_core::service;
use rgo_protocol::{Request, Response};
use serde::{Deserialize, Serialize};

const INSTALL_RECORD_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisedCargo {
    shim_path: String,
    real_cargo: String,
    shim_contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallationRecord {
    schema_version: u32,
    cargo_home: String,
    config_file: String,
    rgo_home: String,
    install_root: String,
    rgo_binary: String,
    wrapper_binary: Option<String>,
    original_rustc_wrapper: Option<String>,
    managed_build_dir: String,
    managed_keys: Vec<String>,
    binary_version: String,
    protocol_version: u32,
    #[serde(default)]
    supervised_cargo: Option<SupervisedCargo>,
}

#[derive(Serialize)]
struct PlannedFile {
    contents: String,
    mode: u32,
}

#[derive(Serialize)]
struct InstallerPlan {
    schema_version: u32,
    files: BTreeMap<String, Option<PlannedFile>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    binaries: BTreeMap<String, String>,
}

pub fn run(
    undo: bool,
    dry_run: bool,
    installer_plan_json: bool,
    no_service: bool,
    no_wrapper: bool,
    supervised: bool,
    real_cargo: Option<PathBuf>,
) -> Result<()> {
    if installer_plan_json && (undo || dry_run || !no_service) {
        bail!("installer activation plans require setup --no-service");
    }
    let dry_run = dry_run || installer_plan_json;
    if supervised && !cfg!(any(unix, windows)) {
        bail!("supervised Cargo setup is unsupported on this platform");
    }
    if undo && (supervised || real_cargo.is_some()) {
        bail!("setup --undo reads the installed mode; omit --supervised and --real-cargo");
    }
    if supervised && no_wrapper {
        bail!("--no-wrapper applies only to native Cargo configuration");
    }
    if !undo && !supervised {
        let version = Command::new("cargo")
            .arg("--version")
            .output()
            .context("checking the active Cargo before setup")?;
        if !version.status.success()
            || !cargo_config::supports_build_dir(&String::from_utf8_lossy(&version.stdout))
        {
            bail!(
                "native setup requires Cargo 1.91 or newer for build.build-dir; active Cargo reported {:?}",
                String::from_utf8_lossy(&version.stdout).trim()
            );
        }
    }
    let cargo_home = cargo_home()?;
    if !cargo_home.is_absolute() {
        bail!(
            "CARGO_HOME must be an absolute path for setup to remain effective across working directories"
        );
    }
    // All rgo setup/undo processes for this Cargo home serialize before they
    // read either the config or the recovery state. Dry runs remain read-only.
    let _setup_lock = if dry_run {
        None
    } else {
        Some(lock_setup(&cargo_home)?)
    };
    let paths = RgoPaths::discover()?;
    paths.validate_root()?;
    let _root_setup_lock = if dry_run {
        None
    } else {
        Some(lock_root_setup(&paths)?)
    };
    let owner_path = paths.state_dir().join("owner-cargo-home");
    let old_owner = read_optional(&owner_path)?;
    let mode_path = paths.state_dir().join("storage-mode");
    let old_mode = read_optional(&mode_path)?;
    let desired_mode = if supervised { "supervised" } else { "native" };
    if !undo {
        let previous_mode = old_mode
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .with_context(|| format!("reading {}", mode_path.display()))?
            .map(str::trim);
        if previous_mode.is_some_and(|mode| mode != "native" && mode != "supervised") {
            bail!("{} has an unknown storage mode", mode_path.display());
        }
        if previous_mode.is_some_and(|mode| mode != desired_mode)
            || (previous_mode.is_none() && supervised && has_build_storage(&paths)?)
        {
            bail!(
                "{} has a different or unverified storage mode; use a fresh RGO_HOME for this mode until explicit migration is supported",
                paths.root.display()
            );
        }
    }
    let cargo_home_text = cargo_home
        .to_str()
        .context("CARGO_HOME must be UTF-8 to record storage-root ownership")?;
    if let Some(owner) = old_owner.as_deref() {
        let owner = std::str::from_utf8(owner)
            .with_context(|| format!("reading {}", owner_path.display()))?;
        if owner.trim_end_matches(['\r', '\n']) != cargo_home_text {
            bail!(
                "{} is already owned by another Cargo home; choose a separate RGO_HOME for this installation",
                paths.root.display()
            );
        }
    }
    let pointer_path = activation_pointer(&cargo_home);
    let old_pointer = read_optional(&pointer_path)?;
    if old_pointer
        .as_deref()
        .and_then(|old| std::str::from_utf8(old).ok())
        .is_some_and(|old_root| {
            old_root.trim_end_matches(['\r', '\n']) != paths.root.to_string_lossy()
        })
    {
        bail!(
            "{} points to a different rgo home; use the previous home to undo setup before moving storage",
            pointer_path.display()
        );
    }
    let effective_cfg_path = cargo_config::effective_home_config(&cargo_home);
    let record_path = cargo_home.join(".rgo-install.json");
    let old_record_bytes = read_optional(&record_path)?;
    let old_record = old_record_bytes
        .as_deref()
        .map(serde_json::from_slice::<InstallationRecord>)
        .transpose()
        .with_context(|| format!("reading {}", record_path.display()))?;
    if let Some(record) = &old_record {
        if ![1, 2, INSTALL_RECORD_VERSION].contains(&record.schema_version) {
            bail!(
                "{} uses unsupported installation record version {}; use a compatible rgo binary",
                record_path.display(),
                record.schema_version
            );
        }
        if (record.schema_version == INSTALL_RECORD_VERSION) != record.supervised_cargo.is_some() {
            bail!(
                "{} has an inconsistent installation mode",
                record_path.display()
            );
        }
        let owned_config = Path::new(&record.config_file);
        if ![cargo_home.join("config"), cargo_home.join("config.toml")]
            .iter()
            .any(|path| path == owned_config)
            || record.cargo_home != cargo_home.to_string_lossy()
            || record.rgo_home != paths.root.to_string_lossy()
        {
            bail!(
                "{} belongs to a different Cargo home or rgo root; restore that installation before changing roots",
                record_path.display()
            );
        }
        if !undo && record.supervised_cargo.is_none() && owned_config != effective_cfg_path {
            bail!(
                "Cargo config precedence changed since setup; run `rgo setup --undo` to remove the old fence before reactivating"
            );
        }
    }
    if !undo
        && old_record.is_some()
        && old_record
            .as_ref()
            .is_some_and(|r| r.supervised_cargo.is_some())
            != supervised
    {
        bail!(
            "installation mode changed; run `rgo setup --undo` before switching native and supervised Cargo"
        );
    }
    let previous_shim = old_record
        .as_ref()
        .and_then(|r| r.supervised_cargo.as_ref());
    #[cfg(windows)]
    let shim_path = {
        let flat = cargo_home.join("rgo/shims").join(shim_name());
        let versioned = cargo_home
            .join("rgo/shims")
            .join(format!("v{}", env!("CARGO_PKG_VERSION")))
            .join(shim_name());
        if let (Some(record), Some(previous)) = (old_record.as_ref(), previous_shim) {
            let path = Path::new(&previous.shim_path);
            let old_component = format!("v{}", record.binary_version);
            if Path::new(&old_component).components().count() != 1 {
                bail!("installation record has an invalid Cargo shim version");
            }
            let old_versioned = cargo_home
                .join("rgo/shims")
                .join(old_component)
                .join(shim_name());
            if path != flat && path != old_versioned {
                bail!("installation record names an unexpected Cargo shim path");
            }
            if path
                .parent()
                .is_some_and(|dir| dir.is_symlink() || dir.parent().is_some_and(Path::is_symlink))
            {
                bail!("refusing a symlinked previous Cargo shim directory");
            }
            if !undo && supervised && record.binary_version != env!("CARGO_PKG_VERSION") {
                if path == flat
                    || !verified_windows_fallback(
                        path,
                        &cargo_home,
                        Path::new(&previous.real_cargo),
                    )?
                {
                    bail!(
                        "Windows supervised upgrade requires an owned versioned shim with a verified Cargo fallback"
                    );
                }
                versioned
            } else {
                path.to_path_buf()
            }
        } else {
            versioned
        }
    };
    #[cfg(unix)]
    let shim_path = cargo_home.join("rgo/shims").join(shim_name());
    #[cfg(windows)]
    let retain_previous_shim = if undo {
        if let Some(shim) = previous_shim {
            let path = Path::new(&shim.shim_path);
            if path.parent() == Some(cargo_home.join("rgo/shims").as_path()) {
                false
            } else {
                verified_windows_fallback(path, &cargo_home, Path::new(&shim.real_cargo))?
            }
        } else {
            false
        }
    } else {
        false
    };
    let shim_dir = shim_path.parent().context("Cargo shim has no parent")?;
    if shim_dir.is_symlink()
        || shim_dir.parent().is_some_and(Path::is_symlink)
        || (cfg!(windows)
            && shim_dir.file_name().is_some_and(|name| name != "shims")
            && shim_dir
                .parent()
                .and_then(Path::parent)
                .is_some_and(Path::is_symlink))
    {
        bail!("refusing a symlinked Cargo shim directory");
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&shim_path) {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!(
                "{} is not a plain file; refusing to modify it",
                shim_path.display()
            );
        }
    }
    #[cfg(windows)]
    if supervised && previous_shim.is_none() {
        let flat = cargo_home.join("rgo/shims").join(shim_name());
        if flat != shim_path && std::fs::symlink_metadata(&flat).is_ok() {
            bail!(
                "{} exists without an rgo ownership record; remove the stale Cargo launcher before activation",
                flat.display()
            );
        }
    }
    let shim_preexisted = shim_path.is_file();
    if let Some(previous) = previous_shim {
        let previous_path = Path::new(&previous.shim_path);
        if previous_path != shim_path
            && previous_path
                .parent()
                .is_some_and(|dir| dir.is_symlink() || dir.parent().is_some_and(Path::is_symlink))
        {
            bail!("refusing a symlinked previous Cargo shim directory");
        }
        if let Ok(metadata) = std::fs::symlink_metadata(previous_path) {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("{} is not a plain file", previous_path.display());
            }
        }
        if previous_path != shim_path && !previous_path.is_file() {
            bail!("previous Cargo shim is missing; repair it before upgrading");
        }
        if previous_path.is_file() && !shim_matches(previous_path, &previous.shim_contents)? {
            bail!(
                "{} changed since setup; refusing to replace or remove it",
                previous_path.display()
            );
        }
    }
    let supervised_cargo = if supervised {
        Some(prepare_supervised_cargo(
            &cargo_home,
            &paths.root,
            &shim_path,
            real_cargo.as_deref(),
        )?)
    } else {
        None
    };
    #[cfg(windows)]
    let shim_fallback = supervised_cargo
        .as_ref()
        .map(|shim| -> Result<(PathBuf, Vec<u8>)> {
            let path = super::windows_cargo_entry::fallback_path(&shim_path)?;
            let contents = serde_json::to_vec_pretty(&super::windows_cargo_entry::ShimFallback {
                schema_version: 1,
                cargo_home: cargo_home.clone(),
                real_cargo: PathBuf::from(&shim.real_cargo),
            })?;
            Ok((path, contents))
        })
        .transpose()?;
    #[cfg(windows)]
    if let Some((path, _)) = &shim_fallback {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("{} is not a plain file", path.display());
            }
        }
    }
    #[cfg(windows)]
    let old_shim_fallback = shim_fallback
        .as_ref()
        .map(|(path, _)| read_optional(path))
        .transpose()?
        .flatten();
    #[cfg(windows)]
    if previous_shim.is_some()
        && shim_fallback.as_ref().is_some_and(|(_, contents)| {
            old_shim_fallback
                .as_deref()
                .is_some_and(|old| old != contents)
        })
    {
        bail!("owned Cargo launcher fallback changed since setup");
    }
    if supervised
        && previous_shim.is_none_or(|previous| Path::new(&previous.shim_path) != shim_path)
        && shim_path.exists()
    {
        #[cfg(windows)]
        let retained_owned = shim_fallback.as_ref().is_some_and(|(_, contents)| {
            old_shim_fallback.as_deref() == Some(contents.as_slice())
                && supervised_cargo.as_ref().is_some_and(|shim| {
                    shim_matches(&shim_path, &shim.shim_contents).unwrap_or(false)
                })
        });
        #[cfg(not(windows))]
        let retained_owned = false;
        if !retained_owned {
            bail!(
                "{} exists without an rgo ownership record",
                shim_path.display()
            );
        }
    }
    #[cfg(windows)]
    if let (Some(previous), Some(next)) = (previous_shim, supervised_cargo.as_ref()) {
        if Path::new(&previous.shim_path) == shim_path
            && previous.shim_contents != next.shim_contents
        {
            bail!("Windows cannot replace an active Cargo shim; use a new versioned release");
        }
        if Path::new(&previous.real_cargo).canonicalize()?
            != Path::new(&next.real_cargo).canonicalize()?
        {
            bail!("Windows supervised upgrade cannot change the real Cargo proxy");
        }
    }
    let legacy_service = old_record
        .as_ref()
        .is_some_and(|record| record.schema_version == 1);
    let cfg_path = if undo {
        old_record
            .as_ref()
            .map(|record| Path::new(&record.config_file).to_path_buf())
            .unwrap_or(effective_cfg_path)
    } else {
        effective_cfg_path
    };
    let config_existed = cfg_path.exists();
    let current = cargo_config::read_or_empty(&cfg_path)?;
    let insp = cargo_config::inspect(&current)?;
    if insp.has_fence {
        let record = old_record.as_ref().with_context(|| {
            format!(
                "{} has an rgo fence but {} is missing; refusing to change unverified Cargo settings",
                cfg_path.display(),
                record_path.display()
            )
        })?;
        if !cargo_config::managed_fence_matches(
            &current,
            &record.managed_build_dir,
            record.wrapper_binary.as_deref(),
        )? {
            bail!(
                "managed Cargo settings in {} changed since setup; refusing to replace or remove user edits",
                cfg_path.display()
            );
        }
    }
    if supervised && insp.has_fence {
        bail!(
            "supervised setup cannot coexist with a global rgo Cargo fence; run native `rgo setup --undo` first"
        );
    }
    if previous_shim.is_some() && insp.has_fence {
        bail!(
            "supervised installation found a global rgo Cargo fence; refusing to change either activation"
        );
    }
    if supervised && insp.build_dir_outside_fence.is_some() {
        bail!(
            "supervised setup requires no global build.build-dir; remove the existing setting from {} before activation",
            cfg_path.display()
        );
    }
    let inner_path = paths.state_dir().join("inner-wrapper");
    let old_inner = read_optional(&inner_path)?;
    let stored_inner = old_inner
        .as_deref()
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let original_inner = insp
        .rustc_wrapper
        .clone()
        .or_else(|| {
            old_record
                .as_ref()
                .and_then(|r| r.original_rustc_wrapper.clone())
        })
        .or(stored_inner.clone());

    let mut managed_wrapper = None;

    let next = if supervised || (undo && previous_shim.is_some()) {
        current.clone()
    } else if undo {
        let removed = cargo_config::remove(&current)?;
        match (
            insp.has_fence,
            insp.rustc_wrapper.as_deref(),
            original_inner.as_deref(),
        ) {
            // An outside-fence wrapper is a later user choice and already
            // survives `remove`. Only restore the stored value when absent.
            (true, None, Some(wrapper)) => cargo_config::restore_rustc_wrapper(&removed, wrapper)?,
            _ => removed,
        }
    } else {
        if let Some(bd) = &insp.build_dir_outside_fence {
            bail!(
                "{} already sets build.build-dir = {bd:?} outside the rgo fence; remove it or run `rgo setup --undo` first",
                cfg_path.display()
            );
        }
        if insp.rustc_workspace_wrapper_outside_fence.is_some() {
            eprintln!(
                "note: keeping existing build.rustc-workspace-wrapper; rgo will not compose an outer wrapper for workspace invocations"
            );
        }
        if insp.has_include && !no_wrapper {
            eprintln!(
                "note: Cargo home config includes other files; keeping their possible wrapper settings and activating storage-only"
            );
        }
        let wrapper = if no_wrapper
            || insp.rustc_workspace_wrapper_outside_fence.is_some()
            || insp.has_include
        {
            None
        } else {
            Some(wrapper_path()?)
        };
        managed_wrapper = wrapper.clone();
        cargo_config::apply(
            &current,
            &Desired {
                build_dir: paths.build_dir_template(),
                rustc_wrapper: wrapper,
                rustc_workspace_wrapper: None,
            },
        )?
    };

    let mut prior_service_wrote_logs = false;
    if !undo && !no_service {
        let executable = std::env::current_exe().context("locating rgo executable")?;
        let previous = old_record
            .as_ref()
            .map(|record| Path::new(&record.rgo_binary));
        if legacy_service {
            service::verify_legacy_service_ownership(&executable, previous)?;
            service::verify_service_ownership(&executable, None)?;
            prior_service_wrote_logs = service::render_legacy(&executable)?.path.is_file();
        } else {
            service::verify_service_ownership(&executable, previous)?;
        }
        prior_service_wrote_logs |=
            service::scoped_service_used_legacy_logs(&executable, previous)?;
    }

    // A v1 installation used one fixed per-user name. Remove only its verified
    // old definition before recording v2 ownership, so an interrupted upgrade
    // can retry without losing track of a still-registered legacy service.
    if !undo && legacy_service && !no_service && !dry_run {
        let executable = std::env::current_exe().context("locating rgo executable")?;
        let previous = old_record
            .as_ref()
            .map(|record| Path::new(&record.rgo_binary));
        service::uninstall_legacy(&executable, previous).context(
            "could not stop the legacy daemon service; Cargo configuration is unchanged",
        )?;
    }

    // A previously no-service installation can later have a daemon registered
    // by direct setup. Refuse a no-service upgrade or undo in that case so its
    // old executable is not silently left running after the version switch.
    if no_service && !dry_run && (undo || old_record.is_some()) {
        let executable = std::env::current_exe().context("locating rgo executable")?;
        let service_status = if legacy_service {
            service::legacy_status(&executable)?
        } else {
            service::status(&executable)?
        };
        if service_status.installed || service_status.running {
            bail!(
                "a daemon service is still registered for this installation; omit --no-service so setup can manage it safely"
            );
        }
    }

    // Stop managed maintenance before restoring Cargo's previous configuration.
    // Removing binaries after undo must never leave a service referring to them.
    if undo && !no_service {
        let executable = std::env::current_exe().context("locating rgo executable")?;
        let previous = old_record
            .as_ref()
            .map(|record| Path::new(&record.rgo_binary));
        if legacy_service {
            service::verify_legacy_service_ownership(&executable, previous)?;
        } else {
            service::verify_service_ownership(&executable, previous)?;
        }
        if !dry_run {
            let uninstall = if legacy_service {
                service::uninstall_legacy(&executable, previous)
            } else {
                service::uninstall(&executable, previous)
            };
            uninstall.context(
                "could not quiesce the daemon service; Cargo configuration is unchanged. Stop the per-user rgo service and retry setup --undo",
            )?;
            println!("removed daemon service");
        }
    }

    let next_inner = if !supervised
        && insp.rustc_workspace_wrapper_outside_fence.is_none()
        && !insp.has_include
        && !no_wrapper
    {
        original_inner.as_deref()
    } else {
        None
    };
    let record_bytes = if !undo && (!dry_run || installer_plan_json) {
        let root = paths
            .root
            .to_str()
            .context("RGO_HOME must be UTF-8 to persist activation")?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let install_root = executable
            .parent()
            .context("rgo executable has no install directory")?;
        let record = InstallationRecord {
            schema_version: if supervised {
                INSTALL_RECORD_VERSION
            } else {
                2
            },
            cargo_home: cargo_home.display().to_string(),
            config_file: cfg_path.display().to_string(),
            rgo_home: root.to_owned(),
            install_root: install_root.display().to_string(),
            rgo_binary: executable.display().to_string(),
            wrapper_binary: managed_wrapper.clone(),
            original_rustc_wrapper: original_inner.clone(),
            managed_build_dir: paths.build_dir_template(),
            managed_keys: if supervised {
                vec![]
            } else if managed_wrapper.is_some() {
                vec!["build.build-dir".into(), "build.rustc-wrapper".into()]
            } else {
                vec!["build.build-dir".into()]
            },
            binary_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: rgo_protocol::PROTOCOL_VERSION,
            supervised_cargo: supervised_cargo.clone(),
        };
        Some(serde_json::to_vec_pretty(&record)?)
    } else {
        None
    };

    if installer_plan_json {
        let mut files = BTreeMap::new();
        #[cfg(windows)]
        let mut binaries = BTreeMap::new();
        #[cfg(not(windows))]
        let binaries: BTreeMap<String, String> = BTreeMap::new();
        let mut add = |path: &Path, contents: Option<String>, mode: u32| {
            files.insert(
                path.display().to_string(),
                contents.map(|contents| PlannedFile { contents, mode }),
            );
        };
        let root = paths.root.to_str().context("RGO_HOME must be UTF-8")?;
        add(&pointer_path, Some(format!("{root}\n")), 0o600);
        add(&inner_path, next_inner.map(str::to_owned), 0o600);
        add(
            &record_path,
            Some(String::from_utf8(
                record_bytes.clone().context("missing installer record")?,
            )?),
            0o600,
        );
        add(&owner_path, Some(format!("{cargo_home_text}\n")), 0o600);
        add(&mode_path, Some(format!("{desired_mode}\n")), 0o600);
        if let Some(shim) = &supervised_cargo {
            #[cfg(windows)]
            {
                let (path, contents) = shim_fallback.as_ref().context("missing Cargo fallback")?;
                add(path, Some(String::from_utf8(contents.clone())?), 0o600);
                binaries.insert(shim.shim_path.clone(), shim.shim_contents.clone());
            }
            #[cfg(unix)]
            add(
                Path::new(&shim.shim_path),
                Some(shim.shim_contents.clone()),
                0o755,
            );
        }
        if next != current {
            #[cfg(unix)]
            let mode = std::fs::metadata(&cfg_path)
                .map(|metadata| metadata.permissions().mode() & 0o7777)
                .unwrap_or(0o600);
            #[cfg(not(unix))]
            let mode = 0o600;
            add(&cfg_path, Some(next), mode);
        }
        println!(
            "{}",
            serde_json::to_string(&InstallerPlan {
                schema_version: 1,
                files,
                binaries,
            })?
        );
        return Ok(());
    }

    if !undo && !dry_run {
        // Build state and wrapper chaining must be ready before Cargo can see
        // the fence. If the config write fails, restore the previous state.
        paths.ensure_layout()?;
        // Migrate existing pins before updating Cargo activation. A service
        // may not be started in --no-service mode, so daemon startup alone
        // cannot close the upgrade window before a direct `cargo clean`.
        for dir in paths.checked_managed_build_dirs()? {
            if context::is_pinned_dir(&dir) {
                context::migrate_legacy_pin(&paths, &dir)?;
            }
        }
        let root = paths
            .root
            .to_str()
            .context("RGO_HOME must be UTF-8 to persist activation")?;
        let prepare = (|| -> Result<()> {
            atomic_write_state(&pointer_path, &format!("{root}\n"))?;
            write_optional(&inner_path, next_inner)?;
            #[cfg(windows)]
            if let Some((path, contents)) = &shim_fallback {
                atomic_write_state_bytes(path, contents)?;
            }
            atomic_write_state_bytes(
                &record_path,
                record_bytes
                    .as_deref()
                    .context("missing installer record")?,
            )?;
            #[cfg(debug_assertions)]
            if std::env::var_os("RGO_SETUP_TEST_EXIT_AFTER_RECORD").is_some() {
                std::process::exit(88);
            }
            atomic_write_state(&owner_path, &format!("{cargo_home_text}\n"))?;
            atomic_write_state(&mode_path, &format!("{desired_mode}\n"))?;
            if let Some(shim) = &supervised_cargo {
                write_shim(Path::new(&shim.shim_path), &shim.shim_contents)?;
            }
            Ok(())
        })();
        if let Err(error) = prepare {
            rollback_setup_state(&[
                (&pointer_path, old_pointer.as_deref()),
                (&inner_path, old_inner.as_deref()),
                (&record_path, old_record_bytes.as_deref()),
                (&owner_path, old_owner.as_deref()),
                (&mode_path, old_mode.as_deref()),
            ])?;
            if let Some(shim) = &supervised_cargo {
                let previous_at_target =
                    previous_shim.filter(|previous| previous.shim_path == shim.shim_path);
                restore_previous_shim(
                    Path::new(&shim.shim_path),
                    previous_at_target,
                    shim_preexisted,
                )?;
            }
            #[cfg(windows)]
            if let Some((path, _)) = &shim_fallback {
                restore_optional(path, old_shim_fallback.as_deref())?;
            }
            return Err(error);
        }
    }

    if next == current {
        println!("{} already up to date", cfg_path.display());
    } else if dry_run {
        println!(
            "--- {} (current)\n+++ {} (after)\n",
            cfg_path.display(),
            cfg_path.display()
        );
        print!("{}", simple_diff(&current, &next));
    } else {
        // Cooperative writers use the setup lock. Detect edits from a writer
        // that does not use it before the atomic replacement as well.
        let update_config = (|| -> Result<()> {
            if let Some(parent) = cfg_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !undo && cargo_config::effective_home_config(&cargo_home) != cfg_path {
                bail!("Cargo home config precedence changed during setup; retry");
            }
            if cargo_config::read_or_empty(&cfg_path)? != current {
                bail!("{} changed during setup; retry", cfg_path.display());
            }
            atomic_write_config(&cfg_path, &next)
        })();
        if let Err(error) = update_config {
            if !undo {
                rollback_setup_state(&[
                    (&pointer_path, old_pointer.as_deref()),
                    (&inner_path, old_inner.as_deref()),
                    (&record_path, old_record_bytes.as_deref()),
                    (&owner_path, old_owner.as_deref()),
                    (&mode_path, old_mode.as_deref()),
                ])?;
                if let Some(shim) = &supervised_cargo {
                    let previous_at_target =
                        previous_shim.filter(|previous| previous.shim_path == shim.shim_path);
                    restore_previous_shim(
                        Path::new(&shim.shim_path),
                        previous_at_target,
                        shim_preexisted,
                    )?;
                }
                #[cfg(windows)]
                if let Some((path, _)) = &shim_fallback {
                    restore_optional(path, old_shim_fallback.as_deref())?;
                }
            }
            return Err(error);
        }
        println!("updated {}", cfg_path.display());
    }
    if dry_run {
        if let Some(shim) = previous_shim.filter(|_| undo) {
            #[cfg(windows)]
            if retain_previous_shim {
                println!(
                    "would retain supervised Cargo shim as an ordinary Cargo fallback {}",
                    shim.shim_path
                );
            } else {
                println!("would remove supervised Cargo shim {}", shim.shim_path);
            }
            #[cfg(not(windows))]
            println!("would remove supervised Cargo shim {}", shim.shim_path);
        } else if let Some(shim) = &supervised_cargo {
            println!("would install supervised Cargo shim {}", shim.shim_path);
        }
    }

    if undo && !dry_run {
        if let Some(shim) = previous_shim {
            let path = Path::new(&shim.shim_path);
            if path.exists() {
                if !shim_matches(path, &shim.shim_contents)? {
                    bail!(
                        "{} changed during undo; refusing to remove it",
                        path.display()
                    );
                }
                #[cfg(windows)]
                let retained = retain_previous_shim;
                #[cfg(not(windows))]
                let retained = false;
                if !retained {
                    std::fs::remove_file(path)
                        .with_context(|| format!("removing {}", path.display()))?;
                } else {
                    println!(
                        "retained Cargo launcher for old shells; it now uses ordinary Cargo storage: {}",
                        path.display()
                    );
                }
            }
        }
        write_optional(&inner_path, None)?;
        if old_pointer.as_deref().is_some_and(|value| {
            std::str::from_utf8(value).is_ok_and(|value| {
                value.trim_end_matches(['\r', '\n']) == paths.root.to_string_lossy()
            })
        }) {
            write_optional(&pointer_path, None)?;
        }
        write_optional(&record_path, None)?;
        if old_owner.is_some() {
            write_optional(&owner_path, None)?;
        }
    }
    if !undo && !dry_run {
        println!("managed build storage: {}", paths.builds_dir().display());
        if let Some(shim) = &supervised_cargo {
            println!("supervised Cargo shim: {}", shim.shim_path);
            let shim_dir = Path::new(&shim.shim_path)
                .parent()
                .context("Cargo shim has no parent")?;
            #[cfg(unix)]
            println!(
                "activate in a shell: export PATH={}:\"$PATH\"",
                shell_quote(shim_dir)?
            );
            #[cfg(windows)]
            println!(
                "activate in PowerShell: $env:PATH = '{};' + $env:PATH",
                shim_dir.display().to_string().replace('\'', "''")
            );
        }
    }

    let mut maintenance_failure = None;
    if no_service {
        if !undo {
            if supervised {
                println!(
                    "service: skipped (--no-service); supervised Cargo starts maintenance only when [gc].auto = true"
                );
            } else {
                println!("service: skipped (--no-service); no background maintenance");
            }
        }
    } else {
        let executable = std::env::current_exe().context("locating rgo executable")?;
        if dry_run {
            let rendered = if undo && legacy_service {
                service::render_legacy(&executable)
            } else {
                service::render(&executable)
            };
            match rendered {
                Ok(rendered) => {
                    if undo {
                        println!(
                            "would remove service {} ({})",
                            rendered.label,
                            rendered.path.display()
                        );
                    } else {
                        println!(
                            "would install service {} at {}",
                            rendered.label,
                            rendered.path.display()
                        );
                        print!("{}", rendered.contents);
                    }
                }
                Err(error) => eprintln!("warning: service unavailable: {error:#}"),
            }
        } else if !undo {
            paths.ensure_layout()?;
            let previous = if legacy_service {
                None
            } else {
                old_record
                    .as_ref()
                    .map(|record| Path::new(&record.rgo_binary))
            };
            match service::install(&executable, &paths, previous) {
                Ok(rendered) => {
                    println!("installed daemon service {}", rendered.label);
                    match wait_for_daemon(&executable, &paths) {
                        Ok(()) => {
                            if let Err(error) =
                                service::prune_legacy_logs(&paths, prior_service_wrote_logs)
                            {
                                eprintln!(
                                    "warning: old daemon logs could not be removed: {error:#}"
                                );
                            }
                            println!("daemon service: healthy");
                        }
                        Err(error) => {
                            eprintln!(
                                "warning: daemon service installed but not healthy: {error:#}"
                            );
                            eprintln!(
                                "remediation: inspect `rgo doctor` and the service logs, then rerun `rgo setup`"
                            );
                            maintenance_failure = Some(format!(
                                "installed daemon service did not become healthy: {error:#}"
                            ));
                        }
                    }
                }
                Err(error) => {
                    eprintln!("warning: could not install daemon service: {error:#}");
                    eprintln!(
                        "remediation: run `rgo setup` again after enabling your per-user service manager, or use `rgo setup --no-service`"
                    );
                    maintenance_failure =
                        Some(format!("could not install daemon service: {error:#}"));
                }
            }
        }
    }
    if let Some(error) = maintenance_failure {
        if old_record.is_none() {
            // A first activation has no prior managed service to preserve.
            // Release both setup locks before using the normal ownership-
            // checked undo path so Cargo is not left pointing at an install
            // that could never start its maintenance service.
            drop(_root_setup_lock);
            drop(_setup_lock);
            let rollback = run(true, false, false, false, false, false, None).and_then(|_| {
                // An empty new root can return to its exact pre-setup mode.
                // Keep the marker if Cargo entered this root during setup.
                if !has_build_storage(&paths)? {
                    rollback_setup_state(&[
                        (&pointer_path, old_pointer.as_deref()),
                        (&inner_path, old_inner.as_deref()),
                        (&record_path, old_record_bytes.as_deref()),
                        (&owner_path, old_owner.as_deref()),
                        (&mode_path, old_mode.as_deref()),
                    ])?;
                }
                if !config_existed
                    && cargo_config::read_or_empty(&cfg_path)? == cargo_config::remove(&next)?
                {
                    std::fs::remove_file(&cfg_path)?;
                }
                Ok(())
            });
            return match rollback {
                Ok(()) => Err(anyhow::anyhow!(
                    "background maintenance is unavailable ({error}); new activation was rolled back"
                )),
                Err(rollback) => Err(anyhow::anyhow!(
                    "background maintenance is unavailable ({error}); automatic rollback failed ({rollback:#}); inspect `rgo doctor` and run `rgo setup --undo`"
                )),
            };
        }
        bail!(
            "Cargo storage is configured, but background maintenance is unavailable ({error}); setup is incomplete"
        );
    }
    if !undo && !dry_run {
        println!("next: run any `cargo build`; then `rgo status`.");
    }
    Ok(())
}

/// Probe the service-created daemon directly. Starting one through
/// `ensure_running` here would hide a broken launchd/systemd/task registration.
fn wait_for_daemon(executable: &Path, paths: &RgoPaths) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let failure = match service::status(executable) {
            Ok(status) if status.supported && status.installed && status.running => {
                match ipc::request_with_timeout(
                    &paths.socket_path(),
                    Request::QueryStatus,
                    Duration::from_secs(1),
                ) {
                    Ok(Response::Status(status)) if status.protocol_compatible => return Ok(()),
                    Ok(Response::Status(_)) => "daemon protocol is incompatible".into(),
                    Ok(other) => format!("unexpected daemon response: {other:?}"),
                    Err(error) => format!("daemon IPC unavailable: {error:#}"),
                }
            }
            Ok(status) => format!("service {}", status.detail),
            Err(error) => format!("service status unavailable: {error:#}"),
        };
        if Instant::now() >= deadline {
            bail!("{failure}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wrapper_path() -> Result<String> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("exe has no parent")?;
    let name = if cfg!(windows) {
        "rgo-rustc-wrapper.exe"
    } else {
        "rgo-rustc-wrapper"
    };
    let p = dir.join(name);
    if !p.exists() {
        bail!(
            "{} not found next to rgo; reinstall or use --no-wrapper",
            p.display()
        );
    }
    let output = Command::new(&p)
        .arg("--rgo-version")
        .output()
        .with_context(|| format!("checking {}", p.display()))?;
    let expected = format!(
        "rgo-rustc-wrapper {} protocol {}",
        env!("CARGO_PKG_VERSION"),
        rgo_protocol::PROTOCOL_VERSION
    );
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != expected {
        bail!(
            "{} is not a matching rgo wrapper ({expected}); reinstall the matched binaries or use --no-wrapper",
            p.display()
        );
    }
    Ok(p.display().to_string())
}

fn prepare_supervised_cargo(
    cargo_home: &Path,
    rgo_home: &Path,
    shim: &Path,
    requested: Option<&Path>,
) -> Result<SupervisedCargo> {
    let installed_shim = shim.canonicalize().ok();
    let real = if let Some(path) = requested {
        path.to_path_buf()
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(shim_name()))
            .find(|path| {
                path != shim
                    && path.is_file()
                    && path.canonicalize().ok().is_some_and(|resolved| {
                        !is_rgo_cargo_shim(&resolved) && installed_shim.as_ref() != Some(&resolved)
                    })
            })
            .context(
                "no real Cargo executable found on PATH; pass --real-cargo /absolute/path/to/cargo",
            )?
    };
    if !real.is_absolute() || real.file_name().is_none_or(|name| name != shim_name()) {
        bail!(
            "the real Cargo proxy must be an absolute path whose basename is `{}`",
            shim_name()
        );
    }
    let resolved_real = real.canonicalize().context("resolving real Cargo")?;
    if resolved_real == std::env::current_exe()?.canonicalize()?
        || real == shim
        || installed_shim.as_ref() == Some(&resolved_real)
        || is_rgo_cargo_shim(&resolved_real)
    {
        bail!("the real Cargo path points to the rgo launcher");
    }
    let version = Command::new(&real)
        .arg("--version")
        .output()
        .with_context(|| format!("checking {}", real.display()))?;
    if !version.status.success()
        || !cargo_config::supports_build_dir(&String::from_utf8_lossy(&version.stdout))
    {
        bail!(
            "supervised setup requires Cargo 1.91 or newer for build.build-dir; {} reported {:?}",
            real.display(),
            String::from_utf8_lossy(&version.stdout).trim()
        );
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    #[cfg(windows)]
    let _ = (cargo_home, rgo_home);
    #[cfg(unix)]
    let contents = format!(
        "#!/bin/sh\nexec {} cargo-shim --real-cargo {} --cargo-home {} --rgo-home {} -- \"$@\"\n",
        shell_quote(&executable)?,
        shell_quote(&real)?,
        shell_quote(cargo_home)?,
        shell_quote(rgo_home)?,
    );
    #[cfg(windows)]
    let contents = format!(
        "binary-blake3:{}",
        blake3::hash(&std::fs::read(&executable)?).to_hex()
    );
    Ok(SupervisedCargo {
        shim_path: shim.display().to_string(),
        real_cargo: real.display().to_string(),
        shim_contents: contents,
    })
}

fn is_rgo_cargo_shim(path: &Path) -> bool {
    if !path.file_name().is_some_and(|name| name == shim_name()) {
        return false;
    }
    path.ancestors().any(|directory| {
        directory.file_name().is_some_and(|name| name == "shims")
            && directory
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "rgo")
    })
}

#[cfg(windows)]
fn verified_windows_fallback(shim: &Path, cargo_home: &Path, real_cargo: &Path) -> Result<bool> {
    let path = super::windows_cargo_entry::fallback_path(shim)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("{} is not a plain file", path.display());
    }
    let fallback: super::windows_cargo_entry::ShimFallback =
        serde_json::from_slice(&std::fs::read(&path)?)?;
    if fallback.schema_version != 1
        || fallback.cargo_home != cargo_home
        || fallback.real_cargo != real_cargo
    {
        bail!("{} changed since setup", path.display());
    }
    Ok(true)
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .context("shell launcher paths must be UTF-8")?;
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn restore_previous_shim(
    path: &Path,
    previous: Option<&SupervisedCargo>,
    existed_before: bool,
) -> Result<()> {
    #[cfg(unix)]
    let _ = existed_before;
    #[cfg(windows)]
    if !existed_before {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    }
    match previous {
        #[cfg(unix)]
        Some(previous) => write_shim(path, &previous.shim_contents),
        #[cfg(windows)]
        Some(previous) if shim_matches(path, &previous.shim_contents)? => Ok(()),
        #[cfg(windows)]
        Some(_) => bail!("cannot restore a changed Windows Cargo shim"),
        #[cfg(windows)]
        None if existed_before => Ok(()),
        None => {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            Ok(())
        }
    }
}

fn has_build_storage(paths: &RgoPaths) -> Result<bool> {
    match std::fs::read_dir(paths.builds_dir()) {
        Ok(mut entries) => entries
            .next()
            .transpose()
            .map(|entry| entry.is_some())
            .map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspecting existing managed build storage"),
    }
}

#[cfg(unix)]
fn write_shim(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().context("Cargo shim has no parent")?;
    if parent.is_symlink() || parent.parent().is_some_and(Path::is_symlink) {
        bail!("refusing a symlinked Cargo shim directory");
    }
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".cargo.{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        output.write_all(contents.as_bytes())?;
        output.sync_all()?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        replace_file_atomically(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
fn write_shim(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    if path.exists() {
        if shim_matches(path, contents)? {
            return Ok(());
        }
        bail!("{} differs from its owned Cargo launcher", path.display());
    }
    let source = std::env::current_exe()?.canonicalize()?;
    if !shim_matches(&source, contents)? {
        bail!("the running rgo executable changed during supervised setup");
    }
    let parent = path.parent().context("Cargo shim has no parent")?;
    if parent.is_symlink()
        || parent.parent().is_some_and(Path::is_symlink)
        || (parent.file_name().is_some_and(|name| name != "shims")
            && parent
                .parent()
                .and_then(Path::parent)
                .is_some_and(Path::is_symlink))
    {
        bail!("refusing a symlinked Cargo shim directory");
    }
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".cargo.{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let result = (|| -> Result<()> {
        let mut input = File::open(&source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        replace_file_atomically(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn shim_name() -> &'static str {
    if cfg!(windows) { "cargo.exe" } else { "cargo" }
}

fn shim_matches(path: &Path, contents: &str) -> Result<bool> {
    let bytes = std::fs::read(path)?;
    if let Some(digest) = contents.strip_prefix("binary-blake3:") {
        return Ok(blake3::hash(&bytes).to_hex().as_str() == digest);
    }
    Ok(bytes == contents.as_bytes())
}

fn lock_setup(cargo_home: &Path) -> Result<File> {
    lock_named(cargo_home, ".rgo-setup.lock")
}

fn lock_root_setup(paths: &RgoPaths) -> Result<File> {
    paths.ensure_layout()?;
    lock_named(&paths.state_dir(), ".rgo-service.lock")
}

fn lock_named(directory: &Path, name: &str) -> Result<File> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_optional(path: &Path, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => atomic_write_state(path, value),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        },
    }
}

fn restore_optional(path: &Path, value: Option<&[u8]>) -> Result<()> {
    match value {
        Some(value) => atomic_write_state_bytes(path, value),
        None => write_optional(path, None),
    }
}

fn rollback_setup_state(entries: &[(&Path, Option<&[u8]>)]) -> Result<()> {
    for (path, contents) in entries {
        restore_optional(path, *contents)?;
    }
    Ok(())
}

fn atomic_write_config(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().context("Cargo config has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&temp, contents.as_bytes())
        .with_context(|| format!("writing {}", temp.display()))?;
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(&temp, metadata.permissions())?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    replace_file_atomically(&temp, path)?;
    Ok(())
}

fn atomic_write_state(path: &Path, contents: &str) -> Result<()> {
    atomic_write_state_bytes(path, contents.as_bytes())
}

fn atomic_write_state_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("rgo state path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&temp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    replace_file_atomically(&temp, path)
}

#[cfg(not(windows))]
fn replace_file_atomically(temp: &Path, path: &Path) -> Result<()> {
    std::fs::rename(temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_file_atomically(temp: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let mut source: Vec<u16> = temp.as_os_str().encode_wide().collect();
    let mut destination: Vec<u16> = path.as_os_str().encode_wide().collect();
    source.push(0);
    destination.push(0);
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x00000001;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x00000008;

fn simple_diff(a: &str, b: &str) -> String {
    let mut out = String::new();
    for l in a.lines().filter(|l| !b.contains(l)) {
        out.push_str(&format!("-{l}\n"));
    }
    for l in b.lines().filter(|l| !a.contains(l)) {
        out.push_str(&format!("+{l}\n"));
    }
    out
}
