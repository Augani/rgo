//! Ownership and crash recovery for the private macOS Cargo guardian pilot.
//! Job cleanup holds the same exclusive scope guards as GC: an absent process
//! or disconnected launcher alone never permits reaping a resource coalition.

#![allow(unsafe_code)] // Effective user ID and no-follow metadata opens.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::ReadDir;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::paths::RgoPaths;

const MAX_RECORD_BYTES: u64 = 16 * 1024;
const RECOVERY_BUDGET: Duration = Duration::from_secs(2);
pub const JOB_PREFIX: &str = "macos-cargo-job-";

mod command;
mod retirement;
pub use retirement::RetirementBusy;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CargoJobOwner {
    version: u32,
    pub label: String,
    pub domain: String,
    pub token: String,
    pub definition: String,
    pub context: PathBuf,
    executable: PathBuf,
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn definition(directory: &Path, owner: &CargoJobOwner) -> Result<String> {
    let scheduling = if owner.version >= 2 {
        "<key>ProcessType</key><string>Interactive</string>"
    } else {
        ""
    };
    Ok(format!(
        "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string><string>macos-cargo-job</string><string>--directory</string><string>{}</string><string>--token</string><string>{}</string><string>--context</string><string>{}</string></array><key>RunAtLoad</key><true/><key>LaunchOnlyOnce</key><true/><key>AbandonProcessGroup</key><true/>{scheduling}<key>StandardOutPath</key><string>/dev/null</string><key>StandardErrorPath</key><string>{}</string></dict></plist>",
        owner.label,
        xml(owner
            .executable
            .to_str()
            .context("guardian executable is not UTF-8")?),
        xml(directory
            .to_str()
            .context("guardian directory is not UTF-8")?),
        owner.token,
        xml(owner
            .context
            .to_str()
            .context("guardian context is not UTF-8")?),
        xml(directory
            .join("guardian.stderr")
            .to_str()
            .context("guardian diagnostic path is not UTF-8")?)
    ))
}

impl CargoJobOwner {
    pub fn new(
        directory: &Path,
        executable: PathBuf,
        context: PathBuf,
        token: String,
    ) -> Result<Self> {
        ensure!(
            token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid Cargo job token"
        );
        let mut owner = Self {
            version: 4,
            label: format!("com.rgo.cargo.{}", &token[..32]),
            domain: format!("gui/{}", unsafe { libc::geteuid() }),
            token,
            definition: String::new(),
            context,
            executable,
        };
        owner.definition = definition(directory, &owner)?;
        ensure!(
            serde_json::to_vec(&owner)?.len() as u64 <= MAX_RECORD_BYTES,
            "Cargo job owner record is too large"
        );
        Ok(owner)
    }

    pub fn target(&self) -> String {
        format!("{}/{}", self.domain, self.label)
    }

    fn validate(&self, directory: &Path) -> Result<()> {
        ensure!(
            self.version == 4
                && self.token.len() == 64
                && self.token.bytes().all(|byte| byte.is_ascii_hexdigit())
                && self.label == format!("com.rgo.cargo.{}", &self.token[..32])
                && self.domain == format!("gui/{}", unsafe { libc::geteuid() })
                && self.executable.is_absolute(),
            "unsupported or mismatched Cargo job owner"
        );
        let state = directory
            .parent()
            .context("Cargo job has no state directory")?;
        let root = state.parent().context("Cargo job has no storage root")?;
        ensure!(
            directory.is_absolute()
                && state.file_name().is_some_and(|name| name == "state")
                && directory
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(JOB_PREFIX)),
            "Cargo job is outside its storage root"
        );
        let relative = self.context.strip_prefix(root.join("builds"))?;
        let mut components = relative.components();
        ensure!(
            matches!(components.next(), Some(Component::Normal(_)))
                && matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none(),
            "invalid Cargo job context"
        );
        ensure!(
            self.definition == definition(directory, self)?,
            "Cargo job definition was edited"
        );
        Ok(())
    }
}

fn verify_directory(directory: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Cargo job directory is not private and owned"
    );
    Ok(())
}

fn read_record(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.len() <= MAX_RECORD_BYTES,
        "unsafe or oversized Cargo job record"
    );
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "Cargo job record grew beyond its limit"
    );
    Ok(bytes)
}

pub fn read_owner(directory: &Path) -> Result<(CargoJobOwner, Vec<u8>)> {
    ensure!(
        !retirement::pending(directory)?,
        "Cargo job retirement is pending"
    );
    verify_directory(directory)?;
    let bytes = read_record(&directory.join("owner.json"))?;
    let owner: CargoJobOwner = serde_json::from_slice(&bytes)?;
    owner.validate(directory)?;
    verify_owner(directory, &owner, &bytes)?;
    Ok((owner, bytes))
}

