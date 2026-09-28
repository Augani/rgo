//! rgo core library. Mechanism lives here; the CLI is a thin shell over it.
//!
//! Module map (see PLAN.md §3):
//! - `paths`    — where `~/.rgo`, `$CARGO_HOME` and the managed dirs are
//! - `config`   — `~/.rgo/config.toml` + machine-aware defaults
//! - `size`     — physical (allocated) byte accounting, hardlink-aware
//! - `context`  — enumerating managed build-dirs and their sidecars
//! - `cargo_config` — fenced edits to `$CARGO_HOME/config.toml` for `rgo setup`
//! - `gc`       — tiered reclamation policy + safety mechanism

pub mod adopt;
pub mod cargo_config;
pub mod config;
pub mod context;
pub mod daemon;
pub mod db;
pub mod gc;
pub mod ipc;
pub mod paths;
pub mod service;
pub mod size;
pub mod supervision;

pub use rgo_protocol as protocol;
