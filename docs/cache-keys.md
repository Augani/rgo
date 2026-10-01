# Cache key audit and classifier table

The dependency cache (`[cache] enabled = true`) is opt-in and conservatively
scoped. Every artifact key is a BLAKE3 digest over a fixed, length-delimited
field list. The key schema is `rgo-cache-v5`. The classifier still has
unverified dynamic-input classes, so caching remains experimental and disabled
by default. A key collision can return incorrect output.

## Key inputs

| Field | Source | Notes |
|---|---|---|
| schema tag | `rgo-cache-v{N}` constant | versioned; a format change rotates all keys |
| compiler identity | `rustc -vV`, canonical sysroot path, Rustup distribution manifest, sysroot compiler bytes, and the resolved invoked compiler's path and bytes | toolchains without the manifest bypass; dynamically loaded compiler libraries are not yet content-verified |
| normalized args | all rustc args after normalization | managed paths rewritten relative to the validated build context, preserving the profile/artifact suffix; ordering preserved |
| source digest | BLAKE3 over `(relpath, bytes)` for every file in the package source root, sorted | strict mode: any symlink fails the digest → bypass |
| source kind | Registry / Git / Workspace | registry and git checkout contents are digested; workspace members are handled separately |
| extern inputs | `(name=file, file-content digest)` for cacheable `--extern` artifacts | externs must resolve inside managed roots, else bypass; dynamic libraries bypass because proc macros may read untracked inputs while executing. Bare `--extern proc_macro` (no path) records `sysroot` — it is pinned by compiler identity. Newer cargo may pass the same crate twice (`.rlib` and `.rmeta` externs); each artifact's digest is keyed separately |
| env digest | `(name, BLAKE3(value))` for every inherited environment variable | conservative response to arbitrary `env!` and `option_env!`; missing versus present values differ; plus `OUT_DIR_CONTENTS` digest when `OUT_DIR` is set. Environment churn can reduce hits. |
| target triple | `--target` value when present | cross builds never collide with host builds |
| remap prefix | `(from, to)` of the applied `--remap-path-prefix` | source path normalization preserves relative suffixes; environment-dependent absolute paths can still distinguish worktrees |
| OUT_DIR digest | content digest of the build-script output dir | build-script-produced inputs change the key |

## Classifier table

`classify()` returns `Cacheable` only inside the v1 boundary. Everything else
returns `Bypass(reason)` and the wrapper compiles normally — a bypass is never
a failure. Reasons are stable strings, recorded in `state/cache-events.log`.

| Invocation shape | Decision |
|---|---|
| `--crate-type` lib / rlib (any mix) | cacheable |
| single `proc-macro` crate type | cacheable |
| single `bin` with `--emit` ⊆ {dep-info, metadata} (no link) | cacheable |
| `--emit` ⊆ {dep-info, metadata, link} | required for cacheability |
| workspace member crate | cacheable only with consistent remap; else `unsupported_workspace_source` |
| `bin`/`cdylib`/`staticlib`/`dylib` with `link` emit | `unsupported_crate_type` |
| emit kinds outside the allowlist (asm, mir, llvm-ir, …) | `unsupported_emit` |
| `-Cincremental`, `-Csave-temps` | `incremental`, `save_temps` |
| `-Z` flag not on the allowlist | `unstable_flag` |
| `-Zembed-metadata` (nightly cargo default) | keyed like any other arg — allowlisted because cargo emits it on every nightly invocation |
| target JSON file/path, explicit `--sysroot` | `custom_target`, `custom_sysroot` |
| `--extern` dynamic library (`.so`, `.dylib`, `.dll`) | `dynamic_extern`; a proc-macro consumer may read inputs absent from the compiler key |
| `-l`, `-Lnative=`, `-Clinker=`, `-Clink-arg*`, `-Clink-self-contained` | `native_input` |
| `-L` path outside build root / sysroot, or unresolvable `--extern` | `external_extern` |
| `OUT_DIR` outside the managed build root | `build_script_output` |
| `OUT_DIR` undigestable (symlinks, unreadable entries) | `invalid_out_dir` |
| source not under an allowed root | `source_outside_cache` |
| source path is a symlink / unreadable | `unsafe_path` / `missing_source` |
| compiler version/sysroot probe fails, or toolchain lacks Rustup manifest/compiler executable | `compiler_identity` |
| no `--out-dir` derivable output | `missing_output_directory` |
| conflicting/multiple workspace remaps | `remap_conflict` |
| non-UTF-8 argument or environment name/value | `unsupported_encoding` |
| inner rustc wrapper composition issues | `inner_wrapper` |
| `[cache] enabled = false` (default) | `cache_disabled` |
| daemon reports storage pressure on acquire | `free_space_pressure` |
| daemon IPC unreachable after ~2s of retries | `daemon_unreachable` |

