# Rust Build Storage Architecture

> **Goal:** A Cargo-compatible wrapper that makes Rust build storage machine-managed, bounded, worktree-aware, concurrency-safe, and largely invisible to developers.

**Working name:** `rgo`  
**Research baseline:** September 2026

## Executive Summary

Rust's disk problem is not fundamentally a cleanup problem; it is a **build-state ownership problem**. Cargo build storage can contain final outputs, compiled dependencies, build-script output, fingerprints, debug information, and rustc incremental state. Large projects, Git worktrees, and AI agents multiply this state across checkouts.

The end-state design is a Cargo-compatible build-storage coordinator. Cargo remains authoritative for package resolution, features, profiles, build scripts, targets, dependency semantics, and build ordering. The coordinator owns storage placement, lifecycle, compiler-result caching, worktree normalization, concurrency, disk budgets, deduplication, observability, and recovery.

> **A checkout should own source code and requested outputs. The machine should own reusable build state.**

This is one end-state architecture, not a sequence of disposable versions.

## 1. Research Conclusions

### Cargo

Cargo exposes supported integration points including `CARGO_TARGET_DIR`, `build.target-dir`, `build.build-dir`, `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `cargo metadata`, configuration overrides, and JSON build messages. Cargo's build directory layout is an internal implementation detail, so this tool must redirect Cargo through supported interfaces rather than manipulate its private layout.

Cargo has automatic GC for portions of its global registry/git caches, but current Cargo documentation does not provide equivalent machine-wide bounded GC for build artifacts.

### Go

Go is the behavioral inspiration. It separates project source, module source cache, and machine-wide build cache. Multiple projects benefit from shared cached compilation state and old build-cache data can be removed automatically. This design improves on the model with an explicit disk budget and free-space reserve.

### sccache

`sccache` proves Rust compiler-result caching behind `RUSTC_WRAPPER` is practical. It also demonstrates boundaries: incremental Rust compilations and linker-invoking crate types require conservative treatment, and absolute checkout paths complicate reuse. Compiler-result caching is therefore one layer, not the whole solution.

## 2. Non-Negotiable Invariants

1. Cargo semantics are authoritative.
2. If equivalence cannot be proven, bypass the cache and compile normally.
3. A cache hit must be observationally equivalent to the compiler invocation it replaces.
4. Corruption can cause a miss or diagnostic, never silent incorrect success.
5. Published CAS objects are immutable.
6. Active or pinned state cannot be garbage-collected.
7. Incremental compiler state is mutable local state, never global immutable state.
8. Deleting a checkout leaves no unmanaged build directory.
9. Multiple processes, worktrees, IDEs, and agents are concurrency-safe.
10. No global build lock.
11. Managed storage has a real soft/hard budget and minimum free-space reserve.
12. Existing Cargo projects require no source modification.
13. Unknown Cargo commands and flags pass through.
14. Users always have an immediate bypass.
15. Correctness never depends on Cargo's undocumented target/build layout.
16. macOS, Linux, and Windows are first-class.
17. Higher cache-hit rate is never more important than correctness.

## 3. User Experience

```bash
rgo build
rgo run
rgo test
rgo check
rgo bench
rgo clippy
rgo doc
rgo build --release
rgo test -p server --features foo
rgo +nightly build
```

Management:

```bash
rgo status
rgo doctor
rgo cache stats
rgo cache gc
rgo cache explain <key>
rgo workspace status
```

Emergency bypass:

```bash
RGO_BYPASS=1 cargo build
```

A compatibility shim may route ordinary `cargo` invocations through the coordinator. It must retain the real Cargo path, detect recursion, preserve signals/stdio/exit codes, and be trivial to bypass.

## 4. Top-Level Architecture

```text
                         rgo CLI / cargo shim
                                 |
                          Command Planner
                                 |
             +-------------------+-------------------+
             |                                       |
      Workspace Resolver                         Policy Engine
             |                                       |
             +-------------------+-------------------+
                                 |
                            Local Daemon
                                 |
        +------------------------+------------------------+
        |                        |                        |
 Build Root Manager       Build Coordinator       Storage Manager
        |                        |                        |
        |                  rustc wrapper                  |
        +-------------------- Cargo ----------------------+
                                 |
                               rustc
                                 |
               +-----------------+-----------------+
               |                                   |
        Mutable build state                  Immutable CAS
