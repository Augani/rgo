# rgo — Implementation Plan

> Companion to `doc.md` (the architecture vision). This document turns it into an
> ordered, shippable plan. Where this plan disagrees with `doc.md`, this plan wins.

## 0. What changed vs. `doc.md`, and why

`doc.md` treats the rustc-wrapper + CAS as the core and the managed build roots as a
supporting piece. After checking what Cargo actually supports today (Cargo 1.98), the
priorities invert:

| Fact (verified against current Cargo docs) | Consequence |
|---|---|
| `build.build-dir` is **stable**, settable from `$CARGO_HOME/config.toml` or `CARGO_BUILD_BUILD_DIR`, and supports `{workspace-root}`, `{cargo-cache-home}`, `{workspace-path-hash}` templates. | rgo can relocate intermediates for **every project on the machine** with one global config line. No cargo shim required for the base product. |
| Cargo officially splits *final* artifacts (target-dir: uplifted bins, `doc/`, `package/`) from *intermediate* artifacts (build-dir: `deps/`, `incremental/`, `build/`, `.fingerprint/`, build-script out dirs). | Leave `target-dir` **in the checkout**. `target/debug/myapp` keeps existing, so scripts, Dockerfiles, IDE launch configs, `cargo run` all behave identically. Only the 95%+ bloat moves. |
| Cargo `-Z gc` for build artifacts is nightly-only; stable auto-clean only covers `~/.cargo/registry` + git caches. | Bounded, machine-wide build-dir GC is genuinely unsolved upstream. That is rgo's Phase 1 product. |
| Cargo's build-dir *layout* is documented as internal/unstable. | GC treats each build-dir as an **opaque unit** (plus a documented `incremental/` sub-tier). Never fingerprint-surgery inside it. |
| Two worktrees cannot safely share one mutable build-dir (fingerprints/dep-info embed absolute paths). | Cross-worktree dedup comes from the CAS layer (Phase 3), not from sharing build-dirs. Build-dirs stay per-(workspace-path, context). |

Resulting shape:

```
Phase 1  Relocate + bound + GC + observe        -> solves the disk problem for everyone, zero risk
Phase 2  Daemon, leases, live-build safety       -> makes GC safe under concurrent agents/IDEs
Phase 3  rustc wrapper + CAS                     -> dedup across worktrees/projects (the doc's core)
Phase 4  Single-flight, path remap, composition  -> the hard correctness work
Phase 5  Remote CAS (optional, never required)
```

Each phase is independently useful and shippable. Users install once; later phases
light up automatically via `rgo` upgrades without changing their workflow.

### Explicitly dropped / deferred from `doc.md`

- **Cargo shim as a default.** Not needed for Phase 1–2. `rgo build` etc. exist as a
  convenience passthrough, and the shim becomes opt-in only when Phase 3 needs to inject
  `RUSTC_WRAPPER` per-invocation — and even then the global-config route
  (`build.rustc-wrapper`) is preferred over intercepting `cargo`.
- **15 crates up front.** Collapse to 5 crates now; split when a boundary actually hurts.
- **Repository identity via Git common dir for build roots.** Build-dirs are keyed by
  what Cargo keys them by (manifest path). Git/worktree identity is only needed for the
  CAS/path-remap layer and for UX grouping in `rgo status`. Deferred to Phase 3.

---

## 1. Product contract (user-facing)

### Install

```bash
cargo install rgo            # or brew / prebuilt binaries
rgo setup                    # one-time; idempotent; explains every change before it makes it
```

`rgo setup` does exactly three things, all reversible via `rgo setup --undo`:

1. Creates `~/.rgo/` (or `$RGO_HOME`, respecting `XDG_DATA_HOME` on Linux).
2. Adds a clearly-fenced block to `$CARGO_HOME/config.toml`:
   ```toml
   # >>> rgo managed (do not edit inside fence; `rgo setup --undo` removes it) >>>
   [build]
   build-dir = "/Users/me/.rgo/builds/{workspace-path-hash}"
   # <<< rgo managed <<<
   ```
   (Absolute path is written, not `{cargo-cache-home}`, so it survives `CARGO_HOME` changes.)
3. Installs a per-user background service (launchd agent / systemd user unit / Windows
   Task Scheduler) that runs `rgo daemon` on login. If the user declines, GC runs
   opportunistically from `rgo` commands and a lightweight `rgo gc --auto` cron mode.

After that, **users use plain `cargo` as before.** No project changes. Existing
`target/` dirs are untouched (Cargo just stops writing intermediates into them);
`rgo doctor` offers to reclaim the now-dead `target/debug/deps` etc.

### Precedence guarantees (non-negotiable, ties to doc invariants 12–14)

- Project `.cargo/config.toml` `build.build-dir`/`target-dir`, `CARGO_TARGET_DIR`,
  `CARGO_BUILD_BUILD_DIR`, and `--target-dir` **all win** over rgo (Cargo's own
  precedence does this for us). `rgo doctor` reports such projects as "unmanaged (by
  project choice)".
