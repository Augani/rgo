# GC lifecycle safety argument (draft; release gate open)

This is the P2 safety case for deleting a managed build context. It separates
mechanisms already exercised from the assumptions and adversarial cases that
still prevent unattended cleanup. The authoritative delivery checklist is
[installation-storage-plan.md](installation-storage-plan.md); `gc.auto` remains
`false` by default. A green test below is evidence for its stated scenario, not
proof for all Cargo commands or all process trees.

## Scope and property

The desired property is: while any admitted Cargo command or descendant can
read or write a managed context, GC neither renames nor deletes that context or
its documented incremental sub-tier. Once GC has taken its exclusion guard, a
new admitted command cannot start using the old context until removal finishes.
An idle, unpinned, attributed context must still be reclaimable when another
context is busy. We do not claim that this bounds compiler RAM: relocation and
GC control disk use.

An **admitted** command is one launched through rgo's supervised `cargo`
entry point with its effective build directory selected by rgo. In native mode,
the Cargo-home `build.build-dir` setting also sends direct Cargo processes into
rgo storage, but those processes have no rgo session guard. Therefore native
mode does **not** satisfy the property for unattended whole-context deletion.
Native setup and daemon startup now reject `[gc].auto = true`. Every destructive
GC pass and explicit `rgo clean` recheck the recorded supervised mode and its
Cargo-home owner. They refuse cleanup if that home's effective config sets a
global build directory or has an unresolved include. Supervised setup rejects
those configs before activation; a dry-run can still inspect native storage.
These admission checks do not cover project, environment, or CLI overrides by
an unsupervised Cargo process, nor prove all supervised process trees safe.
Cargo permits a direct invocation to select any writable build directory with
an explicit override, including rgo's internal path. Because that process runs
as the same user, rgo cannot prevent this deliberate namespace entry with
ordinary directory permissions. The release contract must either exclude
direct invocations explicitly pointed at rgo's managed namespace or obtain a
cooperative Cargo lifecycle hook; this boundary is unresolved, not evidence
that all direct Cargo launches are safe.
Cargo's [current layout source](https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/compiler/layout.rs.html)
also omits its build-directory profile lock on NFS mounts. The profile-lock
heuristic therefore cannot admit an NFS root to native cleanup. Destructive
rgo cleanup now rejects known Linux network, clustered, and FUSE filesystem
types (including NFS, SMB/CIFS, Ceph, AFS, Coda, NCP, 9p, and OCFS2), macOS
non-local volumes, and Windows remote drives; a failed volume query also
blocks it. Linux identifiers come from the [kernel magic header](https://github.com/torvalds/linux/blob/master/include/uapi/linux/magic.h).
Supervised setup with `gc.auto = true` performs this volume check before
writing activation state, and doctor exposes the result independently.
The GC executor repeats the check immediately before staging each deletion;
this narrows, but cannot eliminate, a mount change between check and rename.
This is a conservative exclusion, not proof that every unusual filesystem is
local or that its lock semantics are suitable for automatic deletion.
Undo removes the active owner record, so a retained storage root cannot be
destructively cleaned through `rgo gc` after uninstall; a separately reviewed
offline purge remains release work.
No amount of recent-mtime grace or rustc-wrapper lease can cover a no-op build,
build script, test process, or a Cargo process waiting on a lock. Cargo's
documented [build-directory configuration](https://doc.rust-lang.org/cargo/reference/config.html#buildbuild-dir)
establishes where intermediates go; it does not give rgo a context-wide
ownership lock. The profile `.cargo-build-lock` probe is only an additional
liveness heuristic. Fresh contexts created by supervised Cargo carry a
`supervised_origin` sidecar field. The supervised GC planner protects contexts
without that origin for orphan, age, incremental, and pressure cleanup; its
executor checks the origin again after acquiring the lifecycle guard. For
eligible supervised contexts, GC can skip the ten-minute timestamp grace while
still refusing a held or unreadable profile lock and holding rgo's guard
through deletion. This does not detect direct Cargo writing into an already
supervised context without running the wrapper.

## Proposed exclusion protocol

1. Supervised setup must remove the global managed `build.build-dir` setting.
   The owned shim resolves the workspace before running Cargo and selects an
   rgo-owned context from the canonical workspace manifest path. An unknown,
   unsupported, or override-bearing invocation uses ordinary Cargo storage
   while holding a conservative global session guard. A direct Cargo launch
   then uses its normal checkout build directory, outside GC's namespace.
2. The shim obtains a shared lock on a stable file in `state/locks` before
   creating or entering its selected context. GC obtains a nonblocking
   exclusive lock on that same context file, plus the global guard, and keeps
   them through rename **and** removal. The lock file is outside the renamed
   tree so a waiting command cannot be stranded on a replaced context lock.
   GC skips a busy context without queuing a writer behind a nested build.
3. The Unix launcher `exec`s Cargo with a process-associated `fcntl` lock and
   an inherited `flock` descriptor. The former follows Cargo across `exec`;
   the latter is intended to hold exclusion while an inheriting descendant
   remains after Cargo exits. [POSIX record locks survive exec but are not
   inherited by fork](https://man7.org/linux/man-pages/man2/fcntl_locking.2.html),
   which is why the second mechanism is needed. Lock-file open rejects symlinks
   and checks device/inode identity before and after acquisition.
4. On Windows, the guardian obtains the session lock before creating Cargo.
   It creates Cargo suspended with a private kill-on-close Job Object assigned
   at process creation, resumes it, then holds the lock until Cargo exits and
   the job reports zero active processes. A failed query closes the job and
   kills its remaining members. The Windows lock opener compares the opened
   handle's volume and file ID with the current named file before admitting
   the session or GC pass, and excludes `FILE_SHARE_DELETE` while the handle
   is open so direct rename/delete cannot replace that file. Microsoft
   documents [job assignment at process
   creation](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute),
   [kill-on-close](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_extended_limit_information),
   and the [active-process count](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_accounting_information).
5. Deletion rechecks ownership, the top-level sidecar's supervised origin,
   pin state, workspace availability, and the documented Cargo profile lock
   after acquiring its guard. A missing or unreadable sidecar, native origin,
   lock state, or workspace identity prevents context deletion. Pin decisions
   live outside the evictable tree so a full `cargo clean` cannot silently erase
   intent. New pin/unpin decisions replace their record through a synced staging
   file under the decision lock;
   a failed write leaves the prior decision intact. GC stages by same-volume
   rename into its owned temporary domain before recursive removal.

The ordering required for the first two steps is **session shared lock before
managed access; GC exclusive lock before final eligibility check; release only
after the last possible writer or completed removal**. A session and GC must
agree on the same stable lock identity. The daemon's operation lock also orders
pin changes and lease binding against its GC pass; it does not replace the
process-level session guard.

## Evidence so far

- Unit and private-home fixtures verify context/global lock exclusion,
  missing-sidecar refusal, pin changes after planning, and an unrelated idle
  context being reclaimed while one context is held. The [cross-platform
  rename-pause run](https://github.com/Augani/rgo/actions/runs/36513462716)
  checked that a new session waits at GC's locked and staged points and can
  recreate a fresh context only after removal.
- A focused origin fixture requires a native-origin context to stay protected
  while a supervised neighbor is reclaimed under pressure. It also calls the
  deletion executor on the native context and requires refusal. The local
  full suite and the [18-job platform matrix](https://github.com/Augani/rgo/actions/runs/36890331922)
  passed. The marker
  cannot prove that no later direct Cargo invocation wrote the same context.
- A private-home Unix real-Cargo race pauses an explicit context clean after
  GC takes its stable guard. A newly started unchanged `cargo build` reaches
  its session lock but cannot enter Cargo until deletion finishes; it then
  recreates the managed context while the old generation's marker stays gone.
  The [Windows counterpart](https://github.com/Augani/rgo/actions/runs/36591556311)
  passed using its Job Object assignment marker. This is one start/delete
  ordering case, not the full command and platform matrix.
- The [Windows creation-time assignment run](https://github.com/Augani/rgo/actions/runs/36518298190)
  exercised a guardian kill before Cargo resumed and a real child after
  launch. Earlier [Windows job checks](https://github.com/Augani/rgo/actions/runs/36484591309)
  observed child termination before the guard became available. A live
  `cargo run` fixture protected its own context while another was reclaimed.
- The [green lock-identity matrix](https://github.com/Augani/rgo/actions/runs/36535047318)
  exercised Windows replacement detection and denial of direct lock-file
  rename while an rgo handle is open, alongside the stable installer probe.
- The [green supervised `cargo test` matrix](https://github.com/Augani/rgo/actions/runs/36536240212)
  held a real test executable after rustc finished, refused deletion of its
  context, reclaimed another idle context, and removed the protected context
  after Cargo exited. It covers one ordinary test-process shape on Linux,
  macOS, and Windows; it does not cover an externally brokered process.
- A private-home `cargo build` fixture now holds a real build script after its
  compiler invocation, refuses removal of that active context, reclaims an
  unrelated idle context, and requires the script to write into its `OUT_DIR`
  before Cargo can finish. It passed locally on macOS arm64 and in the [full
  Linux, macOS, and Windows CI matrix](https://github.com/Augani/rgo/actions/runs/36634010013).
  This covers one build-script shape without relying on an
  active rustc-wrapper lease.
- A private-home `cargo doc` fixture holds rustdoc before it writes output,
  refuses removal of the active context, and reclaims an unrelated idle one.
  After release, the requested documentation appears in the checkout and the
  context becomes reclaimable. It passed locally on macOS arm64 and in the
  [Linux and macOS CI workspace suites](https://github.com/Augani/rgo/actions/runs/36785074208);
  other rustdoc-command coverage remains open.
- The supervised launcher refreshes its owned sidecar before each admitted
  Cargo invocation. A private-home unchanged second `cargo build` confirms a
  no-op invocation updates `last_seen` without rustc running. It passed locally
  on macOS arm64 and in the [full Linux, macOS, and Windows CI
  matrix](https://github.com/Augani/rgo/actions/runs/36635824970). Native
  direct-Cargo no-ops do not provide this signal and are not admitted to
  destructive cleanup.
- That fixture now starts four manual GC clients during the held test process,
  pausing the first deletion while three more requests remain in flight. The
  [platform matrix](https://github.com/Augani/rgo/actions/runs/36594347437) passed on a Windows stable
  rerun, and the test passed in both Windows stable and beta logs. The first
  stable attempt timed out in the pre-existing daemon tests before this fixture;
  its cause is unconfirmed. This covers one contention shape, not sustained
  client floods or arbitrary daemon stalls.
- A private Cargo 1.98.0 probe showed full `cargo clean` removes the sidecar
  and in-context pin marker, while `clean -p` leaves the sidecar. The next
  supervised build restores it; durable pin intent survives outside the
  context. The [Cargo 1.91 boundary matrix](https://github.com/Augani/rgo/actions/runs/36537118268)
  also found that package clean preserved the sidecar, full clean removed the
  context, and the next native build restored attribution on Linux, macOS, and
  Windows. The [new Cargo 1.91 boundary matrix](https://github.com/Augani/rgo/actions/runs/36799141501)
  repeats those operations through the supervised launcher on all three OSes:
  `clean -p` retains the sidecar, full `clean` retains the durable pin outside
  the removed context, and a rebuild restores attribution and pin visibility.
  Interrupted clean and clean behavior on later Cargo versions remain open.
- A required [CI matrix](https://github.com/Augani/rgo/actions/runs/36783276609)
  passes across Linux, macOS, and Windows. It is a regression signal for
  existing fixtures, not a full lifecycle proof.

## Counterexamples and unproven assumptions

| Condition | Current treatment | Release work |
|---|---|---|
| Native setup or an external Cargo/IDE launch with a managed build-dir override | The rgo lock does not cover it. Native `gc.auto` stays off; supervised cleanup excludes native-origin sidecars and refuses Cargo-home config drift. It cannot detect a direct process writing a context already marked supervised. | Prevent managed-namespace bypass in the supported activation contract or obtain an upstream Cargo lifecycle hook. |
| A build-script or test descendant escapes the inherited Unix descriptor, or a Windows process starts through an external broker such as WMI | The parent can finish while an external writer remains. | Run real process-tree fixtures, define supported launch semantics, and refuse unattended deletion where exclusion cannot be guaranteed. |
| Same-user replacement of the lock directory or, on Unix, a lock file after its final identity check | Existing holders could lock different file identities even though pre-acquisition checks passed. Windows now denies direct lock-file rename/delete while its handle is open; ancestor mutation remains unproven. | Define and enforce private lock-directory ownership/mutation rules; exercise replacement at every pause point. |
| Unsupported filesystem locking, unreadable state, daemon outage, or failed Windows job creation | Error/guard contention skips GC; supervised launch should use ordinary Cargo storage if it cannot establish protection. | Prove fallback before any managed write on each platform and with service restart/interruption. |
| Concurrent `cargo clean`, no-op/use without rustc, nested Cargo, long `cargo run`, Ctrl-C/crash, multiple GC clients | Only selected mechanism and live-command fixtures exist. | Deterministic real-Cargo race matrix with a positive deletion control in another context. |
| Cargo version changes its build-directory or lock behavior | The current exact-version probes cover relocation, not the full lifecycle. | Version/OS capability table and re-probe before each supported release. |

**Decision:** do not enable unattended destructive GC or advertise bounded
storage until the release-work rows above are closed on the declared platform
matrix, P3 demonstrates actual bounded reclamation, and P4 proves clean-machine
activation/undo. If a process tree cannot be confined by the supervised entry
point, exclude that workflow from managed storage or leave automatic deletion
disabled. Manual destructive `rgo gc` and `rgo clean` require recorded
supervised mode and retain the same defensive checks; their successful
scenarios are not evidence that every automatic deletion is safe.
