# Cache key audit and classifier table

The dependency cache (`[cache] enabled = true`) is opt-in and conservatively
scoped. Every artifact key is a BLAKE3 digest over a fixed, length-delimited
field list; changing any input produces a different key, so an incorrect key
can only cause a *miss*, never a wrong hit — provided every input that affects
output bytes is represented below. This document is the audit table for that
claim (schema version `rgo-cache-v2`).

## Key inputs

| Field | Source | Notes |
|---|---|---|
| schema tag | `rgo-cache-v{N}` constant | versioned; a format change rotates all keys |
| compiler identity | full `rustc -vV` output | version, commit, host, LLVM — not just a version number |
| normalized args | all rustc args after normalization | paths rewritten to roots-relative form; ordering preserved |
| source digest | BLAKE3 over `(relpath, bytes)` for every file in the package source root, sorted | strict mode: any symlink fails the digest → bypass |
| source kind | Registry / Git / Workspace | registry and git checkouts are immutable inputs; workspace members are handled separately |
| extern inputs | `(name, file-content digest)` for every `--extern` | externs must resolve inside managed roots, else bypass; bare `--extern proc_macro` (no path) records `sysroot` — it is pinned by compiler identity |
| env digest | `(name, BLAKE3(value))` for `CARGO_PKG_*`, `CARGO_CFG_*`, `CARGO_CRATE_NAME`, `CARGO_MANIFEST_DIR`, `RUSTUP_TOOLCHAIN` | plus `OUT_DIR_CONTENTS` digest when `OUT_DIR` is set |
| target triple | `--target` value when present | cross builds never collide with host builds |
| remap prefix | `(from, to)` of the applied `--remap-path-prefix` | workspace "from" is omitted so equivalent worktrees share one key |
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
| any `-Z` flag | `unstable_flag` |
| `-l`, `-Lnative=`, `-Clinker=`, `-Clink-arg*`, `-Clink-self-contained` | `native_input` |
| `-L` path outside build root / sysroot, or unresolvable `--extern` | `external_extern` |
| `OUT_DIR` outside the managed build root | `build_script_output` |
| `OUT_DIR` undigestable (symlinks, unreadable entries) | `invalid_out_dir` |
| source not under an allowed root | `source_outside_cache` |
| source path is a symlink / unreadable | `unsafe_path` / `missing_source` |
| `rustc -vV` fails | `compiler_identity` |
| no `--out-dir` derivable output | `missing_output_directory` |
| conflicting/multiple workspace remaps | `remap_conflict` |
| inner rustc wrapper composition issues | `inner_wrapper` |
| `[cache] enabled = false` (default) | `cache_disabled` |
| daemon reports storage pressure on acquire | `free_space_pressure` |

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

## Known residual risks (why cache stays opt-in)

- Non-deterministic rustc output (absolute paths, hashing order) outside the
  normalized/remapped set can produce correct-but-different bytes; covered by
  byte-equality tests on deterministic fixtures, and by remap for workspaces.
  Measured on real cargo: build-script consumers embed their per-context
  `OUT_DIR` path and proc-macro dylibs carry a linker-generated per-build
  field, so cross-*compile* byte equality is only asserted for path-free
  artifacts — hits still return the publisher's verified bytes verbatim.
- A real-cargo differential corpus (compile vs. hit on a crate matrix) remains
  an operator gate before enabling by default, alongside perf measurement.
