//! Fenced, reversible edits to `$CARGO_HOME/config.toml`.
//!
//! rgo owns exactly the region between the two fence comments and never touches anything
//! else. Existing standard tables retain their surrounding formatting; inline and
//! dotted build tables are normalized so owned keys can be fenced reversibly.
//! Precedence is Cargo's: project `.cargo/config.toml`,
//! `CARGO_BUILD_BUILD_DIR` overrides the build directory. `CARGO_TARGET_DIR` and
//! `--target-dir` select final-output locations without cancelling the build directory.

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, value};

pub const FENCE_START: &str =
    "# >>> rgo managed — do not edit inside this fence; `rgo setup --undo` removes it >>>";
pub const FENCE_END: &str = "# <<< rgo managed <<<";

pub const MIN_BUILD_DIR_CARGO: (u32, u32, u32) = (1, 91, 0);

/// Parse Cargo's version banner, including beta and nightly versions. A
/// non-Cargo executable must not be treated as a capable toolchain.
pub fn cargo_version(text: &str) -> Option<(u32, u32, u32)> {
    let version = text
        .trim()
        .strip_prefix("cargo ")?
        .split_whitespace()
        .next()?;
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parts = core.split('.').map(str::parse::<u32>);
    let parsed = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

pub fn supports_build_dir(text: &str) -> bool {
    cargo_version(text).is_some_and(|version| version >= MIN_BUILD_DIR_CARGO)
}

/// Cargo gives the legacy extensionless file precedence when both names exist.
/// Write the file Cargo actually reads instead of leaving an inactive fence in
/// `config.toml` and reporting a false activation.
pub fn effective_home_config(cargo_home: &Path) -> std::path::PathBuf {
    let legacy = cargo_home.join("config");
    if legacy.exists() {
        legacy
    } else {
        cargo_home.join("config.toml")
    }
}

/// A supervised launcher must not inject its command-line build directory if
/// this config may choose one. Includes are treated as unknown because Cargo
/// can resolve them recursively and with version-dependent rules.
pub fn may_set_build_dir(text: &str) -> Result<bool> {
    let doc: DocumentMut = text.parse().context("parsing Cargo configuration")?;
    Ok(doc.get("include").is_some()
        || doc
            .get("build")
            .and_then(|build| build.get("build-dir"))
            .is_some())
}

/// What `rgo setup` wants Cargo to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub build_dir: String,
    /// `None` = leave the user's wrapper setting alone.
    pub rustc_wrapper: Option<String>,
    /// `None` = leave the user's wrapper setting alone.
    pub rustc_workspace_wrapper: Option<String>,
}

/// Facts about the existing config that affect what setup may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    pub has_fence: bool,
    pub has_include: bool,
    pub build_dir_outside_fence: Option<String>,
    pub configured_build_dir: Option<String>,
    pub target_dir: Option<String>,
    pub rustc_wrapper: Option<String>,
    pub configured_rustc_wrapper: Option<String>,
    pub rustc_workspace_wrapper_outside_fence: Option<String>,
}

pub fn inspect(text: &str) -> Result<Inspection> {
    let (outside, inside) = split_fence(text)?;
    let doc: DocumentMut = outside
        .parse()
        .context("parsing cargo config outside rgo fence")?;
    let configured: DocumentMut = text.parse().context("parsing complete cargo config")?;
    let get = |k: &str| {
        doc.get("build")
            .and_then(|b| b.get(k))
            .and_then(Item::as_str)
            .map(str::to_owned)
    };
    Ok(Inspection {
        has_fence: inside.is_some(),
        has_include: doc.get("include").is_some(),
        build_dir_outside_fence: get("build-dir"),
        configured_build_dir: configured
            .get("build")
            .and_then(|build| build.get("build-dir"))
            .and_then(Item::as_str)
            .map(str::to_owned),
        target_dir: get("target-dir"),
        rustc_wrapper: get("rustc-wrapper"),
        configured_rustc_wrapper: configured
            .get("build")
            .and_then(|build| build.get("rustc-wrapper"))
            .and_then(Item::as_str)
            .map(str::to_owned),
        rustc_workspace_wrapper_outside_fence: get("rustc-workspace-wrapper"),
    })
}

