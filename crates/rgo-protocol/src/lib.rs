//! Shared, I/O-free types. Anything written to disk or over IPC by more than one rgo
//! binary is defined here so the wrapper, CLI and daemon can never disagree.

use serde::{Deserialize, Serialize};

/// Bump when any on-disk or on-wire shape here changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// File name of the sidecar rgo writes at the top level of a Cargo build-dir it manages.
/// It is the only thing rgo ever writes inside `builds/<hash>/`.
pub const SIDECAR_FILE: &str = ".rgo-context.json";

/// Environment variable that turns every rgo binary into a pure passthrough.
pub const BYPASS_ENV: &str = "RGO_BYPASS";

/// Environment variable overriding the rgo storage root (default `~/.rgo`).
pub const HOME_ENV: &str = "RGO_HOME";

/// Attribution of a managed build-dir back to the workspace that produced it.
/// Written by the rustc wrapper and the `rgo <cargo-cmd>` passthrough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSidecar {
    pub version: u32,
    /// Absolute path of the workspace root (parent of the root `Cargo.toml`).
    pub workspace_root: String,
    /// Absolute path of the root manifest. If this file disappears the context is an orphan.
    pub manifest_path: String,
    /// `rustc -vV` first line or rustup toolchain name, when known.
    pub toolchain: Option<String>,
    /// Unix seconds.
    pub first_seen: u64,
    /// Unix seconds. Refreshed at most once per day to avoid write churn.
    pub last_seen: u64,
}

/// IPC messages (Phase 2). Framed as length-prefixed JSON over a user-private
/// Unix socket / named pipe. Both sides must send `Hello` first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
        client: String,
    },
    AcquireContextLease {
        build_dir: String,
        pid: u32,
        ttl_secs: u32,
    },
    Heartbeat {
        lease_id: u64,
    },
    ReleaseLease {
        lease_id: u64,
    },
    Touch {
        build_dir: String,
    },
    QueryStatus,
    TriggerGc {
        dry_run: bool,
        aggressive: bool,
    },
    Pin {
        build_dir: String,
    },
    Unpin {
        build_dir: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello { version: u32 },
    Lease { lease_id: u64, expires_in_secs: u32 },
    Ok,
    Status(StatusReport),
    Error { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusReport {
    pub managed_bytes: u64,
    pub incremental_bytes: u64,
    pub reclaimable_bytes: u64,
    pub soft_watermark_bytes: u64,
    pub hard_limit_bytes: u64,
    pub volume_free_bytes: u64,
    pub min_free_bytes: u64,
    pub contexts: u64,
    pub orphaned_contexts: u64,
    pub active_leases: u64,
}