```

The crucial architectural separation is **mutable Cargo state vs immutable reusable state**.

## 5. Storage Layout

```text
~/.rgo/
  config.toml
  state/
    metadata.sqlite
    daemon.sock
    locks/
  repositories/<repository-id>/
  workspaces/<workspace-id>/
  builds/<build-context-id>/
    target/
    build/
  cas/
    objects/00/...
    objects/ff/...
    manifests/
  tmp/
  logs/
```

`builds/` is mutable Cargo-owned state. `cas/` is immutable compiler-cache state. `tmp/` is never authoritative.

## 6. Repository and Worktree Identity

Git worktrees must be recognized as related. Use Git's common repository information plus a stable machine-local repository fingerprint. Do not use remote URL alone; forks, local repositories, rewritten remotes, and multiple remotes make that unsafe. Non-Git projects receive a persistent machine-local identity stored outside the project.

## 7. Workspace Identity

A repository can contain multiple Cargo workspaces:

```text
WorkspaceIdentity = H(
    repository_identity,
    workspace_relative_manifest,
    relevant_workspace_metadata
)
```

Absolute checkout path must not define logical workspace identity.

## 8. Build Context Identity

Mutable build roots are reused only for compatible contexts:

```text
BuildContext = H(
    workspace_identity,
    rust_toolchain,
    rustc_identity,
    host,
    target,
    profile,
    cargo_configuration,
    rustflags,
    rustdocflags,
    relevant_environment_policy,
    feature/package context
)
```

False separation wastes disk. False equivalence risks incorrect builds. **Correctness wins.**

## 9. Do Not Build One Giant Shared `target`

Do not make every project use one mutable `~/.rgo/target`. Cargo's target directory is not a machine-wide content-addressed cache. A giant mutable target creates cross-workspace contamination, lifecycle and locking problems, dependence on Cargo internals, and difficult recovery.

Use:

```text
managed mutable build roots
              +
machine-wide immutable CAS
```

## 10. rustc Wrapper

Cargo invokes `rgo-rustc-wrapper` through the supported compiler-wrapper mechanism:

```text
Cargo -> rgo-rustc-wrapper -> rustc
```

The wrapper classifies cacheability, computes identity, queries the CAS, materializes a hit, or invokes rustc and captures eligible outputs. Unsupported or ambiguous invocations pass directly to rustc.

## 11. Artifact Identity

Never identify an artifact only by crate name, version, and features.

```text
ArtifactKey = H(
    cache_schema_version,
    compiler_identity,
    normalized_rustc_arguments,
    target_triple,
    host_context,
    crate_type,
    crate_metadata,
    source_digests,
    dependency_artifact_identities,
    relevant_environment,
    cfg_values,
    codegen_options,
    toolchain_components,
    proven_native_inputs,
    path_normalization_map
)
```

Study proven cache-key/cacheability logic in systems such as `sccache` rather than inventing a weak approximation.

> If we cannot completely explain why two compiler invocations are equivalent, they are not equivalent.

## 12. Content-Addressed Store

Compiler-cache results live in an immutable CAS:

```text
cas/
  objects/
    01/019aca...
    7f/7f88d1...
    af/af001c...
  manifests/
