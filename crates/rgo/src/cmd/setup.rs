use anyhow::{Context, Result, bail};
use rgo_core::cargo_config::{self, Desired};
use rgo_core::paths::{RgoPaths, cargo_home};

pub fn run(undo: bool, dry_run: bool, _no_service: bool, no_wrapper: bool) -> Result<()> {
    let paths = RgoPaths::discover()?;
    let cfg_path = cargo_home()?.join("config.toml");
    let current = cargo_config::read_or_empty(&cfg_path)?;
    let insp = cargo_config::inspect(&current)?;

    let next = if undo {
        cargo_config::remove(&current)?
    } else {
        if let Some(bd) = &insp.build_dir_outside_fence {
            bail!(
                "{} already sets build.build-dir = {bd:?} outside the rgo fence; remove it or run `rgo setup --undo` first",
                cfg_path.display()
            );
        }
        let wrapper = if no_wrapper || insp.rustc_workspace_wrapper_outside_fence.is_some() {
            if let Some(w) = &insp.rustc_workspace_wrapper_outside_fence {
                eprintln!(
                    "note: keeping your existing build.rustc-workspace-wrapper = {w:?}; context attribution will use `rgo <cmd>` only"
                );
            }
            None
        } else {
            Some(wrapper_path()?)
        };
        cargo_config::apply(
            &current,
            &Desired {
                build_dir: paths.build_dir_template(),
                rustc_workspace_wrapper: wrapper,
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
        std::fs::write(&cfg_path, &next)
            .with_context(|| format!("writing {}", cfg_path.display()))?;
        println!("updated {}", cfg_path.display());
    }

    if !undo && !dry_run {
        paths.ensure_layout()?;
        if let Some(inner) = insp.rustc_wrapper.as_deref() {
            std::fs::write(paths.state_dir().join("inner-wrapper"), inner)?;
        } else {
            let _ = std::fs::remove_file(paths.state_dir().join("inner-wrapper"));
        }
    } else if undo && !dry_run {
        let _ = std::fs::remove_file(paths.state_dir().join("inner-wrapper"));
    }
    if !undo && !dry_run {
        println!("managed build storage: {}", paths.builds_dir().display());
        // TODO(phase 1, step 8): install launchd / systemd --user / schtasks service unless _no_service.
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
