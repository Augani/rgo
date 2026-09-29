# On-disk and wire compatibility policy

rgo maintains several versioned surfaces. The rule for all of them: **never
silent-migrate in a way that can corrupt user state**, and always prefer
rebuild-over-migrate when the filesystem is the source of truth.

| Surface | Version | Location | Policy |
|---|---|---|---|
| IPC protocol | `PROTOCOL_VERSION` = 6 | daemon.sock handshake | strict equality; mismatch → `protocol_mismatch` error → client falls back to plain cargo. Version 6 removes the unused lease-free `CachePublish` request; manifests are published through validated `CacheCommit`. Additive status/GC byte-breakdown fields default when absent; they do not change the handshake version. Never breaks builds. |
| SQLite schema | `SCHEMA_VERSION` = 4 | `state/meta.sqlite` `schema_meta` | the DB is a *derived index*. Confirmed SQLite corruption causes the file set to be renamed aside and rebuilt. Lock contention, permissions, and unsupported newer schemas are reported rather than treated as corruption. Current additive schema changes migrate in place; an incompatible future change needs its own explicit policy. |
| Cache key schema | `CACHE_SCHEMA_VERSION` = 3 | inside every `ArtifactKey` | a key-format change rotates *all* keys — stale entries become unreachable garbage reclaimed by CAS GC. Never reinterpret an old key under a new format. |
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
  a compatible daemon to finish before changing activation. A daemon that
  lacks the request or does not release its singleton lock causes setup/undo
  to stop without removing Cargo settings; it is never killed by an unchecked
  PID from a state file.
- **Downgrade**: a downgraded daemon reads the same filesystem truth. Cache
  entries keyed under a newer schema are simply never looked up (different
  keys), and CAS GC eventually reclaims them.
- **Pin downgrade**: the supervised launcher restores the legacy in-context
  marker when a durable pin survives `cargo clean`, so older readers see it on
  the next supervised build. Native-only Cargo can bypass that restoration;
  do not enable destructive GC with an older daemon until that case is resolved.
- **Emergency rollback**: `rgo setup --undo` removes rgo-owned Cargo settings;
  `RGO_BYPASS=1` bypasses compiler interception but leaves Cargo's configured
  build directory in effect. Deleting
  `$RGO_HOME` entirely is always safe — it contains only derived state.

## What is NOT stable

- The `cache-events.log` line format is diagnostic, not contractual.
- `rgo status` / `rgo ls` human-readable output columns may change; scripts
  should not scrape them (use `--json` where offered, or file an issue).
- Cargo build-dir internals: rgo only relies on its own sidecar/pin files,
  `<profile>/incremental/`, and `<profile>/.cargo-build-lock`.
