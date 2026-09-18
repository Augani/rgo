//! Versioned, I/O-free types shared by the CLI, daemon, and rustc wrapper.
//!
//! The transport uses a four-byte big-endian length followed by one JSON payload. Keeping
//! framing here as byte-buffer helpers means the wrapper can use the exact same wire format
//! without depending on the daemon's database or runtime.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 5;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const DEFAULT_LEASE_TTL_SECS: u32 = 30;
pub const DEFAULT_HEARTBEAT_SECS: u32 = 10;
pub const DEFAULT_SINGLE_FLIGHT_TIMEOUT_SECS: u32 = 30;
pub const CLIENT_TIMEOUT_MILLIS: u64 = 150;

pub const SIDECAR_FILE: &str = ".rgo-context.json";
pub const BYPASS_ENV: &str = "RGO_BYPASS";
pub const HOME_ENV: &str = "RGO_HOME";
pub const LEASE_ENV: &str = "RGO_LEASE_ID";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSidecar {
    pub version: u32,
    pub workspace_root: String,
    pub manifest_path: String,
    pub toolchain: Option<String>,
    pub first_seen: u64,
    pub last_seen: u64,
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
    CachePublish {
        manifest: CacheManifest,
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
    pub incremental_bytes: u64,
    pub reclaimable_bytes: u64,
    pub soft_watermark_bytes: u64,
    pub hard_limit_bytes: u64,
    pub volume_free_bytes: u64,
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
    pub skipped_live: u64,
    pub skipped_leased: u64,
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
