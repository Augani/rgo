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