```

An artifact manifest maps a compiler cache key to its output object set and materialization metadata. Large bytes live in the filesystem; indexes and lifecycle metadata live in SQLite.

## 13. Atomic Publication and Integrity

Never write directly to a final CAS path:

```text
compile -> temporary output -> digest -> verify -> atomic rename -> publish metadata
```

Published objects never mutate. Concurrent producers of identical content converge on the same object. Integrity mismatches quarantine the object and turn requests into cache misses. The metadata DB must be rebuildable from CAS manifests.

## 14. Worktree Path Normalization

Equivalent worktrees often differ only by absolute path. Safe path-valued compiler inputs can map to a virtual workspace root.

Never blindly replace strings. Paths can affect `file!()`, generated code, diagnostics, debug information, build scripts, native tools, and environment variables. Prefer stable compiler path-remapping mechanisms. If a path cannot safely be normalized, keep it in cache identity.

## 15. Incremental Compilation

rustc incremental state is mutable, build-context-specific, large, useful for active edits, and reproducible.

```text
global CAS != incremental state
```

Keep it in the managed build context and give stale incremental data relatively high GC priority. Do not silently disable incremental compilation merely to increase cacheability.

## 16. Build Scripts, Proc Macros, Native Code and Linking

Build scripts can depend on arbitrary files, environment variables, SDKs, external programs, native libraries, network/time, and other state. Cargo remains responsible for running them. Their output stays in mutable build roots unless complete dependency capture proves safe reuse.

Proc macros are host executables and should be cached only when the classifier proves eligibility.

Native builds may involve `cc`, `clang`, `gcc`, `cmake`, `pkg-config`, `ninja`, `bindgen`, headers, and system SDKs. Do not globally reuse native outputs without complete input knowledge.

Final linker output may encode paths, UUIDs/build IDs, debug information, SDK state, or platform metadata. Link normally by default while caching eligible upstream compilation.


## 17. Concurrent Build Deduplication

If several agents request the same missing cache key, use **single-flight** coordination: one producer compiles while the others wait, then all consume the committed result.

Never use a global build/cache lock. Use per-key leases containing producer identity, start time, heartbeat, expiry, and state (`BUILDING`, `COMMITTING`, `READY`, `FAILED`). If a producer dies, its lease expires and a waiter can become producer.

## 18. Local Daemon and IPC

A lightweight daemon coordinates metadata writes, leases, single-flight work, storage accounting, GC, capacity reservations, and health/status queries. It is not a remote build server; compiler processes remain ordinary local children.

If the daemon fails, clients restart it. If coordination cannot be restored, use an uncached/private managed build root rather than risking correctness.

Use Unix domain sockets on macOS/Linux and named pipes on Windows, restricted to the current user. Version the IPC protocol and negotiate compatibility.

Potential messages:

```text
AcquireArtifact
ArtifactHit
ArtifactMiss
AcquireLease
Heartbeat
CommitArtifact
ReleaseLease
ReserveSpace
TouchObject
QueryStats
TriggerGC
```

## 19. Metadata Database

SQLite is suitable for local metadata using WAL mode and transactional migrations.

Core tables:

```text
objects
artifact_manifests
build_contexts
repositories
workspaces
leases
pins
access_summary
gc_runs
schema_meta
```

Do not store artifact blobs in SQLite. Batch access-time updates so cache hits do not create synchronous DB writes. Treat the database as a rebuildable index rather than the sole source of truth.

## 20. Materialization

Preferred strategies:

1. Reflink/clonefile when supported.
2. Hard links only where cached immutability cannot be violated.
3. Normal copies as universal fallback.

Never let a mutable consumer write through a hard link into a CAS object. Detect filesystem capabilities per volume rather than assuming them by OS.

## 21. Disk Budget

The core product promise is **bounded managed build storage**.

Use:

```text
soft watermark
hard managed-storage ceiling
minimum filesystem free-space reserve
```

Example:

```text
Managed:             43 GB
Soft GC watermark:   48 GB
Hard limit:          60 GB
Minimum free disk:   50 GB
Reclaimable:         17 GB
```

Defaults should be machine-aware rather than one universal value.

## 22. Garbage Collection

GC policy can consider:

- Reproducibility.
- Age.
- Physical bytes reclaimable.
- Recent/frequent access.
- Estimated rebuild cost.
- Number of consumers.
- Active leases.
- Explicit pins.

Typical eviction order:

1. Abandoned temporary data.
2. Stale incremental state.
3. Inactive mutable build contexts.
4. Old/low-value immutable CAS objects.
5. Retained final artifacts according to explicit policy.

Leased or pinned state is ineligible. The eviction score is policy and may evolve; it must never be part of build correctness.

## 23. Capacity Reservation and Disk-Full Protection

A hard cache limit alone is insufficient because one compiler or linker process can temporarily need substantial space. Before expensive work, reserve approximate capacity.

If the filesystem crosses the safety floor, stop admitting cache writes, run GC, recover safe headroom, then continue. Never delete files underneath active Cargo/rustc processes.

## 24. Cargo Invocation Strategy

The planner should:

1. Resolve the real Cargo executable.
2. Discover workspace information through supported Cargo mechanisms.
3. Compute repository/workspace/build-context identities.
4. Acquire a context lease.
5. Select managed target/build locations.
6. Inject the compiler wrapper.
7. Preserve original command-line semantics.
8. Launch Cargo with inherited stdio/signals.
9. Consume JSON messages only when machine-readable artifact information is needed.
10. Release leases and update accounting/access data.

Do not scrape Cargo's human terminal output.

## 25. Wrapper Composition

Users may already use `sccache` or another rustc wrapper. Never silently destroy an existing wrapper configuration. Either compose safely, integrate equivalent functionality, or apply explicit configured precedence and explain the decision through `rgo doctor`.

## 26. Configuration

Projects should work with zero configuration.

```toml
[storage]
max_size = "60GB"
soft_watermark = 0.80
minimum_free_space = "50GB"