/// Returns the new file contents with the rgo fence (re)written. Idempotent.
pub fn apply(text: &str, desired: &Desired) -> Result<String> {
    let (outside, _) = split_fence(text)?;
    let mut block = DocumentMut::new();
    block["build"] = toml_edit::table();
    block["build"]["build-dir"] = value(&desired.build_dir);
    if let Some(w) = &desired.rustc_wrapper {
        block["build"]["rustc-wrapper"] = value(w);
    }
    if let Some(w) = &desired.rustc_workspace_wrapper {
        block["build"]["rustc-workspace-wrapper"] = value(w);
    }
    // Cargo merges duplicate `[build]` tables across files but NOT within one file, so if
    // the user already has `[build]`, we must add keys to it rather than emit a second table.
    let mut doc: DocumentMut = outside.parse().context("parsing cargo config")?;
    if doc.get("build").is_some_and(Item::is_table_like) {
        if doc["build"].is_inline_table() {
            let inline = std::mem::take(&mut doc["build"]);
            doc["build"] =
                Item::Table(inline.into_table().map_err(|_| {
                    anyhow::anyhow!("could not normalize inline Cargo build table")
                })?);
        }
        // Dotted `build.jobs = ...` creates an implicit table. Make the
        // existing user settings explicit so the owned keys can sit in a
        // reversible fence inside one legal Cargo `[build]` table.
        doc["build"].as_table_mut().unwrap().set_implicit(false);
        doc["build"].as_table_mut().unwrap().set_dotted(false);
        let mut managed_keys = vec!["build-dir"];
        for (k, v) in block["build"].as_table().unwrap().iter() {
            doc["build"][k] = v.clone();
            if k != "build-dir" {
                managed_keys.push(k);
            }
        }
        let edited = fence_existing_build_keys(&doc.to_string(), &managed_keys);
        if !edited.contains(FENCE_START) {
            bail!(
                "unsupported Cargo [build] syntax: cannot place a reversible rgo fence; leave the file unchanged"
            );
        }
        edited
            .parse::<DocumentMut>()
            .context("validating edited Cargo config")?;
        return Ok(edited);
    }
    let merged = block.to_string();
    let mut out = outside.trim_end().to_owned();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(FENCE_START);
    out.push('\n');
    out.push_str(merged.trim_end());
    out.push('\n');
    out.push_str(FENCE_END);
    out.push('\n');
    out.parse::<DocumentMut>()
        .context("validating edited Cargo config")?;
    Ok(out)
}

fn fence_existing_build_keys(text: &str, keys: &[&str]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(build_start) = lines.iter().position(|line| {
        line.trim()
            .strip_prefix('[')
            .and_then(|table| table.strip_suffix(']'))
            .is_some_and(|table| table.trim() == "build")
    }) else {
        return text.to_owned();
    };
    let build_end = lines
        .iter()
        .enumerate()
        .skip(build_start + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    let is_managed = |line: &str| {
        let trimmed = line.trim_start();
        keys.iter().any(|key| {
            trimmed.starts_with(&format!("{key} =")) || trimmed.starts_with(&format!("{key}="))
        })
    };
    let managed_lines: Vec<&str> = lines[build_start + 1..build_end]
        .iter()
        .copied()
        .filter(|line| is_managed(line))
        .collect();
    if managed_lines.is_empty() {
        return text.to_owned();
    }
    let mut output = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index == build_end {
            output.push_str(FENCE_START);
            output.push('\n');
            for managed in &managed_lines {
                output.push_str(managed);
                output.push('\n');
            }
            output.push_str(FENCE_END);
            output.push('\n');
        }
        if index < build_start + 1 || index >= build_end || !is_managed(line) {
            output.push_str(line);
            output.push('\n');
        }
    }
    if build_end == lines.len() {
        output.push_str(FENCE_START);
        output.push('\n');
        for managed in &managed_lines {
            output.push_str(managed);
            output.push('\n');
        }
        output.push_str(FENCE_END);
        output.push('\n');
    }
    output
}

/// Returns the file contents with the fence removed. Keys that `apply` merged into a
/// pre-existing `[build]` table are removed by name.
pub fn remove(text: &str) -> Result<String> {
    let (outside, inside) = split_fence(text)?;
    if inside.is_none() {
        // A path containing `.rgo` is not proof of ownership. In particular,
        // undo must never remove a value the user added after setup.
        return Ok(text.to_owned());
    }
    Ok(outside.trim_end().to_owned() + "\n")
}

