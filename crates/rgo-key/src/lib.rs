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

pub const CACHE_SCHEMA_VERSION: u32 = 3;
pub const WORKSPACE_REMAP_PREFIX: &str = "/rgo/workspace";

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
    RemapConflict,
    UnsafeWorkspacePath,
    UnsupportedWorkspaceSource,
    InvalidOutDir,
    UnsupportedEncoding,
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
            Self::RemapConflict => "remap_conflict",
            Self::UnsafeWorkspacePath => "unsafe_workspace_path",
            Self::UnsupportedWorkspaceSource => "unsupported_workspace_source",
            Self::InvalidOutDir => "invalid_out_dir",
            Self::UnsupportedEncoding => "unsupported_encoding",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedRoots {
    pub build_root: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub workspace_roots: Vec<PathBuf>,
    pub remap_workspace_paths: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Registry,
    Git,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpec {
    pub kind: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub key: ArtifactKey,
    pub source_kind: SourceKind,
    pub source_root: PathBuf,
    pub source_digest: String,
    pub crate_name: String,
    pub extra_filename: Option<String>,
    pub outputs: Vec<OutputSpec>,
    pub normalized_args: Vec<String>,
    pub compiler_args: Vec<OsString>,
    pub remap_path_prefix: Option<(String, String)>,
    pub out_dir_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Cacheable(Box<Candidate>),
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
    // Lossy conversion would let distinct compiler inputs share a key. A cache miss is
    // cheaper than guessing at the meaning of platform-native strings.
    if args.iter().any(|arg| arg.to_str().is_none())
        || env
            .iter()
            .any(|(key, value)| key.to_str().is_none() || value.to_str().is_none())
    {
        return Classification::Bypass(BypassReason::UnsupportedEncoding);
    }
    let crate_types = comma_values(args, "--crate-type");
    if crate_types.is_empty() {
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
    if fs::symlink_metadata(&source)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Classification::Bypass(BypassReason::UnsafePath);
    }
    let Ok(source) = fs::canonicalize(&source) else {
        return Classification::Bypass(BypassReason::MissingSource);
    };
    let Some((source_root, source_kind)) = package_root(&source, roots) else {
        return Classification::Bypass(BypassReason::SourceOutsideCache);
    };

    let has_link = emits.iter().any(|value| {
        value
            .split_once('=')
            .map_or(value.as_str(), |(kind, _)| kind)
            == "link"
    });
    let is_workspace = source_kind == SourceKind::Workspace;
    let is_bin = crate_types.iter().any(|value| value == "bin");
    let is_proc_macro = crate_types.iter().any(|value| value == "proc-macro");
    let supported_type = crate_types
        .iter()
        .all(|value| matches!(value.as_str(), "lib" | "rlib"))
        || (is_proc_macro
            && crate_types.len() == 1
            && !crate_types.iter().any(|value| value != "proc-macro"))
        || (is_bin
            && crate_types.len() == 1
            && !has_link
            && emits.iter().all(|value| {
                value
                    .split_once('=')
                    .map_or(value.as_str(), |(kind, _)| kind)
                    == "metadata"
                    || value.starts_with("dep-info")
            }));
    if !supported_type {
        return Classification::Bypass(if is_workspace {
            BypassReason::UnsupportedWorkspaceSource
        } else {
            BypassReason::UnsupportedCrateType
        });
    }

    let mut extern_paths = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        if arg == "-Z" {
            match args.get(i + 1).map(|v| v.to_string_lossy()) {
                Some(value) if allowed_z_option(&value) => i += 2,
                _ => return Classification::Bypass(BypassReason::UnstableFlag),
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("-Z") {
            if !allowed_z_option(value) {
                return Classification::Bypass(BypassReason::UnstableFlag);
            }
        }
        if arg == "-C" {
            if let Some(value) = args.get(i + 1).map(|v| v.to_string_lossy().into_owned()) {
                if let Some(reason) = unsafe_codegen_option(&value) {
                    return Classification::Bypass(reason);
                }
            }
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("-C") {
            if let Some(reason) = unsafe_codegen_option(value) {
                return Classification::Bypass(reason);
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
        if arg == "-l"
            || arg.starts_with("-l")
            || arg.starts_with("-Lnative=")
            || arg.starts_with("-Clinker=")
            || arg.starts_with("-Clink-arg=")
            || arg.starts_with("-Clink-self-contained")
        {
            return Classification::Bypass(BypassReason::NativeInput);
        }
        if arg == "-L" {
            let Some(value) = args.get(i + 1).map(|v| v.to_string_lossy().into_owned()) else {
                return Classification::Bypass(BypassReason::NativeInput);
            };
            let path = value
                .split_once('=')
                .map_or(value.as_str(), |(_, path)| path);
            if value.starts_with("native=") {
                return Classification::Bypass(BypassReason::NativeInput);
            }
            if !path_under(Path::new(path), &roots.build_root) && !is_sysroot_path(Path::new(path))
            {
                return Classification::Bypass(BypassReason::ExternalExtern);
            }
            i += 2;
            continue;
        }
        i += 1;
    }

    let out_dir = env
        .iter()
        .find(|(key, _)| key == "OUT_DIR")
        .map(|(_, value)| PathBuf::from(value));
    let out_dir_digest = if let Some(out_dir) = &out_dir {
        if !path_under(out_dir, &roots.build_root) {
            return Classification::Bypass(BypassReason::BuildScriptOutput);
        }
        match digest_tree_strict(out_dir) {
            Ok(digest) => Some(digest),
            Err(_) => return Classification::Bypass(BypassReason::InvalidOutDir),
        }
    } else {
        None
    };

    let existing_remap = remap_path_prefixes(args);
    let remap_path_prefix = if is_workspace {
        let covering = existing_remap
            .iter()
            .filter(|(from, _)| path_under(&source_root, Path::new(from)))
            .collect::<Vec<_>>();
        if covering.len() > 1 {
            return Classification::Bypass(BypassReason::RemapConflict);
        }
        if covering.is_empty() && !existing_remap.is_empty() && roots.remap_workspace_paths {
            return Classification::Bypass(BypassReason::RemapConflict);
        }
        match covering.first() {
            Some((from, to)) => Some(((*from).clone(), (*to).clone())),
            None if roots.remap_workspace_paths => Some((
                source_root.to_string_lossy().into_owned(),
                WORKSPACE_REMAP_PREFIX.to_owned(),
            )),
            None => None,
        }
    } else {
        None
    };

    let source_digest = match digest_tree_strict(&source_root) {
        Ok(digest) => digest,
        Err(_) => return Classification::Bypass(BypassReason::SourceOutsideCache),
    };
    let mut compiler_args = args.to_vec();
    if let Some((from, to)) = &remap_path_prefix {
        if !existing_remap
            .iter()
            .any(|entry| entry == &(from.clone(), to.clone()))
        {
            compiler_args.push(format!("--remap-path-prefix={from}={to}").into());
        }
    }
    let normalized_args = normalize_args(
        &compiler_args,
        roots,
        &source_root,
        source_kind,
        remap_path_prefix.as_ref(),
        out_dir.as_deref(),
    );
    let env_digest = relevant_environment(env, out_dir_digest.as_deref());
    let compiler_identity = match compiler_identity(rustc) {
        Ok(identity) => identity,
        Err(_) => return Classification::Bypass(BypassReason::CompilerIdentity),
    };
    let key = make_key(KeyInputs {
        compiler_identity: &compiler_identity,
        normalized_args: &normalized_args,
        source_digest: &source_digest,
        extern_paths: &extern_paths,
        env_digest: &env_digest,
        args: &compiler_args,
        source_kind,
        remap_path_prefix: remap_path_prefix.as_ref(),
        out_dir_digest: out_dir_digest.as_deref(),
    });
    let outputs = output_specs(&compiler_args);
    if outputs.is_empty() {
        return Classification::Bypass(BypassReason::MissingOutputDirectory);
    }
    let Some(crate_name) = arg_value(args, "--crate-name")
        .map(|value| value.to_string_lossy().into_owned())
        .or_else(|| {
            source
                .file_stem()
                .map(|value| value.to_string_lossy().into_owned())
        })
    else {
        return Classification::Bypass(BypassReason::MissingSource);
    };
    let extra_filename = codegen_value(args, "extra-filename");
    Classification::Cacheable(Box::new(Candidate {
        key,
        source_kind,
        source_root,
        source_digest,
        crate_name,
        extra_filename,
        outputs,
        normalized_args,
        compiler_args,
        remap_path_prefix,
        out_dir_digest,
    }))
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

fn digest_tree_strict(root: &Path) -> std::io::Result<String> {
    let mut files = Vec::new();
    collect_files_strict(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Hasher::new();
    for (relative, path) in files {
        hash_field(&mut hasher, relative.to_string_lossy().as_bytes());
        hash_field(&mut hasher, &fs::read(path)?);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn unsafe_codegen_option(value: &str) -> Option<BypassReason> {
    let option = value.split_once('=').map_or(value, |(name, _)| name);
    match option {
        "incremental" => Some(BypassReason::Incremental),
        "save-temps" => Some(BypassReason::SaveTemps),
        "linker" | "link-arg" | "link-args" | "link-self-contained" => {
            Some(BypassReason::NativeInput)
        }
        _ => None,
    }
}

pub fn compiler_identity(rustc: &Path) -> std::io::Result<String> {
    let output = Command::new(rustc).arg("-vV").output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("rustc -vV failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct KeyInputs<'a> {
    compiler_identity: &'a str,
    normalized_args: &'a [String],
    source_digest: &'a str,
    extern_paths: &'a BTreeMap<String, String>,
    env_digest: &'a [(String, String)],
    args: &'a [OsString],
    source_kind: SourceKind,
    remap_path_prefix: Option<&'a (String, String)>,
    out_dir_digest: Option<&'a str>,
}

fn make_key(input: KeyInputs<'_>) -> ArtifactKey {
    let mut hasher = Hasher::new();
    hash_field(
        &mut hasher,
        format!("rgo-cache-v{CACHE_SCHEMA_VERSION}").as_bytes(),
    );
    hash_field(&mut hasher, input.compiler_identity.as_bytes());
    for arg in input.normalized_args {
        hash_field(&mut hasher, arg.as_bytes());
    }
    hash_field(&mut hasher, input.source_digest.as_bytes());
    hash_field(
        &mut hasher,
        format!("source-kind:{:?}", input.source_kind).as_bytes(),
    );
    for (name, digest) in input.extern_paths {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, digest.as_bytes());
    }
    for (name, value_digest) in input.env_digest {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, value_digest.as_bytes());
    }
    if let Some(target) = target_triple(input.args) {
        hash_field(&mut hasher, target.as_bytes());
    }
    if let Some((from, to)) = input.remap_path_prefix {
        hash_field(&mut hasher, b"remap-from");
        if input.source_kind == SourceKind::Workspace {
            // The source side is intentionally omitted for workspace members: the compiler
            // receives the real checkout root, but the generated artifact contains `to`.
            hash_field(&mut hasher, b"workspace-root");
        } else {
            hash_field(&mut hasher, from.as_bytes());
        }
        hash_field(&mut hasher, b"remap-to");
        hash_field(&mut hasher, to.as_bytes());
    } else {
        hash_field(&mut hasher, b"remap-disabled");
    }
    if let Some(digest) = input.out_dir_digest {
        hash_field(&mut hasher, b"out-dir");
        hash_field(&mut hasher, digest.as_bytes());
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

fn collect_files_strict(
    root: &Path,
    current: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "symlinks are not cacheable",
            ));
        }
        if metadata.is_dir() {
            collect_files_strict(root, &path, out)?;
        } else if metadata.is_file() {
            out.push((path.strip_prefix(root).unwrap_or(&path).to_path_buf(), path));
        }
    }
    Ok(())
}

fn package_root(source: &Path, roots: &AllowedRoots) -> Option<(PathBuf, SourceKind)> {
    let source_kind = if roots.workspace_roots.iter().any(|root| {
        source.starts_with(root)
            || fs::canonicalize(root)
                .map(|canonical| source.starts_with(canonical))
                .unwrap_or(false)
    }) {
        SourceKind::Workspace
    } else if roots.source_roots.iter().any(|root| {
        source.starts_with(root)
            || fs::canonicalize(root)
                .map(|canonical| source.starts_with(canonical))
                .unwrap_or(false)
    }) {
        if source
            .components()
            .any(|component| component.as_os_str() == "checkouts")
        {
            SourceKind::Git
        } else {
            SourceKind::Registry
        }
    } else {
        return None;
    };
    let mut current = source.parent()?;
    loop {
        if current.join("Cargo.toml").is_file() {
            return Some((current.to_path_buf(), source_kind));
        }
        current = current.parent()?;
    }
}

fn remap_path_prefixes(args: &[OsString]) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let value = args[index].to_string_lossy();
        let mapping = if value == "--remap-path-prefix" {
            args.get(index + 1)
                .map(|value| value.to_string_lossy().into_owned())
        } else {
            value
                .strip_prefix("--remap-path-prefix=")
                .map(str::to_owned)
        };
        if let Some(mapping) = mapping {
            if let Some((from, to)) = mapping.split_once('=') {
                result.push((from.to_owned(), to.to_owned()));
            }
        }
        index += if value == "--remap-path-prefix" { 2 } else { 1 };
    }
    result
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
        // Cargo passes `--extern proc_macro` without a path for proc-macro crates; the
        // crate resolves from the sysroot and is already pinned by compiler_identity.
        if value == "proc_macro" {
            output.insert(value.to_owned(), "sysroot".to_owned());
            return true;
        }
        return false;
    };
    let path = PathBuf::from(path);
    if !(path_under(&path, &roots.build_root) || is_sysroot_path(&path)) || !path.is_file() {
        return false;
    }
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    // Newer cargo passes the same crate twice (`.rlib` and `.rmeta` externs);
    // keying by name alone would silently drop one artifact's digest.
    let label = path
        .file_name()
        .map(|file| format!("{name}={}", file.to_string_lossy()))
        .unwrap_or_else(|| name.to_owned());
    output.insert(label, blake3::hash(&bytes).to_hex().to_string());
    true
}

/// `-Z` options cargo passes by default on nightly toolchains. An allowlisted
/// flag is normalized into the key like any other argument; every other `-Z`
/// stays a bypass because unstable flags can alter codegen in ways the key
/// cannot model.
fn allowed_z_option(value: &str) -> bool {
    matches!(value.split('=').next(), Some("embed-metadata"))
}

fn path_under(path: &Path, root: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    let canonical_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canonical_path.starts_with(canonical_root)
}

fn is_sysroot_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "rustlib")
}

fn normalize_args(
    args: &[OsString],
    roots: &AllowedRoots,
    source_root: &Path,
    source_kind: SourceKind,
    remap_path_prefix: Option<&(String, String)>,
    out_dir: Option<&Path>,
) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut value = arg.to_string_lossy().into_owned();
            let build = roots.build_root.to_string_lossy();
            let source = source_root.to_string_lossy();
            value = normalize_path_token(&value, build.as_ref(), "<BUILD_DIR>");
            if source_kind != SourceKind::Workspace || remap_path_prefix.is_some() {
                value = normalize_path_token(&value, source.as_ref(), "<SOURCE_ROOT>");
            }
            if let Some(out_dir) = out_dir {
                value = normalize_path_token(&value, &out_dir.to_string_lossy(), "<OUT_DIR>");
            }
            value
        })
        .collect()
}

