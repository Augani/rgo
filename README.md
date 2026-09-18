# rgo

Bounded, machine-managed Rust build storage. Install once, keep using plain `cargo`;
`rgo` moves intermediate build artifacts out of every checkout's `target/` into one
managed root, and garbage-collects them safely so deleted projects never leave
multi-GB directories behind.

## What it does

- Redirects Cargo's `build.build-dir` to `~/.rgo/builds/{workspace-path-hash}` via a
  reversible fenced block in `$CARGO_HOME/config.toml`.
- Final artifacts stay put: `target/debug/<bin>`, `cargo run`, scripts and IDEs keep
  working identically. Only intermediates (`deps/`, `incremental/`, `.fingerprint/`,
  build-script output) move.
- A per-user daemon coordinates concurrent builds (leases) so GC never deletes a
  live or pinned context.
- Optional phases (compiler-result cache, remote CAS) are implemented but off by
  default while correctness gates run; see `PLAN.md`.

## Install

```bash
cargo install rgo rgo-rustc-wrapper   # both binaries, side by side
# or unpack a release archive for your platform (contains both binaries)
```

`rgo setup` requires `rgo-rustc-wrapper` next to the `rgo` executable.

## Setup

```bash
rgo setup            # one-time, idempotent, prints every change first; --dry-run to preview
rgo doctor           # verify config precedence, toolchains, filesystem, service health
cargo build          # plain cargo; intermediates now land under ~/.rgo/builds
rgo status           # managed bytes, budget, reclaimable
rgo ls               # every managed build-dir with attribution
```

`rgo setup` changes exactly three things, all reversible:

1. Creates `~/.rgo/` (or `$RGO_HOME`).
2. Adds a fenced `[build] build-dir = ".../{workspace-path-hash}"` block — plus
   `build.rustc-wrapper` so dependency invocations can reach the optional cache —
   to `$CARGO_HOME/config.toml`, preserving any wrapper you already had.
3. Installs a per-user background service (launchd/systemd/Task Scheduler) running
   `rgo daemon`. `--no-service` skips this; GC then runs opportunistically.

## Everyday commands

```bash
rgo gc [--dry-run] [--aggressive] [--target 20GB]   # reclaim in tier order
rgo pin <id> / rgo unpin <id>                      # protect a context from GC
rgo clean <id>                                     # remove one context now
rgo adopt [--delete] [dirs...]                     # reclaim stray target/ intermediates
rgo cache stats|explain <key>|verify               # inspect the opt-in cache
```

## Precedence and bypass

Your settings always win. Project `.cargo/config.toml`, `CARGO_TARGET_DIR`,
`CARGO_BUILD_BUILD_DIR` and `--target-dir` override rgo — `rgo doctor` reports such
projects as unmanaged.

`RGO_BYPASS=1` makes every rgo binary a pure passthrough (the wrapper `exec`s rustc,
`rgo <cmd>` execs cargo with `RGO_*` env stripped). Per-project opt-out is simply the
project's own `.cargo/config.toml`, which already takes precedence.

## Recovery

| Symptom | Fix |
|---|---|
| Something, anything | `RGO_BYPASS=1 cargo build` — rgo becomes invisible |
| One project misbehaves | set `build.build-dir`/`target-dir` in its `.cargo/config.toml` |
| Daemon wedged | `pkill rgo` — clients auto-restart it; builds never depend on it |
| Metadata database corrupt | self-heals: moved to `state/meta.sqlite.corrupt-*` and rebuilt from the filesystem |
| CAS object corrupt | quarantined automatically to `~/.rgo/quarantine/`; next build misses cleanly |
| Full uninstall | `rgo setup --undo`, then remove `~/.rgo` and the binaries |

## Upgrade / uninstall

- Upgrade: `cargo install rgo rgo-rustc-wrapper --force`, or unpack a newer archive.
  On-disk state migrates forward automatically; sidecar/protocol versions are
  recorded in `state/meta.sqlite`.
- Uninstall: `rgo setup --undo` removes the fenced config block and the service.
  Then delete `~/.rgo` and the two binaries.

## Configuration

`~/.rgo/config.toml` — all optional, machine-aware defaults:

```toml
[storage]
max_size = "auto"        # auto = clamp(15% of volume, 20GB, 150GB)
soft_watermark = 0.80
min_free_space = "auto"  # auto = max(10% of volume, 20GB)

[gc]
incremental_retention = "7d"
context_retention = "30d"
orphan_grace = "1h"
auto = true

[cache]                  # Phase 3 — off by default
enabled = false
```

## License

MIT OR Apache-2.0