/// Check the complete fenced block before a later setup or undo touches it.
/// The block is either a standalone `[build]` table or key lines inside an
/// existing table; in both cases only the values rgo recorded may be removed.
pub fn managed_fence_matches(
    text: &str,
    expected_build_dir: &str,
    expected_wrapper: Option<&str>,
) -> Result<bool> {
    let (_, Some(body)) = split_fence(text)? else {
        return Ok(false);
    };
    let fragment = body.trim();
    let parsed: DocumentMut = fragment
        .parse()
        .context("parsing rgo-managed Cargo settings")?;
    let with_build = if parsed.get("build").is_some() {
        parsed
    } else {
        format!("[build]\n{fragment}")
            .parse()
            .context("parsing rgo-managed Cargo build settings")?
    };
    if with_build.iter().count() != 1 {
        return Ok(false);
    }
    let Some(build) = with_build.get("build").and_then(Item::as_table) else {
        return Ok(false);
    };
    let expected_count = if expected_wrapper.is_some() { 2 } else { 1 };
    Ok(build.iter().count() == expected_count
        && build.get("build-dir").and_then(Item::as_str) == Some(expected_build_dir)
        && build.get("rustc-wrapper").and_then(Item::as_str) == expected_wrapper)
}

/// Restore a wrapper that setup temporarily moved behind the rgo wrapper.
///
/// The caller supplies the value captured before `apply`; this keeps undo reversible without
/// touching unrelated Cargo configuration.
pub fn restore_rustc_wrapper(text: &str, wrapper: &str) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("parsing cargo config")?;
    if doc.get("build").is_some_and(Item::is_table) {
        doc["build"]["rustc-wrapper"] = value(wrapper);
    } else {
        doc["build"] = toml_edit::table();
        doc["build"]["rustc-wrapper"] = value(wrapper);
    }
    Ok(doc.to_string())
}

/// (text with fence region removed, fence body if present)
fn split_fence(text: &str) -> Result<(String, Option<String>)> {
    let starts: Vec<_> = text.match_indices(FENCE_START).map(|(i, _)| i).collect();
    let ends: Vec<_> = text.match_indices(FENCE_END).map(|(i, _)| i).collect();
    if starts.is_empty() && ends.is_empty() {
        return Ok((text.to_owned(), None));
    }
    if starts.len() != 1 || ends.len() != 1 || ends[0] <= starts[0] {
        bail!("Cargo config contains incomplete or duplicate rgo fences; refusing to change it");
    }
    let start = starts[0];
    let end_start = ends[0];
    let end = end_start + FENCE_END.len();
    let body = text[start + FENCE_START.len()..end_start].to_owned();
    let mut outside = text[..start].to_owned();
    outside.push_str(text[end..].trim_start_matches('\n'));
    Ok((outside, Some(body)))
}