Notes on invocation-derived key inputs:

- `--cap-lints` is part of normalized args, so `cargo build -vv` (which passes
  `--cap-lints warn` where a normal build passes `allow`) produces different
  keys — correct, but expect a parallel key family if you mix verbosity.
- Path-bearing `--extern` inputs are hashed by **file content**, so a
  nondeterministic upstream artifact (build-script consumer, proc-macro dylib)
  cascades a new key into every dependent that reads it.
- The wrapper retries `CacheAcquire` for up to ~2s on transient IPC failures
  (parallel commit bursts can stall responses past the 150ms socket timeout)
  before recording `daemon_unreachable` and compiling normally.
- Toolchain layout: stable cargo emits dep artifacts under `<profile>/deps/`;
  nightly (cargo ≥1.100) uses the per-unit layout
  `<profile>/build/<pkg>/<hash>/out/` with colocated `fingerprint/`. The wrapper
  is layout-agnostic (it works from each invocation's `--out-dir`), and the
  whole `build/` tree is already a documented intermediate, so relocation and
  caching can work on both layouts. Cargo may also use an artifact lock for
  final outputs; rgo has not established a complete lock protocol for safe
  unattended deletion on the new layout. See P2 of the installation plan.

## Integrity and recovery

- Objects are content-addressed (`objects/<2hex>/<digest>`), published via
  write-temp + fsync + digest-verify + atomic rename, and stored read-only.
- `write_manifest` re-verifies every referenced object before publication.
- Every read path verifies: `verify_object` checks size + BLAKE3 before
  materializing; failures move the object to `quarantine/` and degrade to a
  normal compile. A corrupt manifest fails `read_manifest` → daemon reports
  `integrity:` miss → wrapper compiles.
- Single-flight: the daemon arbitrates producer/waiter per key; an expired
  producer lease lets a waiter take over, so a crashed compiler cannot wedge
  a key.
- Materialization order: APFS clonefile → same-volume hardlink → copy. All
  outputs are written only after verification; hit bytes are byte-identical
  to the producer's.
- Output ownership: `--out-dir` is shared by every crate in the graph, so a
  producer collecting outputs from a directory scan only claims files whose
  stem names that invocation (`{lib?}{crate_name}{extra_filename}`).
  Cargo compiles dependencies in parallel; without the stem filter a sibling
  crate's outputs landing mid-scan enter the wrong manifest and poison hits
  (observed: `materialization_failed` on `dep_a` because `dep_proc`'s `.d`
  was captured as a second `dep-info` output).

## Observability

- Wrapper appends one JSON line per invocation to `state/cache-events.log`:
  `{key, outcome: hit|bypass|published|compiled, bytes, reason}`.
- The daemon drains events into SQLite; `rgo status` reports hit/miss/
  corruption counters and single-flight stats.
- Bypasses carry the stable reason strings above; a missing or unexplained
  decision is a bug.
- `RGO_KEY_DEBUG=1` makes the wrapper append each candidate's key, source
  digest, `OUT_DIR` digest and full normalized arg list to
  `state/key-debug.log` — the tool for diffing why two invocations keyed
  differently.

## Known residual risks (why cache stays opt-in)

- Non-deterministic rustc output (absolute paths, hashing order) outside the
  normalized/remapped set can produce correct-but-different bytes; covered by
  byte-equality tests on deterministic fixtures, and by remap for workspaces.
  Measured on real cargo: build-script consumers embed their per-context
  `OUT_DIR` path and proc-macro dylibs carry a linker-generated per-build
  field, so cross-*compile* byte equality is only asserted for path-free
  artifacts — hits still return the publisher's verified bytes verbatim.
- A real-Cargo offline `file://` Git dependency corpus compares path-free
  outputs with a bypass control and runs the resulting binary. The conservative
  all-environment key can miss across Cargo contexts, so this corpus is no
  longer evidence of cross-project hits. An env-gated registry corpus
  (`RGO_CORPUS_ONLINE=1`, override crates/toolchains via `RGO_CORPUS_CRATES` /
  `RGO_CORPUS_TOOLCHAINS`) requires its separate operator/CI run. Dynamic
  input validation and measured reuse remain gates before enabling by default.