[gc]
incremental_retention = "7d"
build_context_retention = "30d"

[cache]
enabled = true

[compat]
cargo_shim = true
```

Any setting that changes compiler semantics must participate in the relevant identity.

## 27. Observability

```text
$ rgo status

Managed storage           41.8 GB
Physical CAS              19.3 GB
Mutable contexts          18.1 GB
Incremental state          4.4 GB
Hard limit                60.0 GB
Reclaimable               14.6 GB

Cache hits                 8,421
Cache misses               1,105
Cache bypasses               281
Concurrent builds avoided    317
Estimated bytes avoided      92 GB
```

`rgo cache explain` should explain why an invocation hit, missed, or bypassed without leaking sensitive environment data. Measure **physical bytes**, not merely apparent logical sizes.

## 28. Security and Privacy

Treat source paths, environment variables, compiler arguments, and build outputs as potentially sensitive.

- Do not put secrets verbatim in object names.
- Hash sensitive identity inputs.
- Restrict local IPC to the current user.
- Do not execute CAS objects merely because they exist.
- Verify object digests before materialization.
- Keep local-only operation fully functional.

If remote caching is ever enabled, it must be opt-in, authenticated, encrypted, namespace-separated, and resistant to cache poisoning.

## 29. Failure Modes and Recovery

### Daemon unavailable
Restart it or fall back to uncached execution in a private managed build root.

### Metadata DB corruption
Move the DB aside and rebuild indexes from manifests/CAS state.

### CAS corruption
Quarantine the object and rebuild it.

### Disk full
Stop cache admission, GC, then continue only if sufficient working space exists.

### Unknown Cargo/rustc behavior
Bypass caching.

### Wrapper conflict
Compose only when proven safe; otherwise use explicit configured precedence.

The system should never require a destructive global `cargo clean` to restore correctness.

## 30. Platform Requirements

### macOS
Use APFS clonefile/reflinks where safe. Correctly account for split debug information and dSYM-related output.

### Linux
Detect filesystem capabilities. Do not assume ext4, Btrfs, and XFS provide identical reflink behavior.

### Windows
Use named pipes and Windows-native file/locking semantics. Account for files that cannot be deleted while open.

Cross-platform behavior is defined by invariants; optimizations are platform-specific.

## 31. Rust Workspace / Module Plan

```text
rgo/
├── crates/
│   ├── rgo-cli
│   ├── rgo-planner
│   ├── rgo-daemon
│   ├── rgo-protocol
│   ├── rgo-workspace
│   ├── rgo-buildroot
│   ├── rgo-rustc-wrapper
│   ├── rgo-key
│   ├── rgo-cas
│   ├── rgo-materialize
│   ├── rgo-db
│   ├── rgo-gc
│   ├── rgo-platform
│   ├── rgo-metrics
│   └── rgo-testkit
└── Cargo.toml
```

Responsibilities:

- **rgo-cli** — Cargo-compatible argument handling and UX.
- **rgo-planner** — Cargo invocation/environment planning.
- **rgo-daemon** — IPC, leases, GC, accounting.
- **rgo-protocol** — versioned IPC messages.
- **rgo-workspace** — Git/Cargo identity.
- **rgo-buildroot** — managed mutable roots.
- **rgo-rustc-wrapper** — cacheability and compiler execution.
- **rgo-key** — canonical hashing and path normalization.
- **rgo-cas** — immutable object store/manifests.
- **rgo-materialize** — reflink/hard-link/copy logic.
- **rgo-db** — SQLite schema/migrations.
- **rgo-gc** — budget/reclamation.
- **rgo-platform** — filesystem/process abstractions.
- **rgo-metrics** — statistics/explainability.
- **rgo-testkit** — fault injection and concurrency fixtures.

Keep policy and mechanism separate. CAS correctness must not depend on GC scoring; compiler-key correctness must not depend on UI.

## 32. Build Order Within the One Architecture

This is implementation dependency order, **not separate product versions**:

1. Freeze invariants, identity rules, schemas, IPC contracts, and managed-root semantics.
2. Implement CLI/planner/workspace identity.
3. Implement daemon, metadata, leases, accounting, and managed build roots.
4. Implement disk budgets, capacity reservations, and GC.
5. Implement rustc wrapper and conservative cacheability classifier.
6. Implement CAS, manifests, atomic publication, and materialization.
7. Implement safe worktree path normalization.
8. Implement single-flight compiler deduplication.
9. Implement Cargo shim/wrapper composition.
10. Complete observability, doctor, integrity checking, and recovery.
11. Only then consider optional remote CAS support.

Do not ship artifact reuse until differential testing proves equivalence. Conservative bypasses are a feature, not a failure.

## 33. Testing Strategy

### Compatibility corpus
Run real Cargo projects through normal Cargo and `rgo`, comparing exit status, produced artifacts, test behavior, executable behavior, diagnostics class, and rebuild behavior.

### Matrix
Test:

- stable, beta, nightly
- macOS, Linux, Windows
- GNU/MSVC toolchains
- host and cross compilation
- workspaces
- features
- custom profiles
- build scripts
- proc macros
- native dependencies
- rustflags/rustdocflags
- custom linkers
- clippy/rustdoc
- tests/benches/examples
- offline/frozen
- path/git dependencies
- multiple registries
- Git worktrees

### Concurrency torture
Run dozens of builds while GC operates. Kill producers, clients, and the daemon at arbitrary points. Verify no deadlocks, leaked leases, or incorrect hits.

### Fault injection
Test disk-full conditions, permission failures, DB corruption, CAS bit flips, process crashes, atomic-rename failures, clock changes, and filesystem capability differences.

### Worktree stress
Create/delete/recreate many worktrees with equivalent and divergent commits. Measure actual physical storage and cache reuse.

## 34. Acceptance Gates

Before calling the architecture production-safe:

- Zero known incorrect cache hits in the compatibility corpus.
- Crash recovery never requires manual cache deletion.
- Concurrent builds cannot corrupt CAS objects.
- GC never removes leased/pinned state.
- Bypass mode always works.
- Existing Cargo command behavior remains intact.
- Build output remains correct across supported platforms.
- Cache/database corruption degrades to rebuilding, not project failure.
- Worktrees demonstrate meaningful physical-storage reduction.
- Managed storage respects configured limits within documented transient headroom.
- The tool can explain misses/bypasses well enough to debug correctness.

## 35. What We Explicitly Do Not Do

Do not:

- Fork Cargo unnecessarily.
- Reimplement Cargo dependency resolution.
- Parse Cargo's private target layout as an API.
- Share arbitrary `target` directories globally.
- Disable incremental compilation without explicit user policy.
- Assume crate version/features fully identify compiled output.
- Cache unknown native/build-script behavior optimistically.
- Delete active build state.
- Require a cloud service.
- Optimize hit rate at the expense of correctness.

## 36. Long-Term Extension: Remote Cache

The local architecture should make a remote CAS possible without redesigning the system.

```text
local CAS
    |
