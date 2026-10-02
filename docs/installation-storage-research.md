# Research: install once, keep using Cargo

**Reviewed:** 2026-09-27–28. **Repository baseline:** `838fb22`.
**Status:** research baseline with an active implementation follow-up; see the dated supervised-Cargo finding in the plan.
**Implementation checklist:** [installation-storage-plan.md](installation-storage-plan.md).

## Recommendation

Keep the current Cargo-native integration: one installer configures `build.build-dir`, a small compiler wrapper, and a user service. Users continue to run ordinary Cargo commands. Keep separate mutable build directories for each workspace; share only compiler results whose inputs are demonstrably equivalent. Make bounded storage and trustworthy cleanup the first release, with compiler caching experimental until its correctness gates pass.

**2026-09-28 correction:** Cargo-native relocation alone cannot prove whole-command lifetime safety for unattended GC. The implementation now has a separate opt-in Unix supervised launcher that selects the managed build directory only for intercepted Cargo processes and retains an external context lock through the real Cargo process. Native relocation remains an evaluation path with automatic destructive GC off. The original recommendation below describes the baseline decision, not a completed safety proof or current public default. The [implementation checklist](installation-storage-plan.md) tracks the remaining installer, bypass, platform, and GC gates.

The repository already implements much of this architecture. The review found release blockers, including a reproduced incorrect cache hit and a pressure policy that can retain every workspace's only context. Existing checked boxes in `PLAN.md` establish implementation history, not sufficient evidence for shipping the current guarantees.

“Memory” in this proposal primarily means **disk storage**: the `target/` problem. Peak compiler RAM is a separate resource problem. Cache hits may avoid some compiler work, but relocation and garbage collection do not impose a RAM limit.

## 1. What Rust already shares, and what Go does differently

| Layer | Cargo today | What rgo should own |
|---|---|---|
| Downloaded dependency sources | Registry archives/sources and Git repositories live in Cargo home and are reused by projects using that home. | Observe separately; leave downloading, resolution, source integrity, and source-cache cleanup to Cargo. |
| Mutable build state | Workspace builds retain compiler intermediates, incremental state, build-script products, and variants. | Central placement, attribution, safe retention, resource accounting, and cleanup. |
| Reusable compiler results | Reuse depends on matching compilation inputs; compiler caches can help across workspaces. | A conservative, optional immutable cache, only after semantic correctness is demonstrated. |
| Final outputs | Executables, docs, packages, and other requested products have user-facing locations. | Preserve their expected locations and report them separately from managed intermediates. |