fn normalize_path_token(value: &str, root: &str, replacement: &str) -> String {
    let mut roots = vec![root.to_owned()];
    if let Some(without_private) = root.strip_prefix("/private/") {
        roots.push(format!("/{without_private}"));
    }
    for root in roots {
        if let Some(rest) = value
            .strip_prefix(&root)
            .filter(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
        {
            return format!("{replacement}{rest}");
        }
        if let Some(index) = value.find(&root) {
            let prefix = &value[..index];
            let rest = &value[index + root.len()..];
            if (prefix.ends_with('=') || prefix.ends_with(':'))
                && (rest.is_empty()
                    || rest.starts_with('/')
                    || rest.starts_with('\\')
                    || rest.starts_with('='))
            {
                return format!("{prefix}{replacement}{rest}");
            }
        }
    }
    value.to_owned()
}

fn relevant_environment(
    env: &[(OsString, OsString)],
    out_dir_digest: Option<&str>,
) -> Vec<(String, String)> {
    let mut values = env
        .iter()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                blake3::hash(value.to_string_lossy().as_bytes())
                    .to_hex()
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    if let Some(digest) = out_dir_digest {
        values.push(("OUT_DIR_CONTENTS".into(), digest.into()));
    }
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

fn codegen_value(args: &[OsString], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let mut i = 0;
    while i < args.len() {
        let value = args[i].to_string_lossy();
        if value == "-C" {
            i += 1;
            continue;
        }
        let option = value.strip_prefix("-C").unwrap_or(&value);
        if let Some(rest) = option.strip_prefix(&prefix) {
            return Some(rest.to_owned());
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
                workspace_roots: Vec::new(),
                remap_workspace_paths: false,
            },
        );
        assert_eq!(
            result,
            Classification::Bypass(BypassReason::UnsupportedCrateType)
        );
    }

    #[test]
    fn workspace_remapping_preserves_environment_dependent_paths() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first/member");
        let second = dir.path().join("second/member");
        for root in [&first, &second] {
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("Cargo.toml"), "[package]\nname='member'\n").unwrap();
            fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
        }
        let args = |root: &Path, build: &Path| {
            vec![
                "--crate-name".into(),
                "member".into(),
                "--crate-type=lib".into(),
                "--emit=metadata".into(),
                "--out-dir".into(),
                build.as_os_str().to_owned(),
                root.join("src/lib.rs").into_os_string(),
            ]
        };
        let env = |root: &Path| {
            vec![(
                OsString::from("CARGO_MANIFEST_DIR"),
                root.as_os_str().to_owned(),
            )]
        };
        let classify_with = |root: &Path, remap| {
            classify(
                Path::new("rustc"),
                &args(root, &dir.path().join("build")),
                &env(root),
                &AllowedRoots {
                    build_root: dir.path().join("build"),
                    source_roots: Vec::new(),
                    workspace_roots: vec![root.parent().unwrap().to_path_buf()],
                    remap_workspace_paths: remap,
                },
            )
        };
        let first_without = match classify_with(&first, false) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        let second_without = match classify_with(&second, false) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        assert_ne!(first_without.key, second_without.key);

        let first_with = match classify_with(&first, true) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        let second_with = match classify_with(&second, true) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        // rustc can embed CARGO_MANIFEST_DIR with env! even when source paths are
        // remapped. Different values must therefore produce different cache keys.
        assert_ne!(first_with.key, second_with.key);
        assert!(
            first_with
                .compiler_args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("--remap-path-prefix="))
        );
    }

    #[test]
    fn arbitrary_environment_changes_and_missing_values_change_keys() {
        let (_dir, root, source) = fixture();
        let args = vec![
            "--crate-name".into(),
            "demo".into(),
            "--crate-type=lib".into(),
            "--emit=metadata".into(),
            "--out-dir".into(),
            root.as_os_str().to_owned(),
            source.into_os_string(),
        ];
        let roots = AllowedRoots {
            build_root: root.clone(),
            source_roots: vec![root.parent().unwrap().to_path_buf()],
            workspace_roots: Vec::new(),
            remap_workspace_paths: false,
        };
        let key =
            |env: &[(OsString, OsString)]| match classify(Path::new("rustc"), &args, env, &roots) {
                Classification::Cacheable(candidate) => candidate.key,
                other => panic!("expected cacheable invocation: {other:?}"),
            };
        let absent = key(&[]);
        let alpha = key(&[("APP_BUILD_FLAVOR".into(), "alpha".into())]);
        let beta = key(&[("APP_BUILD_FLAVOR".into(), "beta".into())]);
        assert_ne!(absent, alpha);
        assert_ne!(alpha, beta);
    }

    #[test]
    fn normalized_paths_keep_their_relative_suffixes() {
        assert_eq!(
            normalize_path_token("/work/a/src/lib.rs", "/work/a", "<SRC>"),
            "<SRC>/src/lib.rs"
        );
        assert_eq!(
            normalize_path_token("/work/a/src/other.rs", "/work/a", "<SRC>"),
            "<SRC>/src/other.rs"
        );
        assert_eq!(
            normalize_path_token("--extern=dep=/work/a/libdep.rlib", "/work/a", "<SRC>"),
            "--extern=dep=<SRC>/libdep.rlib"
        );
    }

    #[test]
    fn widened_workspace_classes_require_safe_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace/member");
        let build = dir.path().join("build");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&build).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='member'\n").unwrap();
        let source = root.join("src/lib.rs");
        fs::write(&source, "pub fn value() -> u32 { 1 }\n").unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: Vec::new(),
            workspace_roots: vec![root.parent().unwrap().to_path_buf()],
            remap_workspace_paths: false,
        };
        let classify_type = |crate_type: &str, emit: &str, env: &[(OsString, OsString)]| {
            classify(
                Path::new("rustc"),
                &[
                    "--crate-name".into(),
                    "member".into(),
                    format!("--crate-type={crate_type}").into(),
                    format!("--emit={emit}").into(),
                    "--out-dir".into(),
                    build.as_os_str().to_owned(),
                    source.as_os_str().to_owned(),
                ],
                env,
                &roots,
            )
        };
        assert!(matches!(
            classify_type("bin", "metadata", &[]),
            Classification::Cacheable(_)
        ));
        assert!(matches!(
            classify_type("proc-macro", "metadata,link", &[]),
            Classification::Cacheable(_)
        ));

        let out_dir = build.join("out");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(
            out_dir.join("generated.rs"),
            "pub const GENERATED: u8 = 1;\n",
        )
        .unwrap();
        let env = vec![(OsString::from("OUT_DIR"), out_dir.into_os_string())];
        assert!(matches!(
            classify_type("lib", "metadata", &env),
            Classification::Cacheable(_)
        ));
    }

    #[test]
    fn bare_proc_macro_extern_is_cacheable_and_names_the_invocation() {
        let (_dir, root, source) = fixture();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: vec![root.parent().unwrap().to_path_buf()],
            workspace_roots: Vec::new(),
            remap_workspace_paths: false,
        };
        let classify_with = |extra: &[&str]| {
            let mut args = vec![
                "--crate-name".into(),
                "demo".into(),
                "--crate-type=proc-macro".into(),
                "--emit=dep-info,link".into(),
                "-C".into(),
                "extra-filename=-abc123".into(),
                "--out-dir".into(),
                build.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ];
            args.extend(extra.iter().map(OsString::from));
            classify(Path::new("rustc"), &args, &[], &roots)
        };
        let candidate = match classify_with(&["--extern", "proc_macro"]) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        assert_eq!(candidate.crate_name, "demo");
        assert_eq!(candidate.extra_filename.as_deref(), Some("-abc123"));
        assert!(matches!(
            classify_with(&["--extern", "helper"]),
            Classification::Bypass(BypassReason::ExternalExtern)
        ));
    }

    #[test]
    fn allowlisted_z_option_is_keyed_and_others_still_bypass() {
        let (_dir, root, source) = fixture();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: vec![root.parent().unwrap().to_path_buf()],
            workspace_roots: Vec::new(),
            remap_workspace_paths: false,
        };
        let classify_with = |extra: &[&str]| {
            let mut args = vec![
                "--crate-name".into(),
                "demo".into(),
                "--crate-type=lib".into(),
                "--emit=dep-info,metadata,link".into(),
                "--out-dir".into(),
                build.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ];
            args.extend(extra.iter().map(OsString::from));
            classify(Path::new("rustc"), &args, &[], &roots)
        };
        // Nightly cargo passes `-Z embed-metadata=no` by default; it is keyed
        // like any other argument rather than bypassing the cache.
        let keyed = match classify_with(&["-Z", "embed-metadata=no"]) {
            Classification::Cacheable(candidate) => candidate.key,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        let without_flag = match classify_with(&[]) {
            Classification::Cacheable(candidate) => candidate.key,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        assert_ne!(keyed, without_flag);
        assert!(matches!(
            classify_with(&["-Zembed-metadata=no"]),
            Classification::Cacheable(_)
        ));
        // Everything else under -Z remains unstable and bypassed.
        for args in [vec!["-Z", "time-passes"], vec!["-Ztime-passes"]] {
            assert!(matches!(
                classify_with(&args),
                Classification::Bypass(BypassReason::UnstableFlag)
            ));
        }
    }

    #[test]
    fn duplicate_extern_names_keep_each_artifact_digest() {
        let (_dir, root, source) = fixture();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        // Nightly cargo passes the same crate twice: once for its `.rlib` and
        // once for its `.rmeta`.
        let rlib = build.join("libdep-abc.rlib");
        let rmeta = build.join("libdep-abc.rmeta");
        fs::write(&rlib, b"rlib-v1").unwrap();
        fs::write(&rmeta, b"rmeta").unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: vec![root.parent().unwrap().to_path_buf()],
            workspace_roots: Vec::new(),
            remap_workspace_paths: false,
        };
        let key_now = || {
            let args: Vec<OsString> = vec![
                "--crate-name".into(),
                "demo".into(),
                "--crate-type=lib".into(),
                "--emit=dep-info,metadata,link".into(),
                "--out-dir".into(),
                build.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
                "--extern".into(),
                format!("dep={}", rlib.display()).into(),
                "--extern".into(),
                format!("dep={}", rmeta.display()).into(),
            ];
            match classify(Path::new("rustc"), &args, &[], &roots) {
                Classification::Cacheable(candidate) => candidate.key,
                other => panic!("expected cacheable candidate, got {other:?}"),
            }
        };
        let before = key_now();
        fs::write(&rlib, b"rlib-v2").unwrap();
        assert_ne!(before, key_now());
    }

    #[test]
    fn crate_name_falls_back_to_source_file_stem() {
        let (_dir, root, source) = fixture();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: vec![root.parent().unwrap().to_path_buf()],
            workspace_roots: Vec::new(),
            remap_workspace_paths: false,
        };
        let candidate = match classify(
            Path::new("rustc"),
            &[
                "--crate-type=lib".into(),
                "--emit=metadata,link".into(),
                "-Cextra-filename=-deadbeef".into(),
                "--out-dir".into(),
                build.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ],
            &[],
            &roots,
        ) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected cacheable candidate, got {other:?}"),
        };
        // rustc's own default derives the crate name from the source file stem.
        assert_eq!(candidate.crate_name, "lib");
        assert_eq!(candidate.extra_filename.as_deref(), Some("-deadbeef"));
    }

    #[test]
    fn classifier_has_stable_reasons_for_unsafe_inputs() {
        let (_dir, root, source) = fixture();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: vec![root.parent().unwrap().to_path_buf()],
            workspace_roots: Vec::new(),
            remap_workspace_paths: false,
        };
        let base = || {
            vec![
                "--crate-name".into(),
                "demo".into(),
                "--crate-type=lib".into(),
                "--emit=metadata,link".into(),
                "--out-dir".into(),
                build.as_os_str().to_owned(),
                source.as_os_str().to_owned(),
            ]
        };
        let cases = [
            (vec!["--crate-type=bin"], BypassReason::UnsupportedCrateType),
            (vec!["--emit=asm"], BypassReason::UnsupportedEmit),
            (vec!["-C", "incremental=cache"], BypassReason::Incremental),
            (vec!["-C", "save-temps"], BypassReason::SaveTemps),
            (vec!["-Zunstable-options"], BypassReason::UnstableFlag),
            (vec!["-lfoo"], BypassReason::NativeInput),
            (vec!["-C", "link-arg=-nostdlib"], BypassReason::NativeInput),
            (vec!["-L", "native=cache"], BypassReason::NativeInput),
            (vec!["-L", "/outside"], BypassReason::ExternalExtern),
            (
                vec!["--extern=other=/outside/libother.rlib"],
                BypassReason::ExternalExtern,
            ),
        ];
        for (extra, expected) in cases {
            let mut args = base();
            for value in extra {
                args.push(value.into());
            }
            assert_eq!(
                classify(Path::new("rustc"), &args, &[], &roots),
                Classification::Bypass(expected)
            );
        }

        let outside = tempfile::tempdir().unwrap();
        let out_dir = outside.path().join("out");
        fs::create_dir_all(&out_dir).unwrap();
        let env = vec![(OsString::from("OUT_DIR"), out_dir.into_os_string())];
        assert_eq!(
            classify(Path::new("rustc"), &base(), &env, &roots),
            Classification::Bypass(BypassReason::BuildScriptOutput)
        );

        #[cfg(unix)]
        {
            let link = root.join("symlink.rs");
            std::os::unix::fs::symlink(&source, &link).unwrap();
            let mut args = base();
            *args.last_mut().unwrap() = link.into_os_string();
            assert_eq!(
                classify(Path::new("rustc"), &args, &[], &roots),
                Classification::Bypass(BypassReason::UnsafePath)
            );
        }
    }

    #[test]
    fn user_remap_is_honored_and_conflicts_are_bypassed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let member = workspace.join("member");
        let build = dir.path().join("build");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::create_dir_all(&build).unwrap();
        fs::write(member.join("Cargo.toml"), "[package]\nname='member'\n").unwrap();
        let source = member.join("src/lib.rs");
        fs::write(&source, "pub fn value() {}\n").unwrap();
        let roots = AllowedRoots {
            build_root: build.clone(),
            source_roots: Vec::new(),
            workspace_roots: vec![workspace.clone()],
            remap_workspace_paths: true,
        };
        let base = vec![
            "--crate-name".into(),
            "member".into(),
            "--crate-type=lib".into(),
            "--emit=metadata".into(),
            "--out-dir".into(),
            build.as_os_str().to_owned(),
            source.as_os_str().to_owned(),
        ];
        let mut explicit = base.clone();
        explicit.push(
            format!(
                "--remap-path-prefix={}=/custom/workspace",
                workspace.display()
            )
            .into(),
        );
        let candidate = match classify(Path::new("rustc"), &explicit, &[], &roots) {
            Classification::Cacheable(candidate) => candidate,
            other => panic!("expected explicit remap to be accepted: {other:?}"),
        };
        assert_eq!(
            candidate.remap_path_prefix,
            Some((workspace.display().to_string(), "/custom/workspace".into()))
        );
        assert_eq!(
            candidate
                .compiler_args
                .iter()
                .filter(|arg| arg.to_string_lossy().starts_with("--remap-path-prefix="))
                .count(),
            1
        );

        let mut unrelated = base.clone();
        unrelated.push("--remap-path-prefix=/unrelated=/other".into());
        assert_eq!(
            classify(Path::new("rustc"), &unrelated, &[], &roots),
            Classification::Bypass(BypassReason::RemapConflict)
        );
    }
}
