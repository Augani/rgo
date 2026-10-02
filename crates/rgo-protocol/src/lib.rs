//! Versioned, I/O-free types shared by the CLI, daemon, and rustc wrapper.
//!
//! The transport uses a four-byte big-endian length followed by one JSON payload. Keeping
//! framing here as byte-buffer helpers means the wrapper can use the exact same wire format
//! without depending on the daemon's database or runtime.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 8;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const DEFAULT_LEASE_TTL_SECS: u32 = 30;
pub const DEFAULT_HEARTBEAT_SECS: u32 = 10;
pub const DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS: u32 = 30;
pub const CLIENT_TIMEOUT_MILLIS: u64 = 150;
pub const CACHE_EVENT_LOG_FILE: &str = "cache-events.log";
pub const CACHE_EVENT_LOG_LOCK: &str = "cache-events.lock";
pub const CACHE_EVENT_LOG_TRUNCATED: &str = "cache-events.truncated";
pub const CACHE_EVENT_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

pub const SIDECAR_FILE: &str = ".rgo-context.json";
pub const BYPASS_ENV: &str = "RGO_BYPASS";
pub const HOME_ENV: &str = "RGO_HOME";
pub const LEASE_ENV: &str = "RGO_LEASE_ID";
/// Revision of producer/lifecycle admission, independent of IPC compatibility.
pub const SUPERVISED_CONTEXT_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSidecar {
    pub version: u32,
    /// True when the root came from Cargo's workspace resolution (or the
    /// supervised launcher). Older wrapper sidecars need one-time validation.
    #[serde(default)]
    pub workspace_verified: bool,
    /// Set only when rgo created this context through its supervised Cargo
    /// launcher. Older/native contexts keep the conservative lock-time grace.
    #[serde(default)]
    pub supervised_origin: bool,
    /// Older supervised contexts predate the current producer/lifecycle policy.
    #[serde(default)]
    pub supervision_version: u32,
    pub workspace_root: String,
    pub manifest_path: String,
    /// Unix device ID or Windows volume serial at attribution time. Older
    /// sidecars omit it and cannot prove a missing workspace volume is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_device: Option<u64>,
    /// Linux mount ID at attribution time. A device ID alone cannot
    /// distinguish a bind mount from its underlying filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mount_id: Option<u64>,
    pub toolchain: Option<String>,
    pub first_seen: u64,
    pub last_seen: u64,
}

