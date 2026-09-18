# On-disk and wire compatibility policy

rgo maintains several versioned surfaces. The rule for all of them: **never
silent-migrate in a way that can corrupt user state**, and always prefer
rebuild-over-migrate when the filesystem is the source of truth.

| Surface | Version | Location | Policy |
|---|---|---|---|
| IPC protocol | `PROTOCOL_VERSION` = 5 | daemon.sock handshake | strict equality; mismatch → `protocol_mismatch` error → client falls back to plain cargo. Never breaks builds. |
| SQLite schema | `SCHEMA_VERSION` = 4 | `state/meta.sqlite` `schema_meta` | the DB is a *derived index*, never authoritative — contexts, pins, cache manifests, and accounting are all rebuilt from filesystem truth (`builds/`, `.rgo-pin`, `cas/`). On open failure the corrupt file set is renamed aside (`meta.sqlite.corrupt-*`) and rebuilt empty. Schema changes bump the constant and rebuild rather than migrate. |
| Cache key schema | `CACHE_SCHEMA_VERSION` = 2 | inside every `ArtifactKey` | a key-format change rotates *all* keys — stale entries become unreachable garbage reclaimed by CAS GC. Never reinterpret an old key under a new format. |
| CAS manifest | `MANIFEST_VERSION` = 1 | `cas/manifests/`, remote store | fetched manifests must match the version and key exactly or the fetch fails closed to a local miss. Local manifests are verified object-by-object before publication. |
| Context sidecar | `version` (= `PROTOCOL_VERSION` when written) | `<build-dir>/.rgo-context.json` | forward-tolerant: readers ignore unknown fields; a missing or unparsable sidecar yields an anonymous context that is still accounted and GC-eligible. Sidecars are rewritten atomically (temp+rename). |
| Pin marker | unversioned | `<build-dir>/.rgo-pin` | presence-only signal; the `pins` DB table is rebuilt from markers on every reconcile. Deleting a context deletes its pin. |
| Remote wire protocol | `/v1/` in URL path | remote endpoint | versioned by URL prefix; a v2 would use a different prefix and may coexist with v1 namespaces. See `docs/remote-cas.md`. |
| Cargo config fence | unversioned | `$CARGO_HOME/config.toml` `[build]` keys written by `rgo setup` | `setup --undo` removes only what rgo wrote; user keys outside the fence are never touched. |

## Upgrade and downgrade rules

- **Upgrade**: old daemon exits when its socket is replaced; the new daemon
  rebuilds DB state from the filesystem on first start. Sidecars written by
  older wrappers are read (not rewritten) until a new build refreshes them.
- **Downgrade**: a downgraded daemon reads the same filesystem truth. Cache
  entries keyed under a newer schema are simply never looked up (different
  keys), and CAS GC eventually reclaims them.
- **Emergency rollback**: `rgo setup --undo` restores cargo defaults;
  `RGO_BYPASS=1` bypasses everything without uninstalling. Deleting
  `$RGO_HOME` entirely is always safe — it contains only derived state.

## What is NOT stable

- The `cache-events.log` line format is diagnostic, not contractual.
- `rgo status` / `rgo ls` human-readable output columns may change; scripts
  should not scrape them (use `--json` where offered, or file an issue).
- Cargo build-dir internals: rgo only relies on its own sidecar/pin files,
  `<profile>/incremental/`, and `<profile>/.cargo-build-lock`.
