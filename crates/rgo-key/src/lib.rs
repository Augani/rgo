//! Conservative rustc cache classification and deterministic artifact keys.
//!
//! This crate deliberately does not know about the daemon or the CAS.  It only answers the
//! question "can this invocation be cached safely?" and, when the answer is yes, produces a
//! stable key from inputs that are observable without modifying Cargo's build directory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use blake3::Hasher;
use serde::{Deserialize, Serialize};

pub const CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassReason {
    CacheDisabled,
    UnsupportedCrateType,
    UnsupportedEmit,
    Incremental,
    SaveTemps,
    UnstableFlag,
    ExternalExtern,
    NativeInput,
    BuildScriptOutput,
    SourceOutsideCache,
    MissingSource,
    MissingOutputDirectory,
    UnsafePath,
    CompilerIdentity,
    InnerWrapper,
    MissingCacheState,
}

impl std::fmt::Display for BypassReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::CacheDisabled => "cache_disabled",
            Self::UnsupportedCrateType => "unsupported_crate_type",
            Self::UnsupportedEmit => "unsupported_emit",
            Self::Incremental => "incremental",
            Self::SaveTemps => "save_temps",
            Self::UnstableFlag => "unstable_flag",
            Self::ExternalExtern => "external_extern",
            Self::NativeInput => "native_input",
            Self::BuildScriptOutput => "build_script_output",
            Self::SourceOutsideCache => "source_outside_cache",
            Self::MissingSource => "missing_source",
            Self::MissingOutputDirectory => "missing_output_directory",
            Self::UnsafePath => "unsafe_path",
            Self::CompilerIdentity => "compiler_identity",
            Self::InnerWrapper => "inner_wrapper",
            Self::MissingCacheState => "missing_cache_state",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedRoots {
    pub build_root: PathBuf,
    pub source_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpec {
    pub kind: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub key: ArtifactKey,
    pub source_root: PathBuf,
    pub source_digest: String,
    pub outputs: Vec<OutputSpec>,
    pub normalized_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Cacheable(Candidate),
    Bypass(BypassReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactKey(String);

impl ArtifactKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Classify a rustc invocation and compute its key when it is within the deliberately small v1
/// cacheability boundary.
pub fn classify(
    rustc: &Path,
    args: &[OsString],
    env: &[(OsString, OsString)],
    roots: &AllowedRoots,
) -> Classification {
    let crate_types = comma_values(args, "--crate-type");
    if crate_types.is_empty()
        || crate_types
            .iter()
            .any(|v| !matches!(v.as_str(), "lib" | "rlib"))
    {
        return Classification::Bypass(BypassReason::UnsupportedCrateType);
    }
    let emits = comma_values(args, "--emit");
    if emits.is_empty()
        || emits.iter().any(|v| {
            !matches!(
                v.split_once('=').map_or(v.as_str(), |(k, _)| k),
                "dep-info" | "metadata" | "link"
            )
        })
    {
        return Classification::Bypass(BypassReason::UnsupportedEmit);
    }

    let Some(source) = source_argument(args) else {
        return Classification::Bypass(BypassReason::MissingSource);
    };
    let source = PathBuf::from(source);
    let Ok(source) = fs::canonicalize(&source) else {
        return Classification::Bypass(BypassReason::MissingSource);
    };
    let Some(source_root) = package_root(&source, roots) else {
        return Classification::Bypass(BypassReason::SourceOutsideCache);
    };

    let mut extern_paths = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        if arg == "-Z" || arg.starts_with("-Z") {
            return Classification::Bypass(BypassReason::UnstableFlag);
        }
        if arg == "-C" {
            if let Some(value) = args.get(i + 1).map(|v| v.to_string_lossy().into_owned()) {
                if value.starts_with("incremental=") {
                    return Classification::Bypass(BypassReason::Incremental);
                }
                if value == "save-temps" || value.starts_with("save-temps=") {
                    return Classification::Bypass(BypassReason::SaveTemps);
                }
            }
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("-C") {
            if value == "incremental" || value.starts_with("incremental=") {
                return Classification::Bypass(BypassReason::Incremental);
            }
            if value == "save-temps" || value.starts_with("save-temps=") {
                return Classification::Bypass(BypassReason::SaveTemps);
            }
        }
        if arg == "--extern" {
            let Some(value) = args.get(i + 1).map(|v| v.to_string_lossy().into_owned()) else {
                return Classification::Bypass(BypassReason::ExternalExtern);
            };
            if !record_extern(&value, roots, &mut extern_paths) {
                return Classification::Bypass(BypassReason::ExternalExtern);
            }
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--extern=") {
            if !record_extern(value, roots, &mut extern_paths) {
                return Classification::Bypass(BypassReason::ExternalExtern);
            }
        }
        if arg == "-l" || arg.starts_with("-l") || arg == "-L" || arg.starts_with("-Lnative=") {
            return Classification::Bypass(BypassReason::NativeInput);
        }
        i += 1;
    }

    if env.iter().any(|(k, _)| k == "OUT_DIR") {
        return Classification::Bypass(BypassReason::BuildScriptOutput);
    }

    let source_digest = match digest_tree(&source_root) {
        Ok(digest) => digest,
        Err(_) => return Classification::Bypass(BypassReason::SourceOutsideCache),
    };
    let normalized_args = normalize_args(args, roots, &source_root);
    let env_digest = relevant_environment(env);
    let compiler_identity = match compiler_identity(rustc) {
        Ok(identity) => identity,
        Err(_) => return Classification::Bypass(BypassReason::CompilerIdentity),
    };
    let key = make_key(
        &compiler_identity,
        &normalized_args,
        &source_digest,
        &extern_paths,
        &env_digest,
        args,
    );
    let outputs = output_specs(args);
    if outputs.is_empty() {
        return Classification::Bypass(BypassReason::MissingOutputDirectory);
    }
    Classification::Cacheable(Candidate {
        key,
        source_root,
        source_digest,
        outputs,
        normalized_args,
    })
}

pub fn digest_tree(root: &Path) -> std::io::Result<String> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Hasher::new();
    for (relative, path) in files {
        let bytes = fs::read(path)?;
        hash_field(&mut hasher, relative.to_string_lossy().as_bytes());
        hash_field(&mut hasher, &bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn compiler_identity(rustc: &Path) -> std::io::Result<String> {
    let output = Command::new(rustc).arg("-vV").output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("rustc -vV failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn make_key(
    compiler_identity: &str,
    normalized_args: &[String],
    source_digest: &str,
    extern_paths: &BTreeMap<String, String>,
    env_digest: &[(String, String)],
    args: &[OsString],
) -> ArtifactKey {
    let mut hasher = Hasher::new();
    hash_field(
        &mut hasher,
        format!("rgo-cache-v{CACHE_SCHEMA_VERSION}").as_bytes(),
    );
    hash_field(&mut hasher, compiler_identity.as_bytes());
    for arg in normalized_args {
        hash_field(&mut hasher, arg.as_bytes());
    }
    hash_field(&mut hasher, source_digest.as_bytes());
    for (name, digest) in extern_paths {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, digest.as_bytes());
    }
    for (name, value_digest) in env_digest {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, value_digest.as_bytes());
    }
    if let Some(target) = target_triple(args) {
        hash_field(&mut hasher, target.as_bytes());
    }
    ArtifactKey(hasher.finalize().to_hex().to_string())
}

fn hash_field(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn collect_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
        } else if metadata.is_file() {
            out.push((path.strip_prefix(root).unwrap_or(&path).to_path_buf(), path));
        }
    }
    Ok(())
}

fn package_root(source: &Path, roots: &AllowedRoots) -> Option<PathBuf> {
    let allowed = roots.source_roots.iter().any(|root| {
        source.starts_with(root)
            || fs::canonicalize(root)
                .map(|canonical| source.starts_with(canonical))
                .unwrap_or(false)
    });
    if !allowed {
        return None;
    }
    let mut current = source.parent()?;
    loop {
        if current.join("Cargo.toml").is_file() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn source_argument(args: &[OsString]) -> Option<OsString> {
    args.iter()
        .rfind(|arg| {
            let value = arg.to_string_lossy();
            !value.starts_with('-') && value.ends_with(".rs")
        })
        .cloned()
}

fn comma_values(args: &[OsString], flag: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        let value = if arg == flag {
            i += 1;
            args.get(i).map(|v| v.to_string_lossy().into_owned())
        } else {
            arg.strip_prefix(&format!("{flag}=")).map(str::to_owned)
        };
        if let Some(value) = value {
            result.extend(value.split(',').map(str::to_owned));
        }
        i += 1;
    }
    result
}

fn record_extern(value: &str, roots: &AllowedRoots, output: &mut BTreeMap<String, String>) -> bool {
    let Some((name, path)) = value.split_once('=') else {
        return false;
    };
    let path = PathBuf::from(path);
    if !(path_under(&path, &roots.build_root) || is_sysroot_path(&path)) || !path.is_file() {
        return false;
    }
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    output.insert(name.to_owned(), blake3::hash(&bytes).to_hex().to_string());
    true
}

fn path_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        || fs::canonicalize(root)
            .map(|canonical| path.starts_with(canonical))
            .unwrap_or(false)
}

fn is_sysroot_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "rustlib")
}

