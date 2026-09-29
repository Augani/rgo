# rgo

Experimental machine-managed Rust build storage. `rgo setup` configures Cargo so
plain `cargo` commands put intermediates in a managed root. Automatic cleanup,
installation, and cache correctness are still being validated against the
[release checklist](docs/installation-storage-plan.md); this is not yet an
install-and-forget release.

## What it does

- Redirects Cargo's `build.build-dir` to `~/.rgo/builds/{workspace-path-hash}` via a
  reversible fenced block in `$CARGO_HOME/config.toml`.
- Final artifacts stay put: `target/debug/<bin>`, `cargo run`, scripts and IDEs keep
  working identically. Only intermediates (`deps/`, `incremental/`, `.fingerprint/`,
  build-script output) move.
- A per-user daemon records compiler leases. A compiler wrapper cannot observe
  every Cargo operation; full-session GC safety is an open release gate, so
  unattended destructive cleanup is disabled by default.
- Optional phases (compiler-result cache, remote CAS) are implemented but off by
  default while correctness gates run; see `PLAN.md`.

## Development checkout

```bash
cargo build                           # builds rgo and rgo-rustc-wrapper together
target/debug/rgo setup --no-service   # development evaluation in a private Cargo home
```

`rgo setup` requires `rgo-rustc-wrapper` next to the `rgo` executable. A
verified one-command installer and published matched bundles are release work,
not yet available. Evaluate setup with private `HOME`, `CARGO_HOME`, and
`RGO_HOME` until the [safety gates](docs/installation-storage-plan.md) close.
The existing [`rgo` package on crates.io](https://crates.io/crates/rgo) is an
unrelated project. The planned source package for this CLI is `rgo-storage`;
it still installs an executable named `rgo`.

## Setup

```bash
rgo setup            # one-time, idempotent; --dry-run to preview
rgo doctor           # inspect config, toolchains, and service without writing files
rgo doctor --json    # versioned, machine-readable diagnostic report
rgo doctor --verify  # disposable plain-Cargo build; fails if activation is not observed
cargo build          # plain cargo; intermediates now land under ~/.rgo/builds
rgo status           # managed bytes, budget, reclaimable
rgo ls               # every managed build-dir with attribution
```

`rgo setup` currently performs these changes; the complete upgrade and rollback
lifecycle remains release work:

1. Creates `~/.rgo/` (or `$RGO_HOME`).
2. Adds a fenced `[build] build-dir = ".../{workspace-path-hash}"` block — plus
   `build.rustc-wrapper` so dependency invocations can reach the optional cache —
   to Cargo's active home config file (`config` or `config.toml`), preserving any
   wrapper you already had. It keeps a small activation record and custom-root
   pointer in Cargo home so fresh Cargo processes find the same rgo storage.
   One storage root belongs to one Cargo home; use a separate `RGO_HOME` when
   configuring a second `CARGO_HOME`.
3. Installs a per-user background service (launchd/systemd/Task Scheduler) running
   `rgo daemon`, then checks that the service is running and its daemon answers
   IPC. Failed startup is reported as degraded maintenance. `--no-service`
   skips this. In native mode plain Cargo will not start background maintenance;
   the opt-in supervised launchers on Unix and Windows start a daemon on Cargo
   use only when `[gc].auto = true` is explicitly configured for evaluation.
   Native setup rejects `[gc].auto = true`, and destructive `rgo gc` and
   `rgo clean` require supervised mode: native relocation has no guard covering
   an entire Cargo session.

For private Unix evaluation of full Cargo-session supervision, use
`rgo setup --supervised --real-cargo /absolute/path/to/cargo --no-service` in
an isolated Cargo home. This installs an owned launcher at
`$CARGO_HOME/rgo/shims/cargo` without setting a global Cargo build directory.
Put that directory first on `PATH` to use unchanged `cargo` commands; `rgo
doctor --verify` checks the active path and runs a disposable build. The mode
keeps direct Cargo invocations outside rgo's managed GC area by default.
The launcher is tied to its installing Cargo home and storage root. If a
different installation's shim is first on `PATH`, it forwards to the real
Cargo proxy and uses that home's ordinary local storage; put the matching
shim first to manage that home.
`rgo setup --undo --no-service` removes the owned launcher. Switching between
native and supervised modes requires undo and a fresh `RGO_HOME` until a safe
storage migration exists. The opt-in Unix bundle installer (`--supervised --no-service`)
adds an owned PATH block to standard Bash or zsh login and interactive startup
files, then removes only that block on `--uninstall`. Direct `rgo setup` does
not edit shell files. The installer prints an `export PATH=...` command for
its current shell; custom startup layouts and GUI-launched IDEs still need
explicit activation checks. Unattended GC remains off by default; enabling it
is still experimental while the lifecycle safety gate is open.

For a private Windows evaluation, run
`rgo setup --supervised --real-cargo C:\absolute\path\to\cargo.exe --no-service`
to install an owned
`$CARGO_HOME\rgo\shims\v<version>\cargo.exe` copy of the matched `rgo.exe`. Put its
directory first on `PATH` in the shell or tool launching Cargo, then run
`rgo doctor --verify`. This leaves the rustup Cargo proxy in place; direct
invocations of that proxy use ordinary local storage. `rgo setup --undo --no-service`
removes the activation but retains the versioned shim as a fallback for open
shells. The installer can opt into this mode with `-Supervised -NoService` and
`-RealCargo`; it places the shim ahead of Cargo on User PATH. Automatic
destructive GC remains disabled by default.

For a no-service activation, setup and undo stop a compatible daemon started
by plain supervised Cargo before changing the installation. If that daemon
cannot be stopped, the activation change fails without removing Cargo settings.

For a private Windows evaluation, `scripts/install-windows.ps1` accepts an
exact `-ReleaseTag`, a local `-Archive`, and its `-Sha256`, together with
`-DevelopmentBundle -NoService` for locally built bundles. For a release
bundle it requires `-Repository OWNER/REPO` and verifies the archive digest
and GitHub artifact attestation before activation. It installs the matched
executables in a versioned Cargo-home directory, runs `setup` and
`doctor --verify`, and adds its command directory to the user PATH when
needed (`-NoUserPath` keeps PATH changes in the current process for CI).
`-NoWrapper` selects storage-only activation when wrapper composition is not
desired. The installer also accepts the storage-only record that `rgo setup`
chooses automatically for an existing workspace wrapper or included config.
`-Repair` restores missing owned files from the same verified bundle;
`-Uninstall` removes owned activation and command copies while retaining
versioned binaries and managed data. A version change is available only for an
existing `-NoService` installation when the new verified bundle is installed
with `-NoService`; the installer journals the Cargo activation files, retains
the old binary pair, and can restore the old activation after a partial switch.
The effective wrapper mode must remain the same during a version change.
Supervised version changes also switch the owned User PATH entry to the new
versioned shim while retaining old shims for open shells. Service-managed
upgrades remain refused. The local-bundle installer has passed
private-home Windows CI, but no public release asset or tagged attestation has
been verified yet.

## Everyday commands

```bash
rgo gc [--dry-run] [--aggressive] [--target 20GB]   # preview in any mode; reclaim in supervised mode
rgo pin <id> / rgo unpin <id>                      # protect a context from GC
rgo clean <id>                                     # remove one supervised context now
rgo adopt [dirs...]                                # report legacy target/ storage; read-only
rgo cache stats|explain <key>|verify               # inspect the opt-in cache
```

A pin protects the context from rgo cleanup until `rgo unpin`. An explicit
`cargo clean` still removes its build files; the pin intent survives and
protects the context after the next build. `rgo ls` shows retained pins even
while their contexts are absent; `rgo unpin <id>` releases one.

## Precedence and bypass

Cargo treats intermediate and final paths separately. Project
`build.build-dir` or `CARGO_BUILD_BUILD_DIR` can override rgo's managed
intermediates. `--target-dir`, `CARGO_TARGET_DIR`, and project
`build.target-dir` move final outputs but can leave managed intermediates
enabled. Cargo 1.91 or newer is required for `build.build-dir`.

`RGO_BYPASS=1` bypasses rgo's wrapper behavior; it does not disable Cargo's
configured build directory or a running maintenance service. To put one project
back on ordinary local intermediates, set `build.build-dir =
"{workspace-root}/target"` in that project's `.cargo/config.toml` (or select
the matching path for a custom target directory).
In supervised mode, `RGO_BYPASS=1` runs the real Cargo proxy without the
launcher-provided build-directory override. The launcher still holds a global
GC guard for that command's lifetime.