impl ContextSidecar {
    pub fn is_current_supervised(&self) -> bool {
        self.supervised_origin && self.supervision_version == SUPERVISED_CONTEXT_VERSION
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseScope {
    Workspace { workspace_root: String },
    Context { build_dir: String },
    Cache { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
        client: String,
    },
    AcquireLease {
        scope: LeaseScope,
        pid: u32,
        ttl_secs: u32,
    },
    BindLease {
        lease_id: u64,
        build_dir: String,
        workspace_root: Option<String>,
    },
    Heartbeat {
        lease_id: u64,
    },
    ReleaseLease {
        lease_id: u64,
    },
    Touch {
        build_dir: String,
        workspace_root: Option<String>,
        physical_bytes: Option<u64>,
        incremental_bytes: Option<u64>,
    },
    QueryStatus,
    TriggerGc {
        dry_run: bool,
        aggressive: bool,
        auto: bool,
        target_bytes: Option<u64>,
    },
    Pin {
        build_dir: String,
    },
    Unpin {
        build_dir: String,
    },
    Clean {
        build_dir: String,
    },
    CacheLookup {
        key: String,
        pid: u32,
        ttl_secs: u32,
    },
    CacheAcquire {
        key: String,
        pid: u32,
        ttl_secs: u32,
    },
    CacheWait {
        key: String,
        pid: u32,
        ttl_secs: u32,
    },
    CacheCommit {
        key: String,
        lease_id: u64,
        manifest: CacheManifest,
    },
    CacheFail {
        key: String,
        lease_id: u64,
        reason: String,
    },
    RecordCacheEvent {
        event: CacheEvent,
    },
    QueryCacheStats,
    ExplainCache {
        key: String,
    },
    VerifyCache,
    QueryRemoteStatus,
    ProbeRemote,
    /// Ask a private daemon to finish active work and release its singleton lock.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        version: u32,
    },
    Lease {
        lease_id: u64,
        expires_in_secs: u32,
    },
    Ok,
    Status(StatusReport),
    Gc(GcReport),
    CacheHit {
        manifest: CacheManifest,
        lease_id: Option<u64>,
    },
    CacheMiss {
        reason: String,
    },
    CacheProducer {
        key: String,
        lease_id: u64,
        expires_in_secs: u32,
    },
    CacheWait {
        key: String,
        retry_after_millis: u64,
        expires_in_secs: u32,
    },
    CacheRemotePending {
        key: String,
        retry_after_millis: u64,
    },
    CacheRemoteFailed {
        key: String,
        reason: String,
    },
    CacheReady {
        manifest: CacheManifest,
        lease_id: Option<u64>,
    },
    CacheFailed {
        reason: String,
    },
    CacheCommitted {
        accepted: bool,
    },
    CacheStats(CacheStatsReport),
    CacheExplanation(CacheExplanation),
    CacheVerify(CacheVerifyReport),
    RemoteStatus(RemoteStatusReport),
    RemoteProbe(RemoteProbeReport),
    RemoteError(RemoteErrorReport),
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheObject {
    pub digest: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheOutput {
    pub kind: String,
    pub name: String,
    pub object: CacheObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheManifest {
    pub version: u32,
    pub key: String,
    pub outputs: Vec<CacheOutput>,
    pub stdout: Option<CacheObject>,
    pub stderr: Option<CacheObject>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEvent {
    pub key: Option<String>,
    pub outcome: String,
    pub bytes: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatsReport {
    pub enabled: bool,
    pub manifests: u64,
    pub objects: u64,
    pub cas_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    /// At least one wrapper observation was dropped because the event log
    /// filled or a malformed record could not be replayed.
    #[serde(default)]
    pub observations_incomplete: bool,
    pub last_verify_at: u64,
    pub last_verify_error: Option<String>,
    pub single_flight_producers: u64,
    pub single_flight_waiters: u64,
    pub single_flight_timeouts: u64,
    pub single_flight_takeovers: u64,
    pub active_builds: u64,
    pub remote: RemoteStatusReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheExplanation {
    pub key: String,
    pub state: String,
    pub reason: Option<String>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheVerifyReport {
    pub checked_manifests: u64,
    pub checked_objects: u64,
    pub quarantined: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusReport {
    pub managed_bytes: u64,
    #[serde(default)]
    pub build_bytes: Option<u64>,
    #[serde(default)]
    pub cas_bytes: Option<u64>,
    #[serde(default)]
    pub auxiliary_bytes: u64,
    #[serde(default)]
    pub protected_context_bytes: u64,
    pub incremental_bytes: u64,
    /// Estimated build-context and old temp/quarantine bytes eligible under an
    /// aggressive pass. CAS eviction is planned separately and is not included.
    pub reclaimable_bytes: u64,
    /// Estimated bytes with an eligible action across builds, CAS, and temp
    /// storage under an aggressive preview. This is not a free-space promise.
    #[serde(default)]
    pub eligible_managed_bytes: Option<u64>,
    /// Portion of the configured budget excess that no currently eligible
    /// action can cover. Absent in reports from older daemons.
    #[serde(default)]
    pub unmet_budget_bytes: Option<u64>,
    #[serde(default)]
    pub unmet_budget_reason: Option<String>,
    /// Observed free-space reserve shortfall, independently of the size budget.
    /// Absent when the volume reading is unavailable or the daemon is older.
    #[serde(default)]
    pub free_space_deficit_bytes: Option<u64>,
    /// Portion of the reserve shortfall not covered by currently eligible
    /// allocated-byte estimates. Deletion may return less actual free space.
    #[serde(default)]
    pub unmet_free_space_bytes: Option<u64>,
    #[serde(default)]
    pub unmet_free_space_reason: Option<String>,
    pub soft_watermark_bytes: u64,
    pub hard_limit_bytes: u64,
    /// Legacy numeric field; zero also represented a failed volume probe.
    pub volume_free_bytes: u64,
    /// None means the filesystem's free space could not be measured. This
    /// optional field keeps older daemon/client report shapes readable.
    #[serde(default)]
    pub volume_free_observed_bytes: Option<u64>,
    pub min_free_bytes: u64,
    pub contexts: u64,
    pub orphaned_contexts: u64,
    pub active_leases: u64,
    pub pinned_contexts: u64,
    pub daemon_pid: u32,
    pub last_gc_reclaimed_bytes: u64,
    pub last_gc_at: u64,
    pub last_gc_error: Option<String>,
    pub cache: CacheStatsReport,
    pub protocol_compatible: bool,
    pub remote: RemoteStatusReport,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteStatusReport {
    pub enabled: bool,
    pub configured: bool,
    pub healthy: bool,
    pub endpoint: Option<String>,
    pub namespace: Option<String>,
    pub protocol_compatible: bool,
    pub queue_depth: u64,
    pub hits: u64,
    pub misses: u64,
    pub authentication_failures: u64,
    pub corruptions: u64,
    pub uploads: u64,
    pub downloads: u64,
    pub upload_bytes: u64,
    pub download_bytes: u64,
    pub retries: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProbeReport {
    pub ok: bool,
    pub namespace: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteErrorReport {
    pub code: String,
    pub status: Option<u16>,
    pub retryable: bool,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcReport {
    pub dry_run: bool,
    pub managed_bytes: u64,
    pub target_bytes: u64,
    pub reclaimed_bytes: u64,
    pub planned_bytes: u64,
    /// Actions selected by the planner but rejected by deletion-time safety checks or I/O.
    #[serde(default)]
    pub skipped_execution_actions: u64,
    #[serde(default)]
    pub skipped_execution_bytes: u64,
    #[serde(default)]
    pub first_execution_skip: Option<String>,
    pub skipped_live: u64,
    pub skipped_leased: u64,
    #[serde(default)]
    pub skipped_pinned: u64,
    #[serde(default)]
    pub skipped_unavailable: u64,
    #[serde(default)]
    pub protected_context_bytes: u64,
    /// CAS bytes protected by active producer/consumer leases or running
    /// uploads. Queued uploads may be retired under local storage pressure.
    /// Measured at the start of the pass.
    #[serde(default)]
    pub cas_eviction_deferred_bytes: u64,
    #[serde(default)]
    pub min_free_bytes: u64,
    /// Allocated-byte estimate measured after a real GC pass; absent for previews.
    #[serde(default)]
    pub remaining_managed_bytes: Option<u64>,
    #[serde(default)]
    pub remaining_build_bytes: Option<u64>,
    #[serde(default)]
    pub remaining_cas_bytes: Option<u64>,
    #[serde(default)]
    pub remaining_auxiliary_bytes: Option<u64>,
    /// Filesystem free space observed before and after GC. Other processes can
    /// change these values, so their difference is not attributed to rgo.
    #[serde(default)]
    pub volume_free_before_bytes: Option<u64>,
    #[serde(default)]
    pub volume_free_after_bytes: Option<u64>,
    pub actions: Vec<GcAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcAction {
    pub tier: u8,
    pub path: String,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Truncated,
    Empty,
    TooLarge(usize),
    TrailingBytes,
    InvalidJson(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated frame"),
            Self::Empty => write!(f, "empty frame"),
            Self::TooLarge(n) => write!(f, "frame is too large: {n} bytes"),
            Self::TrailingBytes => write!(f, "frame has trailing bytes"),
            Self::InvalidJson(e) => write!(f, "invalid JSON: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let payload = serde_json::to_vec(value)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T: for<'de> Deserialize<'de>>(frame: &[u8]) -> Result<T, FrameError> {
    if frame.len() < 4 {
        return Err(FrameError::Truncated);
    }
    let len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(len));
    }
    if frame.len() < len + 4 {
        return Err(FrameError::Truncated);
    }
    if frame.len() != len + 4 {
        return Err(FrameError::TrailingBytes);
    }
    serde_json::from_slice(&frame[4..]).map_err(|e| FrameError::InvalidJson(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let request = Request::QueryStatus;
        let frame = encode_frame(&request).unwrap();
        assert_eq!(decode_frame::<Request>(&frame).unwrap(), request);
    }

    #[test]
    fn rejects_truncated_and_oversized_frames() {
        assert_eq!(
            decode_frame::<Request>(&[0, 0, 0]).unwrap_err(),
            FrameError::Truncated
        );
        let mut frame = (MAX_FRAME_SIZE as u32 + 1).to_be_bytes().to_vec();
        frame.extend(std::iter::repeat_n(0, MAX_FRAME_SIZE + 1));
        assert!(matches!(
            decode_frame::<Request>(&frame),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn hello_is_versioned() {
        let request = Request::Hello {
            version: PROTOCOL_VERSION,
            client: "test".into(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("hello"));
        assert!(json.contains("version"));
    }

    #[test]
    fn older_sidecars_without_workspace_identity_remain_readable() {
        let old = r#"{"version":5,"workspace_root":"/work","manifest_path":"/work/Cargo.toml","toolchain":null,"first_seen":1,"last_seen":2}"#;
        let sidecar: ContextSidecar = serde_json::from_str(old).unwrap();
        assert!(!sidecar.supervised_origin);
        assert_eq!(sidecar.supervision_version, 0);
        assert!(!sidecar.is_current_supervised());
        assert_eq!(sidecar.workspace_device, None);
        assert_eq!(sidecar.workspace_mount_id, None);
        assert!(
            !serde_json::to_string(&sidecar)
                .unwrap()
                .contains("workspace_device")
        );
        assert!(
            !serde_json::to_string(&sidecar)
                .unwrap()
                .contains("workspace_mount_id")
        );
    }

    #[test]
    fn cache_messages_are_versioned_and_round_trip() {
        let request = Request::CacheLookup {
            key: "deadbeef".into(),
            pid: 7,
            ttl_secs: DEFAULT_LEASE_TTL_SECS,
        };
        let frame = encode_frame(&request).unwrap();
        assert_eq!(decode_frame::<Request>(&frame).unwrap(), request);
        let response = Response::CacheMiss {
            reason: "not_found".into(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("cache_miss"));
    }

    #[test]
    fn status_distinguishes_unknown_free_space_without_breaking_older_reports() {
        let mut older = serde_json::to_value(StatusReport {
            volume_free_bytes: 0,
            ..Default::default()
        })
        .unwrap();
        older
            .as_object_mut()
            .unwrap()
            .remove("volume_free_observed_bytes");
        older
            .as_object_mut()
            .unwrap()
            .remove("eligible_managed_bytes");
        older.as_object_mut().unwrap().remove("unmet_budget_bytes");
        older.as_object_mut().unwrap().remove("unmet_budget_reason");
        for field in [
            "free_space_deficit_bytes",
            "unmet_free_space_bytes",
            "unmet_free_space_reason",
        ] {
            older.as_object_mut().unwrap().remove(field);
        }
        let decoded: StatusReport = serde_json::from_value(older).unwrap();
        assert_eq!(decoded.volume_free_observed_bytes, None);
        assert_eq!(decoded.eligible_managed_bytes, None);
        assert_eq!(decoded.unmet_budget_bytes, None);
        assert_eq!(decoded.unmet_budget_reason, None);
        assert_eq!(decoded.free_space_deficit_bytes, None);
        assert_eq!(decoded.unmet_free_space_bytes, None);
        assert_eq!(decoded.unmet_free_space_reason, None);

        let current = StatusReport {
            volume_free_bytes: 0,
            volume_free_observed_bytes: Some(0),
            free_space_deficit_bytes: Some(4096),
            unmet_free_space_bytes: Some(2048),
            unmet_free_space_reason: Some("protected build contexts".into()),
            ..Default::default()
        };
        let decoded: StatusReport =
            serde_json::from_value(serde_json::to_value(current).unwrap()).unwrap();
        assert_eq!(decoded.volume_free_observed_bytes, Some(0));
        assert_eq!(decoded.free_space_deficit_bytes, Some(4096));
        assert_eq!(decoded.unmet_free_space_bytes, Some(2048));
        assert_eq!(
            decoded.unmet_free_space_reason.as_deref(),
            Some("protected build contexts")
        );
    }

    #[test]
    fn single_flight_messages_round_trip() {
        let request = Request::CacheAcquire {
            key: "key".into(),
            pid: 42,
            ttl_secs: DEFAULT_LEASE_TTL_SECS,
        };
        let frame = encode_frame(&request).unwrap();
        assert_eq!(decode_frame::<Request>(&frame).unwrap(), request);
        let response = Response::CacheWait {
            key: "key".into(),
            retry_after_millis: 100,
            expires_in_secs: DEFAULT_LEASE_TTL_SECS,
        };
        let frame = encode_frame(&response).unwrap();
        assert!(matches!(
            decode_frame::<Response>(&frame).unwrap(),
            Response::CacheWait { .. }
        ));
    }

    #[test]
    fn remote_messages_round_trip_with_structured_status() {
        let request = Request::ProbeRemote;
        let frame = encode_frame(&request).unwrap();
        assert_eq!(decode_frame::<Request>(&frame).unwrap(), request);
        let response = Response::RemoteStatus(RemoteStatusReport {
            enabled: true,
            configured: true,
            healthy: true,
            namespace: Some("toolchain-target".into()),
            ..Default::default()
        });
        let frame = encode_frame(&response).unwrap();
        assert!(matches!(
            decode_frame::<Response>(&frame).unwrap(),
            Response::RemoteStatus(_)
        ));
    }
}