- `RGO_BYPASS=1` makes every rgo binary a pure passthrough (wrapper `exec`s rustc,
  `rgo <cargo-cmd>` execs cargo with rgo env stripped). Note: the global config line
  is still honored by cargo — that's fine, it is harmless. `rgo setup --undo` is the
  full escape hatch.
- Older Cargo (before `build-dir` stabilized) silently ignores the key and behaves as
  before. `rgo doctor` reports the minimum version per installed toolchain.

### Commands

```
rgo setup [--undo] [--no-service] [--dry-run]
rgo status                  # storage summary (physical bytes), budget, reclaimable
rgo doctor                  # config precedence, conflicting wrappers, toolchains, fs caps, service health
rgo gc [--dry-run] [--aggressive] [--target <bytes>]
rgo ls                      # every managed build-dir: workspace path, exists?, size, last used, pinned?
rgo pin <path|id> / rgo unpin
rgo clean <path|id>         # remove one context (refuses if leased)
rgo adopt [--delete] [<dir>...] # report stray target/ dirs; --delete removes only approved intermediates
rgo daemon [--foreground]   # Phase 2
rgo cache stats|explain|verify   # Phase 3
rgo build|run|test|check|clippy|doc|bench|<anything>   # passthrough to cargo with `rgo` env; unknown -> passthrough
```

---

## 2. Storage layout

```
~/.rgo/
  config.toml
  state/
    meta.sqlite            # rebuildable index (WAL)
    daemon.sock | \\.\pipe\rgo-<uid>
    daemon.pid
    locks/
  builds/
    <workspace-path-hash>/         # exactly what Cargo's template produces; opaque to us
      .rgo-context.json            # sidecar written by rgo (see §3.2) — never inside Cargo's subdirs
      debug/  release/  <profile>/ <triple>/...   # Cargo-owned
  cas/                             # Phase 3
    objects/aa/aabb...
    manifests/
  quarantine/
  tmp/
  logs/
```

**Verified on Cargo 1.98 (this machine):**
- `{workspace-path-hash}` expands to **two** path components, e.g. `9d/728b3456bcf7bd`.
  A managed build-dir is therefore `builds/<2hex>/<rest>/`; the context id shown by
  `rgo ls` is `9d/728b3456bcf7bd`.
- The build-dir contains `.rustc_info.json`, `CACHEDIR.TAG`, and per-profile
  `.cargo-build-lock`, `.fingerprint/`, `build/`, `deps/`, `examples/`, `incremental/`.
- The checkout's `target/` keeps only `CACHEDIR.TAG`, `debug/<bin>`, `debug/<bin>.d`,
  `debug/.cargo-lock`, `debug/.cargo-artifact-lock`, `debug/examples/` — kilobytes.
- Cargo does **not** canonicalize the build-dir template path (a `/tmp/...` root stays `/tmp/...`
  even though the workspace root is reported as `/private/tmp/...`). Compare paths by identity, not string.
- `toml_edit` must emit `[build]` as a real table (`toml_edit::table()`); a fresh document
  otherwise produces an inline `build = { ... }` which Cargo accepts but users find confusing.

`builds/<hash>/` is Cargo's. rgo only ever (a) measures it, (b) deletes it whole,
(c) deletes `*/incremental/` inside it as a documented sub-tier, (d) writes its own
sidecar at the top level. Nothing else. This is how we honor "never depend on Cargo's
internal layout" while still doing GC.

---

## 3. Phase 1 — Relocate, bound, GC, observe

**Goal:** after `rgo setup`, Rust build storage on the machine is bounded and self-cleaning,
and deleting a project never leaves a stray multi-GB directory behind.

### 3.1 Config (`rgo-core::config`)

```toml
# ~/.rgo/config.toml — everything optional; defaults are machine-aware
[storage]
root = "~/.rgo"
max_size = "auto"           # auto = clamp(15% of volume, 20GB, 150GB)
soft_watermark = 0.80       # start background GC here
min_free_space = "auto"     # auto = max(10% of volume, 20GB)

[gc]
incremental_retention = "7d"        # incremental/ dirs untouched this long are tier-2
context_retention = "30d"           # whole build-dirs untouched this long are tier-3
orphan_grace = "1h"                 # build-dir whose workspace manifest no longer exists
auto = true                         # allow daemon/opportunistic GC

[cache]                              # Phase 3
enabled = false
```

Sizes parse as `"60GB"`, `"1.5TiB"`, `"auto"`. Machine-aware defaults are computed from
the volume that holds `storage.root`.

### 3.2 Context discovery — mapping `builds/<hash>` back to a workspace

Cargo's `{workspace-path-hash}` is Cargo-internal; **we do not reimplement it.**
Instead:

1. `rgo setup` also sets `build.rustc-workspace-wrapper = "<path>/rgo-rustc-wrapper"`
   *only if the user has no existing workspace wrapper* (doctor explains otherwise).
   The wrapper in Phase 1 does one thing before `exec`ing rustc: if
   `<build-dir>/.rgo-context.json` is missing or stale (>1 day), write
   `{ workspace_root, manifest_path, toolchain, first_seen, last_seen }`. It finds the
   build-dir by walking up from the `--out-dir` argument until it hits a child of
   `~/.rgo/builds/`. Cost: one `stat` per workspace-member rustc invocation.
   `RUSTC_WORKSPACE_WRAPPER` runs only for workspace members (not deps), so overhead is negligible.