## Recovery

| Symptom | Fix |
|---|---|
| Compiler wrapper misbehaves | `RGO_BYPASS=1 cargo build` bypasses wrapper/cache behavior; Cargo relocation remains active |
| Wrapper executable missing | for an installer-owned Unix `--no-service` install, rerun its verified same-version bundle with `--repair --no-service`; for a Windows installer pilot, rerun the same verified archive with `-Repair`; otherwise run `rgo setup --undo` with a working `rgo` executable before reinstalling the matched pair |
| One project misbehaves | set `build.build-dir`/`target-dir` in its `.cargo/config.toml` |
| Daemon unavailable | inspect `rgo doctor` and the service logs, then rerun `rgo setup` for this installation; native `--no-service` provides no automatic maintenance |
| Metadata database corrupt | confirmed SQLite corruption is moved to `state/meta.sqlite.corrupt-*` and the index is rebuilt; other open errors are reported |
| CAS object corrupt | quarantined automatically to `~/.rgo/quarantine/`; next build misses cleanly |
| Remove an installer-owned Unix `--no-service` activation | run the same installer's `--uninstall` option; it restores Cargo settings, removes owned command links and supervised shell PATH blocks, and retains versioned binaries and managed data for explicit later cleanup |
| Remove an installer-owned Windows activation | run `scripts/install-windows.ps1 -Uninstall` with the same destination options; it verifies ownership, undoes Cargo setup, removes owned command copies and its PATH entry, and retains versioned binaries and managed data |
| Other installations | `rgo setup --undo`, then remove binaries only after confirming no Cargo build still needs them; decide separately whether to retain managed data |

## Upgrade / uninstall

- Upgrade of development builds: run `rgo setup --undo` with the working pair,
  replace both binaries together, then rerun setup. The Unix installer has a
  private `--no-service` upgrade/rollback pilot. It can recover an interrupted
  setup when every tracked file still matches its recorded prior or planned
  value; a later user edit stops automatic recovery. Its `--repair` option can
  restore missing or damaged files in the active owned version from the same
  verified bundle. Service-managed upgrades and
  cross-version verification remain release work.
- Uninstall: the Unix installer's `--uninstall` path handles an owned
  `--no-service` installation and resumes an interrupted removal. It leaves
  versioned binaries for Cargo processes already using their absolute paths,
  and retains managed data. For other installations, `rgo setup --undo`
  removes the fenced config block and service; complete installer and data
  removal remains release work.

From a development checkout, run `python3 scripts/install-unix.py --uninstall`
for an installer-owned `--no-service` activation in the default Cargo home.

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
cache_retention = "30d" # unused compiler-result manifests; shared objects remain referenced
auto = false             # unattended deletion awaits the Cargo lifecycle safety gate

[cache]                  # Phase 3 — off by default
enabled = false
```

## License

MIT OR Apache-2.0
