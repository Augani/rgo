//! Fenced, reversible edits to `$CARGO_HOME/config.toml`.
//!
//! rgo owns exactly the region between the two fence comments and never touches anything
//! else. Because `toml_edit` preserves formatting, user content outside the fence is
//! byte-for-byte unchanged. Precedence is Cargo's: project `.cargo/config.toml`,
//! `CARGO_BUILD_BUILD_DIR`, `CARGO_TARGET_DIR` and `--target-dir` all override this.

use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, value};

pub const FENCE_START: &str =
    "# >>> rgo managed — do not edit inside this fence; `rgo setup --undo` removes it >>>";
pub const FENCE_END: &str = "# <<< rgo managed <<<";

/// What `rgo setup` wants Cargo to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub build_dir: String,
    /// `None` = leave the user's wrapper setting alone.
    pub rustc_workspace_wrapper: Option<String>,
}

/// Facts about the existing config that affect what setup may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    pub has_fence: bool,
    pub build_dir_outside_fence: Option<String>,
    pub target_dir: Option<String>,
    pub rustc_wrapper: Option<String>,
    pub rustc_workspace_wrapper_outside_fence: Option<String>,
}

pub fn inspect(text: &str) -> Result<Inspection> {
    let (outside, _inside) = split_fence(text);
    let doc: DocumentMut = outside
        .parse()
        .context("parsing cargo config outside rgo fence")?;
    let get = |k: &str| {
        doc.get("build")
            .and_then(|b| b.get(k))
            .and_then(Item::as_str)
            .map(str::to_owned)
    };
    Ok(Inspection {
        has_fence: text.contains(FENCE_START),
        build_dir_outside_fence: get("build-dir"),
        target_dir: get("target-dir"),
        rustc_wrapper: get("rustc-wrapper"),
        rustc_workspace_wrapper_outside_fence: get("rustc-workspace-wrapper"),
    })
}

/// Returns the new file contents with the rgo fence (re)written. Idempotent.
pub fn apply(text: &str, desired: &Desired) -> Result<String> {
    let (outside, _) = split_fence(text);
    let mut block = DocumentMut::new();
    block["build"] = toml_edit::table();
    block["build"]["build-dir"] = value(&desired.build_dir);
    if let Some(w) = &desired.rustc_workspace_wrapper {
        block["build"]["rustc-workspace-wrapper"] = value(w);
    }
    // Cargo merges duplicate `[build]` tables across files but NOT within one file, so if
    // the user already has `[build]`, we must add keys to it rather than emit a second table.
    let mut doc: DocumentMut = outside.parse().context("parsing cargo config")?;
    let merged = if doc.get("build").is_some_and(Item::is_table) {
        for (k, v) in block["build"].as_table().unwrap().iter() {
            doc["build"][k] = v.clone();
        }
        return Ok(doc.to_string());
    } else {
        block.to_string()
    };
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
    Ok(out)
}

/// Returns the file contents with the fence removed. Keys that `apply` merged into a
/// pre-existing `[build]` table are removed by name.
pub fn remove(text: &str) -> Result<String> {
    let (outside, inside) = split_fence(text);
    let mut doc: DocumentMut = outside.parse().context("parsing cargo config")?;
    if inside.is_none() {
        if let Some(b) = doc.get_mut("build").and_then(Item::as_table_mut) {
            for k in ["build-dir", "rustc-workspace-wrapper"] {
                if b.get(k).and_then(Item::as_str).is_some_and(looks_like_ours) {
                    b.remove(k);
                }
            }
            if b.is_empty() {
                doc.remove("build");
            }
        }
        return Ok(doc.to_string());
    }
    Ok(outside.trim_end().to_owned() + "\n")
}

fn looks_like_ours(v: &str) -> bool {
    v.contains(".rgo") || v.contains("rgo-rustc-wrapper")
}

/// (text with fence region removed, fence body if present)
fn split_fence(text: &str) -> (String, Option<String>) {
    let Some(start) = text.find(FENCE_START) else {
        return (text.to_owned(), None);
    };
    let Some(end_rel) = text[start..].find(FENCE_END) else {
        return (text.to_owned(), None);
    };
    let end = start + end_rel + FENCE_END.len();
    let body = text[start + FENCE_START.len()..start + end_rel].to_owned();
    let mut outside = text[..start].to_owned();
    outside.push_str(text[end..].trim_start_matches('\n'));
    (outside, Some(body))
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
    fn preserves_user_content_and_merges_existing_build_table() {
        let user = "[net]\nretry = 5 # keep me\n\n[build]\njobs = 4\n";
        let a = apply(user, &desired()).unwrap();
        assert!(a.contains("retry = 5 # keep me"));
        assert!(a.contains("jobs = 4"));
        assert_eq!(
            a.matches("[build]").count(),
            1,
            "must not emit two [build] tables"
        );
        let r = remove(&a).unwrap();
        assert!(r.contains("jobs = 4") && !r.contains("build-dir"));
    }

    #[test]
    fn inspect_reports_conflicts() {
        let i = inspect("[build]\ntarget-dir = \"/x\"\nrustc-wrapper = \"sccache\"\n").unwrap();
        assert_eq!(i.target_dir.as_deref(), Some("/x"));
        assert_eq!(i.rustc_wrapper.as_deref(), Some("sccache"));
        assert!(!i.has_fence);
    }
}
