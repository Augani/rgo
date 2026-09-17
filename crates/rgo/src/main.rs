//! `rgo` CLI. Thin shell over `rgo-core`. Unknown subcommands pass through to cargo.

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
    /// One-time machine setup: config fence in $CARGO_HOME/config.toml, ~/.rgo layout, background service.
    Setup {
        #[arg(long)]
        undo: bool,
        #[arg(long)]
        dry_run: bool,
        /// Skip installing the launchd/systemd/schtasks service.
        #[arg(long)]
        no_service: bool,
        /// Do not set build.rustc-workspace-wrapper (context attribution then relies on `rgo <cmd>`).
        #[arg(long)]
        no_wrapper: bool,
    },
    /// Storage summary: managed bytes, budget, reclaimable.
    Status,
    /// Check configuration precedence, conflicting wrappers, toolchains, filesystem, service health.
    Doctor,
    /// Reclaim storage in tier order (tmp, orphans, stale incremental, stale contexts, pressure).
    Gc {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        aggressive: bool,
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
    /// Find stray `target/` directories outside rgo and offer to remove their intermediates.
    Adopt {
        roots: Vec<std::path::PathBuf>,
    },
    /// Run the coordination daemon (Phase 2).
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("RGO_LOG").add_directive("info".parse()?),
        )
        .without_time()
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Setup {
            undo,
            dry_run,
            no_service,
            no_wrapper,
        }) => cmd::setup::run(undo, dry_run, no_service, no_wrapper),
        Some(Cmd::Status) => cmd::status::run(),
        Some(Cmd::Doctor) => cmd::doctor::run(),
        Some(Cmd::Gc {
            dry_run,
            aggressive,
        }) => cmd::gc::run(dry_run, aggressive),
        Some(Cmd::Ls) => cmd::ls::run(),
        Some(Cmd::Pin { id }) => cmd::pin::run(&id, true),
        Some(Cmd::Unpin { id }) => cmd::pin::run(&id, false),
        Some(Cmd::Clean { id }) => cmd::clean::run(&id),
        Some(Cmd::Adopt { roots }) => cmd::adopt::run(roots),
        Some(Cmd::Daemon { foreground }) => cmd::daemon::run(foreground),
        None => cmd::passthrough::run(cli.cargo_args),
    }
}
