use std::path::PathBuf;

use anyhow::Result;

pub fn run(_roots: Vec<PathBuf>) -> Result<()> {
    // TODO(phase 1, step 7): walk roots (default ~/Projects ~/src ~/code ~/dev, configurable),
    // find `target/` dirs whose sibling `Cargo.toml` exists (or not: abandoned), measure them,
    // and offer to delete intermediates (`deps/ build/ incremental/ .fingerprint/`) while
    // keeping uplifted binaries. Since Cargo no longer writes intermediates there after
    // `rgo setup`, this is a one-time migration helper. Must skip dirs whose project overrides
    // build-dir/target-dir in `.cargo/config.toml`.
    println!("not implemented yet");
    Ok(())
}
