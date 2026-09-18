use std::path::Path;

use anyhow::{Context, Result, bail};
use rgo_core::cargo_config::{self, Desired};
use rgo_core::paths::{RgoPaths, cargo_home};
use rgo_core::service;

pub fn run(undo: bool, dry_run: bool, no_service: bool, no_wrapper: bool) -> Result<()> {
    let paths = RgoPaths::discover()?;
    let cfg_path = cargo_home()?.join("config.toml");
    let current = cargo_config::read_or_empty(&cfg_path)?;
    let insp = cargo_config::inspect(&current)?;
    let stored_inner = std::fs::read_to_string(paths.state_dir().join("inner-wrapper"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let original_inner = insp.rustc_wrapper.clone().or(stored_inner);

    let next = if undo {
        let removed = cargo_config::remove(&current)?;
        match (insp.has_fence, original_inner.as_deref()) {
            (true, Some(wrapper)) => cargo_config::restore_rustc_wrapper(&removed, wrapper)?,
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
        let wrapper = if no_wrapper || insp.rustc_workspace_wrapper_outside_fence.is_some() {
            None
        } else {
            Some(wrapper_path()?)
        };
        cargo_config::apply(
            &current,
            &Desired {
                build_dir: paths.build_dir_template(),
                rustc_wrapper: wrapper,
                rustc_workspace_wrapper: None,
            },
        )?
    };

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
        if let Some(parent) = cfg_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write_config(&cfg_path, &next)?;
        println!("updated {}", cfg_path.display());
    }

    if !undo && !dry_run {
        paths.ensure_layout()?;
        if insp.rustc_workspace_wrapper_outside_fence.is_none()
            && !no_wrapper
            && let Some(inner) = original_inner.as_deref()
        {
            atomic_write_state(&paths.state_dir().join("inner-wrapper"), inner)?;
        } else {
            let _ = std::fs::remove_file(paths.state_dir().join("inner-wrapper"));
        }
    } else if undo && !dry_run {
        let _ = std::fs::remove_file(paths.state_dir().join("inner-wrapper"));
    }
    if !undo && !dry_run {
        println!("managed build storage: {}", paths.builds_dir().display());
    }

    if no_service {
        if dry_run {
            println!("service: skipped (--no-service)");
        }
    } else {
        let executable = std::env::current_exe().context("locating rgo executable")?;
        if dry_run {
            match service::render(&executable) {
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
        } else if undo {
            if let Err(error) = service::uninstall(&executable) {
                eprintln!("warning: could not remove daemon service: {error:#}");
                eprintln!(
                    "remediation: remove the per-user rgo service with the platform service manager"
                );
            } else {
                println!("removed daemon service");
            }
        } else {
            paths.ensure_layout()?;
            match service::install(&executable, &paths) {
                Ok(rendered) => println!("installed daemon service {}", rendered.label),
                Err(error) => {
                    eprintln!("warning: could not install daemon service: {error:#}");
                    eprintln!(
                        "remediation: run `rgo setup` again after enabling your per-user service manager, or use `rgo setup --no-service`"
                    );
                }
            }
        }
    }
    if !undo && !dry_run {
        println!("next: run any `cargo build`; then `rgo status`.");
    }
    Ok(())
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
    Ok(p.display().to_string())
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
    std::fs::write(&temp, contents.as_bytes())?;
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
