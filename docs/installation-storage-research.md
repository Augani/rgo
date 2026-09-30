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
| F9 | Plain Cargo attribution may name a workspace member as the root. | Wrapper `attribute()` falls back to `CARGO_MANIFEST_DIR/Cargo.toml`, which is the package manifest. | Removing a member can be mistaken for deleting the workspace; membership and workspace identity need distinct treatment. |
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
was subsequently pushed to `master`; public CI and release artifact verification
remain open.

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