pub fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired() -> Desired {
        Desired {
            build_dir: "/home/u/.rgo/builds/{workspace-path-hash}".into(),
            rustc_wrapper: Some("/home/u/bin/rgo-rustc-wrapper".into()),
            rustc_workspace_wrapper: None,
        }
    }

    #[test]
    fn apply_to_empty_then_remove_roundtrips() {
        let a = apply("", &desired()).unwrap();
        assert!(
            a.contains(FENCE_START) && a.contains("[build]\nbuild-dir = "),
            "must be a real table, not inline:\n{a}"
        );
        assert_eq!(apply(&a, &desired()).unwrap(), a, "idempotent");
        assert_eq!(remove(&a).unwrap().trim(), "");
    }

    #[test]
    fn incomplete_or_duplicate_fences_are_not_changed() {
        for text in [
            format!("{FENCE_START}\n[build]\nbuild-dir = \"/tmp/x\"\n"),
            format!("{FENCE_END}\n"),
            format!("{FENCE_START}\n{FENCE_END}\n{FENCE_START}\n{FENCE_END}\n"),
        ] {
            assert!(inspect(&text).is_err());
            assert!(apply(&text, &desired()).is_err());
            assert!(remove(&text).is_err());
        }
    }

    #[test]
    fn fenced_values_must_still_be_exactly_the_owned_values() {
        let managed = apply("[build]\njobs = 2\n", &desired()).unwrap();
        assert!(
            managed_fence_matches(
                &managed,
                &desired().build_dir,
                desired().rustc_wrapper.as_deref()
            )
            .unwrap()
        );
        let with_extra = managed.replace(FENCE_END, &format!("jobs = 7\n{FENCE_END}"));
        assert!(
            !managed_fence_matches(
                &with_extra,
                &desired().build_dir,
                desired().rustc_wrapper.as_deref()
            )
            .unwrap()
        );
    }

    #[test]
    fn inline_and_dotted_build_tables_keep_user_settings_after_undo() {
        for input in [
            "build = { jobs = 2, rustflags = [\"-C\", \"debuginfo=1\"] }\n",
            "build.jobs = 2\nbuild.rustflags = [\"-C\", \"debuginfo=1\"]\n",
        ] {
            let applied =
                apply(input, &desired()).unwrap_or_else(|error| panic!("{input:?}: {error:#}"));
            let active: DocumentMut = applied.parse().unwrap();
            assert_eq!(active["build"]["jobs"].as_integer(), Some(2));
            assert_eq!(
                active["build"]["build-dir"].as_str(),
                Some(desired().build_dir.as_str())
            );
            let undone = remove(&applied).unwrap();
            let restored: DocumentMut = undone.parse().unwrap();
            assert_eq!(restored["build"]["jobs"].as_integer(), Some(2));
            assert!(restored["build"].get("build-dir").is_none());
            assert_eq!(
                restored["build"]["rustflags"][1].as_str(),
                Some("debuginfo=1")
            );
        }
    }

    #[test]
    fn preserves_user_content_and_merges_existing_build_table() {
        let user = "[net]\nretry = 5 # keep me\n\n[build]\njobs = 4\n";
        let a = apply(user, &desired()).unwrap();
        assert!(a.contains("retry = 5 # keep me"));
        assert!(a.contains("jobs = 4"));
        assert!(a.contains(FENCE_START) && a.contains(FENCE_END));
        assert_eq!(
            a.matches("[build]").count(),
            1,
            "must not emit two [build] tables"
        );
        let r = remove(&a).unwrap();
        assert!(r.contains("jobs = 4") && !r.contains("build-dir"));
    }

    #[test]
    fn existing_build_keys_are_fenced_without_swallowing_interleaved_user_keys() {
        let user = "[build]\njobs = 4\nbuild-dir = \"/old\"\nnet = \"keep\"\n";
        let applied = apply(user, &desired()).unwrap();
        let removed = remove(&applied).unwrap();
        assert!(removed.contains("jobs = 4"));
        assert!(removed.contains("net = \"keep\""));
    }

    #[test]
    fn inspect_reports_conflicts() {
        let i = inspect("[build]\ntarget-dir = \"/x\"\nrustc-wrapper = \"sccache\"\n").unwrap();
        assert_eq!(i.target_dir.as_deref(), Some("/x"));
        assert_eq!(i.rustc_wrapper.as_deref(), Some("sccache"));
        assert!(!i.has_fence);
    }

    #[test]
    fn undo_without_fence_preserves_user_paths_that_look_like_rgo() {
        let user = "[build]\nbuild-dir = \"/home/me/.rgo-custom\"\nrustc-wrapper = \"/opt/rgo-rustc-wrapper\"\n";
        assert_eq!(remove(user).unwrap(), user);
    }

    #[test]
    fn legacy_config_wins_when_both_files_exist() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            effective_home_config(home.path()),
            home.path().join("config.toml")
        );
        std::fs::write(home.path().join("config"), "[net]\noffline = true\n").unwrap();
        assert_eq!(
            effective_home_config(home.path()),
            home.path().join("config")
        );
    }

    #[test]
    fn malformed_build_configuration_fails_before_writing_invalid_config() {
        for config in ["build = \"not a table\"\n", "build = {\n"] {
            assert!(apply(config, &desired()).is_err(), "{config}");
        }
    }

    #[test]
    fn supervised_override_detection_accepts_only_known_non_build_dir_config() {
        assert!(!may_set_build_dir("[build]\njobs = 2\n").unwrap());
        assert!(may_set_build_dir("[build]\nbuild-dir = \"custom\"\n").unwrap());
        assert!(may_set_build_dir("build.build-dir = \"custom\"\n").unwrap());
        assert!(may_set_build_dir("include = [\"other.toml\"]\n").unwrap());
        assert!(may_set_build_dir("[build]\nbuild-dir = 42\n").unwrap());
    }
}
