# Focused lifecycle audits

`closed-fd-writer.rs` reproduces the known Unix inherited-descriptor gap. It is
an audit of the current mechanism, not a safety regression that expects correct
behavior, and is outside the automatic workspace test suite.

To repeat the private-home audit from the repository root:

```sh
cargo build
cp docs/probes/closed-fd-writer.rs crates/rgo/tests/closed_fd_audit.rs
cargo test -p rgo-storage --test closed_fd_audit -- --nocapture
rm crates/rgo/tests/closed_fd_audit.rs
```

The audit uses `rgo-testkit::Sandbox`, a dependency-free real Cargo project, and
Python's normal `subprocess.Popen` behavior with a detached session. It confirms
that the child's Cargo-provided output path belongs to the managed context,
lets Cargo exit, checks guard availability, requests GC, then releases the child
to attempt a write. The child has a bounded lifetime and reports its result
outside the evictable context. It never activates the developer's Cargo home.

Observed on macOS arm64, October 2, 2026:

```text
guard_available=true; context_deleted=true; gc_success=true;
late_write=[Errno 2] No such file or directory: .../out/late-build-output
```

This expected-bug assertion must be replaced with a protected-context and
successful-write regression when the independent supervisor is integrated.

`macos-launch-once.py` is a separate manual launchd/kernel audit. It creates only
unique jobs in temporary private directories; it never runs Cargo or setup.
It observes one-use registration removal after normal exit and SIGKILL while a
detached closed-FD writer survives, then kernel reaping after its late write.
Run `python3 docs/probes/macos-launch-once.py /tmp/rgo-launch-once.json` on macOS.
The [recorded local result](2026-10-02-macos-launch-once.json) is mechanism evidence;
the installed-launcher regression and declared platform matrix remain required.