fn normalize_args(args: &[OsString], roots: &AllowedRoots, source_root: &Path) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut value = arg.to_string_lossy().into_owned();
            let build = roots.build_root.to_string_lossy();
            let source = source_root.to_string_lossy();
            value = normalize_path_token(&value, build.as_ref(), "<BUILD_DIR>");
            normalize_path_token(&value, source.as_ref(), "<SOURCE_ROOT>")
        })
        .collect()
}

fn normalize_path_token(value: &str, root: &str, replacement: &str) -> String {
    if value == root
        || value
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
    {
        return replacement.to_owned();
    }
    if let Some(index) = value.find(root) {
        let prefix = &value[..index];
        if prefix.ends_with('=') || prefix.ends_with(':') {
            return format!("{prefix}{replacement}");
        }
    }
    value.to_owned()
}

fn relevant_environment(env: &[(OsString, OsString)]) -> Vec<(String, String)> {
    let mut values = env
        .iter()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy();
            let relevant = key.starts_with("CARGO_PKG_")
                || key.starts_with("CARGO_CFG_")
                || key == "CARGO_CRATE_NAME"
                || key == "CARGO_MANIFEST_DIR"
                || key == "RUSTUP_TOOLCHAIN";
            relevant.then(|| {
                (
                    key.into_owned(),
                    blake3::hash(value.to_string_lossy().as_bytes())
                        .to_hex()
                        .to_string(),
                )
            })
        })
        .collect::<Vec<_>>();
    values.sort();
    values
}

