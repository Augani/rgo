# Benchmarks and measurement methodology

## Recorded results (macOS 15, APFS, Cargo 1.98, 2026-09-18)

Fixture: workspace {app, cli, bench} bins + {shared} lib; serde, serde_json,
tokio(full), rand, anyhow — ~60 crates. Full data: `docs/dogfood-2026-09-18.md`.

| Metric | Plain cargo | rgo managed | Delta |
|---|---|---|---|
| Checkout `target/` after `cargo build` | 194.3 MiB | 8.7 MiB | −95.5% on the checkout volume |
| + no-op + incremental + `--release` | — | 11.1 MiB | intermediates stay out of the checkout |
| Allocated-byte estimate removed by orphan GC | — | 181.2 MiB of 482.1 | contexts orphaned by checkout deletion are collected |
| Cross-volume (APFS image) `target/` | — | 11.1 MiB | uplifted outputs are copied, not hardlinked |

These are historical, single-fixture measurements. Relocation reduced the
checkout's `target/` size; it did not reduce total machine storage by 95.5%.
The balance moved to `$RGO_HOME/builds`. A manual orphan-GC pass reclaimed one
context, but unattended cleanup remains disabled by default until the
[lifecycle and budget gates](installation-storage-plan.md) pass. Same-volume
hardlinks can make uplifted outputs cheap on the checkout volume; cross-volume
copies add another copy of each final binary. These runs did not establish a
compiler RAM reduction. The storage figures are allocated-file estimates, not
unique physical extent ownership. The recorded GC estimate does not establish
181.2 MiB of volume space returned: CoW sharing, snapshots, external hardlinks,
sparse/compressed allocation, and filesystem metadata can change that result.
Measure volume free space before and after a pass separately; concurrent
outside writes can also affect that observed delta.

## How to reproduce

```bash
# Private environment — never your real cargo home.
export D=/tmp/rgo-bench HOME=$D/home CARGO_HOME=$D/home/.cargo RGO_HOME=$D/home/.rgo
export RUSTUP_HOME=~/.rustup RUSTUP_TOOLCHAIN=stable
rgo setup --no-service
cd $PROJECT && cargo build
du -sm $PROJECT/target $RGO_HOME/builds   # managed vs. a plain-cargo baseline
```

1. **Storage growth**: `du -s` the checkout `target/` and `builds/` after
   build, rebuild, `--release`, and a second checkout (worktree).
2. **GC effectiveness**: in a private home with no active Cargo process,
   delete a checkout, age/orphan its context, run `rgo gc` and
   `rgo gc --aggressive`, then compare status estimates with measured allocated
   bytes and volume free space. This manual scenario does not prove safe
   unattended deletion during concurrent builds.
3. **Experimental cache reuse**: in disposable homes only, opt into
   `[cache] enabled = true`, build two checkouts of the same workspace with
   `remap_workspace_paths`, and compare byte-for-byte outputs with uncached
   controls. The cache remains off by default while its P6 correctness and
   useful-reuse gates are open.
4. **Added build latency**: wall-clock `cargo build` before/after setup on a
   warm checkout. The wrapper adds a classify + daemon round-trip (~150 ms
   client timeout worst case) per rustc invocation; a hit returns in
   milliseconds vs. seconds for real compiles.

## Performance gates (before cache ships on-by-default)

- Cache-hit latency must be materially below compile latency — covered in
  tests by a 3 s producer vs. <2 s hit on the same key.
- Wrapper overhead per rustc invocation should stay in the low-millisecond
  range on a warm daemon; measure `rgo status` daemon round-trip under load.
- GC must never delete a context in use by Cargo, a valid lease, or a pin.
  Current fixtures cover selected cases; the full process-lifetime race matrix
  remains a P2 release gate.
- No rgo-attributable build failure is acceptable under daemon loss, CAS
  corruption, remote outage, or storage pressure. Existing fault-injection
  fixtures are partial evidence, not a complete failure matrix.

## Known measurement gaps (operator gates)

- Real-rustc differential corpus (byte-identity across a crate matrix) —
  tests use deterministic fake-rustc fixtures.
- Filesystem matrix numbers beyond APFS/ext4 — CI has a btrfs smoke lane;
  XFS/ZFS are untested.
- ENOSPC behavior requires a full-volume or quota fixture not yet automated.
- Very large workspaces (>1 k crates) — dogfooding so far is a ~60-crate
  fixture.
