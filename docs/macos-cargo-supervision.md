# macOS Cargo supervisor — implementation checklist

Priority: macOS first, as requested on October 2, 2026. This is the next P2
implementation batch in [the accepted plan](installation-storage-plan.md).
Automatic GC stays off by default. The private launcher pilot uses kernel
tracking when `RGO_MACOS_SUPERVISOR_PILOT=1`; normal activation still uses the
descriptor mechanism and the cleanup safety gate remains open.

## Problem and selected mechanism

A real private-home build script launched a detached Python writer using normal
`subprocess.Popen` descriptor handling. Cargo exited, both inherited lifecycle
guards disappeared, and manual GC deleted the writer's Cargo-provided `OUT_DIR`.
The later write failed with `ENOENT`. The repeatable audit is in
[probes/README.md](probes/README.md). The same gap affects opted-in automatic GC.

Polling parent PIDs or process groups cannot establish complete descendant
coverage: children can detach, and short-lived parents disappear between scans.
XNU rejects kqueue's `NOTE_TRACK`, `NOTE_TRACKERR`, and `NOTE_CHILD` flags with
`ENOTSUP`; `NOTE_FORK` alone is not recursive tracking.
[Apple's implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_event.c)
therefore does not supply the required fork-to-exit supervisor through this API.

Apple documents resource-coalition membership as immutable after process
creation and inherited by fork/exec/posix_spawn. Launchd creates coalitions for
its jobs. Inference: an independently owned Cargo job can keep every descendant
accounted even after `setsid`, parent exit, and closing inherited descriptors.
[Apple's coalition design](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/coalitions.md)
also says coalition IDs are not reused within a boot. The receipt therefore
includes the kernel boot-session UUID so it cannot confuse identities from
different boots.

The observer dynamically resolves private interfaces instead of relying on
missing public SDK declarations. The first two resource counters are
`tasks_started` and `tasks_exited`; XNU reads them under the coalition lock and
copies only the caller's requested prefix of the structure.
[Counter definition](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/coalition.h),
[locked counter snapshot](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/coalition.c),
[copy semantics](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/sys_coalition.c).
These are private interfaces, so successful observation on one OS is not a
supported-version commitment. Missing APIs and query errors must use ordinary
storage before admission, or preserve an already admitted context.

## Implemented prototype

- [x] Reproduce deletion with a real Cargo build and a detached, closed-FD writer.
- [x] Add a dependency-light runtime observer in `rgo-core`; leave the per-rustc
  wrapper unchanged.
- [x] Bind registration to the held `SessionGuard`'s own storage root and scope.
- [x] Write an atomic, synced receipt outside the evictable context before
  starting managed work. Record boot identity and all retained coalition IDs.
- [x] Limit a receipt to 4 KiB and 32 coalitions. Reject corrupt, mismatched,
  unsupported, oversized, or symlinked records. Never replace query failure
  with an idle count; unresolved IDs remain protected.
- [x] Make GC check global and selected-context receipts after acquiring its
  exclusive lifecycle guards. Those guards fence new registration through
  deletion. Prune only validated previous-boot receipts or observed zero-task
  coalitions on the current boot.
- [x] Exercise an isolated launchd job in one private-home real-Cargo fixture:
  Cargo and its parent exit, the writer remains, real GC preserves its folder,
  a corrupt receipt refuses exclusion, the late write succeeds, and real GC
  reclaims the context after the writer exits.
- [x] Make doctor report the ordinary Unix launcher's known descriptor gap.
- [ ] Complete the supported macOS version/architecture matrix. Local evidence
  is macOS 27.2 build 26B5091g, arm64. The prototype also passed the macOS
  14.8.9 build 23J631 arm64 beta workspace suite at `325b2e5` in
  [platform CI](https://github.com/Augani/rgo/actions/runs/36992158237).
  Intel and the full supported runtime range remain unverified.

The complete [18-job run](https://github.com/Augani/rgo/actions/runs/36992158237)
passed at `325b2e5`, including macOS stable/beta/nightly suites and the existing
100-project checks. This confirms the prototype's current integration does not
break those workflows; it does not validate a shipping Cargo supervisor.

The normal launcher still uses the earlier descriptor mechanism. Registering
its existing process directly would bind a context to Terminal, an IDE, or
another launching application's coalition and retain it until that application
exits. The installed launcher's private pilot now creates its own launchd job;
the fixture no longer supplies a separate test-only helper.

## Launcher pilot — October 2, 2026

- [x] Create a private, uniquely named launchd job with no restart/demand
  triggers, and a one-use local rendezvous. Check the exact owned definition
  before cleanup and preserve edited definitions.
- [x] Authenticate the local peer with its Unix user identity and a random
  invocation token. Bound request frames and owner-record reads.
- [x] Transfer the original three standard descriptors with `SCM_RIGHTS` and
  encode argument/environment values as native bytes. The extended existing
  fixture verifies stdin/EOF, stdout/stderr, non-UTF-8 bytes, the Cargo `--`
  separator, and an application's exit code 17.
- [x] Hold the original session through preparation; the job acquires its own
  context guard and syncs the kernel receipt before the caller commits Cargo.
  Preparation errors fall back before any managed Cargo write. Failure after
  the commit does not retry Cargo.
- [x] Return the primary Cargo result while the guardian retains exclusion for
  surviving descendants. Release only the same-process guard so unrelated
  contexts can still be reclaimed.
- [x] Retire the kernel receipt under the held context guard when the guardian
  is its coalition's only remaining task and will perform no further managed
  work. Remove the rendezvous before bootout can reap the coalition. The
  healthy fixture verifies job-directory cleanup and released exclusion.
- [x] Extend the same fixture to kill the actual guardian after Cargo exits.
  Its closed-FD writer remains kernel-counted and real GC preserves the folder;
  the late write succeeds and idle GC reclaims it afterward.
- [x] Move to admission revision 3 and protocol 8. A separate macOS coalition
  discriminator prevents this pilot from reusing descriptor-only contexts.
  The existing setup recovery fixture now exercises recorded protocol 7;
  the unchanged Shutdown compatibility range still includes protocol 6.
- [x] Relay SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGTSTP, and SIGCONT. Observe
  primary stop events and reflect them in the caller's shell job. The existing
  interrupt fixture now uses the actual macOS pilot, requires a stop/resume
  cycle before Ctrl-C, preserves the surviving child, and reclaims an idle
  neighbor. This is process-group evidence with redirected descriptors;
  controlling-terminal compatibility is still open.
- [x] Persist the selected context in a versioned job-owner record before
  bootstrap. Daemon maintenance and actual GC recover recognized idle jobs
  under the context's exclusive GC guards. Receipt pruning is synced before
  bootout can reap the identity. Neither a missing PID nor a query failure is
  accepted as completion.
- [x] Bound owner/plist reads, reject symlinks and unsupported schemas, compare
  the exact generated definition, and preserve edited definitions or added
  content. Cleanup unlinks only the four known job entries. Maintenance keeps
  its directory iterator across passes, reads at most 16 entries per pass,
  and bounds its bootout helper. The same coalition fixture verifies an edited
  idle job survives, then maintenance recovers it after restoration with
  automatic destructive GC disabled.
- [x] Keep terminal invocations in checkout storage before pilot admission.
  Passing terminal descriptors to a different launchd session does not make
  that session own the shell's controlling terminal. The next transport batch
  must establish and verify this handoff before enabling terminal admission.
- [ ] Complete controlling-terminal evidence, every abandoned-job interruption,
  every startup/cancellation boundary, and the declared runtime/architecture
  matrix before making this the normal installed launcher path.

The [18-job platform run](https://github.com/Augani/rgo/actions/runs/36997090024)
passed at `0caa7ba`, including the actual installed-launcher coalition fixture,
macOS stable/beta/nightly workspace suites, source builds, installer probes,
and the existing 100-project recovery checks. That run validates the stream
handoff pilot before this recovery/stop-resume follow-up.

The two existing private regressions passed locally on macOS 27.2 arm64 with
the recovery and stop/resume implementation; build, format, and diff checks
also passed. This is still a pilot. Updated platform evidence is required for
this batch; a focused `macos-15-intel` lane runs those same two cases because
[GitHub identifies that runner as Intel](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
No additional Rust test case was added in this follow-up.

Power loss or cancellation between individual metadata removals and bootout,
loaded-job replacement, every preparation/commit boundary, and the complete
signal-disposition and terminal matrix still require work. Older pilot owner
records are preserved when their schema cannot establish the recovery scope.

## Integrate with unchanged Cargo commands

- [ ] Add a private per-invocation launchd job with a unique owned label, no
  `KeepAlive`, sockets, Mach services, or demand triggers. Verify ownership
  before inspecting, starting, stopping, or removing a job. Preserve an edited
  or unrelated definition.
- [ ] Have the owned PATH launcher start that job without replacing rustup's
  Cargo proxy. Keep the caller's exact `+toolchain`, flags, working directory,
  environment, and selected compiler/wrapper behavior.
- [ ] Transfer stdin/stdout/stderr over an authenticated local Unix socket
  using descriptor passing. Preserve terminals, pipes, redirected files,
  interactive input, EOF, and Cargo's output/exit status. Private request
  records must preserve non-UTF-8 arguments and environments.
- [ ] Establish the startup handshake while the caller holds its existing
  lifecycle guard. The job must obtain its bound guard and sync its coalition
  receipt before Cargo can write managed intermediates. Startup failure must
  fall back to checkout storage before any managed write.
- [ ] Return Cargo's exit status while retaining descendant protection until
  kernel membership drains. Define Ctrl-C, termination, shell closure, nested
  Cargo, and terminal job-control behavior without changing everyday commands.
  Verify process-group and session behavior explicitly; descriptor passing
  alone does not establish compatibility with terminal job control.
- [ ] Fence job restart and namespace reuse. A zero-count snapshot alone is
  insufficient if a loaded job can admit future unguarded work.
- [ ] Recover owned jobs and receipts after launcher, guardian, or daemon
  crashes. A missing/reaped coalition is an error, not evidence of zero tasks.
  Define durable completion before removing a job that can reap its coalition.
- [ ] Introduce a fresh admission namespace and IPC policy revision. Older
  contexts remain protected, and older daemons must not delete new contexts
  without checking coalition receipts. Retain explicit old-daemon shutdown
  during installation upgrades.

## Shipping checks

- [ ] Extend the same detached-writer fixture to the actual installed launcher,
  then require the existing crash, interrupt, waiting-Cargo, nested-Cargo, test,
  run, and deletion-pause fixtures to pass with positive idle reclamation.
- [ ] Verify SDK/runtime capability detection on the declared supported macOS
  versions and architectures; unsupported runtimes stay outside managed GC.
- [ ] Measure per-command launch overhead, no-op builds, receipt contention,
  bounded job/receipt cleanup, and concurrent projects. Re-run the existing
  100-project recovery check after activation; do not add a second copy.
- [ ] Check real rust-analyzer and GUI IDE startup, clean install, repair,
  upgrade, undo, uninstall, and recovery with a live Cargo descendant.
- [ ] Keep arbitrary external brokers and intentional direct entry into rgo's
  writable namespace outside any guarantee until their contract is resolved.
- [ ] Update the P2 safety argument and product claims before considering
  automatic cleanup defaults. Implement Linux supervision afterward.