pub fn verify_owner(directory: &Path, owner: &CargoJobOwner, bytes: &[u8]) -> Result<()> {
    ensure!(
        !retirement::pending(directory)?,
        "Cargo job retirement is pending"
    );
    verify_directory(directory)?;
    owner.validate(directory)?;
    ensure!(
        read_record(&directory.join("owner.json"))? == bytes
            && read_record(&directory.join("job.plist"))? == owner.definition.as_bytes(),
        "Cargo job ownership changed"
    );
    Ok(())
}

/// Maintenance may resume a validated retirement after its original header is
/// gone. Startup always uses `read_owner`, which refuses a pending retirement.
pub fn recovery_owner(directory: &Path) -> Result<(CargoJobOwner, Vec<u8>)> {
    retirement::owner(directory)
}

/// The caller has not supplied any invocation yet. Registration failure can
/// therefore fence the rendezvous without admitting a managed writer. The
/// one-use job exits on startup failure and launchd removes that incarnation.
pub fn bootstrap(
    directory: &Path,
    owner: &CargoJobOwner,
    bytes: &[u8],
    deadline: Instant,
) -> Result<()> {
    verify_owner(directory, owner, bytes)?;
    let status = command::run(
        Command::new("/bin/launchctl")
            .arg("bootstrap")
            .arg(&owner.domain)
            .arg(directory.join("job.plist"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        deadline,
    )?;
    ensure!(status.success(), "Cargo job bootstrap failed: {status}");
    Ok(())
}

/// Only call after durable retirement under a session guard, or while holding
/// exclusive GC guards. This removes only owned metadata. Launchd retires its
/// one-use job on exit; cleanup never unloads another registration by label.
pub fn cleanup(directory: &Path, owner: &CargoJobOwner, bytes: &[u8]) -> Result<()> {
    ensure!(
        cleanup_step(directory, owner, bytes, 4)?,
        "Cargo job retirement remains incomplete"
    );
    Ok(())
}

/// Same guard precondition as `cleanup`. Bound removals while retaining a
/// synced sibling journal; a later maintenance process can finish the step.
pub fn cleanup_step(
    directory: &Path,
    owner: &CargoJobOwner,
    bytes: &[u8],
    max_files: usize,
) -> Result<bool> {
    retirement::advance(directory, owner, bytes, max_files)
}

/// Retain the directory iterator across maintenance passes, so busy or edited
/// jobs cannot starve later jobs and each pass has a bounded number of reads.
#[derive(Default)]
pub struct RecoveryScanner {
    entries: Option<ReadDir>,
}

impl RecoveryScanner {
    pub fn scan(&mut self, paths: &RgoPaths, max_entries: usize) -> Result<usize> {
        if self.entries.is_none() {
            self.entries = Some(std::fs::read_dir(paths.state_dir())?);
        }
        let mut recovered = 0;
        let started = Instant::now();
        for _ in 0..max_entries {
            let Some(entry) = self.entries.as_mut().expect("initialized iterator").next() else {
                self.entries = None;
                break;
            };
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let directory_name = name
                .strip_prefix(".retiring-")
                .and_then(|name| name.strip_suffix(".json"))
                .unwrap_or(name);
            if !directory_name.starts_with(JOB_PREFIX) {
                continue;
            }
            let directory = paths.state_dir().join(directory_name);
            let recover = (|| -> Result<bool> {
                let (owner, bytes) = recovery_owner(&directory)?;
                ensure!(
                    directory.parent() == Some(paths.state_dir().as_path()),
                    "Cargo job root changed"
                );
                // Receipt validation observes every retained kernel identity.
                // Unresolved or corrupt identities return an error. Versioned
                // receipts also recognize proven reaping. Hold the guard
                // through metadata removal, including the restart fence.
                let Some(_guard) = crate::supervision::try_lock_gc(paths, Some(&owner.context))?
                else {
                    return Ok(false);
                };
                cleanup(&directory, &owner, &bytes)?;
                Ok(true)
            })();
            match recover {
                Ok(true) => recovered += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(%error, path = %directory.display(), "retaining unresolved Cargo job")
                }
            }
            // Do not start another retirement after this pass's elapsed work
            // budget has been spent.
            if started.elapsed() >= RECOVERY_BUDGET {
                break;
            }
        }
        Ok(recovered)
    }
}
