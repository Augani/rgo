//! `rgo` CLI. Thin shell over `rgo-core`. Unknown subcommands pass through to cargo.

mod bounded_log;
mod cmd;

use std::ffi::OsString;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rgo", version, about = "Bounded, machine-managed Rust build storage.", long_about = None)]
#[command(args_conflicts_with_subcommands = true, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Anything that is not an rgo command is passed to cargo unchanged, e.g. `rgo build --release`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    cargo_args: Vec<OsString>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Experimental supervised Cargo launcher used only in private probes.
    #[command(hide = true)]
    CargoShim {
        #[arg(long)]
        real_cargo: std::path::PathBuf,
        /// Cargo home that owns this PATH launcher; a different active home is passed through.
        #[arg(long)]
        cargo_home: Option<std::path::PathBuf>,
        /// Storage root that owns this PATH launcher; an override is passed through.
        #[arg(long)]
        rgo_home: Option<std::path::PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cargo_args: Vec<OsString>,
    },
    /// Inspect and verify the opt-in compiler-result cache.
    Cache {
        #[command(subcommand)]
        command: cmd::cache::Command,
    },
    /// Activate managed storage and a background service.
    Setup {
        #[arg(long)]
        undo: bool,
        #[arg(long)]
        dry_run: bool,
        /// Private installer protocol: report the exact no-service activation writes.
        #[arg(long, hide = true, requires = "no_service", conflicts_with_all = ["undo", "dry_run"])]
        installer_plan_json: bool,
        /// Skip installing the launchd/systemd/schtasks service.
        #[arg(long)]
        no_service: bool,
        /// Do not set build.rustc-workspace-wrapper (context attribution then relies on `rgo <cmd>`).
        #[arg(long)]
        no_wrapper: bool,
        /// Install an opt-in Cargo launcher under $CARGO_HOME/rgo/shims.
        #[arg(long)]
        supervised: bool,
        /// Absolute path to the real Cargo proxy, retaining its `cargo` basename.
        #[arg(long, requires = "supervised")]
        real_cargo: Option<std::path::PathBuf>,
    },
    /// Storage summary: managed bytes, budget, reclaimable.
    Status,
    /// Check configuration precedence, conflicting wrappers, toolchains, filesystem, service health.
    Doctor {
        /// Emit structured diagnostics for installers and other tools.
        #[arg(long)]
        json: bool,
        /// Run a disposable plain-Cargo build and fail if managed activation is not observed.
        #[arg(long)]
        verify: bool,
    },
    /// Reclaim storage in tier order (tmp, orphans, stale incremental, stale contexts, pressure).
    Gc {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        aggressive: bool,
        /// Run only when the configured automatic-GC trigger is active.
        #[arg(long)]
        auto: bool,
        /// Reclaim until managed storage is at or below this size.
        #[arg(long)]
        target: Option<String>,
    },
    /// List managed build contexts.
    Ls,
    /// Protect a context from GC.
    Pin {
        id: String,
    },
    Unpin {
        id: String,
    },
    /// Remove one context now (refuses if it looks live).
    Clean {
        id: String,
    },
    /// Report legacy `target/` directories and their total allocated storage.
    Adopt {
        /// Former deletion option; now fails because safe selective removal is unproven.
        #[arg(long)]
        delete: bool,
        roots: Vec<std::path::PathBuf>,
    },
    /// Run the coordination daemon (Phase 2).
    Daemon {
        #[arg(long)]
        foreground: bool,
        /// Storage root for a service-launched daemon, independent of its environment.
        #[arg(long, hide = true)]
        home: Option<std::path::PathBuf>,
    },
}

fn main() -> Result<()> {
    #[cfg(windows)]
    if cmd::windows_cargo_entry::is_shim_invocation()? {
        return cmd::windows_cargo_entry::run();
    }
    let cli = Cli::parse();
    let daemon_log = match &cli.cmd {
        Some(Cmd::Daemon {
            foreground: true,
            home,
        }) => {
            let paths = if let Some(root) = home {
                rgo_core::paths::RgoPaths { root: root.clone() }
            } else {
                rgo_core::paths::RgoPaths::discover()?
            };
            Some(bounded_log::BoundedLog::new(
                paths.logs_dir().join("daemon.log"),
            ))
        }
        _ => None,
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("RGO_LOG").add_directive("info".parse()?),
        )
        .without_time()
        .with_target(false);
    if let Some(log) = &daemon_log {
        subscriber.with_writer(log.clone()).init();
        let log = log.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            let _ = log.append(format!("daemon panic: {panic}\n").as_bytes());
            previous(panic);
        }));
    } else {
        subscriber.init();
    }

    let result = match cli.cmd {
        Some(Cmd::CargoShim {
            real_cargo,
            cargo_home,
            rgo_home,
            cargo_args,
        }) => cmd::cargo_shim::run(
            &real_cargo,
            cargo_home.as_deref(),
            rgo_home.as_deref(),
            None,
            cargo_args,
        ),
        Some(Cmd::Cache { command }) => cmd::cache::run(command),
        Some(Cmd::Setup {
            undo,
            dry_run,
            installer_plan_json,
            no_service,
            no_wrapper,
            supervised,
            real_cargo,
        }) => cmd::setup::run(
            undo,
            dry_run,
            installer_plan_json,
            no_service,
            no_wrapper,
            supervised,
            real_cargo,
        ),
        Some(Cmd::Status) => cmd::status::run(),
        Some(Cmd::Doctor { json, verify }) => cmd::doctor::run(json, verify),
        Some(Cmd::Gc {
            dry_run,
            aggressive,
            auto,
            target,
        }) => cmd::gc::run(dry_run, aggressive, auto, target),
        Some(Cmd::Ls) => cmd::ls::run(),
        Some(Cmd::Pin { id }) => cmd::pin::run(&id, true),
        Some(Cmd::Unpin { id }) => cmd::pin::run(&id, false),
        Some(Cmd::Clean { id }) => cmd::clean::run(&id),
        Some(Cmd::Adopt { roots, delete }) => cmd::adopt::run(roots, delete),
        Some(Cmd::Daemon { foreground, home }) => cmd::daemon::run(foreground, home),
        None => cmd::passthrough::run(cli.cargo_args),
    };
    if let Some(error) = result.as_ref().err().filter(|_| daemon_log.is_some()) {
        tracing::error!(error = %format!("{error:#}"), "daemon exited with an error");
    }
    result
}