MISS
    |
remote CAS
    |
MISS
    |
compile locally
    |
commit local
    |
optional upload
```

Remote storage must remain optional. Artifact manifests, digests, namespace rules, and compiler identity already provide the foundation.

Do not make remote caching part of the correctness path.

## 37. Core Product Promise

The desired experience is:

```text
Install rgo.
Use Rust normally.
Create ten worktrees.
Let several coding agents build concurrently.
Delete projects.
Switch toolchains.
Build debug and release.

Do not think about target directories.
```

The machine owns reusable build state, keeps active state useful, deduplicates work where equivalence is provable, and reclaims reproducible data before disk pressure becomes the developer's problem.

The success metric is not “we clean `target` better.”

It is:

> **Rust storage consumption should scale with unique useful build state, not with the number of projects, worktrees, agents, and abandoned target directories.**

## 38. Primary Technical Risks

The hardest parts are:

1. Correct compiler cache identity.
2. Safe path normalization across worktrees.
3. Existing rustc-wrapper composition.
4. Correct handling of build scripts/native inputs.
5. GC while builds are active.
6. Physical-storage accounting across reflinks/hard links.
7. Cross-platform file semantics.
8. Preserving debugging/source paths.
9. Cache schema/toolchain compatibility.
10. Avoiding performance regressions from hashing and metadata operations.

These should drive the engineering and testing effort.

## 39. Final Architecture Decision

The system is **not a cleaner** and **not merely a shared target directory**.

It is a local Rust build-storage runtime consisting of:

```text
Cargo-compatible command layer
            +
managed mutable build contexts
            +
rustc interception/cacheability layer
            +
machine-wide immutable CAS
            +
repository/worktree identity
            +
single-flight concurrency coordination
            +
bounded automatic GC
            +
crash-safe metadata/recovery
```

Cargo remains the source of truth for how Rust projects are built. `rgo` becomes the source of truth for **where the resulting build state lives, when it can be reused, and when it should disappear**.

