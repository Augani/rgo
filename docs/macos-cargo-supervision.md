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
  The full supported runtime range remains unverified; Intel pilot evidence
  is recorded below.

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
- [x] Before the terminal transport batch, keep terminal invocations in checkout storage before pilot admission.
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

The [19-job run](https://github.com/Augani/rgo/actions/runs/36998573830) passed
at `5557f94`, including the full existing platform suites and the focused Intel
lane. Its recorded runtime is macOS 15.7.9 build 24G830, x86_64, Cargo/rustc
1.99.0. Both guardian cases passed there; local evidence uses Cargo/rustc 1.98.0
on macOS 27.2 arm64. This broadens the pilot's observation matrix without
closing terminal, cancellation, or the full supported-runtime gate.

Power loss or cancellation between individual metadata removals and bootout,
loaded-job replacement, every preparation/commit boundary, and the complete
signal-disposition and terminal matrix still require work. Older pilot owner
records are preserved when their schema cannot establish the recovery scope.

## Terminal handoff and measured launch overhead

The existing 24 no-op/12 edit-build paired probe, with cache and automatic GC
off, measured the actual nonterminal guardian at `5557f94`. Every captured
result was checked for guardian fallback before acceptance. The
[raw samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian.json) contain
binary hashes, toolchain versions, and the source commit.

| Median | Plain Cargo | Guardian pilot | Added time |
|---|---:|---:|---:|
| Cached no-op | 23.0 ms | 264.1 ms | 241.1 ms |
| Edit build | 135.2 ms | 289.4 ms | 154.2 ms |

Both exceed the proposed 100 ms absolute allowance for this small crate. The
pilot must remain opt-in until this is reduced. Apple's
[launchd contract](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5)
says unspecified/Standard jobs receive CPU and I/O limits and describes
Interactive as an application-like classification. Scheduling and timer
coalescing are hypotheses to measure, not an established explanation of these
samples.

A [private session observation](probes/macos-terminal-session-observation.json)
also found that a new launchd job was a process-group leader in session 1 and
could not directly adopt a PTY. Moving only that job to its parent's existing
group, then creating its own session, allowed PTY adoption. Apple's
[setpgid](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/setpgid.2.html)
and [setsid](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/setsid.2.html)
contracts explain these preconditions. This primitive observation does not
implement or verify Cargo terminal behavior.

- [ ] Measure bootstrap, startup, Cargo runtime, and exit-notification costs;
  distinguish runtime from notification latency and extend beyond one crate.
- [x] Use an explicit Interactive job classification and readiness-driven
  SIGCHLD/control-socket waiting instead of periodic primary-exit sleeps.
  Preserve recognition of the exact historical owned job definition.
- [x] Preflight the caller's controlling terminal and descriptor identities
  before admission. Pass pipes/files directly; unsupported terminal shapes
  stay in checkout storage.
- [x] Give the guardian its own session and PTY before starting Cargo. Validate
  the parent/session preconditions and fall back on errors. Keep Cargo in
  that session so its group has a living parent and ordinary job-control
  signals still work.
- [x] Pass the PTY master and original terminal descriptor over the existing
  authenticated channel. Initialize the PTY with original terminal settings
  and window size; use bounded buffers and descriptor readiness.
- [x] Establish Cargo's foreground group before exec can read the terminal.
  Preserve redirected streams, native bytes, interactive input, `/dev/tty`,
  EOF, output draining, resize events, and primary exit/signal results.
- [ ] Restore the caller's terminal before stopping or exiting; reapply relay
  mode only in the foreground. Handle `fg`, `bg`, TOSTOP, and nested terminal
  consumers. The guardian must restore its own known relay settings after
  caller failure without overwriting later shell edits.
- [x] Retain the master after the primary result while descendants survive;
  hand off output draining so caller exit does not prematurely hang up their
  PTY. Keep the existing kernel receipt through the final possible writer.
- [x] Extend the existing fixture with a real interactive shell/PTY rather than
  add another equivalent Rust case. Verify stop/resume, resize, EOF, Ctrl-C,
  terminal restoration, and late descendant output.
- [x] Repeat the same paired latency probe after the combined transport change,
  then run the existing platform and Intel checks once for that batch.
- [ ] Enable the normal installed macOS path only after terminal compatibility,
  startup/recovery boundaries, supported runtimes, and latency meet their gates.

At `0bb4ee3`, the private pilot admits supported terminal invocations through a
PTY owned by the guardian's separate session. Cargo receives its foreground
group before exec; only terminal standard descriptors are replaced, while
files and pipes pass through. The authenticated channel carries the original
terminal and PTY master descriptors, bounded configuration frames, and signal
or foreground updates. The caller uses bounded readiness-driven relay buffers.
The guardian retains its master after the primary exits, so a detached child
can write after the shell has regained its prompt. Original ignored signal
dispositions and blocked signals are restored in Cargo's child before exec.

The same existing real-Cargo fixture now drives an actual private `zsh -f`
session. Local macOS 27.2 arm64 checks passed `/dev/tty`, isatty and color,
input/EOF, resize, Ctrl-Z/fg/bg, background-input suspension, TOSTOP,
Ctrl-C and status 130, application status 17, mixed pipes/redirection, normal
terminal restoration, late detached output, and cleanup after caller SIGKILL.
The existing coalition crash and interrupted-Cargo cases also passed. No new
Rust test case was added; all-bin build and warning-free all-target Clippy passed.
The combined batch's platform results are recorded below.

**Unregistered terminal crash defect:** the original observation reproduces incomplete
terminal restoration after the caller is killed with SIGKILL. Zsh partly
changes the original termios state before the guardian's exact-mode restoration
check. The guardian preserves that changed state rather than overwriting it;
some relay input/output and local-mode flags remain. Job retirement still
completes. The later explicitly registered zsh prototype described below repairs
that observed race locally and now asserts restoration. Unregistered shells
retain the original defect. Broader shell configurations, nested terminal
consumers, and supported-runtime evidence remain separate compatibility work.

New version-2 job definitions explicitly request Interactive scheduling; exact
version-1 definitions remain recognizable. SIGCHLD and control-socket readiness
replace the guardian's periodic primary-state sleep, and listener readiness
replaces the caller's startup sleep. The
[first combined samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian-interactive.json)
at `0bb4ee3` measured 25.4 ms plain versus 151.9 ms guardian for no-op builds,
and 131.4 ms versus 259.3 ms for edits. Added medians of 126.5/127.9 ms remain
above the proposed 100 ms allowance. These are nonterminal, cache-disabled
single-crate results; they do not measure PTY or larger-workspace latency.

Rust's [Unix process implementation](https://doc.rust-lang.org/src/std/sys/process/unix/unix.rs.html)
rejects the posix_spawn path when a pre-exec callback is installed and otherwise
inherits the parent's signal mask. The installed Rust 1.98 source confirms
those conditions. The follow-up preserves that path for nonterminal Cargo
when the guardian's mask and ignored dispositions already match the caller;
terminal foreground setup and differing signal states still use the callback.
The [follow-up samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian-spawn.json)
at `dd09d11` measured 22.8 ms plain versus 136.5 ms guardian for no-ops and
128.1 ms versus 248.7 ms for edits. Added medians of 113.7/120.6 ms still exceed
the proposed allowance. Both probes used debug binaries; measure the optimized
installer pair before deciding the shipped-binary performance gate. Focused
checks and warning-free all-target Clippy passed for this final source.

The shell driver now waits for uniquely marked command completion rather than
an asynchronously redrawn prompt, and waits for actual background suspension.
The existing recovery check awaits launchd unload after metadata removal; those
are distinct completion points. These corrections avoid requiring an arbitrary
fixed pause or treating the first disappeared directory as completed bootout.
The [19-job matrix](https://github.com/Augani/rgo/actions/runs/37005085858)
passed at `50a8405`, including the focused Intel macOS lane, full macOS
stable/beta/nightly suites, and Rust 1.85 source builds on all three platforms.
This validates this batch; the declared runtime range, broader interruption
matrix, terminal crash defect, and activation gates remain open.

The [optimized-pair samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian-release.json)
at `50a8405` used the complete `cargo build --release` output, the same private
setup and paired 24 no-op/12 edit-build procedure, cache off, automatic GC off,
and no accepted guardian fallback. Median no-op time was 24.9 ms plain versus
134.3 ms guardian; edit time was 126.5 ms versus 236.2 ms. Added 109.4/109.7 ms
remains above the proposed allowance. Debug and optimized observations are
kept separately with their executable hashes; this single-crate nonterminal
result does not close broader performance validation.

The crash observation agrees with zsh 5.9's
[job handling](https://github.com/zsh-users/zsh/blob/zsh-5.9/Src/jobs.c): an
unfrozen shell can snapshot the terminal when its foreground job finishes,
before reattaching its own group. Its
[line editor](https://github.com/zsh-users/zsh/blob/zsh-5.9/Src/Zle/zle_main.c)
repairs selected local flags, rather than every relay change. Inference: a late
physical-terminal restore cannot establish that the shell's saved snapshot is
correct. A faster exit observer alone is not a race proof. Do not relax the
exact-mode restore into an unconditional overwrite or freeze a user's shell
globally as a workaround.

Moving a same-session child into the guardian's coalition is also not a general
unprivileged substitute: XNU's
[spawn path](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_exec.c)
requires a privileged source coalition or the coalition-spawn entitlement for
an explicitly selected coalition. Ordinary parent-session spawning must not
assume that privilege. The PTY design therefore remains an opt-in prototype
pending a reviewed terminal-mode ownership solution.

### Bounded helpers and Cargo query overlap

- [x] Bound `launchctl bootstrap` as well as guardian connection waiting.
  Keep the owned record through registration and attempt safe cleanup when
  failure occurs before any invocation is supplied. After request transmission
  starts, the guardian owns receipt retirement.
- [x] Wait for bootstrap/bootout helpers with a kernel exit notification rather
  than a fixed polling pause. Kill and reap a timed-out direct helper; a timeout
  does not authorize retiring any live coalition receipt.
- [x] Add opt-in debug timing for selection, activation, owned-file preparation,
  bootstrap, connection, admission, primary start, and primary result. Keep
  diagnostics on stderr, including when Cargo emits JSON on stdout.
- [x] Overlap fresh Cargo version and workspace queries. Preserve the selected
  proxy, `+toolchain`, manifest arguments, and unmanaged fallback when either
  query fails. Do not infer the active toolchain from rustup proxy bytes.
- [ ] Close the complete performance gate with representative workloads;
  neither a passing no-op median nor the short diagnostic probe is sufficient.

At `c788254`, bootstrap and listener waiting share the original 20-second
startup deadline; socket operations use the remaining interval. The bounded
helper uses Apple's [kqueue exit notification contract](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kevent.2.html),
but obtains the actual result with `waitpid` through `Child::try_wait`. Bootout
retains its existing two-second deadline. Subsequent handshake operations and
filesystem stalls still need the broader interruption audit; this change does
not establish a strict wall-clock limit for every startup phase.

The [24 no-op/12 edit-pair run](benchmarks/2026-10-02-macos-arm64-cargo-guardian-parallel.json)
used optimized binaries from that clean source, with cache and automatic GC
off and no accepted fallback. Median no-op time was 22.3 ms plain versus
120.9 ms guardian, adding 98.6 ms. Edits were 124.0 ms versus 235.7 ms, adding
111.7 ms. The no-op result meets the proposed absolute allowance in this run;
the edit result does not. Do not enable normal activation on these results.
New reports label nearest-rank percentiles explicitly; older reports retain
the historical percentile calculation and raw samples.

The separate [six-no-op/two-edit diagnostic probe](benchmarks/2026-10-02-macos-arm64-cargo-guardian-stages.json)
records the debug stages and exact executable hashes. Median no-op stages were
22.2 ms for the overlapping queries, 21.1 ms for owned-file preparation,
5.1 ms for bootstrap, 19.0 ms for connection, and 23.4 ms for admission.
These are stage observations from another run with diagnostic output; summing
their medians is not an end-to-end sample, and its short sample count does not
replace the paired performance gate. Durable metadata and admission must not
be weakened to meet a timing target.

The helper timeout/reaping check, existing coalition/terminal and interrupt
cases, unsupported-Cargo passthrough case, all-bin debug/release builds, and
warning-free all-target Clippy passed locally. The
[combined 19-job platform run](https://github.com/Augani/rgo/actions/runs/37007749125)
passed, including Intel macOS and the full existing suites, without another
full local run.

### Shell cooperation feasibility

A [private zsh primitive probe](probes/2026-10-02-macos-shell-owned-terminal-recovery.json)
compared a synthetic raw foreground child killed with SIGKILL, first without
cooperation and then with a one-shot shell-owned `precmd` finalizer. A private
foreground function saved `stty -g` before starting that child and armed the
finalizer; the finalizer ran `stty` with those saved settings and disarmed itself.
The baseline retained changed flags. The cooperating shell recovered its exact
saved mode, including on the subsequent `stty -g` command, and preserved a later
intentional `-ixon` edit through another prompt. An initial `-echo` comparison
was unsuitable because zsh normally repairs echo itself.

This demonstrates one recovery primitive on local zsh 5.9; it is not rgo
integration, a lease-ownership proof, installer activation, or other-shell
coverage. The private foreground function is only the experiment harness.
The installed Cargo PATH entrypoint remains unchanged.

- [x] Check whether shell-owned finalization can repair the reproduced saved-mode
  problem, without freezing the shell or weakening the guardian's exact check.
- [x] Design a bounded terminal lease identifying the terminal, boot/session,
  original caller and shell, saved mode, and command generation. Preserve it
  until acknowledged recovery; a vanished Cargo job directory is insufficient.
- [x] Implement recovery eligibility before changing any mode. Reject stale/reused
  identities, unknown shell state, a different foreground owner, and later edits.
- [x] Prototype explicitly registered zsh cooperation with reversible hook
  ownership, preserving existing hooks/functions. Keep Cargo arguments and PATH
  interception intact; unsupported terminal shapes remain outside managed GC.
- [x] Extend the existing real-Cargo shell driver for caller SIGKILL and subsequent
  prompt editing, intentional edits, background jobs, terminal disconnect, hook
  removal, and stale receipts. Reuse the driver rather than add an equivalent case.
- [ ] Measure the cooperating terminal path and validate the supported shell/
  runtime range before considering normal activation.

The registered pilot uses a hidden `macos-terminal-host init-zsh` command; it
does not change installer profiles or normal activation. For a private zsh 5.9
evaluation, with the existing Cargo pilot configured, initialize that shell with:

```zsh
eval "$(/absolute/path/to/rgo macos-terminal-host init-zsh)"
```

Cargo commands still resolve through the installed PATH launcher. Registration
adds owned hook functions to zsh's existing hook arrays and preserves an existing
`precmd` function. Duplicate initialization recognizes its exact function bodies;
`__rgo_terminal_undo` removes only those unchanged functions and owned records.
Unknown conflicting functions are refused. This is a private prototype entrypoint,
not the shipped one-command installer.

Each host has a random token and private directory outside evictable Cargo job
data. Bounded no-follow records bind the kernel boot ID, PID/start-time/session
identities, terminal device identity, command generation, and original termios.
The caller writes its lease before entering raw relay mode. A normal exact-mode
restoration disarms it; job retirement cannot discard an unacknowledged lease.
A different caller cannot overwrite that pending lease, even after its original
process disappears. The owning shell must acknowledge it first.
Recovery requires the owning shell and foreground terminal, a confirmed vanished
original caller, matching command generation, and the exact relay mode or zsh
5.9's documented local-flag normalization. Only the kernel's `PENDIN` queue-state
bit is excluded from mode comparison. Reused PIDs and later generations retire
the identified stale lease without applying its saved mode. Uncertain identities
or unknown data disable recovery and preserve the record.

The command counter uses zsh's [system and files builtins](https://zsh.sourceforge.io/Doc/Release/Zsh-Modules.html),
with exclusive no-follow temporary creation and an atomic same-directory rename.
An external helper in every `preexec` hook reproduced a background job-control
failure and was removed. The counter is cleared at each prompt, so a missing
`preexec` hook cannot reuse that completed generation. A missing or changed owned
finalizer marks recovery disabled, making later terminal Cargo use ordinary
checkout storage instead of silently reverting to the unregistered pilot.
Host locks are nonblocking: a stopped
caller cannot freeze its shell waiting for a lock. Atomic lease/counter visibility
survives process crashes; drive flushes are unnecessary because reboot invalidates
their boot/process identities. The initial host record is synced.

The existing real-Cargo shell driver passes normal terminal behavior with these
hooks present, exact restoration after caller SIGKILL and on the subsequent
command, intentional later mode changes, replayed old generations, a mismatched
start time for a reused PID, preserved hooks, owned undo, and disconnecting an
active physical terminal locally on macOS 27.2 arm64. Removing the owned finalizer
also passed a real unchanged Cargo invocation in ordinary checkout storage with
no guardian admission. Disconnect retired its
guardian; zsh's [SIGHUP handling](https://github.com/zsh-users/zsh/blob/zsh-5.9/Src/signals.c)
uses its [signal-exit path](https://github.com/zsh-users/zsh/blob/zsh-5.9/Src/builtin.c),
which returns exit code 1 rather than dying from an uncaught signal. The fixture
asserts that behavior and leaves no live probe jobs after failure.
It adds no Rust test case. The supported runtime/shell range, other hook ordering,
abandoned-host metadata recovery, nested Cargo, and representative terminal-path
performance remain explicit follow-up gates. All-bin build, the two existing
focused macOS cases, and warning-free all-target Clippy passed locally.

The existing latency probe now accepts `--macos-guardian --zsh-terminal` to
compare separate private interactive zsh sessions. Only the managed session
registers recovery. Alternating paired commands use the same disposable crate
and Cargo home; their timer includes command dispatch, hooks, Cargo, and the
completed prompt. Any guardian fallback invalidates the measurement. A short
debug run verified this measurement path and bounded PTY teardown. The
[optimized 24-no-op/12-edit samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian-zsh.json)
come from clean `ba023fa` with exact binary hashes, cache and automatic GC off,
and no accepted guardian fallback. No-op medians were 28.8 ms plain versus
138.9 ms registered guardian, adding 110.1 ms; edit medians were 130.9 ms versus
250.5 ms, adding 119.6 ms. Both exceed the proposed 100 ms allowance. Timers end
at the completed prompt, so these samples include shell recovery overhead and
are kept separate from the previous nonterminal subprocess measurements.
Normal activation remains off; this tiny-crate result does not replace the
representative-workload gate.

The [19-job platform run](https://github.com/Augani/rgo/actions/runs/37014509991)
passed at `ba023fa`, including the actual registered-shell fixture in the
Intel guardian lane and full macOS stable/beta/nightly suites, all three Rust
1.85 source builds, Linux/Windows suites, installer checks, and the existing
100-project recovery probes. This closes this batch's platform validation;
it does not close the broader supported-shell, lifecycle, IDE, or performance
release gates.

## Cargo job retirement and scope binding

The earlier cleanup removed `owner.json` and the job directory before calling
`bootout`. A process interruption between those operations left no recovery
record. Cleanup now syncs a sibling `.retiring-macos-cargo-job-*.json` journal
before its first unlink and keeps it through the unload acknowledgement.

- [x] Bind new owner schema 3's context to its generated `--context` program
  argument. The guardian requires that exact argument and invocation scope.
  An edit to only the owner context invalidates its definition instead of
  redirecting maintenance to another apparently idle scope. Preserve schemas
  1/2, which cannot establish this binding.
- [x] Recheck owner bytes and the absence of a retirement journal after acquiring
  the guardian's context guard and before publishing its kernel receipt.
- [x] Sync the job directory's parent before bootstrap. Preserve partial or
  unknown later preparation content instead of invoking a recursive temporary
  directory destructor on failure.
- [x] Overlap independent owner/plist flushes, then independent job/parent
  directory flushes; require every flush to succeed before bootstrap.
- [x] Wake the caller's relay with a process-lifetime, close-on-exec pipe for
  guardian replies and signals. Avoid the recurring foreground polling delay.
  Retain a 100 ms background-only check for `fg` of a silent running job;
  zsh 5.9 only sends `SIGCONT` when the job was stopped. Extend the existing
  terminal fixture to verify this handoff before application input/output or
  exit. Read original-terminal input only after configuring relay mode, and
  recheck foreground ownership after poll returns from a stop/resume.
- [x] Retain the exact owner bytes, directory identity, and known file/socket
  fingerprints in a bounded, private, no-follow journal outside the job folder.
  Publish new job and terminal retirement journals without replacing an
  existing destination. Preserve extra files or changed bytes/inodes.
- [x] Fence the rendezvous first and reject pending retirement during startup.
  Resume after the owner header or entire job folder is gone. Existing exclusive
  GC guards still establish build-lifetime safety; the journal does not replace
  receipt or kernel observations.
- [x] Preserve the journal on unload timeout or failure. Accept successful
  `bootout` or the observed `ESRCH` already-unloaded result, rather than treating
  every nonzero status as completion. Other results preserve recovery state.
- [x] Extend the existing fixture for an edited context while its closed-FD
  writer remains alive, an actual old schema/definition, interrupted removal,
  a later edited definition, real daemon restart, and journal replay after a
  completed unload. No new Rust test case was added.
- [ ] Complete every preparation/commit interruption and incomplete-record
  recovery path, including a full filesystem failure/cancellation matrix.
- [x] Eliminate Cargo-job unloading by label and verify that a foreign loaded
  replacement survives original-job metadata cleanup on the platform matrix.
- [x] Record optimized terminal-path timing for this batch.
- [x] Record the full platform matrix and focused macOS 14 diagnostic run,
  including the initial failed attempt and its single retry.
- [ ] Resolve the observed macOS stable interrupt failure before considering
  normal activation.

The journal holds at most four known entries, regular reads remain bounded to
16 KiB, and the journal is limited to 64 KiB. Removal steps are bounded; the
same two-second helper deadline and exclusive scope guards remain in force.
The two existing local cases passed schema 3, journal replay, the reply/signal
wakeup, the silent running background handoff, and terminal recovery on macOS
27.2 arm64 (16.25 s and 2.83 s). All-bin build, zero-warning all-target Clippy,
format and diff checks passed.

The first [platform attempt](https://github.com/Augani/rgo/actions/runs/37031117420/attempts/1)
at `779397e` passed 18 jobs, including Intel and the full macOS beta/nightly
suites. macOS stable passed its coalition/terminal fixture, but the existing
interrupt case failed when sending Ctrl-C immediately after resume: its
launcher group was no longer reachable. The initial assertion recorded no
errno, exit status, or stderr, so the cause is unresolved. A focused local run
and eight bounded repeats passed; the fixture now includes those diagnostics
without weakening its assertions. The single failed-job retry passed. CI
also supports a manual, focused run of these same two cases on `macos-14` or
`macos-15-intel`; normal push/PR runs keep the full matrix. This lets subsequent
signal diagnostics target the affected runtime without repeating unrelated
platform suites. This failure remains evidence and an open activation gate.

All 19 jobs have passing results in the
[completed platform run](https://github.com/Augani/rgo/actions/runs/37031117420),
including the two advisory nightly lanes, all source-minimum builds, and the
existing installer and 100-project checks. The native source is `779397e`.
The [focused diagnostic run](https://github.com/Augani/rgo/actions/runs/37033579731)
at `c6df6b8` passed both existing cases on macOS 14.8.9 build 23J631 arm64,
Cargo 1.99.0. That follow-up changes test diagnostics, CI selection and evidence,
and leaves the native implementation unchanged. Its five unrelated job
definitions are intentionally skipped, rather than counted as platform passes.

The optimized [24 no-op/12 edit-build paired samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian-job-recovery.json)
used clean source `779397e`, Cargo/rustc 1.98.0, registered stock zsh 5.9, and
the actual installed guardian with cache and automatic GC off. No guardian
fallback was accepted. Samples include command dispatch through the completed
prompt. No-op medians were 24.5 ms plain versus 118.6 ms supervised, adding
94.1 ms; edit medians were 123.6 ms versus 216.6 ms, adding 93.0 ms. This run
meets the proposed 100 ms median allowance for this small crate, compared with
the earlier registered-shell run's 110.1/119.6 ms added medians. These are
separate paired runs, not a claim that each individual change caused a specific
improvement. No-op supervised p95 was 185.4 ms versus the earlier 154.7 ms,
so tail variability remains visible. Representative workloads, concurrency,
wrapper percentiles, and the wider performance release gate remain open.

## Abandoned terminal-host retirement

The registered terminal's guardian can retire before its shell record is
acknowledged. Daemon maintenance now handles that owned metadata separately
from build-data GC, including when automatic destructive GC is disabled. This
path never opens the terminal and never applies saved terminal modes.

- [x] Share the existing bounded host/lease schemas and native process identity
  observer between the caller and daemon, without changing the serialized
  format or adding dependencies to the rustc wrapper.
- [x] Preserve current-boot live shells or lease callers, uncertain process
  queries, unsupported records, nonlocal volumes, unknown entries, and changed
  bytes or file identities during journaled retirement. Explicit undo instead requires the same current
  shell, boot, and terminal and still refuses a live caller.
- [x] Use nonblocking owned locks and check their boolean acquisition result.
  The earlier `fs4` call ignored `Ok(false)` under contention; both host and
  retirement locks now refuse to proceed when busy.
- [x] Sync a sibling retirement journal before the first unlink. Bind its
  authorization to the exact original bytes, directory identity, and every
  known file's identity, length, and digest. Revalidate all remaining files
  before each bounded step; preserve edits or additions.
- [x] Resume after the original host header has been removed, then sync the
  parent directory before removing the journal. Runtime helpers never recreate
  a missing lock, and shell hooks disable recovery while retirement is pending.
- [x] Reuse the existing fixture for a real surviving process in the exited
  shell's session, actual lock contention, an added file, an edited remaining
  counter, and restart of the real daemon after the first journaled unlink.
- [ ] Complete interruption coverage while publishing registration/counters,
  including temporary or incomplete records lacking enough ownership evidence.
- [x] Validate this batch in platform CI before counting its platform evidence.

Maintenance retains its directory iterator, considers at most 16 entries per
pass, and removes at most two metadata files per entry. A 50 ms elapsed budget
is checked between entries; it is not a hard bound on filesystem syscall time.
Unknown temporary or incomplete registration directories remain preserved.
This addresses abandoned complete host records, not every startup interruption,
the full terminal runtime matrix, or the performance activation gate.

Local macOS 27.2 arm64 validation passed the all-bin build, the two existing
focused macOS cases, all-target Clippy with warnings denied, format, and diff
checks. The extended coalition case completed in 17.86 seconds; no equivalent
Rust test case or redundant full local suite was added. The
[19-job platform run](https://github.com/Augani/rgo/actions/runs/37020240840)
passed at `95d7358`, including the extended case in the Intel guardian lane and
full macOS stable/beta/nightly suites, all three Rust 1.85 source builds,
Linux/Windows workspace and installer checks, and the existing 100-project
recovery probes. This completes this batch's platform evidence without closing
the remaining startup, runtime, IDE, or performance release gates.

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

## One-use job retirement — October 2, 2026

Loaded definitions can change after an ownership query. The private Cargo
pilot now avoids that query/unload race entirely: owner schema 4 requires
`LaunchOnlyOnce`, and rgo never sends Cargo-job `bootout` by label. Launchd
retires the actual job incarnation after normal exit or SIGKILL; rgo's journal
only retires its four known metadata entries. Historical schemas 1–3 and their
journals remain preserved. The earlier schema-3 unload evidence above is
implementation history, not the current mechanism.

[Apple's launchd implementation](https://github.com/apple-oss-distributions/launchd/blob/d448a1c8f70a61202f8705f94337f686b87c30c4/src/core.c)
removes inactive one-use jobs after their first start. This older source is a
design clue; modern behavior also needs live evidence. The separate
[manual audit](probes/macos-launch-once.py) and its [raw result](probes/2026-10-02-macos-launch-once.json)
on macOS 27.2 arm64 observed both normal exit and SIGKILL: the registration
vanished, a detached closed-FD writer remained counted, its late write
succeeded, and only afterward did the resource-coalition query return exact
`-1`/`ESRCH`. It never activates a Cargo home or adds an automatic Rust case.

XNU's [reaping implementation](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/coalition.c)
requires termination and zero active references before removing the coalition
lookup entry; its [syscall](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/sys_coalition.c)
returns `ESRCH` when that lookup is absent. The same checks are present in
[10063.141.1 reaping](https://github.com/apple-oss-distributions/xnu/blob/xnu-10063.141.1/osfmk/kern/coalition.c),
[10063.141.1 lookup](https://github.com/apple-oss-distributions/xnu/blob/xnu-10063.141.1/bsd/kern/sys_coalition.c),
[11215.81.4 reaping](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/kern/coalition.c),
and [11215.81.4 lookup](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/bsd/kern/sys_coalition.c).
Inference: exact absence is retirement evidence only for a previously observed
ID on the same boot. Other observation failures remain unresolved; private API
or runtime changes still require the declared platform gate.

Receipt schema 2 records IDs only after a positive live counter observation.
It recognizes explicit reaping under GC's exclusion guards; schema 1 continues
requiring a successful zero count. Admission revision 4, a new macOS namespace
discriminator, and protocol 9 prevent older policies from accepting these new
contexts. Setup's existing previous-protocol recovery case selects protocol 8;
Shutdown compatibility still starts at protocol 6.

The existing coalition fixture now replaces the killed guardian's registration
with an independent job of the same name. The writer remains protected and
completes its late write; after kernel reaping, an old receipt still refuses GC,
while the new receipt allows actual reclamation. Edited metadata is retained;
restarted maintenance and replay after directory removal leave the replacement
process alive. No Rust test case was added.

The first local run passed these checks, then timed out waiting for a later
terminal command to start. Clippy was running concurrently, but no diagnostic
established the cause. One focused retry passed the coalition/terminal fixture
in 19.36 s and the existing interrupt fixture in 3.33 s. All-bin build,
zero-warning all-target Clippy, formatting, and diff checks passed. The terminal
driver now includes bounded process, foreground,
terminal-mode, and private guardian-log diagnostics on future timeouts; its
assertions and deadlines are unchanged. This observation remains open alongside
the earlier macOS stable interrupt failure. Normal activation and automatic GC
remain off; platform results for this policy are recorded below.

The optimized [one-use-job terminal samples](benchmarks/2026-10-02-macos-arm64-cargo-guardian-once.json)
at clean `b4f9e46` completed 24 no-op and 12 edit pairs, with matched binary
SHA-256 checks and no accepted guardian fallback. Both cache and automatic GC
were off. Plain/managed medians were 24.0/116.6 ms for no-op and 126.7/221.7 ms
for edit builds, adding 92.6/95.0 ms. Managed p95 was 125.7/240.9 ms. This meets
the proposed median allowance for the same tiny crate; larger/concurrent
workloads and the release performance gate remain open.

At `b4f9e46`, the [platform run](https://github.com/Augani/rgo/actions/runs/37037063311)
passed the new coalition/replacement and interrupt cases on macOS 14.8.9
build 23J631 arm64 (8.61 s / 1.43 s) and macOS 15.7.9 build 24G830 Intel
(18.92 s / 2.04 s), both using Cargo/rustc 1.99.0. The complete macOS
stable/beta/nightly suites and all three Rust 1.85 source-build lanes passed.
All 19 jobs completed successfully without a retry, including the complete
Linux/Windows suites, installer and service probes, exact Cargo boundary
checks, source builds, nightly layout checks, btrfs smoke, and 100-project
budget recovery.
The replacement/unload race gate is closed by removing label-based unloading;
the two unexplained terminal/interrupt failures and remaining startup,
cancellation, runtime, and performance gates stay open.

## Preparation cancellation — October 2, 2026

- [x] Reproduce SIGINT captured at the end of preparation still committing
  Cargo, and SIGTERM plus a preparation error starting fallback Cargo.
- [x] Refuse the commit and fallback after captured termination, returning the
  requested signal. Restore terminal state before original signal actions on
  every subsequent preparation error.
- [x] Preserve originally ignored termination signals during preparation. The
  ignored-SIGINT positive control must commit real Cargo and finish successfully.
- [x] Observe continuation/window notifications in the caller even when its
  original dispositions/mask would suppress delivery. Cargo retains its
  captured mask/dispositions. The existing interrupt case starts with SIGCONT
  ignored and blocked, then requires stop/resume, Ctrl-C, a protected surviving
  descendant, and actual reclamation of an idle neighbor.
- [x] Extend only the existing coalition and interrupt cases. The deterministic
  handshake audit requires a private owned directory, is enabled explicitly,
  and is excluded from release binaries. Each cancellation subcase requires
  owned job retirement and actual idle-context deletion before continuing.
- [x] Verify this change across the existing platform lanes. Local all-bin
  build and zero-warning all-target Clippy passed; the coalition/terminal case
  passed in 31.09 s and the interrupt case in 3.41 s on macOS 27.2 arm64.
- [ ] Cover the remaining startup cuts and signal timing after the cancellation
  check, and complete the inherited signal/mask and terminal compatibility
  matrix before normal activation.
- [ ] Verify originally blocked termination and pending signals across exec;
  classify unsupported caller states before admission rather than losing
  notifications when Cargo starts in a different process.
- [ ] Verify post-exec changes to an initially ignored action, including a
  running application that installs its own handler and direct signals to a
  background shell job's process group.
- [ ] Resolve the original macOS stable interrupt failure and the later local
  terminal startup timeout. The original failed job's archived log reports
  0.91 s for the interrupt case; its child's thirty-second expiry cannot explain
  that failure. The new cancellation checks do not establish its cause.

Before the fix, the audit recorded SIGINT with `committed=true` and signal exit
2, SIGTERM with `committed=false` but successful exit 0 through fallback, and
ignored SIGINT with `committed=true` and exit 0. After the fix, the first two
recorded `committed=false` with signal exits 2 and 15; the positive control still
committed and exited 0. The existing writer, replacement-job, journal recovery,
and controlling-terminal checks also passed in the same local run. The result
closes these two reproduced paths; the broader startup/cancellation gate stays
open. Protocol 9 and admission revision 4 are unchanged.

The [captured subcase observations](probes/2026-10-02-macos-preparation-cancellation.json)
record the baseline instrumentation, before/after statuses, and exact local
command. Optimized all-bin build, formatting and diff checks also passed.
Audit strings are absent from release `rgo`/`rgo-rustc-wrapper`, with their
presence in debug `rgo` as the positive control. The earlier `b4f9e46` benchmark
is historical performance evidence and does not measure this signal-handling
change.

The [19-job platform run](https://github.com/Augani/rgo/actions/runs/37042790239)
passed at `db8bbfe` without a retry. Actual coalition/interrupt times were
9.53 s / 1.40 s on macOS 14.8.9 build 23J631 arm64 and 20.89 s / 2.14 s on
macOS 15.7.9 build 24G830 Intel, both with Cargo/rustc 1.99.0. The Intel lane
recorded all three expected cancellation statuses. Full macOS
stable/beta/nightly, Linux/Windows workspace suites, source builds, Cargo
boundaries, nightly layouts, installers/services, btrfs and budget recovery
checks all passed. Normal activation and automatic GC remain off while the
remaining startup, inherited-signal, IDE/runtime and performance gates are open.

## Runtime signal forwarding — October 2, 2026

- [x] Reproduce lost runtime SIGINT when the caller starts with it ignored and
  the running application replaces that action. The same interrupt case fails
  its ten-second termination assertion at native baseline `db8bbfe`.
- [x] Keep preparation-time ignored notifications discarded, clear transient
  capture bits before the handoff, and enable group forwarding when committing
  Cargo. The child retains its native actions/mask and applies its current
  action when the guardian delivers a notification.
- [x] Strengthen the existing interrupt case with a custom SIGINT handler and
  require its exact exit code 73. Retain ignored/blocked SIGCONT, stop/resume,
  surviving-descendant protection and actual idle-neighbor reclamation.
- [x] Publish fixture ready/result values and audit release actions atomically.
  The first combined run observed an existing but empty result file; the
  producer previously exposed existence before complete bytes. Success
  assertions, readers and deadlines are unchanged.
- [x] Pass the two existing cases locally: final custom-handler interrupt
  4.93 s, coalition/terminal/recovery 24.21 s. The intermediate default-action
  variant passed in 5.15 s; the three preparation subcases retain their prior
  expected commit/status outcomes. All-bin debug/release build, zero-warning
  all-target Clippy, format and diff checks passed.
- [x] Verify this batch across the existing platform lanes.
- [ ] Complete the remaining inherited-signal matrix and timing cuts, including
  masked pending termination, failures during the commit handoff, other changed
  actions, and notifications around child creation/exec. Keep normal activation
  and automatic GC off until the broader safety/runtime gates are resolved.

The [captured observations](probes/2026-10-02-macos-runtime-signals.json) record
the native baseline, the initial default-action repro, the final custom-handler
proof, and the result-publication failure. The old unexplained platform
interrupt failure and local terminal timeout remain unresolved. This batch
does not measure performance or close the complete inherited-signal contract;
the earlier benchmark remains historical evidence. Protocol/admission and
receipt/job ownership schemas are unchanged.

The [19-job platform run](https://github.com/Augani/rgo/actions/runs/37046807684)
passed at `c24958d` on its first attempt. Coalition/custom-handler interrupt
times were 12.73 s / 1.64 s on macOS 14.8.9 build 23J631 arm64 and 30.46 s /
2.84 s on macOS 15.7.9 build 24G830 Intel, both with Cargo/rustc 1.99.0.
The exact exit-code-73 assertion passed in both cases; the Intel log also records
all three expected preparation outcomes. Every existing macOS
stable/beta/nightly, Linux/Windows, source-build, Cargo-boundary, nightly-layout,
installer/service, btrfs and budget-recovery lane passed. The broader timing,
masked-signal, IDE/runtime and performance gates remain open.

## Blocked pending termination — October 2, 2026

- [x] Reproduce a blocked SIGTERM queued during preparation disappearing from
  managed Cargo. The same private case proves that ordinary fallback retained
  the notification and invoked the application's handler with exit code 74.
- [x] Capture forwarded notifications in the launcher independently of the
  child's inherited mask. Originally blocked termination is queued for Cargo,
  rather than treated as a preparation cancellation.
- [x] Restore captured, unforwarded blocked notifications to the launching
  thread's kernel pending set before fallback exec. Block relay signals while
  restoring actions/mask; use thread-directed `raise` for pending restoration.
- [x] Preserve actual signal death when the child unblocks an originally
  blocked signal. Restore the launcher state, then unblock the result signal
  before terminating the launcher with that signal.
- [x] Extend the existing preparation subcases with managed and injected-error
  blocked-SIGTERM paths. Require the application's inherited mask, signal exit
  15 for managed execution, exit code 74 for fallback, owned-job retirement,
  and actual idle-context deletion. No new Rust test case was added.
- [x] Pass the combined local coalition/terminal/recovery case in 21.58 s.
  All five preparation subcases produced their required commit/status outcomes.
  The existing custom-handler interrupt case passed in 3.68 s, retaining its
  exact exit code 73, stop/resume, descendant protection and idle reclamation.
  All-bin debug/release builds, zero-warning Clippy, format and diff checks
  passed. Debug audit strings are absent from both release executables.
- [ ] Verify the final batch across the existing platform lanes.
- [ ] Complete other inherited masks/actions, unsupported signal classes, and
  timing immediately around commit, child creation and exec. This queued-TERM
  case does not close those races or the supported IDE/runtime/performance gate.

The [captured observations](probes/2026-10-02-macos-blocked-signals.json) record
the failed managed path and successful fallback control before the fix. The
debug audit observes either the kernel pending set or a captured notification
without consuming it, then releases preparation explicitly. The final managed
variant requires native signal death instead of a numerically similar exit
code. Protocol 9, admission revision 4 and ownership/receipt schemas are
unchanged. Normal activation and automatic GC remain off, and the two earlier
unexplained failures remain unresolved.

Apple documents that exec preserves the signal mask and ignored actions in its
[execve reference](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/execve.2.html).
The fixture checks that contract with real Cargo in private homes; it does not
replace evidence for notifications at every handoff boundary.

The [platform run](https://github.com/Augani/rgo/actions/runs/37051245534) at
`43dfe6a` passed the signal fixtures on macOS 14.8.9 build 23J631 arm64 stable
(11.38 s coalition / 1.47 s interrupt) and macOS 15.7.9 build 24G830 Intel
(34.41 s / 3.57 s), both with Cargo/rustc 1.99.0. The full arm64 nightly lane
also passed. Full-matrix acceptance remains open: macOS beta's existing
`cas_only_pressure_eviction_respects_active_cache_leases` failed because an
evicted manifest's object remained. That core code was unchanged by the signal
batch; the failure's cause is unresolved, not classified as harmless or fixed.

The isolated existing core test passed locally in 0.64 s; all 116 core tests
passed under the locally installed beta 1.99.0-beta.8 in 2.38 s. This differs
from CI's 1.100.0-beta.2 and macOS 14 runtime. The assertion now includes its GC
report, retaining the exact deletion requirement. A manual-only workflow runs
the existing core suite on the affected macOS 14 beta lane, avoiding another
full platform run just to obtain those diagnostics.

The full run completed with 18 of 19 individual jobs passing on its first
attempt. The [focused core diagnostic](https://github.com/Augani/rgo/actions/runs/37052395428)
at `9452c3d` passed all 116 existing tests in 1.97 s on macOS 14.8.9 build
23J631 arm64 with Cargo/rustc 1.100.0-beta.2, matching the failed lane. This is
non-reproduction with stronger diagnostics, not an explanation or a GC fix.
The original failure stays open; no retry or weaker assertion converts it into
completed full-platform acceptance.

## Queued commit and startup recovery — October 2, 2026

- [x] Reproduce a prepared blocked SIGTERM arriving too late: pause the caller
  after commit and require the application to inspect its own kernel pending
  set before unblocking. The baseline application observes `missing` and exits
  0; fallback observes `pending` and exits through its handler with code 74.
- [x] Advertise the guardian's pending-before-exec capability. Refuse an absent
  capability before terminal activation or commit. Accept legacy `S`; send a
  complete `Q` plus four-byte pending mask for new callers.
- [x] Validate queued bits against forwarded signals and the captured blocked,
  non-ignored state. Restore native actions/mask, then recreate those pending
  notifications in the single-threaded child before exec. Retain unsent bits
  in the caller's restoration guard until the complete packet is written.
- [x] Separate prepared and running jobs. Every error from the commit phase
  leaves the record incomplete and may use checkout fallback after restoring
  terminal/signals and acknowledging cancellation. A completed record yields
  only a running job; its errors never start another Cargo invocation.
- [x] Reproduce and fix two incomplete-record outcomes in the existing case:
  actual write-half shutdown after three bytes originally returns code 1 for
  both queued TERM and captured INT. The final paths require no guardian commit,
  fallback handler code 74 for TERM, and signal exit 2 without fallback for INT.
- [x] Pass all seven startup subcases plus the existing writer/terminal/recovery
  requirements locally in 24.72 s. The existing interrupt case passed in 3.57 s
  with exact code 73, stop/resume, survivor protection and actual idle deletion.
  No new Rust test case was added; all pause/cut hooks are debug-only.
- [x] Verify the final batch in the existing platform lanes: all 19 individual
  jobs passed on attempt 1 at `e474461`, including advisory nightly. Arm64
  stable/beta/nightly passed the two actual cases; Intel passed all seven
  startup subcases and both cases with captured output.
- [x] Exercise both healthy same-protocol handoffs with actual old/current
  binaries in private homes. Hold the existing session-lock barrier, start the
  caller, atomically switch the installed pair and release it. Old `S` commits
  to the current guardian; the current caller rejects the old greeting and
  falls back before commit. Each application runs once, returns exactly 17,
  preserves output, retires its job and permits actual idle-context deletion.
- [ ] Exercise full live upgrade/recovery, terminal and signal-state skew,
  already-running old guardians, and the remaining mask/action,
  unsupported-state and post-commit/child-creation timing matrix. Resolve the
  earlier interrupt/terminal failures and the recorded CAS-reclamation failure
  before full release acceptance. Normal activation and automatic GC stay off.

The [captured observations](probes/2026-10-02-macos-queued-commit.json) distinguish
the old one-byte commit's lost notification from the intermediate queued
transport's missing fallback/cancellation handling. The final application
observes `pending` while the caller is paused, before any runtime signal relay.
Each subcase retires its owned guardian and requires real idle-context deletion
before continuing. The prefix cut exercises the actual socket error and strict
record decoder; it does not accept an error code as equivalent to cancellation.
Protocol 9, admission revision 4 and cleanup ownership/receipt schemas are
unchanged. The [live binary observations](probes/2026-10-02-macos-guardian-version-skew.json)
record both basic handoffs and two probe corrections: an absent initial barrier
file and a lexical `/tmp` versus `/private/tmp` comparison. Neither required a
production fix; only the second direction was repeated after correcting path
comparison. The broader live upgrade gate remains open.

The [full platform run](https://github.com/Augani/rgo/actions/runs/37057092962)
passed on macOS 14.8.9 build 23J631 arm64: stable 1.99.0 took 11.72 s coalition /
1.32 s interrupt; beta 1.100.0-beta.2 took 12.43 s / 1.69 s; advisory nightly
1.101.0 took 11.31 s / 1.59 s. Intel macOS 15.7.9 build 24G830 with stable 1.99.0
took 27.75 s / 2.19 s. The
[individual job record](probes/2026-10-02-macos-queued-commit-ci.json) includes all
19 conclusions rather than inferring nightly acceptance from the overall run.
This verifies the batch; the broader release gates and three earlier
unexplained failures remain open.

## Completed GC exclusion — October 2, 2026

- [x] Reproduce exclusive locks surviving a completed GC operation through
  duplicated open file descriptions in the existing context/global lock case.
  The pre-fix assertion fails while all passive copies remain open.
- [x] Explicitly unlock only GC's exclusive Unix flock guards. Wrap each
  successful acquisition immediately so early errors release partial ownership.
  Leave Cargo's inherited shared descendant locks unchanged.
- [x] Close GC's process-associated record descriptors before its legacy flock
  guards and release the same-process guard last. This prevents a completed
  operation's descriptor closure from clearing a newly admitted record lock.
- [x] Pass all 116 core cases locally in 2.15 s, including the duplicated-lock
  assertion, existing shared/legacy session protections and strict CAS pressure
  reclamation. No Rust test case was added.
- [x] Verify the final code on the existing platform matrix: all 19 jobs passed
  on attempt 1 at `e474461`. Each macOS arm64 stable/beta/nightly lane passed
  all 116 core cases, including the strict CAS and duplicated-lock assertions.
  The earlier beta CAS failure remains unexplained: the duplicated-lock
  reproduction proves a separate defect, not its historical cause.

The [lock observations](probes/2026-10-02-gc-exclusive-descriptors.json) record
the deterministic failure and correction. Duplication models a concurrent fork
retaining a descriptor before exec closes it, without forking a multithreaded
test runner. Explicit unlock ends an actual completed operation; it never
weakens the shared locks protecting Cargo or authorizes cleanup of a live job.