Cargo home already contains the shared download cache. Different `CARGO_HOME` values, containers, or vendored sources can still create multiple copies. rgo should not invent another source cache. [Cargo Home](https://doc.rust-lang.org/cargo/guide/cargo-home.html)

Go also has **two caches**: a module source cache and a build cache. Its module cache is shared by projects, but has no maximum size and is not automatically pruned. Its build cache is safe for concurrent Go commands and periodically removes older entries. The useful inspiration is the automatic build-cache experience, not a promise that downloading each dependency once eliminates build storage. [Go module cache](https://go.dev/ref/mod#module-cache), [Go build-cache behavior](https://go.dev/src/cmd/go/alldocs.go)

Rust artifacts depend on more than crate name/version: compiler and sysroot, target, profile, features, flags, dependency outputs, environment, generated inputs, and sometimes arbitrary compile-time code. Equivalent worktrees are a better initial reuse workload than unrelated projects. This matches the Cargo maintainer's assessment of shared-cache limitations. [A Vision for Cargo](https://epage.github.io/blog/2026/08/cargo-vision/#shared-caches)

## 2. Integration options

| Option | Same user commands? | Reach and limitations | Decision |
|---|---|---|---|
| Shell alias or function for `cargo` | Yes, in configured shells | Does not reliably cover IDE process launches, direct executable paths, or other shells. Adds shell-specific quoting and forwarding behavior. | Reject as the default. |
| Cargo `[alias]` | Only for new shorthand commands | Cannot redefine Cargo's built-in subcommands. | Cannot meet the requirement. |
| A `cargo` executable earlier on `PATH` | Usually | Can supervise entire invocations, but must preserve rustup selection, signals, recursion, custom subcommands, nested Cargo, and executable-path discovery. Absolute toolchain paths bypass it. | Contingency only if a demonstrated lifecycle requirement needs it. |
| Global `CARGO_TARGET_DIR` | Yes | Changes final output paths and puts unrelated projects in one mutable tree; cleaning has broad scope. | Reject for default integration. |
| Global `build.build-dir` plus compiler wrapper | Yes | Applies wherever Cargo reads the configured home, including direct Cargo launches. Overrides and alternative homes need accurate reporting. Wrapper does not cover the complete Cargo lifetime. | Recommended foundation; GC lifecycle proof remains mandatory. |
| Cargo fork | Requires custom distribution | Strongest access to lifecycle and graph information, largest maintenance burden. | Prefer upstream collaboration instead. |

Bash does not expand aliases in noninteractive shells by default. Rustup already provides proxy executables for toolchain selection; an extra Cargo shim would need to preserve that behavior. [Bash aliases](https://www.gnu.org/software/bash/manual/html_node/Aliases.html), [rustup proxies](https://rust-lang.github.io/rustup/concepts/proxies.html)

Cargo allows a global `build.build-dir` and path templates; `build.rustc-wrapper` supplies the interception point. Cargo aliases cannot replace built-in commands. These are separate mechanisms with different responsibilities. [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)

`build.build-dir` became stable in **Cargo 1.91**. rgo's source-build MSRV and the Cargo version required to manage a user's projects must be documented separately. Older toolchains cannot be advertised as receiving the same managed-storage behavior. [Cargo stabilization record](https://doc.rust-lang.org/cargo/reference/unstable.html#build-dir)

The target/build split lets requested final artifacts retain their paths while intermediates move. Cargo explicitly treats the build-directory layout as internal; tools that depend on old internal paths still need compatibility testing. [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)

### The recommended installed state

Conceptually, setup writes the following keys into the effective Cargo home, merging with existing TOML instead of duplicating tables:

```toml
[build]
build-dir = "/absolute/rgo/root/builds/{workspace-path-hash}"
rustc-wrapper = "/absolute/stable/install/path/rgo-rustc-wrapper"
```

The user continues with `cargo build`, `cargo test`, `cargo run`, `cargo check`, and `cargo +nightly …`. Neither project manifests nor shell aliases are required. A user service performs maintenance even when the user only invokes Cargo.

This reaches one configured **user/environment**, not every container or remote machine. Existing custom wrappers may take precedence. Installation must distinguish “storage managed”, “compiler cache connected”, and “automatic maintenance healthy”.

## 3. Current repository review

| Area | Present in the repository | Assessment |
|---|---|---|
| Setup and undo | Fenced Cargo config edits, sibling-wrapper discovery, platform service integration. | Reuse; harden transactions, effective config discovery, repair, and stable install paths. |
| Storage management | Workspace-hash roots, sidecars, pins, physical allocation scanning, tiered GC, adoption. | Useful foundation; budget and liveness issues below block the advertised contract. |
| Coordination | User daemon, IPC, SQLite index, leases, operation serialization, failure fallbacks. | Reuse; wrapper leases are not full Cargo-session leases. |
| Compiler-result cache | Key classifier, CAS, materialization, single-flight, diagnostics, opt-in remapping. | Off by default; reproduced wrong hit prevents promotion. |
| Remote cache | Optional HTTP CAS and tests. | Defer additional investment while local release blockers remain. |
| Packaging | Workflow bundles both executables and checksums. | No complete verified one-command install-and-activate flow. |
| Evidence | Offline real-Cargo tests, an optional registry corpus, historical APFS dogfood data. | Good starting coverage; several completion claims exceed the evidence. |

Primary code reviewed: `crates/rgo/src/cmd/{setup,doctor,passthrough,gc}.rs`, `crates/rgo-core/src/{cargo_config,paths,context,gc,daemon,db,service,size,config}.rs`, `crates/rgo-rustc-wrapper/src/main.rs`, `crates/rgo-key/src/lib.rs`, `crates/rgo-materialize/src/lib.rs`, tests, and both workflows. These are repository-relative references at the baseline above.

### Confirmed findings and their release impact

| ID | Finding | Evidence | Consequence |
|---|---|---|---|
| F1 | Target-directory overrides do not disable managed intermediates. | Isolated real-Cargo probes below; `precedence.rs` checks final output presence but does not prove absence of managed intermediates. | Correct docs, doctor, opt-out UX, and tests. Do not force new interception merely to preserve a mistaken claim. |
| F2 | Doctor labels Cargo 1.85 as build-dir compatible. | `doctor.rs::check_toolchains` compares against `(1, 85, 0)`; upstream stabilization is 1.91. | Separate runtime capability from source-build MSRV; test the true boundary. |
| F3 | Pressure eviction protects the latest context of every workspace. | `gc.rs::plan` constructs `keep_latest`; reproduced 6 MiB against a 1 MB target with no eligible action. | With normally one context per workspace, this can protect all recent-but-idle projects indefinitely until age policy permits eviction. |
| F4 | The budget is not a unified builds-plus-CAS budget. | `daemon.rs::maintenance` and `run_gc` sum contexts; `append_unreferenced_cas` keeps every object referenced by any retained manifest. | Add manifest eviction, active-publication protection, total accounting, and log/temp bounds before claiming a bounded cache. |
| F5 | Expired orphans do not independently trigger automatic GC. | Maintenance and `gc --auto` return early without size/free-space pressure. | A deleted project's context can remain past the documented orphan grace while under budget. |
| F6 | Optional cache can return the wrong program result after an environment change. | Real compiler probe below; `relevant_environment` hashes a short allowlist but excludes arbitrary `env!` inputs. | Correctness blocker, even for immutable Git/registry source. Keep cache experimental and fix input identity. |
| F7 | GC liveness check releases its probe lock before rename. | `context.rs::lock_files_for_safety` drops each file handle; `gc.rs::stage_and_remove` later renames. | A start/check/delete race remains a design issue. Rename alone is not proof of safety on Unix. |
| F8 | Claimed nightly lock coverage was absent at baseline. | Earlier plans mentioned `.cargo-artifact-lock`; baseline `context.rs::lock_files` enumerated only `.cargo-build-lock` and `.cargo-lock`. The current heuristic uses only the allowed `.cargo-build-lock`. | Do not claim new-layout GC safety without a reviewed contributor-contract change and a complete lifecycle proof. |
| F9 | Plain Cargo attribution may name a workspace member as the root. | A private native-Cargo regression reproduced the member root. The wrapper now asks `cargo locate-project --workspace --manifest-path <package manifest>` when creating a context and marks Cargo-resolved sidecars; older unmarked sidecars are corrected on the next primary package compile. | This fixes fresh native member builds and eventual repair of legacy sidecars, but a warm no-op build does not invoke rustc, and the broader workspace lifecycle matrix remains open. |
| F10 | `--no-service` does not itself ensure maintenance after plain Cargo use. | Wrapper `request()` only connects; daemon startup lives in rgo command handling. | Relocation may work while cleanup never runs. Make degraded state explicit or add a tested bounded startup path. |
| F11 | Full test suite stalled in concurrent DB initialization. | Sample captured seven worker threads in `Barrier::wait` and the parent waiting to join. | Investigate startup/recovery failures and make test failures bounded. No full-suite pass claimed. |

F7 is a code-review risk, not a reproduced deletion of live data. F4, F5, F8–F10 are direct implementation observations; their full failure matrices were not exercised here. F1, F3, and F6 were reproduced in private sandboxes.

Additional audit work: `normalize_path_token` removes the suffix beneath a normalized root, so different paths can collapse to one normalized token. Source-root hashes do not establish the identity of arbitrary external includes or proc-macro inputs. Materialization permits read-only hardlinks on some paths; owner-controlled permissions are not a general immutability boundary. These require explicit negative cases, not an assumption that matching cached bytes prove correctness.

### Isolated experiment A: unchanged commands and overrides

Environment: macOS arm64, local Cargo `1.98.0 (797e8a9bc 2026-08-05)`, rustc `1.98.0 (88d9e12ae 2026-08-18)`. Each probe used private `HOME`, `CARGO_HOME`, and `RGO_HOME`; `rgo setup --no-service`; dependency-free projects; and real `cargo build --offline`.

| Build setting | Intermediates managed by rgo? | Expected final executable present? |
|---|---|---|
| Plain `cargo build` | Yes | Yes |
| `CARGO_TARGET_DIR=<custom>` | Yes | Yes, at custom target |
| `cargo build --target-dir <custom>` | Yes | Yes, at custom target |
| Project `build.target-dir` only | Yes | Yes, at custom target |
| `CARGO_BUILD_BUILD_DIR=<custom>` | No | Yes |
| `RGO_BYPASS=1 cargo build` | Yes | Yes |
| Project `build.build-dir="{workspace-root}/target"` | No | Yes |

For the CLI-only target override, the executable was inspected at the requested target; a separate metadata invocation did not inherit that one-off flag. All probes inspected Cargo's reported build directory as well. There is no inference that target placement overrides an explicitly configured build directory.

**Proposed contract:** respect Cargo's independent settings. `RGO_BYPASS` bypasses wrapper/cache behavior, but does not undo Cargo configuration or automatically exclude the directory from a separately running GC service. Full rollback is coordinated disablement/undo, not merely an environment variable.

### Isolated experiment B: budget enforcement

Created three different existing workspace manifests and three managed contexts, each holding a 2 MiB file. Sidecars and timestamps were one hour old, no pins or leases, no recent locks. Configuration: `max_size="1MB"`, `min_free_space="0B"`, automatic GC disabled for controlled execution. A private foreground daemon handled:

```text
rgo gc --dry-run --aggressive --target 1MB
nothing to reclaim (managed 6.0 MiB, target 976.6 KiB)
```

This isolates the permanent “latest for every workspace” protection from live-build protection. The recommended replacement is a preference for recent contexts that yields under pressure, while active builds and explicit pins remain protected.

### Isolated experiment C: a wrong cache hit

A private, locally committed `file://` Git dependency contained:

```rust
pub fn value() -> &'static str { env!("APP_BUILD_FLAVOR") }
```

Three identical consumer manifests in different checkouts printed that value. The Git source revision, compiler, features, and source files stayed unchanged. A private daemon had `[cache] enabled=true`, `[gc] auto=false`, and zero minimum-free-space reserve to avoid pressure bypasses. `cargo fetch` accessed only the local Git fixture; compilation used `--offline`.

| Consumer | Environment | Wrapper mode | Printed result |
|---|---|---|---|
| Publisher | `APP_BUILD_FLAVOR=alpha` | Cache enabled | `alpha` |
| Second checkout | `APP_BUILD_FLAVOR=beta` | Cache enabled; recorded hit | **`alpha` — incorrect** |
| Fresh control checkout | `APP_BUILD_FLAVOR=beta` | `RGO_BYPASS=1` | `beta` |

Publisher and consumer had the same cache key; the consumer logged `outcome:"hit"`. This distinguishes input-key unsoundness from a damaged CAS object. Verifying that restored bytes equal publisher bytes is necessary for integrity but insufficient for compilation correctness.

Reproduce by creating the dependency and three consumers in a temporary directory; set the environment independently for each `cargo build`; run each final executable; compare against the bypassed control; inspect cache events. Keep the temporary path short enough for the platform IPC socket and assert daemon readiness before interpreting cache results. An initial probe under a long system temporary path bypassed with `daemon_unreachable`; that run was not evidence of cache correctness.

### Validation performed

- `cargo build`: passed; rebuilt the workspace binaries.
- `cargo clippy --all-targets`: passed without warnings.
- `cargo test`: did not finish; stopped the stalled `db::tests::concurrent_writers_survive_wal_busy_contention` process after capturing its stack sample. Cause is not yet established.
- `cargo test -- --skip db::tests::concurrent_writers_survive_wal_busy_contention`: completed, 103 tests reported passed and one filtered out. This does not clear the full-suite gate; the env-gated online corpus was not exercised.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- The optional online registry corpus, other operating systems, fresh-machine installers, and a real low-space filesystem were not rerun for this review.

No developer Cargo configuration, shell aliases, or system services were changed. Temporary probe daemons were terminated and sandbox data removed. No implementation fixes are included in this research change.

## 4. Current alternatives and upstream direction

| Approach | Strength | Limitation relative to rgo's intended experience |
|---|---|---|
| Cargo's source-cache GC | Already automatic for Cargo-home downloads. | Does not currently provide the desired stable build-storage lifecycle. |
| sccache | Established compiler caching; global wrapper configuration; useful comparison baseline. | Cannot cache incremental Rust compilations or crates invoking the system linker; cached results and local build trees still need storage management. |
| cargo-sweep | Selectively cleans build artifacts by age, toolchain, or size. | A maintenance tool to invoke or schedule; does not establish the desired cross-project immutable compiler cache. |
| cargo-reapi | Explores worktree reuse, coalescing, strict input control, and resource coordination. | Its documented workflow uses a driver and strict sandbox prerequisites. It is a design reference, not a verified drop-in substitute for unchanged Cargo here. |
| Cargo's planned shared cache | Can use Cargo's own dependency graph and build lifecycle. | An upstream project direction, not evidence that a stable end-to-end solution has shipped. |

Cargo's automatic source-cache GC excludes build artifacts and is disabled offline. rgo should coexist with it instead of competing for Cargo-home deletion. [Cargo global cache cleanup](https://doc.rust-lang.org/cargo/reference/config.html#global-caches)

sccache's own documentation specifies the incremental and linking limits; disabling incremental globally would alter the normal edit/build tradeoff. Retain user settings and benchmark the combination. [sccache caveats](https://github.com/mozilla/sccache#known-caveats), [sccache cache-key inputs](https://github.com/mozilla/sccache/blob/main/docs/Caching.md)

cargo-sweep documents selective cleanup. cargo-reapi documents both its custom command flow and sandbox requirements. Their upstream claims were reviewed, not independently benchmarked in this task. [cargo-sweep](https://github.com/holmgr/cargo-sweep), [cargo-reapi](https://github.com/TamedTornado/cargo-reapi)

As checked during this review, whole-directory GC, cache-size policy design, moving regular projects' build directories to Cargo home by default, and a per-user compiled artifact cache still have open upstream issues. Those are separate efforts. [Whole-directory GC #13136](https://github.com/rust-lang/cargo/issues/13136), [size policy #13062](https://github.com/rust-lang/cargo/issues/13062), [default location #16147](https://github.com/rust-lang/cargo/issues/16147), [compiled artifact cache #5931](https://github.com/rust-lang/cargo/issues/5931)

The accepted 2026 Cargo shared-cache goal explicitly starts conservatively and seeks nightly experimentation before stabilization. Build-dir layout v2 was re-stabilized in a PR merged on August 18, 2026 and assigned to 1.100.0. Some book pages still describe its earlier unstable state; versioned behavior tests and release-specific sources must take precedence over assuming “nightly” means one permanent layout. [2026 goal](https://goals.rust-lang.org/2026/cargo-cross-workspace-cache.html), [layout v2 PR](https://github.com/rust-lang/cargo/pull/17354)

**Inference for rgo:** the durable opportunity is installation, lifecycle policy, reliable accounting, compatibility diagnostics, worktree UX, and convenient adoption of safe upstream caching. Keep the native cache behind a replaceable internal boundary. Reassess it against upstream Cargo before investing in broader custom caching or remote features.

## 5. Boundaries that should shape the plan

1. **Storage relocation is not total disk reduction.** Historical measurements reduced checkout `target/` from 194.3 MiB to 8.7 MiB while creating 181.2 MiB of managed intermediates. That primarily proves relocation. Measure total allocated storage, actual frees, and reuse separately. See [existing dogfood data](dogfood-2026-09-18.md).
2. **A budget is an eviction policy, not a filesystem quota.** Active/pinned contexts may exceed it. A build can consume disk faster than cleanup frees it. Explain protected bytes and unmet targets; never silently promise a strict maximum while refusing to evict all eligible data.
3. **Wrapper lifetime is shorter than Cargo lifetime.** No-op builds, build-script execution, rustdoc, running tests, and gaps between compiler invocations require their own safety argument. A grace period or successful stress test is not that argument.
4. **IDEs can override wrappers.** rust-analyzer documents using its own `RUSTC_WRAPPER` for build scripts and offers separate target-directory settings. Integration must test these paths without requiring users to disable editor features. [rust-analyzer configuration](https://rust-analyzer.github.io/book/configuration.html#rust-analyzer.cargo.buildScripts.useRustcWrapper)
5. **Some inputs are executable behavior.** Build scripts and procedural macros can access external state. Digests of package sources and `OUT_DIR` alone cannot prove all inputs were captured. [Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html), [Rust procedural macros](https://doc.rust-lang.org/reference/procedural-macros.html)
6. **A source install involves two packages and a registry naming constraint.** The `rgo` name [already belongs to an unrelated Go compiler toolchain](https://crates.io/crates/rgo), so `cargo install rgo` would install the wrong program. The CLI package has been renamed to `rgo-storage` while retaining the `rgo` executable. `rgo-rustc-wrapper` remains a separate package. Internal runtime dependencies now declare exact registry versions alongside local paths; the private, path-only `rgo-testkit` dev-dependency is omitted from the published package by Cargo, and the integration tests that use it are excluded from the CLI package. The packages cannot be verified against crates.io as a complete install until the internal crates have actually been published in dependency order. Cargo installs executable targets from selected packages. [cargo install](https://doc.rust-lang.org/cargo/commands/cargo-install.html), [path dependency publication](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#specifying-path-dependencies), [Cargo packaging](https://doc.rust-lang.org/cargo/commands/cargo-package.html)
7. **Recovery must work without the wrapper binary.** A missing executable configured as a Cargo wrapper fails before rgo can choose to pass through. Stable install paths, upgrade ordering, and undo-before-delete matter more than a generic “fail open” claim.
8. **Physical savings are filesystem-specific.** Hardlink allocation accounting is useful but does not measure unique CoW extents or actual bytes freed when external links remain. Report estimates honestly and validate against volume-level measurements.

These findings lead to the staged, reviewable checklist in [the proposed plan](installation-storage-plan.md).

### Implementation follow-up (2026-09-28)

The crates.io API returned an existing `rgo` package (version 0.1.0, unrelated
repository) and no matching packages for `rgo-storage`, `rgo-rustc-wrapper`, or
the internal publishable crate names on 2026-09-28. Name availability is only a
point-in-time observation, not a reservation. All eight publishable packages
contain both license files and pass local `cargo package` verification when
unpublished dependencies are patched to extracted `.crate` contents. A local
`cargo install --locked --offline` of the two packaged binaries activated a
private Cargo home, plain Cargo built a disposable project, and doctor verified
relocation; setup undo then restored the private config. The CI source-package
preflight now reproduces this sequence. Actual crates.io resolution and public
source installation still require publication in dependency order. The
checkout initially had no Git remote and its declared repository URL was
unavailable. On 2026-09-28 an owned public [Augani/rgo](https://github.com/Augani/rgo)
repository was created and configured as `origin`. The guarded implementation
was subsequently pushed to `master`. The [15-job public CI matrix](https://github.com/Augani/rgo/actions/runs/36799141501)
passed on Linux, macOS, and Windows, including the Cargo 1.91 supervised
clean-and-pin probe. A [branch-only release run](https://github.com/Augani/rgo/actions/runs/36798553223)
built all four platform archives and the complete bundle; a downloaded copy
passed all six SHA-256 checks and all six workflow/source-commit attestation
checks. A tagged release, public installer endpoint, clean-machine bootstrap,
and unattended-GC safety remain unverified.

Service ownership follow-up: the original fixed launchd/systemd/Task Scheduler
name let distinct storage roots contend for one per-user registration. The
current implementation derives the registration name from `RGO_HOME`, stores a
single Cargo-home owner marker beside non-evictable root state, and recognizes
the v1 fixed name during setup/undo. The Windows task now passes an explicit
daemon home. Local rendering and private-home setup tests cover the identity
and same-root rejection, but no live service-manager upgrade or restart/login
matrix has run. A root path reached through different aliases may still need
normalization; shared-root Cargo homes are rejected, not coordinated. Windows
task-action ownership remains unverified.

Microsoft documents `schtasks /Query /TN … /XML` as a way to export one task's
definition, and the Task Scheduler XML schema places the executable and its
arguments in the `Exec` action's `Command` and `Arguments` elements
([query command](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-query),
[action schema](https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-exec-actiongroup-element)).
The Windows path now compares the exported `Exec` action with the installation
record before replacement or removal, and treats query failures other than
the expected missing-task HRESULT as errors. A local XML fixture and Windows
cross-compilation pass; Task Scheduler's actual XML encoding, exit codes,
action normalization, and restart behavior still require a Windows runner.
Status continues to use task registration as its running signal; setup also
requires compatible daemon IPC, but this does not independently attribute the
process to that scheduled task.

The [systemd environment directive](https://github.com/systemd/systemd/blob/main/man/systemd.exec.xml)
requires quoting an assignment containing spaces and expands `%` specifiers.
The scoped Linux unit now quotes its executable and `RGO_HOME` assignment,
escapes literal `%` (and `$` in the executable command), while keeping the
old rendering available for fixed-name service migration. The ownership
comparison also accepts a v1 file with no `RGO_HOME` directive, which the old
setup emitted when that variable was absent. A Linux cross-check of `rgo-core`
passes using a temporary Zig C wrapper; actual systemd manager behavior still
requires the Linux CI and service smoke.

The migration audit found another safety mismatch: the old `rgo adopt --delete`
selected `deps`, `build`, and `.fingerprint` beneath legacy `target/` directories.
Those names exceed the contributor layout contract, and a lock probe could not
exclude a fresh plain-Cargo process. The command now reports whole-target
allocated-byte estimates only, explicitly including final outputs and user
files; `--delete` fails without modifying them. The estimate is not a
reclaimable-byte claim. The implementation also stopped inspecting
`.cargo-lock` and `.cargo-artifact-lock` for liveness; only the allowed
`.cargo-build-lock` remains as a defense-in-depth heuristic, with unattended
destructive GC still disabled.

Cargo's current [`clean --dry-run --verbose` option](https://doc.rust-lang.org/cargo/commands/cargo-clean.html) previews what a whole-target clean would remove, but the documented no-option clean deletes the entire target directory, including final outputs that rgo promises to leave alone. `rgo adopt --preview-full-clean <project>` now invokes that Cargo preview for one explicit target with its build directory directed to the same target. A private-home Cargo 1.91 probe confirmed the preview leaves a built executable intact; a real-Cargo sandbox fixture covers the command. Cargo requires a valid `CACHEDIR.TAG` to preview cleaning the target, so an arbitrary synthetic directory cannot be passed off as a Cargo-owned target. This preview is useful for evaluating an explicitly destructive cleanup choice; it is not a safe selective-deletion plan or a reclaimable-intermediates estimate. The read-only adoption scan also treats an unreadable nested target path as an unavailable estimate instead of silently undercounting it.

The initial fixes added sandbox regressions for F1, F3, and F6, corrected the
doctor version boundary, and made database-open recovery discriminate confirmed
SQLite corruption from ordinary open errors. These changes do not close the
installation or unattended-cleanup release gates.

Cargo's [current `Layout::new` source](https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/compiler/layout.rs.html)
shows the build-directory lock acquired before compilation and retained in the
layout. rgo currently probes a lock and then drops its handle before renaming
the context. **Inference:** a Cargo process already waiting on the old lock-file
inode can proceed after rgo has renamed or removed its directory, while a new
process may create a lock at the original path. This is a lifecycle race to
reproduce and resolve; a passing ordinary build/GC stress test does not prove
whole-directory deletion safe. The checked storage release remains blocked by P2.

Using `cargo clean` as the unattended whole-context deleter does not close this
gap: Cargo's [clean implementation](https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/ops/cargo_clean.rs.html)
explicitly skips acquiring a lock for whole-directory clean and removes both
target and build roots. It also removes final outputs that rgo promises to leave
in the checkout. Package-specific clean takes a layout lock, but its behavior
is different from reclaiming an entire idle context.

A private Cargo 1.98.0 `build`/`clean` probe with an explicit `build.build-dir`
also confirmed that whole-directory `cargo clean` removes rgo's top-level
sidecar and pin marker along with the build directory. The current supervised
launcher `exec`s Cargo and has no post-command restoration hook. This supports
treating a missing sidecar as unverified attribution during GC; it does not
require an explicit clean to clear an rgo pin. Pin intent is now stored under
`state/pins`, outside Cargo's build directory, and `rgo unpin` records an
authoritative unpinned decision.
The repeatable private-home supervised fixture also confirms that `clean -p`
leaves the sidecar and that the next build after full `clean` recreates it on
the installed Cargo 1.98.0. Other Cargo versions remain unverified.

The current [layout implementation](https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/compiler/layout.rs.html)
places its build lock under each target/profile and can omit it on NFS. It
creates that profile directory before opening the lock. **Inference:** even
holding every lock observed during a scan would not exclude a Cargo process
starting a previously unseen profile, and moving the parent directory still
changes the path behind any process already waiting on an observed lock. A
lock-held, in-place removal of the documented incremental subdirectory may be
worth a separate scoped experiment, but it cannot prove safe whole-context
eviction or bound all build intermediates. This is a feasibility finding, not a
completed safety proof; P2 needs a cooperating Cargo lifecycle hook or an
explicitly chosen interception design with a mixed-launch safety contract.

Cargo's [proposal to move build directories into its global cache by default](https://github.com/rust-lang/cargo/issues/16147)
remains open and explicitly lists [whole-target GC](https://github.com/rust-lang/cargo/issues/13136)
as a blocker. This is upstream confirmation that central relocation and
whole-build lifecycle cleanup are separate problems. It does not change rgo's
current safety decision: `gc.auto` remains off until P2 has a coordination
mechanism and test evidence.

A further private Cargo 1.98 probe evaluated the supervised alternative. With
`--config build.build-dir="<private-root>/{workspace-path-hash}"`, a normal
build kept its final binary in the checkout while moving intermediate `deps`
outside it; a direct Cargo invocation in a different project, without that
override, used its checkout's `target/debug/deps`. `cargo +stable` accepted the
same command-line override. This supports isolating shimmed builds in a GC
namespace that direct Cargo does not select by default, while preserving the
user's `cargo` spelling. It does not yet test PATH activation, signals, nested
invocations, locks, or GC. [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html)
documents that command-line `--config` overrides environment and config-file
values. [Cargo metadata](https://doc.rust-lang.org/cargo/commands/cargo-metadata.html)
provides `workspace_root` for deriving an rgo-owned context ID, but labels
`build_directory` unstable, so the latter is unsuitable as the stable lock key.
The P2 plan now records the guarded shim candidate and its required proof.

The subsequent Unix pilot uses a separate rgo-owned hash of Cargo's reported
`workspace_root`, so it does not parse Cargo's unstable hash layout or rely on
the unstable `build_directory` metadata field. In a private Cargo home, a PATH
entry point ran the same `cargo build` and `cargo +stable run` spellings while
holding an external lock through Cargo's process lifetime. The live GC probe
preserved that active context, reclaimed an unrelated idle context, and later
reclaimed the original after Cargo exited. A direct real-Cargo invocation used
checkout intermediates, and a member `--manifest-path` resolved to its virtual
workspace root. This narrows the feasibility uncertainty but is not a proof
for Windows, nested/overridden commands, PATH activation across IDEs, or
unsupervised launches into a user-selected managed path.

A 2026-09-28 follow-up checked the installed Cargo 1.98 stable and
`cargo 1.100.0-nightly (e8cb624d5 2026-08-22)`. In two disposable Cargo homes,
plain nightly `cargo build --offline` and nightly with
`-Z build-dir-new-layout` both honored rgo's configured build directory,
wrote an rgo context sidecar, left the requested binary in the checkout's
`target/debug`, and placed a `.cargo-build-lock` under the observed `debug/`
profile. This is only a small compatibility probe, not a GC safety test or a
cross-platform guarantee. Cargo's [new build-dir layout transition](https://github.com/rust-lang/cargo/pull/17354)
is moving forward, so the nightly layout needs a separate fixture before rgo
claims support for its incremental-tier or liveness heuristics. Meanwhile the
[current nightly GC source](https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/workspace/gc.rs.html)
still calls the target/artifact cleanup hook future work (`Gc::gc` runs the
global cache tracker and then says other GC operations, such as target
cleaning, may be added later). The [whole-target GC issue](https://github.com/rust-lang/cargo/issues/13136)
remains open. Thus the new layout is not evidence of a safe context-wide
cleanup API.

Setup follow-up in this implementation pass: rgo now serializes its own
setup/undo processes, writes a durable activation pointer and install record
in Cargo home before exposing the config fence, checks a matched wrapper
version/protocol, and restores the previous state if the config update fails.
A sandbox build from a fresh process without `RGO_HOME` reached a custom root;
the explicit `rgo doctor --verify` disposable plain-Cargo build observed its
sidecar there and rejected a build-directory environment override. Ordinary
doctor inspection now has structured JSON output and does not run write-based
filesystem probes. These results do not demonstrate GUI, Windows-service,
external-config-writer, or cross-platform installer behavior yet.


## Compiler-broker admission follow-up — 2026-10-02

Sccache's [execution-mode documentation](https://github.com/mozilla/sccache/blob/main/docs/Architecture.md#execution-modes)
places default compiler execution and output writes in its background server.
Inference: guarding Cargo's process tree alone cannot prove that this writer
has finished after its client exits. Client-side mode may be ignored for some
options, so that environment setting is insufficient to establish protection.
No sccache corruption was reproduced in this review.

The implemented supervised policy preserves custom compiler/wrapper commands
and uses ordinary Cargo storage for them. It checks inherited producer settings
and Cargo's ancestor/home config filenames and include chains; unreadable
settings also use ordinary storage. Only an adjacent matched rgo wrapper without
an inner wrapper is allowed. The no-wrapper path needs no extra executable
probe. Doctor reports the exception. The existing Unix real-Cargo fixture passed
both environment-selected and included-config wrappers, confirmed the wrapper
actually ran, checked correct program output, and required that the managed
context was neither refreshed nor newly created.

Admission revision 2 uses a fresh context namespace. Older contexts remain
accounted and protected; refresh cannot promote them. The focused GC fixture
preserved both native and old supervised contexts while reclaiming a current
idle neighbor. Protocol 7 rejects an older daemon's admission policy; setup can
request only the unchanged Shutdown operation using a recorded protocol 6.
The existing no-service lifecycle fixture passed that wire-version upgrade and
required release of the daemon singleton before activating the new record.

The [green 18-job matrix](https://github.com/Augani/rgo/actions/runs/36967189762)
passed all workspace suites, zero-warning Clippy, installation/lifecycle pilots,
Cargo-version boundaries, btrfs, and 100-project recovery on the declared lanes.
The revised custom-rustdoc fixture also passed ordinary-storage passthrough and
global-guard release. No new test cases were added: existing fixtures were extended.

External brokers launched by build scripts, linkers, runners, or custom toolchains
and settings changed after inspection remain unproven. The default still leaves
automatic GC and compiler caching off. This is progress on the configured
producer boundary, not completion of P2 or the storage release gate.


## Free-space reserve reporting follow-up — 2026-10-02

Status previously explained only managed-size budget excess. A root could be
below that limit while its volume lacked the configured free-space reserve;
protected storage then left no explicit reserve-shortfall explanation. Status
now reports the observed deficit and the portion not covered by current eligible
allocated-byte estimates, with protection/ineligibility reasons. The fallback
without a daemon can show an observed deficit but leaves eligibility unknown.

The existing pinned-budget fixture retains its reclamation/unpin controls and
now also exercises a below-budget root with pinned data and an impossible
configured reserve, without filling the volume. Its IPC and CLI checks passed
locally and in the [18-job matrix](https://github.com/Augani/rgo/actions/runs/36969296634), along with
backward-compatible status decoding. Source accounting notes, README, and the
historical benchmark table distinguish allocation estimates from returned
volume space; snapshots, CoW sharing, outside links, and concurrent outside
writes prevent an exact physical-savings claim. This is reporting progress;
it does not close the broader P2/P3 release gates.

## Automatic defaults on small volumes — 2026-10-02

The fixed 20 GiB automatic floor exceeded the capacity of the existing 2 GiB
btrfs fixture. Automatic policy resolution now uses a floor of
`min(20 GiB, capacity / 4)` for both the managed-size target and free-space
reserve. Their sum stays within half the reported capacity; defaults on volumes
of at least 80 GiB and explicit byte settings are preserved. A zero-capacity
report fails resolution. The existing configuration fixture covers capacity
boundaries and large inputs, and the existing pressure fixture checks the actual
mounted btrfs root while retaining positive reclamation controls. The
[18-job matrix](https://github.com/Augani/rgo/actions/runs/36972164863) passed all
jobs. This corrects capacity feasibility, not current free-space availability or
safe behavior after every ENOSPC/publication failure.

## Automatic no-progress retry limit — 2026-10-02

Low free space could previously request a full automatic GC pass on every
30-second maintenance tick, even when pins or data outside rgo made the reserve
unattainable. A pass that reclaims nothing while pressure remains now establishes
a two-minute monotonic retry delay. Bounded metadata maintenance continues;
queued unpins and newly idle supervised Cargo sessions can request earlier
recovery, and explicit GC bypasses the automatic scheduler. Restarting the
daemon establishes a fresh scheduling baseline.

The existing pinned-budget fixture observes completed maintenance ticks without
another full-GC record, then requires unpin recovery within 15 seconds while
the two-minute delay remains active. It passed locally with the all-bin build,
zero-warning all-target Clippy, formatting, and diff checks. The wider single-pass
work bound, launch-storm behavior, and P2 lifecycle proof remain open.
The [18-job matrix](https://github.com/Augani/rgo/actions/runs/36973401620) also
passed, including full workspace suites and 100-project recovery on Linux,
macOS, and Windows. No new test cases were added; the existing fixture gained
the retry and recovery assertions.

## Resumable Windows uninstall — 2026-10-02

The Windows installer previously removed activation, commands, PATH entries,
and ownership metadata in sequence without an uninstall snapshot. It also used
command digests observed before undo when later removing those commands. The
new uninstall journal binds retries to the original installer/pending state and
activation record. Pending removal blocks install, file digests are rechecked
before deletion, and changed installer metadata is preserved. The journal remains
usable after the installer-state file has been removed; completed repeated
uninstall is harmless. Existing versioned binaries, Cargo fallback copies, and
managed storage remain outside removal.

The existing private Windows installer probe adds controlled failures after
undo, first command removal, PATH restoration, and state removal. It requires
recovery, rejects an intervening install, preserves edited command/state files,
and retains its running-Cargo and exact raw PATH/type controls. A local PowerShell
helper probe passed with undo mocked; both installer/probe scripts parse. A second
local control edited the command during mocked undo and verified the later digest
check preserved it. The [18-job matrix](https://github.com/Augani/rgo/actions/runs/36976744030)
passed, including the live Windows installer recovery and running-Cargo/PATH
controls, full workspace suites, and 100-project recovery. These boundaries do not prove every internal
core undo or service-manager write, power-loss durability, or safe data purge.

## Linux service activation — 2026-10-02

A persistent systemd user manager can search a different configuration root from
the setup process. Enabling the owned unit by absolute path lets systemd create
the required link into its load path ([systemctl enable semantics](https://github.com/systemd/systemd/blob/main/man/systemctl.xml)).
New scoped units also omit `After=default.target`: the target automatically
orders itself after wanted services, so the former reverse ordering could create
a startup cycle ([target default dependencies](https://github.com/systemd/systemd/blob/main/man/systemd.target.xml)).
Ownership checks still recognize the exact historical scoped definitions.
Explicit setup now enables the definition, resets that unit's failed/start-limit
state, and starts or restarts it. This lets repair and rapid upgrade rollback
recover an exhausted counter while ordinary automatic restarts retain systemd's
limit ([reset-failed semantics](https://github.com/systemd/systemd/blob/main/man/systemctl.xml)).

The existing Unix service-installer probe now covers Linux stable with a real
user manager and private Cargo/storage homes. It exercises crash recovery, a
fresh manager start observed through read-only doctor queries, replacement of
the historical definition, and the existing native/supervised repair, interrupted
upgrade, and uninstall sequence. Restarting the user manager requires explicit
disposable CI opt-in and refuses a worker running inside that manager. The probe
sets private XDG configuration roots for both installation modes and reports the
unit status/journal on failure. Local all-bin build and zero-warning Clippy
passed. The [18-job matrix](https://github.com/Augani/rgo/actions/runs/36987224017)
passed after the explicit start-limit recovery change, including the complete
Linux lifecycle sequence, full workspace suites, zero-warning Clippy, and the
100-project recovery probes. No new Rust test cases were added.
This does not establish GUI login,
reboot, WSL/container behavior, or every service-manager interruption boundary.

## macOS descendant supervision — 2026-10-02

A private-home real Cargo audit reproduced deletion while a build-script child
was still alive. Python's normal `subprocess.Popen` closes descriptors by
default, and `start_new_session=True` detaches the child. After Cargo exited,
rgo's inherited lock no longer protected the child: manual GC removed its
managed `OUT_DIR`, and the subsequent write failed with `ENOENT`.
[Python's process API](https://docs.python.org/3/library/subprocess.html),
[repeatable audit](probes/README.md). This is a concrete counterexample, beyond
the previously inferred compiler-broker boundary. Automatic GC staying off does
not protect an explicit manual GC request.

The user selected macOS first for stronger supervision. XNU's process kqueue
filter rejects recursive `NOTE_TRACK` flags, so a fork notification or repeated
PID scan is insufficient evidence for complete descendant tracking.
[Apple's event-filter implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_event.c).
Apple's resource coalitions preserve membership across fork, exec, and
posix_spawn; launchd creates them and IDs are not reused within a boot.
Inference: a unique owned launchd job provides a useful kernel lifetime domain
for Cargo and closed-FD descendants.
[Apple's coalition design](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/coalitions.md).
Launching inside a terminal or IDE's existing coalition instead would retain
storage for that application's entire lifetime, which does not satisfy useful
independent project reclamation.

The experimental `rgo-core` path now dynamically observes kernel membership and
records it, with the boot-session UUID, in a synced file outside the evictable
context. GC checks those receipts after acquiring its existing exclusion
guards. Corrupt state or failed queries refuse cleanup; an absent coalition is
never treated as an empty one. Receipts are bounded to 4 KiB and 32 coalition
IDs. A single private launchd fixture on macOS 27.2 arm64 now demonstrates real
GC preserving the writer after Cargo exits, rejection of a malformed receipt,
a successful late write, and reclamation after the writer exits. Build and
zero-warning Clippy passed locally. The
[18-job matrix](https://github.com/Augani/rgo/actions/runs/36992158237) passed at
`325b2e5`, including macOS stable/beta/nightly workspace suites, source builds
on Rust 1.85, installer lifecycle probes, and existing 100-project recovery.
The macOS beta job's log identifies macOS 14.8.9 build 23J631, arm64, and a
successful coalition fixture. This does not establish Intel or the full macOS
runtime range. One new automatic Rust regression was added; the expected-bug
audit remains outside the routine suite.

This remains a prototype: ordinary installed Cargo launchers do not register
receipts, older daemon policies do not check them, and the private observation
interfaces need supported-version evidence. Doctor now warns about the ordinary
Unix cleanup gap. The detailed [macOS implementation checklist](macos-cargo-supervision.md)
covers launchd job ownership, original descriptor/argument handoff, signals,
startup failure, crash recovery, job reaping, admission/IPC revision changes,
and actual launcher/IDE validation. P2 and the install-and-forget release gate
remain open.

## macOS installed-launcher pilot — 2026-10-02

The installed Cargo shim now has a private `RGO_MACOS_SUPERVISOR_PILOT=1` path
that creates a unique owned launchd job. A bounded Unix-socket handshake checks
the peer's user identity and invocation token, transfers the original standard
descriptors with `SCM_RIGHTS`, and carries arguments/environment as native byte
vectors. The job holds its context guard and syncs the kernel receipt before
the caller's one-use start commit. Preparation failure uses checkout storage;
failure after commit never launches Cargo a second time.
[Apple's descriptor-message API](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/recvmsg.2.html),
[peer-identity API](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getpeereid.3.html).

The existing coalition fixture now uses the actual installed launcher instead
of a separate test-only job helper. It kills the guardian after Cargo exits,
requires a closed-FD writer to remain protected from real GC, verifies its late
write, and requires reclamation after exit. In the same fixture, healthy
completion preserves stdin/EOF, stdout/stderr, non-UTF-8 argument and environment
bytes, the application argument separator, and exit status 17; the job directory
and receipt then retire. It passed locally on macOS 27.2 arm64. The existing
setup recovery fixture also passed recorded protocol-7 Shutdown recovery.

Admission revision 3 and protocol 8 fence the new policy from older daemons.
The coalition path uses a distinct namespace so it cannot reuse descriptor-only
histories. Normal activation is not changed yet: terminal signal/job-control
proof, complete abandoned-job recovery, startup/cancellation boundaries, and the supported
runtime matrix remain in the [macOS checklist](macos-cargo-supervision.md).
Automatic GC stays off by default. The fixture count is unchanged.

The next follow-up persists a versioned job-owner record containing its context
before bootstrap. Both daemon maintenance and real GC acquire exclusive scope
guards before recovering idle jobs, and sync receipt pruning before reaping a
coalition. Missing/reaped IDs and invalid records remain errors. Reads and
metadata passes are bounded; edited definitions and added content survive.
The existing coalition fixture now verifies restoration followed by maintenance
recovery with destructive automatic GC off. The existing interrupt case runs
the installed pilot and verifies SIGTSTP/SIGCONT followed by Ctrl-C, retained
child protection, and unrelated idle reclamation. Both passed locally.

Terminal descriptors need a separate handoff: Apple's `tcsetpgrp` requires the
controlling terminal and process group to belong to the caller's session.
Descriptor passing alone cannot establish this relationship for a launchd job.
The private pilot therefore falls back to checkout storage for terminal
invocations before admission while that transport is implemented.
[Apple's terminal process-group contract](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/tcsetpgrp.3.html).
A focused `macos-15-intel` lane reuses the two existing cases to extend kernel
interface and launcher evidence beyond arm64. All metadata interruption points,
loaded-job replacement, terminal control, and the declared runtime range remain
open; this follow-up adds no Rust test case.

The [19-job platform run](https://github.com/Augani/rgo/actions/runs/36998573830)
passed at `5557f94`. Its focused Intel lane identifies macOS 15.7.9 build
24G830, x86_64, and Cargo/rustc 1.99.0; both real-launcher fixtures passed there.
The local existing paired benchmark also ran the actual guardian with cache
and automatic GC off, requiring no guardian fallback. Its
[recorded samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian.json) show
241.1 ms median cached-build overhead and 154.2 ms edit-build overhead, exceeding
the proposed 100 ms allowance for this tiny crate. Performance is therefore
still an activation gate. The next [concrete implementation checklist](macos-cargo-supervision.md#terminal-handoff-and-measured-launch-overhead)
combines terminal handoff, latency attribution, and scheduling/notification
improvements before normal activation.

The terminal transport at `0bb4ee3` now creates a guardian-owned PTY and sets
Cargo's foreground group before exec. The extended existing private zsh fixture
locally exercised controlling `/dev/tty`, colors, input/EOF, resize,
Ctrl-Z/fg/bg, TOSTOP, Ctrl-C, mixed redirected streams, normal mode restoration,
and late detached output. It adds no Rust test case. Interactive job scheduling
and child/socket readiness reduced the
[first combined probe](benchmarks/2026-10-02-macos-arm64-cargo-guardian-interactive.json)
to 126.5/127.9 ms added median no-op/edit latency, which still fails the proposed
100 ms allowance. The
[nonterminal spawn follow-up](benchmarks/2026-10-02-macos-arm64-cargo-guardian-spawn.json)
at `dd09d11` measured 113.7/120.6 ms added medians, still above that allowance.
These debug-binary probes do not settle optimized installer performance.

The subsequent [optimized-pair probe](benchmarks/2026-10-02-macos-arm64-cargo-guardian-release.json)
at `50a8405` measured 109.4/109.7 ms added median no-op/edit time with the same
sample counts and private activation. That still exceeds the proposed 100 ms
allowance; it does not justify changing the target or enabling normal activation.
The same source passed the [19-job matrix](https://github.com/Augani/rgo/actions/runs/37005085858),
including Intel macOS, full platform suites, and the Rust 1.85 source builds.

The following batch bounded the bootstrap helper and overlapped fresh Cargo
version/workspace queries. Its [optimized paired probe](benchmarks/2026-10-02-macos-arm64-cargo-guardian-parallel.json)
at `c788254` measured 98.6 ms added no-op time and 111.7 ms added edit time.
The edit result still exceeds the allowance. A separate
[diagnostic probe](benchmarks/2026-10-02-macos-arm64-cargo-guardian-stages.json)
records the stage costs, so further optimization can target observed delays
without removing durable ownership or coalition admission.

Caller SIGKILL exposed another activation defect: zsh can partly alter the
original terminal before the guardian's exact-mode restore. Job retirement
completes, but remaining relay flags affect the terminal. The observation prints
the before/after settings explicitly and does not claim crash restoration
passed. Mode ownership, prompt editing after failure, other shells, and the
remaining interruption/runtime matrix stay open in the
[implementation checklist](macos-cargo-supervision.md#terminal-handoff-and-measured-launch-overhead).

## macOS abandoned terminal-host recovery — 2026-10-02

The subsequent registered-zsh prototype stores its lease outside Cargo job
data. Complete host records now have a separate maintenance retirement path:
current-boot shells and lease callers must be confirmed no longer live;
uncertain identities, nonlocal filesystems, unsupported records, added files,
and changed bytes or inodes preserve the metadata. Background retirement never
opens a terminal or applies saved settings. A synced sibling journal survives
individual removals, so the daemon can resume without the original host header.
Authorization is tied to the exact bytes included in that journal, and the
directory removal is synced before the journal disappears.

Inspection of the installed `fs4` API found that nonblocking lock acquisition
returns `Ok(false)` for contention. The earlier terminal helper ignored that
boolean. Host and retirement operations now require `true`. The existing
macOS fixture checks contention with a distinct live descriptor, a surviving
process in the exited shell's session, preservation of added/edited content,
and continuation by a real daemon restarted after the first unlink. It adds
no Rust test case. Incomplete registrations and all interruption points remain
outside this completed-record design; the broader activation and performance
gates remain open in the [checklist](macos-cargo-supervision.md#abandoned-terminal-host-retirement).

The [19-job platform run](https://github.com/Augani/rgo/actions/runs/37020240840)
passed at `95d7358`, including the extended registered-shell fixture in the
Intel guardian lane and full macOS stable/beta/nightly suites, all three Rust
1.85 source builds, Linux/Windows suites and installer checks, and the existing
100-project recovery probes. Local macOS 27.2 arm64 all-bin build, the two
existing focused cases, all-target Clippy with warnings denied, format, and
diff checks also passed. This is completed-batch evidence, not normal activation
or an unattended-storage release claim.

## macOS Cargo job retirement audit — 2026-10-02

Inspection found that job cleanup could remove its ownership header and folder
before the unload helper completed. A crash left no durable record for another
process to finish that operation. The new bounded sibling journal retains the
exact owner bytes, directory identity, and four known entry fingerprints through
unload acknowledgement. Startup refuses a pending journal; recovery acquires the
existing exclusive scope guards and receipt checks before resuming. Changed
remaining files survive. New journal publication cannot overwrite an existing
destination. Later preparation failures also preserve unknown or incomplete
content rather than recursively removing the temporary job directory.

The owner context was also absent from its generated definition. An edit to
that field alone could redirect housekeeping to another idle context. New
schema 3 includes the context as a program argument; startup matches that
argument, the owner, and the invocation, and rechecks ownership after acquiring
the guard. Older schemas are preserved. The existing real-Cargo fixture covers
the edited field while its detached writer is alive, an actual old definition,
partial removal, later edits, daemon restart, and journal retry after unload.
Platform results, the initial interrupt failure and optimized measurements are
recorded below.

The terminal relay now uses an owned wake pipe for replies and signal delivery
instead of its recurring foreground timer. It retains a background-only
foreground-discovery check: [zsh 5.9's job implementation](https://github.com/zsh-users/zsh/blob/zsh-5.9/Src/jobs.c#L2547)
attaches an already-running job's terminal without sending `SIGCONT`. A silent
running background process is therefore included in the existing terminal
fixture. Independent startup flushes now overlap, but all file flushes complete
before directory flushes, and every required flush completes before bootstrap.
The optimized measurements below assess the combined path; they do not isolate
each change's causal contribution.

The local `launchctl(1)` manual states that `print` output is not an API, so
its human-readable structure must not become a production ownership proof.
Apple provides [SMJobCopyDictionary](https://developer.apple.com/documentation/servicemanagement/smjobcopydictionary(_:_:)?language=objc)
for a structured description, but marks it deprecated. The local SDK header
also warns that the returned dictionary is incomplete and unstable across OS
releases, and that the function will be removed without a replacement. This
API alone therefore cannot establish the remaining loaded-definition ownership
gate across supported runtimes. Its actual domain/error behavior and a bounded,
runtime-gated alternative still need investigation. The journal changes do not
close that gate or claim a launch-latency improvement.

The optimized [registered-shell paired probe](benchmarks/2026-10-02-macos-arm64-cargo-guardian-job-recovery.json)
at clean `779397e` completed 24 no-op and 12 edit-build pairs with actual
guardian admission, cache off, and automatic GC off. Added median wall time was
94.1 ms for no-op builds and 93.0 ms for edit builds, within the proposed 100 ms
allowance for this tiny crate. No-op supervised p95 was 185.4 ms, higher than
the earlier registered-shell sample; the raw results retain that variability.
The paired fixture and compiler match the earlier run, but these separate runs
do not isolate the causal contribution of each implementation change. Larger
and concurrent workloads remain a release gate.

The first [19-job platform attempt](https://github.com/Augani/rgo/actions/runs/37031117420/attempts/1)
at `779397e` passed 18 jobs, including the Intel guardian cases and complete
macOS beta/nightly suites. macOS stable's coalition case passed, but its
interrupt fixture could not signal the launcher group immediately after
resume. The original assertion lacked errno, child status and stderr, so the
failure is not classified as a harness race or a runtime defect. Eight focused
local repeats passed. Failure diagnostics were added to the same fixture with
all assertions retained. The single failed platform job's retry passed, giving
all 19 jobs passing results. A separate
[focused macOS 14 run](https://github.com/Augani/rgo/actions/runs/37033579731)
at `c6df6b8` passed both existing cases with the new diagnostics; unrelated
platform jobs were intentionally skipped. Neither success establishes the cause
of the initial failure or a resolved interrupt gate.

### One-use macOS Cargo jobs avoid replacement races

The pilot now uses `LaunchOnlyOnce` and removes only its owned metadata; it no
longer unloads Cargo jobs by name. This avoids needing a loaded-definition query
that remains valid until a later unload. A separate private [launchd audit](probes/macos-launch-once.py)
observed registration removal after normal exit and SIGKILL while a detached,
closed-FD writer remained kernel-counted. The writer's late write succeeded;
only afterward did the coalition become reaped. The [raw result](probes/2026-10-02-macos-launch-once.json)
records both cases on macOS 27.2 arm64.

Inference from Apple's pinned [reaping implementation](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/coalition.c)
and [absent-ID syscall result](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/sys_coalition.c):
exact `-1`/`ESRCH` establishes retirement for a positively observed, same-boot
coalition ID. The private API/runtime gate still applies. Receipt schema 2
expresses that policy; schema 1 retains its previous strict protection. Owner
schema 4 requires the one-use definition, while admission revision 4/protocol 9
isolate the new policy. The existing fixture checks actual GC, a replacement job
surviving metadata cleanup, old-receipt protection, and journal replay. A terminal
startup timeout remains unexplained after one passing local retry; the broader
safety gates stay open in the [macOS checklist](macos-cargo-supervision.md#one-use-job-retirement--october-2-2026).

At clean `b4f9e46`, the optimized [registered-shell paired benchmark](benchmarks/2026-10-02-macos-arm64-cargo-guardian-once.json)
completed 24 no-op and 12 edit pairs with no accepted supervisor fallback and
cache/automatic GC off. Added median latency was 92.6 ms and 95.0 ms respectively;
managed p95 was 125.7 ms and 240.9 ms. This small-crate result meets the proposed
median allowance; it does not establish representative or concurrent performance
or isolate a causal per-change improvement.

The [19-job platform run](https://github.com/Augani/rgo/actions/runs/37037063311)
completed successfully at `b4f9e46`, with no retry. It passed the replacement
and interrupt cases on macOS 14.8.9 arm64 and macOS 15.7.9 Intel, the full
existing workspace/platform suites, source builds, installer/service probes,
Cargo boundaries, and budget recovery. This closes the Cargo-job replacement
race by eliminating label-based unloading; the earlier unexplained failures
and wider P2/P5 release gates remain open.

### macOS preparation cancellation

A deterministic pause in the existing private-home coalition fixture reproduced
two late-preparation bugs. With SIGINT already captured, the old caller still
committed managed Cargo. With SIGTERM captured and preparation completion forced
to fail, it launched checkout-storage fallback Cargo and exited successfully.
The ignored-SIGINT positive control committed and succeeded, proving that the
guardian's commit observation was enabled. The fixture aggregates the subcases
before failing so it also cleans up owned jobs and verifies actual idle GC.

The caller now checks captured termination before commit and before fallback,
and owns terminal/signal restoration in declaration order so preparation errors
restore terminal state before restoring original signal actions. Original
`SIG_IGN` actions are preserved rather than replaying an ignored notification
after Cargo may have installed another disposition. SIGCONT and SIGWINCH remain
observable for process continuation and PTY sizing. Cargo retains its captured
original mask/dispositions. Apple's pinned
[signal implementation](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sig.c#L1913)
explicitly resumes stopped processes for SIGCONT even when blocked or ignored;
the caller must reproduce that effect for Cargo's separate process group.

After the fix, the same SIGINT/SIGTERM subcases both exited by the requested
signal without a commit; ignored SIGINT still committed successfully. The
existing interrupt fixture also passed after starting the caller with SIGCONT
both ignored and blocked. Local macOS 27.2 arm64 passed the two existing cases
in 31.09 s and 3.41 s, plus all-bin build and zero-warning all-target Clippy.
The debug-only handshake hook is not compiled into release binaries. Platform
verification is pending; this evidence covers the observed late-preparation
point, not every startup cut or a signal arriving after the cancellation check.

The original macOS stable interrupt failure completed in 0.91 s according to
its archived job log, so its child's thirty-second expiry does not explain that
failure. It still lacks the diagnostics required to establish a cause. Neither
this cancellation fix nor later passing runs resolve that original failure or
the separate terminal startup timeout.

The [captured observations](probes/2026-10-02-macos-preparation-cancellation.json)
retain the three before/after subcase statuses. Optimized all-bin build,
formatting, and diff checks passed; audit markers are absent from release
`rgo`/`rgo-rustc-wrapper` and present in debug `rgo` as a positive control. The previous
`b4f9e46` performance sample does not measure this change.
