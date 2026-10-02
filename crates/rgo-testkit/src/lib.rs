//! Throwaway sandboxes for integration tests. Each `Sandbox` gets its own HOME,
//! CARGO_HOME (registry is shared read-only via `CARGO_HOME` copy-on-first-use later),
//! and RGO_HOME so tests never touch the developer's real machine state.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use tempfile::TempDir;

pub struct Sandbox {
    _root: TempDir,
    pub home: PathBuf,
    pub cargo_home: PathBuf,
    pub rgo_home: PathBuf,
    pub projects: PathBuf,
}

impl Sandbox {
    pub fn new() -> Result<Self> {
        // Filesystem-specific CI probes must place the *whole* private sandbox
        // on the mounted volume. Setting RGO_HOME in the parent process is not
        // enough because every child command receives this sandbox's RGO_HOME.
        let root = if let Some(parent) = std::env::var_os("RGO_TEST_SANDBOX_PARENT") {
            tempfile::tempdir_in(parent)?
        } else {
            tempfile::tempdir()?
        };
        let home = root.path().join("home");
        let cargo_home = home.join(".cargo");
        let rgo_home = home.join(".rgo");
        let projects = root.path().join("projects");
        for d in [&home, &cargo_home, &rgo_home, &projects] {
            std::fs::create_dir_all(d)?;
        }
        Ok(Self {
            _root: root,
            home,
            cargo_home,
            rgo_home,
            projects,
        })
    }

    /// A `Command` with the sandbox environment applied.
    pub fn cmd(&self, program: impl AsRef<Path>) -> Command {
        let mut c = Command::new(program.as_ref());
        #[cfg(not(windows))]
        c.env_clear();
        #[cfg(windows)]
        {
            // The runner's MSVC linker configuration is distributed across
            // process variables and a full clear makes rustc select Git's
            // unrelated link.exe. Keep the toolchain environment while still
            // isolating Cargo/rgo state and removing inherited overrides.
            for key in [
                "CARGO_TARGET_DIR",
                "CARGO_BUILD_BUILD_DIR",
                "RUSTC_WRAPPER",
                "RUSTC_WORKSPACE_WRAPPER",
                "CARGO_BUILD_RUSTC_WRAPPER",
                "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
                "RUSTC",
                "RUSTDOC",
                "CARGO_BUILD_RUSTC",
                "CARGO_BUILD_RUSTDOC",
                "RGO_INNER_RUSTC_WRAPPER",
                "RGO_BYPASS",
                "RGO_LEASE_ID",
            ] {
                c.env_remove(key);
            }
        }
        c.env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("RGO_HOME", &self.rgo_home)
            .env(
                "RUSTUP_HOME",
                std::env::var_os("RUSTUP_HOME")
                    .unwrap_or_else(|| real_home().join(".rustup").into()),
            )
            .env(
                "RUSTUP_TOOLCHAIN",
                std::env::var_os("RUSTUP_TOOLCHAIN").unwrap_or_else(|| "stable".into()),
            );
        c
    }

    pub fn cargo(&self) -> Command {
        self.cmd("cargo")
    }

    /// Minimal dependency-free binary crate so tests stay offline.
    pub fn simple_bin(&self, name: &str) -> Result<PathBuf> {
        let dir = self.projects.join(name);
        std::fs::create_dir_all(dir.join("src"))?;
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n"
            ),
        )?;
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() { println!(\"hello from rgo test\"); }\n",
        )?;
        Ok(dir)
    }

    /// Workspace with `members` binary crates, plus one shared lib they all depend on.
    pub fn workspace(&self, name: &str, members: &[&str]) -> Result<PathBuf> {
        let dir = self.projects.join(name);
        std::fs::create_dir_all(&dir)?;
        let list = members
            .iter()
            .map(|m| format!("\"{m}\""))
            .chain(["\"common\"".to_owned()])
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[workspace]\nresolver = \"2\"\nmembers = [{list}]\n"),
        )?;
        std::fs::create_dir_all(dir.join("common/src"))?;
        std::fs::write(
            dir.join("common/Cargo.toml"),
            "[package]\nname = \"common\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )?;
        std::fs::write(
            dir.join("common/src/lib.rs"),
            "pub fn greet() -> &'static str { \"hi\" }\n",
        )?;
        for m in members {
            std::fs::create_dir_all(dir.join(m).join("src"))?;
            std::fs::write(
                dir.join(m).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{m}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncommon = {{ path = \"../common\" }}\n"
                ),
            )?;
            std::fs::write(
                dir.join(m).join("src/main.rs"),
                "fn main() { println!(\"{}\", common::greet()); }\n",
            )?;
        }
        Ok(dir)
    }

    pub fn write_cargo_config(&self, contents: &str) -> Result<()> {
        std::fs::write(self.cargo_home.join("config.toml"), contents)
            .context("writing sandbox cargo config")
    }
}

/// `cargo test -p rgo-storage` does not rebuild sibling bin packages, so integration tests that
/// rely on `rgo-rustc-wrapper` must build it explicitly (in the real environment, not the sandbox).
pub fn ensure_workspace_bins_built() -> Result<()> {
    // Integration tests in one process run concurrently. Repeated builds can
    // relink a binary just as another test starts it; build the pair once.
    static BUILT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    match BUILT.get_or_init(|| {
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args([
                "build",
                "--quiet",
                "-p",
                "rgo-rustc-wrapper",
                "-p",
                "rgo-storage",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .map_err(|error| format!("building workspace bins: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("building workspace bins failed".into())
        }
    }) {
        Ok(()) => Ok(()),
        Err(error) => anyhow::bail!("{error}"),
    }
}

fn real_home() -> PathBuf {
    PathBuf::from(
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("HOME"),
    )
}
