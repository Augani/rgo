# Remote CAS protocol (v1) and operations guide

rgo's remote cache is a **provider-neutral HTTP content store**. It is off by
default, is never on the build correctness path, and every failure mode —
offline, timeout, TLS, auth, corruption — degrades to a local cache miss and a
normal compile. Remote failures can only reduce the hit rate.

## Wire protocol

All requests are authenticated HTTP against a single base:

```
{endpoint}/v1/{namespace}/manifests/{key}
{endpoint}/v1/{namespace}/objects/{digest}
```

| Operation | Method | Path | Success | Other statuses |
|---|---|---|---|---|
| Fetch manifest | GET | `/v1/{ns}/manifests/{key}` | 200 + manifest JSON body | 404 = miss |
| Fetch object | GET | `/v1/{ns}/objects/{digest}` | 200 + raw bytes | 404 = miss |
| Upload object | PUT | `/v1/{ns}/objects/{digest}` | any 2xx | 409 = already exists, verified by GET |
| Upload manifest | PUT | `/v1/{ns}/manifests/{key}` | any 2xx | 409 = already exists, verified by GET |
| Probe | GET | `/v1/{ns}/manifests/__rgo_probe__` | 200 or 404 = healthy | — |

- `namespace` and `{key}`/`{digest}` path segments are restricted to
  `[A-Za-z0-9._-]` at configuration time and percent-encoded on the wire, so
  path traversal is impossible by construction.
- `Authorization: Bearer <token>` is sent on every request. The token lives
  only in `RGO_REMOTE_TOKEN` (or `[remote] token_env`'s variable) — never in
  URLs, logs, diagnostics, persisted state, or this protocol's payloads.
- Redirects are never followed. Response bodies are bounded at read time:
  objects at `remote.max_object_size` (default 2 GiB), manifests at 16 MiB,
  PUT responses at 64 KiB.
- Manifests are the rgo `Manifest` JSON (`version: 1`): `{key, outputs[]:
  {kind, name, object: {digest, size, mode}}, stdout?, stderr?, created_at}`.

## Integrity contract

- Objects are content-addressed: a fetched object's BLAKE3 must equal both the
  requested digest and the manifest's declared `size`. A 200 with mismatched
  bytes is a corruption event, not a hit.
- A fetched manifest must parse as version 1 with a matching `key`, safe
  output names (no absolute paths, `..`, or separators), and 64-hex digests.
- Every remote object is re-published into the local CAS (`put_bytes`,
  temp+fsync+digest-verify+rename, read-only mode) *before* the manifest is
  written — a remote fetch can never publish a manifest pointing at bytes rgo
  hasn't verified locally.
- Upload conflicts (409) are resolved by re-downloading and digest-verifying;
  identical remote content is success, different content is an error.
- Any malformed, oversized, or unverifiable response fails closed to a local
  miss. Nothing remote can poison the local CAS.

## Configuration (opt-in)

```toml
[cache]
enabled = true          # required; remote is ignored without the local cache

[remote]
enabled = true
endpoint = "https://cas.example.com"   # https only; http allowed solely for
                                     # loopback with allow_insecure_loopback
namespace = "team-a"                   # isolates this store from all others
token_env = "RGO_REMOTE_TOKEN"         # env var that holds the bearer token
timeout = "5s"
max_object_size = "2GiB"
upload = true                          # set false for read-only consumers
```

`rgo setup` and upgrades never enable remote. `RGO_BYPASS=1` disables the
wrapper entirely; `remote.enabled = false` (or unsetting `token_env`) turns
remote off while leaving the local cache intact.

## Behavior under failure

- **Offline / refused / timeout / TLS failure** → `Transport` error → local
  miss; uploads are retried with exponential backoff (1 s doubling, capped at
  ~64 s) and give up after 7 attempts as `FAILED`.
- **401/403** → `authentication_failure` counter + terminal job state (no
  retry storm); **429** → `rate_limited`; both surface via `rgo status`.
- **Corrupt/undersized responses** → `corruption` counter + local miss.
- Upload jobs are daemon-owned and serialized one-at-a-time through the
  `remote_jobs` table; `RUNNING` jobs recover to `RETRY` on daemon restart.
- Local GC, builds, and `rgo` commands never block on remote availability.

## Privacy and retention notes

- Artifacts are opaque compile outputs, but they embed workspace-relative
  paths and crate metadata; namespaces are the isolation boundary — use one
  per trust domain.
- rgo does not delete remote objects: retention is the provider's
  responsibility. Local GC never touches remote storage.
- The bearer token is only ever in process memory, the environment, and the
  Authorization header. It is never written to config, the database, object
  names, or log/diagnostic output.

## Interop status

The crate is covered by two independent fixtures: canned-response servers
(exercising status/bounds/auth mapping) and a stateful in-memory CAS
implementation (exercising PUT 201/409 dedup, GET verification, and namespace
isolation). Running the client against a third-party CAS provider remains an
operator gate before depending on a given deployment.
