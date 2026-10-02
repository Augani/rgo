# On-disk and wire compatibility policy

rgo maintains several versioned surfaces. The rule for all of them: **never
silent-migrate in a way that can corrupt user state**, and always prefer
rebuild-over-migrate when the filesystem is the source of truth.

| Surface | Version | Location | Policy |
|---|---|---|---|
| IPC protocol | `PROTOCOL_VERSION` = 9 | daemon.sock handshake | strict equality; mismatch → `protocol_mismatch` error → client falls back to plain cargo. Version 9 requires admission revision 4 and versioned macOS coalition-reaping policy; version 8 introduced the isolated guardian policy, and version 7 revised producer admission. Older daemons are never trusted for ordinary operations. Version 6 removed the unused lease-free `CachePublish` request. Additive status/GC byte-breakdown fields default when absent; they do not change the handshake version. Free-space deficit and unmet-reserve estimates are optional and remain unknown when omitted. Never breaks builds. |
| SQLite schema | `SCHEMA_VERSION` = 7 | `state/meta.sqlite` `schema_meta` | the DB is a *derived index*. Confirmed SQLite corruption causes the file set to be renamed aside and rebuilt. Lock contention, permissions, and unsupported newer schemas are reported rather than treated as corruption. Version 7 deduplicates nullable remote-upload jobs and adds a unique index so a running upload cannot lose GC protection to a duplicate worker. Current migrations preserve the remaining state in place; an incompatible future change needs its own explicit policy. |
| Cache key schema | `CACHE_SCHEMA_VERSION` = 7 | inside every `ArtifactKey` | a key-format change rotates *all* keys — stale entries become unreachable garbage reclaimed by CAS GC. Never reinterpret an old key under a new format. |
| CAS manifest | `MANIFEST_VERSION` = 1 | `cas/manifests/`, remote store | fetched manifests must match the version and key exactly or the fetch fails closed to a local miss. Local manifests are verified object-by-object before publication. |
| Context sidecar | `version` (= `PROTOCOL_VERSION` when written) | `<build-dir>/.rgo-context.json` | forward-tolerant: readers ignore unknown fields; a missing or unparsable sidecar yields an anonymous context that is accounted but protected from GC. Sidecars are rewritten atomically (temp+rename). |
| Pin intent | `pin` / `unpin` text (old empty records mean pinned) | `state/pins/<shard>/<name>.pin`; legacy `<build-dir>/.rgo-pin` | the stable decision overrides the legacy marker. Explicit unpin first writes a tombstone so a late compatibility marker cannot resurrect a pin; rgo prunes it after the context is absent and no supervised Cargo session can still restore the marker. Daemon maintenance revisits decisions in bounded batches after a crash. The `pins` DB table indexes existing pinned contexts; reconciliation imports old markers only when no decision exists. `cargo clean` may remove a context and its marker, but the stable decision protects its next build until `rgo unpin`. Absent pinned contexts remain visible in `rgo ls` and can be unpinned by ID. |
| Remote wire protocol | `/v1/` in URL path | remote endpoint | versioned by URL prefix; a v2 would use a different prefix and may coexist with v1 namespaces. See `docs/remote-cas.md`. |
| Cargo config fence | unversioned | `$CARGO_HOME/config.toml` `[build]` keys written by `rgo setup` | `setup --undo` removes only what rgo wrote; user keys outside the fence are never touched. |

## Upgrade and downgrade rules

- **Upgrade**: no-service setup asks the old daemon to exit and waits for its
  singleton lock before changing activation; installer-managed service upgrades
  remain disabled. A later daemon rebuilds derived DB state from the filesystem
  on first start. Sidecars written by older wrappers are read (not rewritten)
  until a new build refreshes them.
- **No-service shutdown**: the additive `Shutdown` request lets setup wait for
  a compatible daemon to finish before changing activation. Setup alone may
  retry this unchanged operation with a recorded protocol-6 through protocol-8 daemon; all other
  operations retain strict current-version equality. A daemon that
  lacks the request or does not release its singleton lock causes setup/undo
  to stop without removing Cargo settings; it is never killed by an unchecked
  PID from a state file.
- **Downgrade**: a downgraded daemon reads the same filesystem truth. Cache
  entries keyed under a newer schema are simply never looked up (different
  keys), and CAS GC eventually reclaims them.
- **Supervised context admission**: revision 4 salts the workspace namespace
  and is required for destructive build-context cleanup. Older contexts remain
  accounted and protected, even after a wrapper refresh. Their pin decisions
  remain attached to their old IDs; this upgrade does not move them or their
  build data into the new namespace. Do not run an older destructive daemon
  against retained contexts: it cannot enforce the new admission rule.
- **macOS guardian pilot**: owner schema 4 requires the exact `LaunchOnlyOnce`
  definition. Cleanup removes only its known metadata and never unloads a
  Cargo job by label. Owner schemas 1–3 and their pending journals are preserved.
  Coalition receipt schema 2 admits exact `ESRCH` for positively observed,
  same-boot IDs as reaped; every other observation error preserves protection.
  Receipt schema 1 retains its strict zero-count policy, including protection
  when its ID cannot be queried. Neither schema enables normal pilot activation
  or automatic GC.
  The per-job greeting now advertises `pending_before_exec`; a current caller
  falls back before terminal activation/commit when it is absent or false.
  The guardian accepts legacy `S`, and current callers send `Q` plus a four-byte
  big-endian pending mask. Only forwarded, originally blocked, non-ignored
  signals are accepted. The complete five-byte record is required before spawn;
  queued signals are recreated while blocked in the child before exec. Incomplete
  commit errors restore state and permit checkout fallback after acknowledging
  cancellation. A completed record yields a running job whose errors never
  retry Cargo. This capability negotiation does not change protocol 9,
  admission revision 4, or receipt/owner schemas: cleanup authority is unchanged.
- **Unix GC lock lifetime**: completed exclusive GC operations explicitly
  release their flock guards, including on partial acquisition errors. Passive
  descriptor copies cannot retain that exclusion after the operation ends.
  Process-associated record descriptors close before the same-process guard
  admits another operation. Cargo's inherited shared descendant locks retain
  their close-only lifetime so running descendants remain protected.
- **Pin downgrade**: the supervised launcher restores the legacy in-context
  marker when a durable pin survives `cargo clean`, so older readers see it on
  the next supervised build. Native-only Cargo can bypass that restoration;
  do not enable destructive GC with an older daemon until that case is resolved.
- **Emergency rollback**: `rgo setup --undo` removes rgo-owned Cargo settings;
  `RGO_BYPASS=1` bypasses compiler interception but leaves Cargo's configured
  build directory in effect. Do not delete `$RGO_HOME` while Cargo, the daemon,
  or an owned activation may still use it. It contains the user's storage
  policy and durable pin decisions as well as rebuildable indexes and build
  outputs. Quiesce and undo the installation first, then inspect retained
  data before explicitly removing it.

## What is NOT stable

- The `cache-events.log` line format is diagnostic, not contractual.
- `rgo status` / `rgo ls` human-readable output columns may change; scripts
  should not scrape them (use `--json` where offered, or file an issue).
- Cargo build-dir internals: rgo only relies on its own sidecar/pin files,
  `<profile>/incremental/`, and `<profile>/.cargo-build-lock`.