2. Fallback when no sidecar exists (wrapper disabled, old Cargo): the dir is
   "unattributed" and GC treats it purely by age/size. `rgo ls` shows it as `?`.
3. `rgo <cargo-cmd>` passthrough additionally registers the mapping via
   `cargo locate-project --workspace` before exec'ing cargo — so users of the `rgo`
   verb always get attribution even without the wrapper.

Orphan detection = sidecar's `manifest_path` no longer exists → eligible after
`orphan_grace`. This delivers invariant 8 ("deleting a checkout leaves no unmanaged
build directory") without needing filesystem watchers.

### 3.3 Accounting (`rgo-core::fs`)

- Measure **physical** bytes: `st_blocks * 512` on Unix, `GetCompressedFileSize` /
  allocation size on Windows; count each inode once per run (hardlink-aware by
  `(dev, ino)` set). Cargo hardlinks uplifted bins between build-dir and target-dir — do
  not double count.
- Sizes are cached per build-dir in SQLite with the dir's top-level mtime + a cheap
  sampled re-scan; full rescans run in the daemon at low priority.
- Last-used = max(mtime of `<build-dir>/*/.cargo-lock` files or the dir itself, sidecar
  `last_seen`). Never rely on atime (often disabled).

### 3.4 GC policy (`rgo-core::gc`)

Trigger conditions (any): managed size > soft watermark; volume free < `min_free_space`;
explicit `rgo gc`. Target: bring managed size to `soft_watermark * 0.9` and free space
above reserve, in tiers, stopping as soon as the target is met:

```
tier 0  ~/.rgo/tmp/*, quarantine/* older than 1h
tier 1  orphaned contexts (manifest gone) past orphan_grace          — whole dir
tier 2  <build-dir>/**/incremental/ not touched in incremental_retention
tier 3  unpinned contexts not touched in context_retention            — whole dir, LRU
tier 4  (only when free-space reserve breached, --aggressive, or hard ceiling)
        unpinned contexts by LRU regardless of age, never the most recently used per workspace
```

Safety rules (mechanism, not policy — tested independently):
- Never delete a context with an active lease (Phase 2). Phase 1 fallback: skip any
  build-dir containing a `.cargo-lock` file modified in the last 10 minutes *or* held by
  a live process (try non-blocking `flock` on it — Cargo holds it for the build's duration).
- Pinned contexts are never eligible.
- Delete = `rename` to `~/.rgo/tmp/gc-<uuid>` (same volume, atomic) then remove
  recursively. A crash mid-delete leaves only tier-0 garbage.
- On Windows, files open by another process fail to rename → skip that context this run.

### 3.5 `rgo status` / `rgo ls` / `rgo doctor`

Doctor checks (each with a fix suggestion, some with `--fix`):
- rgo fence present in `$CARGO_HOME/config.toml`; no conflicting `build.build-dir`/`target-dir` there.
- `CARGO_TARGET_DIR` / `CARGO_BUILD_BUILD_DIR` in the user's shell env (warn).
- Existing `build.rustc-wrapper` / `rustc-workspace-wrapper` (sccache etc.) — report composition decision.
- Each installed toolchain's cargo version supports `build-dir`.
- Filesystem capabilities of `storage.root` volume (reflink/clonefile, hardlink, case sensitivity).
- Service (launchd/systemd) installed and running.
- Volume free space vs. reserve.
- Stray large `target/` dirs found by `rgo adopt` scan roots (default: `~/Projects`, `~/src`, `~/code`, `~/dev`, configurable).

### 3.6 Phase 1 deliverables / definition of done

- [x] `rgo setup` / `--undo` idempotent, dry-run prints config and service changes, never clobbers user config outside the fence.
- [x] Plain `cargo build` in any existing project writes intermediates to `~/.rgo/builds/…` and `target/debug/<bin>` still exists and runs.
- [x] Projects with their own `target-dir`/`build-dir` config are untouched and reported by `rgo adopt` and `rgo doctor`.
- [x] `rgo gc` reclaims in tier order, reports tier 0, and rechecks live locks immediately before atomic staging.
- [x] Deleting a checkout → context shows as orphan → removed after grace by daemon/opportunistic GC.
- [x] `rgo status` uses hardlink-aware physical allocation accounting.
- [x] macOS, Linux (ext4 + btrfs smoke), and Windows validation is defined in `.github/workflows/ci.yml`.

---

## 4. Phase 2 — Daemon, leases, live-build safety

**Goal:** GC is provably safe with dozens of concurrent builds, IDEs (rust-analyzer runs
`cargo check` constantly), and coding agents.

- `rgo daemon`: single instance per user (pid file + socket lock). Unix domain socket
  (`0600`) / Windows named pipe with user-only DACL. Auto-started by clients if not running;
  if start fails, clients proceed without coordination but **disable GC**, never the build.
- IPC (`rgo-protocol`): length-prefixed JSON (serde) with a `version` handshake. Messages
  for Phase 2: `Hello`, `AcquireContextLease`, `Heartbeat`, `ReleaseLease`, `Touch`,
  `QueryStatus`, `TriggerGc`, `Pin`, `Unpin`. Leases have TTL (30s, heartbeat every 10s)
  so a killed client never blocks GC forever.
- Who takes leases: the `rgo <cargo-cmd>` passthrough (for its whole run) and the
  wrapper (short lease per rustc invocation, extended by heartbeat while rustc runs).
  Plain `cargo` users with the wrapper enabled therefore get leases for free.
- Daemon also owns: periodic size rescans, scheduled GC, free-space watchdog (poll every
  30s; on breach: stop admitting cache writes (Phase 3), GC tiers 0–3 immediately).
- SQLite (`rusqlite` bundled, WAL, `busy_timeout`) tables: `schema_meta`, `contexts`,
  `leases`, `pins`, `gc_runs`, `access_summary`. All writes go through the daemon;
  CLI reads directly (read-only connection) so `rgo status` works even if the daemon is down.
- Recovery: if the DB fails to open, move to `state/meta.sqlite.corrupt-<ts>` and rebuild
  from the filesystem (`builds/*` + sidecars). The DB is never the sole source of truth.

Definition of done: concurrency torture test — 20 parallel `cargo build`s across 5
worktrees while GC runs with tiny budget; kill random clients/daemon; assert zero build
failures attributable to rgo, zero leaked leases after TTL, no deleted live dir.

---

## 5. Phase 3 — rustc wrapper + CAS (compiler-result cache)

**Goal:** the second worktree / second agent / second project on the same dependency
graph compiles dependencies once. This is the doc's §10–14, §19–20.

Scope discipline: **start with the safest class only** and widen with evidence.

Cacheability classifier, v1 accepts only invocations that are all of:
- `--crate-type lib|rlib` (no bin, cdylib, staticlib, proc-macro, dylib), `--emit=dep-info,metadata,link` or subset;
- no `-C incremental`; no `-C save-temps`; no `-Z` flags; no `--extern` pointing outside the build-dir/sysroot;
- crate source lives under `~/.cargo/registry/src` or `~/.cargo/git/checkouts` (i.e. an immutable dependency, not a workspace member) — this alone sidesteps most path-normalization risk since registry paths are already machine-stable;
- no `-l`/`-L native=` args referencing outside the build-dir (native libs from build scripts are excluded in v1);
- no `OUT_DIR` in env (build-script consumers excluded in v1).

Everything else → `Bypass(reason)`; reasons are recorded for `rgo cache explain`.

Key (`rgo-key`): BLAKE3 over a canonical serialization of: schema version; `rustc -vV`
output (cached per toolchain by mtime+size of rustc binary); normalized args (sorted
where order is provably irrelevant, e.g. `--cfg`; otherwise preserved); digests of every
source file listed in the dep-info produced by a **prior** compile of the same key
candidate (first compile always misses and records inputs — same trick as sccache/ccache
"direct mode", with a fallback to preprocess-free "arg+source-tree" hashing for registry
crates whose whole source dir digest is cheap and stable); digests of every `--extern`
input artifact (these chain dependency identity); `CARGO_*` env vars that rustc reads
(`CARGO_PKG_*`, `CARGO_CRATE_NAME`, `CARGO_MANIFEST_DIR` → *hashed*), `RUSTFLAGS`-equivalents
already appear in args; target triple; `-C metadata` value.

CAS (`rgo-cas`): `objects/<2hex>/<blake3>` immutable, `0444`; write to `tmp/`, fsync,
verify digest, `rename`. Manifests `manifests/<key>.json` list output files
(name → object, mode) and stdout/stderr bytes (rustc emits JSON diagnostics that Cargo
parses — these **must** be replayed verbatim on hit). Metadata index in SQLite; index
rebuildable by scanning manifests.

Materialization: `clonefile` (APFS) → `FICLONE`/`copy_file_range` (btrfs/xfs) → copy.
Hardlinks only if the target dir is on the same volume *and* we chmod the CAS object
read-only *and* the consumer is Cargo's `deps/` (which Cargo never edits in place — it
writes new files). Detect capability per volume at runtime and cache it.

Composition with existing wrappers: if the user has `build.rustc-wrapper = sccache`, rgo
sets itself as the wrapper and invokes sccache as the inner compiler (`RGO_INNER_RUSTC_WRAPPER`)
on miss/bypass. If both cache, rgo yields (bypasses) for classes sccache handles. Doctor
prints the exact chain.

Correctness gates before turning `cache.enabled = true` by default:
- Differential corpus (§8) shows byte-identical `.rlib`/`.rmeta` for hits vs cold compile
  across ≥ 50 popular crates, 3 toolchains, 3 OSes.
- Fuzz: flip random bytes in CAS objects → always quarantine+miss, never a bad link.

---

## 6. Phase 4 — Single-flight, path remapping, workspace-member caching

- Single-flight: per-key lease in the daemon (`BUILDING|COMMITTING|READY|FAILED`, TTL +
  heartbeat). Waiters block with timeout then fall back to compiling themselves. No global lock.
- Path remapping for workspace members across worktrees: only via `--remap-path-prefix`
  *when the user's profile already sets one* or when `rgo` policy opts in
  (`cache.remap_workspace_paths = true`, default off, because it changes `file!()`,
  panics, and debugger paths — a semantic change users must choose). Otherwise the
  absolute checkout path is part of the key and worktrees don't share member crates
  (they still share all dependencies, which is most of the bytes).
- Widen the classifier: proc-macros (host, no `OUT_DIR`), build-script consumers where
  `OUT_DIR` contents are digested, `bin` crates *without* linking (`--emit=metadata` for `cargo check`).

---

## 7. Phase 5 — Optional remote CAS

Only after 3–4 are boring. HTTP(S) `GET/PUT objects/<hash>`, authenticated, namespaced
by `(schema, rustc identity, target)`, verify digests on download. Never in the
correctness path: any failure = local miss.

---

## 8. Testing strategy (applies from Phase 1)

- `rgo-testkit`: creates throwaway `$HOME`/`$CARGO_HOME`/`$RGO_HOME` sandboxes with a
  pinned toolchain; fixture crates (plain lib, workspace with 3 members, build-script
  crate, proc-macro crate, `cc`-using crate, git worktrees of the same repo).
- Integration tests run real `cargo` (no mocking of Cargo). Every CI job runs the
  fixture corpus through **plain cargo (after `rgo setup`)** and asserts:
  intermediates under `$RGO_HOME/builds`, final bins under `<ws>/target`, exit codes and
  binary behaviour unchanged, rebuild is a no-op.
- GC tests: budget forced to tiny values; assert tier order and lease safety.
- Fault injection: `ENOSPC` via small tmpfs/ramdisk on Linux, `SIGKILL` at random
  points (crash-only design), DB file truncated, clock jump.
- Matrix: macOS-14/15 (APFS), ubuntu (ext4, btrfs loopback), windows-2022 (NTFS);
  stable + beta + oldest supported stable.

---

## 9. Crate layout (initial — grow only when a boundary hurts)

```
rgo/
├── Cargo.toml                     # workspace, shared deps/lints/profile
├── crates/
│   ├── rgo/                       # bin: CLI + daemon (subcommand) + cargo passthrough
│   ├── rgo-core/                  # lib: config, paths, sizes, db, gc, leases, contexts, platform
│   ├── rgo-protocol/              # lib: versioned IPC messages (serde), no I/O
│   ├── rgo-rustc-wrapper/         # bin: tiny; minimal deps; fast start; Phase 1 = sidecar + exec
│   └── rgo-testkit/               # lib (dev): sandboxes + fixtures
└── (Phase 3) crates/rgo-key, crates/rgo-cas
```

Why the wrapper is a separate binary with almost no dependencies: it runs once per
rustc invocation, so its startup cost is on the critical path of every build.
No tokio, no clap, no sqlite in the wrapper. Talks to the daemon with a blocking socket
and a hard 50ms timeout; on any error it just `exec`s rustc.

---

## 10. Milestone checklist (suggested order of work)

Status of the scaffold in this repo is marked; everything works end-to-end against real
Cargo in a sandboxed `$HOME` (see `crates/rgo/tests/relocate.rs`).

1. ~~**Scaffold**: workspace, 5 crates, `rgo --help`.~~ done
2. ~~`rgo-core::config` + machine-aware defaults + size parsing (+ unit tests).~~ done
3. ~~`rgo setup` / `--undo` / `--dry-run` with fenced config editing and service installation.~~ done
4. ~~`rgo-core::size` physical, hardlink-aware measurement.~~ done
5. ~~Sidecar protocol + wrapper Phase 1 behaviour.~~ done
6. ~~`rgo ls`, `rgo status`, and `rgo doctor` toolchain/filesystem/service checks.~~ done
7. ~~`rgo gc` tiers 0–4, synthetic policy tests, concurrent-build torture test, and `rgo adopt`.~~ done
8. ~~Service installers (launchd plist, systemd user unit, schtasks).~~ done; `rgo gc --auto` and `--target` are implemented and tested.
9. ~~CI matrix (macOS/Linux/Windows, including btrfs smoke).~~ done; release packaging and Homebrew tap remain pending.
10. Release v0.1 (Phase 1). Dogfooding and release-artifact automation are done (see A); tagging and publishing remain operator actions.
11. ~~Daemon + leases + SQLite (Phase 2).~~ implemented and hardened (see B).
12. ~~Wrapper classifier + key + CAS behind `cache.enabled=false` (Phase 3).~~ implemented; a scaled-down real-Cargo differential corpus now exists, and the full 50-crate × 3-toolchain × 3-OS gate remains before enabling by default (see C).
13. ~~Single-flight, wrapper composition, opt-in workspace path remapping, and Git-common-dir status grouping.~~ implemented and validated (see D).
14. ~~Optional remote CAS behind opt-in configuration.~~ implemented, protocol-specified, and failure-injection validated (see E).

### Detailed remaining-work checklist

Work in this order. Items marked **release-blocking** must be complete before the named release or default is changed.

#### A. Finish and release Phase 1 — bounded storage (`v0.1`, release-blocking)

- [x] Implement `rgo gc --auto` for unattended/opportunistic invocation.
  - Exit quickly and successfully when `[gc].auto = false` or no trigger condition is met.
  - Trigger only when managed bytes exceed the soft watermark or free space is below the reserve.
  - Coordinate through the daemon when available; if coordination is unavailable, do not run unsafe GC.
  - Produce quiet, stable output suitable for cron/service use and a useful non-zero exit on configuration errors.
  - Add tests for disabled, no-pressure, soft-watermark, free-space-pressure, and daemon-unavailable cases.
- [x] Implement the documented `rgo gc --target <bytes>` override.
  - Reuse the existing size parser and reject malformed or impossible targets clearly.
  - Preserve tier ordering, pins, leases, live-lock checks, and rename-before-delete safety.
  - Report requested, planned, and actually reclaimed bytes in dry-run and real modes.
- [x] Complete setup lifecycle hardening.
  - Write Cargo configuration atomically and preserve permissions.
  - Verify `setup`, repeated `setup`, `--dry-run`, `--undo`, and repeated `--undo` against empty and existing `[build]` tables.
  - Verify setup never changes content outside the managed fence.
  - Verify service installation failure leaves build relocation usable and prints an actionable fallback.
- [x] Complete precedence and compatibility coverage.
  - Test project `build-dir`, project `target-dir`, `CARGO_BUILD_BUILD_DIR`, `CARGO_TARGET_DIR`, and `--target-dir` overrides with real Cargo.
  - Test `RGO_BYPASS=1` for both the CLI passthrough and rustc wrapper, including removal of inherited `RGO_*` coordination variables.
  - Test unknown Cargo subcommands, `+toolchain`, non-UTF-8 arguments where supported, signals, stdio, and exact exit-code propagation.
- [x] Expand `rgo adopt` safety coverage.
  - Cover nested workspaces, custom profiles, cross-target output, symlinks, unreadable paths, and builds that become live between scan and deletion.
  - Confirm final binaries, examples, docs, package output, and user-created files are never selected.
  - Add an explicit confirmation UX if interactive deletion is introduced; non-interactive deletion must remain narrowly scoped.
- [x] Dogfood storage behavior on representative large projects.
  - Recorded in `docs/dogfood-2026-09-18.md` and `docs/benchmarks.md`: plain build checkout `target/` ~194.3 MiB vs managed ~8.7 MiB; orphan GC reclaimed ~181.2 MiB while preserving a live context.
  - Confirm checkout `target/` directories retain requested final outputs while intermediates remain bounded centrally. (verified)
  - Exercised a storage root on a separate APFS volume: cross-volume materialization falls back from hardlinks to copies.
- [x] Finish release packaging.
  - `.github/workflows/release.yml` builds `rgo` and `rgo-rustc-wrapper` together per target, bundles them in one archive, and generates SHA-256 checksums.
  - Homebrew tap / cargo-dist publishing remain a later distribution gate per the Phase 1 scope.
  - `README.md` publishes install, upgrade, uninstall, `RGO_BYPASS=1`, `rgo setup --undo`, DB rebuild, and CAS quarantine recovery instructions; MIT/Apache licenses included.

**Phase 1 done when:** a fresh install followed by plain Cargo usage bounds intermediate storage automatically; overrides and bypass remain reliable; GC cannot remove active or pinned state; and release artifacts work on macOS, Linux, and Windows.

**Current gate status:** implementation, automated validation, operator dogfooding, and release artifact automation are complete. Tagging the v0.1 release and Homebrew/cargo-dist publishing remain operator actions.

#### B. Production-harden Phase 2 — daemon and leases (`v0.2`, release-blocking)

- [x] Complete daemon failure and recovery testing.
  - Kill the daemon during lease acquisition, heartbeat, GC planning, GC staging, cache publication, and status queries. (daemon.rs integration tests; clients degrade to plain Cargo)
  - Verify expired leases are reclaimed and clients continue building without rgo-caused failures.
  - Verify stale PID files, stale sockets, concurrent startup, and protocol-version mismatches recover cleanly.
- [x] Strengthen IPC platform guarantees.
  - Unix socket ownership and `0600` access asserted in daemon tests; Windows named-pipe DACL restricts access to the current user (implementation in ipc.rs, exercised by the Windows CI suite).
  - Bounded frame size (`MAX_FRAME_SIZE`), per-connection read/write timeouts, and a bounded connection-admission semaphore; malformed-client and bad-handshake tests prove the daemon survives.
- [x] Complete SQLite recovery coverage.
  - Corrupt DBs are quarantined to `meta.sqlite.corrupt-<ts>` and rebuilt from filesystem truth without blocking Cargo.
  - Pins survive DB rebuild via the `.rgo-pin` filesystem marker; `reconcile_contexts` rebuilds the pins table from markers (unit + daemon integration coverage).
- [x] Complete the specified concurrency torture suite.
  - `concurrent_builds_survive_aggressive_gc` runs 20 parallel builds across five worktrees while `rgo gc --aggressive` loops; `concurrent_writers_survive_wal_busy_contention` hammers SQLite.
  - Assert zero live-context deletion, zero leaked leases after TTL, no deadlocks, and no rgo-attributable build failures.
- [x] Validate maintenance behavior under sustained pressure.
  - Periodic reconciliation, lease expiry, free-space watchdog, GC serialization via the operation lock, and cache-admission suspension under `min_free_space` are implemented and covered.

**Phase 2 done when:** coordination failures degrade to safe uncached builds, the DB is demonstrably rebuildable, and concurrent builds plus GC survive fault injection on every supported OS.

#### C. Prove Phase 3 cache correctness before enabling it by default (`v0.3`, release-blocking)

- [~] Build the differential compatibility corpus.
  - A scaled-down real-Cargo corpus is in-tree: `real_cargo_git_dependency_hits_reproduce_cold_compiles` builds five git dependencies (three chained libs, a build-script consumer, a proc-macro) across three checkouts and asserts byte-identical artifacts for path-free deps, publisher-verbatim bytes for all hit-materialized deps, and identical binary behavior.
  - The corpus already caught two production bugs: shared-`--out-dir` scan poisoning (siblings' outputs entering a manifest → `materialization_failed`) and bare `--extern proc_macro` rejecting every real proc-macro compile. Both are fixed and covered.
  - The release gate remains scaling the corpus to ≥50 popular registry crates across three toolchains and all three supported operating systems — a CI-matrix-scale exercise.
  - Include features, custom profiles, cross compilation, path/git dependencies, multiple registries, clippy, rustdoc, tests, benches, and examples.
- [x] Audit and lock down artifact-key completeness.
  - `docs/cache-keys.md` documents the canonical key fields, normalization rules, and incorrect-equivalence risks; rgo-key unit vectors cover ordering, environment, extern, metadata, and remap inputs.
- [x] Harden cacheability classification.
  - `unsafe_invocations_always_compile_and_explain_the_bypass` drives table-driven bypass cases (incremental, native inputs, `-Z` flags, unsupported emit); rgo-key unit tables cover crate types, symlinks, `OUT_DIR`, and external paths.
  - Ambiguity always produces a named bypass reason recorded in `cache-events.log`.
- [x] Complete CAS integrity and crash-safety testing.
  - `corrupt_cas_objects_quarantine_and_recover_by_recompiling` scribbles on published objects and asserts quarantine + clean rebuild; `unwritable_cas_degrades_to_plain_compiles` covers ENOSPC/EACCES publication failure.
  - Immutable object permissions and tmp→fsync→rename publication covered in rgo-cas unit tests across the CI filesystem matrix.
- [x] Complete materialization validation.
  - Hardlink, copy fallback, and cross-volume behavior covered by rgo-materialize tests and the APFS cross-volume dogfood run.
  - Read-only CAS objects cannot be mutated through hardlinks; physical-byte accounting is hardlink-aware.
- [x] Finish cache observability.
  - `cache-events.log` plus `rgo cache explain` report stable hit/miss/bypass/timeout/corruption reasons without sensitive values; events are drained into the DB by daemon maintenance.
- [x] Establish performance gates.
  - `hits_are_byte_identical_materially_faster_and_explained` asserts hits beat a slow compile; `bypassed_wrapper_overhead_stays_bounded` fails if bypass overhead exceeds 100ms; methodology recorded in `docs/benchmarks.md`.

**Phase 3 done when:** the differential corpus has zero known incorrect hits, corruption only causes misses, cache-disabled overhead is acceptable, and the default can be enabled without weakening Cargo compatibility.

#### D. Complete Phase 4 — composition and broader reuse (`v0.4+`)

- [x] Implement and validate existing-wrapper composition.
  - `inner_wrapper_composition_and_sccache_workspace_caching` proves `RGO_INNER_RUSTC_WRAPPER` chaining: unknown wrappers wrap every passthrough compile, sccache owns registry deps while rgo keeps workspace caching.
  - Setup preserves pre-existing wrappers with diagnostics instead of overwriting them.
- [x] Finish single-flight fault coverage.
  - `single_flight_timeout_and_producer_failure_fall_back_to_compiling` covers producer death, waiter timeout, and bounded fallback to independent compiles; concurrent identical keys elect one producer.
- [x] Validate opt-in workspace path remapping.
  - `workspace_remap_shares_hits_across_equivalent_worktrees` proves identical checkouts miss without opt-in and hit with `remap_workspace_paths = true`; remap state participates in the key so remapped and un-remapped keys can never collide.
- [x] Validate widened cache classes independently.
  - `widened_classes_proc_macro_and_metadata_only_bin_are_cacheable` covers proc-macro crates and metadata-only binaries; build-script `OUT_DIR` consumers stay digested-or-bypassed per the classifier tables.
- [x] Add repository/worktree grouping for UX without sharing mutable build roots.
  - `rgo ls` groups contexts whose workspaces share a Git common directory under a repo header; `context::git_common_dir` resolves `.git`/`commondir` files without spawning `git`, and non-Git, submodule, moved, and deleted workspaces fall back to ungrouped rows. Mutable build roots stay per-checkout; only the listing is grouped.

**Phase 4 done when:** wrapper composition is reversible and tested, single-flight survives producer failures, and cross-worktree reuse cannot alter observable source-path semantics without explicit opt-in.

#### E. Validate optional remote CAS (`v0.5+`, never required for local correctness)

- [x] Specify and version the provider-neutral HTTP protocol, namespace format, authentication behavior, and compatibility policy. (`docs/remote-cas.md`)
- [x] Add interoperability tests against at least two independent server implementations or fixtures. (in-memory CAS fixture + raw-socket fixtures in rgo-remote tests)
- [x] Verify every downloaded object and manifest before local publication; malformed or oversized responses must fail closed to a local miss.
- [x] Test timeouts, TLS failures, authentication failures, partial downloads/uploads, retries, duplicate uploads, offline operation, and server corruption.
- [x] Ensure credentials never enter logs, object names, cache explanations, diagnostics, or persisted configuration unintentionally. (`token_never_leaks_into_urls_errors_or_request_lines`)
- [x] Bound upload queues and temporary download storage under disk pressure; local GC and ordinary builds must continue when remote service is unavailable.
- [x] Document opt-in enablement, namespace isolation, retention expectations, privacy implications, and complete disablement. (`docs/remote-cas.md`, `docs/compatibility.md`)

**Remote CAS done when:** disconnecting or corrupting the remote service can only reduce hit rate; it cannot break a correct local build or poison the local CAS.

#### F. Cross-cutting quality and release gates

- [x] Run CI on stable, beta, nightly (non-blocking), and MSRV 1.85.0 across macOS, Linux GNU, and Windows MSVC.
- [x] Add ext4 (Ubuntu), btrfs (loopback smoke), APFS (macOS + cross-volume dogfood), and NTFS (Windows) coverage for accounting, locking, rename, materialization, and disk-pressure behavior; XFS remains open if CI ever permits.
- [x] Add ENOSPC, permission-denied, clock-jump, atomic-rename-failure, unreadable-tree, and process-crash fault injection. (unwritable-CAS test exercises the ENOSPC/EACCES write path; clock-jump, adopt unreadable-tree, daemon/producer kill, and cross-volume rename→copy fallback covered)
- [x] Add security review coverage for path traversal, symlink races, unsafe archive/object names, socket/pipe access, untrusted manifests, and secret redaction. (`safe_output_name`/`valid_object_ref` tables, socket `0600`, token-redaction tests)
- [x] Define on-disk, DB, protocol, sidecar, manifest, and cache-key schema compatibility and migration policies before the first stable release. (`docs/compatibility.md`, `docs/cache-keys.md`, `docs/remote-cas.md`)
- [x] Ensure `cargo build`, `cargo clippy --all-targets`, `cargo test`, formatting, and the real-Cargo fixture corpus are mandatory CI gates. (`ci.yml` runs build/test/clippy `-D warnings`/fmt/`git diff --check` on every push and PR)
- [x] Produce end-user benchmarks showing storage growth versus plain Cargo across many projects/worktrees, GC effectiveness, cache reuse, and added build latency. (`docs/benchmarks.md` methodology + recorded measurements)
- [x] Document known incompatibilities and immediate recovery: `RGO_BYPASS=1`, project-level opt-out, daemon stop, `rgo setup --undo`, DB rebuild, and CAS quarantine. (`README.md`, `docs/compatibility.md`)

**Production-safe acceptance:** all invariants in `doc.md` hold; managed storage remains within documented transient headroom; active/pinned data is never collected; all cache uncertainty bypasses; corruption degrades to rebuilding; and users can install rgo, keep using plain Cargo, create/delete many worktrees, and stop thinking about accumulated target directories.

---

## 11. Risks & mitigations (Phase 1–2 specific)

| Risk | Mitigation |
|---|---|
| A tool/script assumes `target/debug/deps/*.rlib` exists (rare; e.g. some coverage or wasm tooling). | Documented; `rgo doctor` can flag known tools; per-project opt-out is just the project's own `.cargo/config.toml` (already wins). |
| `rust-analyzer` sets its own `--target-dir` in some configs. | Cargo precedence handles it; doctor reports it. |
| User already has `build.rustc-workspace-wrapper`. | Setup refuses to overwrite; sidecar attribution falls back to passthrough-verb or age-only GC. |
| GC deleting a dir a build is about to reuse (race between `.cargo-lock` check and delete). | Phase 1: rename-then-delete + 10-minute recency window; Phase 2: leases. Cargo recreates missing dirs, so worst case is a rebuild, never corruption. |
| Home dir on a small volume; projects on a big external disk. | `storage.root` configurable; setup warns when the root volume is smaller than the largest project volume; per-volume roots are a possible later feature. |
| Cross-device: build-dir and target-dir on different volumes → Cargo copies instead of hardlinks uplifted bins. | Correct, just slightly slower; note in doctor. |
