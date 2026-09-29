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
No amount of recent-mtime grace or rustc-wrapper lease can cover a no-op build,
build script, test process, or a Cargo process waiting on a lock. Cargo's
documented [build-directory configuration](https://doc.rust-lang.org/cargo/reference/config.html#buildbuild-dir)
establishes where intermediates go; it does not give rgo a context-wide
ownership lock. The profile `.cargo-build-lock` probe is only an additional
liveness heuristic.

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
5. Deletion rechecks ownership, the top-level sidecar, pin state, workspace
   availability, and the documented Cargo profile lock after acquiring its
   guard. A missing or unreadable sidecar, lock state, or workspace identity
   prevents context deletion. Pin decisions live outside the evictable tree so
   a full `cargo clean` cannot silently erase intent. GC stages by same-volume
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
- A private Cargo 1.98.0 probe showed full `cargo clean` removes the sidecar
  and in-context pin marker, while `clean -p` leaves the sidecar. The next
  supervised build restores it; durable pin intent survives outside the
  context. This is version-specific evidence only.
- The current required [CI matrix](https://github.com/Augani/rgo/actions/runs/36531530596)
  passes across Linux, macOS, and Windows. It is a regression signal for
  existing fixtures, not a full lifecycle proof.

## Counterexamples and unproven assumptions

| Condition | Current treatment | Release work |
|---|---|---|
| Native setup or an external Cargo/IDE launch with a managed build-dir override | The rgo lock does not cover it. `gc.auto` stays off. | Prevent managed-namespace bypass in the supported activation contract or obtain an upstream Cargo lifecycle hook. |
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
disabled. A manual `rgo gc` remains an explicit operation with the same
defensive checks; it is not evidence that automatic deletion is safe.
