# rgo — notes for contributors and agents

- Architecture vision: `doc.md`. Implementable plan and current status: `PLAN.md` (wins on conflict).
- Build: `cargo build` (must build all bins; `cargo test -p rgo` alone does not rebuild `rgo-rustc-wrapper`).
- Verify: `cargo clippy --all-targets` (workspace lints: clippy::all = warn, keep at zero) and `cargo test`.
- Integration tests use `rgo-testkit::Sandbox` (private HOME/CARGO_HOME/RGO_HOME) and run real cargo
  offline. Never run `rgo setup` against the developer's real `$CARGO_HOME` from tests.
- Manual e2e: `HOME=/tmp/x CARGO_HOME=/tmp/x/.cargo RGO_HOME=/tmp/x/.rgo RUSTUP_HOME=~/.rustup RUSTUP_TOOLCHAIN=stable target/debug/rgo setup --no-service`, then `cargo build` in any project.
- `crates/rgo-rustc-wrapper` must stay dependency-light (no clap/tokio/sqlite/tracing); it runs per rustc invocation.
- rgo never depends on the internal layout of a Cargo build-dir beyond: the top-level sidecar and
  `.rgo-pin` marker it writes,
  `<profile>/incremental/` (documented) and `<profile>/.cargo-build-lock` (liveness heuristic).