fn target_triple(args: &[OsString]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let value = args[i].to_string_lossy();
        if value == "--target" {
            return args.get(i + 1).map(|v| v.to_string_lossy().into_owned());
        }
        if let Some(target) = value.strip_prefix("--target=") {
            return Some(target.to_owned());
        }
        i += 1;
    }
    None
}

fn output_specs(args: &[OsString]) -> Vec<OutputSpec> {
    let out_dir = arg_value(args, "--out-dir").map(PathBuf::from);
    let mut specs = Vec::new();
    for emit in comma_values(args, "--emit") {
        let (kind, explicit) = emit
            .split_once('=')
            .map_or((emit.as_str(), None), |(k, v)| (k, Some(v)));
        let path = explicit.map(PathBuf::from).or_else(|| out_dir.clone());
        if let Some(path) = path {
            specs.push(OutputSpec {
                kind: kind.to_owned(),
                path,
            });
        }
    }
    specs
}

fn arg_value(args: &[OsString], flag: &str) -> Option<OsString> {
    let mut i = 0;
    while i < args.len() {
        let value = args[i].to_string_lossy();
        if value == flag {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = value.strip_prefix(&format!("{flag}=")) {
            return Some(value.into());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("registry/src/index/demo-1.0.0");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        let source_file = source.join("src/lib.rs");
        let mut file = fs::File::create(&source_file).unwrap();
        writeln!(file, "pub fn demo() {{}}").unwrap();
        (dir, source, source_file)
    }

    #[test]
    fn source_digest_is_order_independent() {
        let (_dir, root, _source) = fixture();
        let first = digest_tree(&root).unwrap();
        fs::write(root.join("z"), "z").unwrap();
        fs::write(root.join("a"), "a").unwrap();
        let second = digest_tree(&root).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, digest_tree(&root).unwrap());
    }

    #[test]
    fn rejects_unsafe_invocation() {
        let (_dir, root, source) = fixture();
        let args = vec![
            "--crate-name".into(),
            "demo".into(),
            "--crate-type=bin".into(),
            "--emit=link".into(),
            source.into_os_string(),
        ];
        let result = classify(
            Path::new("rustc"),
            &args,
            &[],
            &AllowedRoots {
                build_root: root.clone(),
                source_roots: vec![root.parent().unwrap().to_path_buf()],
            },
        );
        assert_eq!(
            result,
            Classification::Bypass(BypassReason::UnsupportedCrateType)
        );
    }
}
