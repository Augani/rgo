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
  bytes or file identities. Explicit undo instead requires the same current
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
- [ ] Validate this batch in platform CI before counting its platform evidence.

Maintenance retains its directory iterator, considers at most 16 entries per
pass, and removes at most two metadata files per entry. A 50 ms elapsed budget
is checked between entries; it is not a hard bound on filesystem syscall time.
Unknown temporary or incomplete registration directories remain preserved.
This addresses abandoned complete host records, not every startup interruption,
the full terminal runtime matrix, or the performance activation gate.

Local macOS 27.2 arm64 validation passed the all-bin build, the two existing
focused macOS cases, all-target Clippy with warnings denied, format, and diff
checks. The extended coalition case completed in 17.86 seconds; no equivalent
Rust test case or redundant full local suite was added. Platform CI is pending.

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
