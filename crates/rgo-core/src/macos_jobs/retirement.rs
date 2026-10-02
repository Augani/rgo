//! Persisted retirement of identified job metadata. Context/receipt exclusion
//! is supplied by the caller; this journal never establishes build-data safety.

use super::{CargoJobOwner, JOB_PREFIX, MAX_RECORD_BYTES, verify_directory};
use crate::paths::RgoPaths;
use anyhow::{Context, Result, ensure};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_JOURNAL_BYTES: u64 = 64 * 1024;

fn marker(directory: &Path) -> Result<PathBuf> {
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("Cargo job name is missing")?;
    ensure!(
        name.starts_with(JOB_PREFIX)
            && name.len() > JOB_PREFIX.len()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "unsupported Cargo job directory name"
    );
    Ok(directory
        .parent()
        .context("Cargo job has no parent")?
        .join(format!(".retiring-{name}.json")))
}

pub(super) fn pending(directory: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(marker(directory)?) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Content {
    Regular { length: u64, digest: String },
    Socket,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    identity: Identity,
    content: Content,
}

impl Fingerprint {
    fn read(path: &Path, socket: bool) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1,
            "Cargo job entry is not uniquely owned"
        );
        if socket {
            ensure!(
                metadata.file_type().is_socket(),
                "Cargo job rendezvous was replaced"
            );
            return Ok(Self {
                identity: Identity::of(&metadata),
                content: Content::Socket,
            });
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let opened = file.metadata()?;
        ensure!(
            opened.is_file()
                && Identity::of(&opened) == Identity::of(&metadata)
                && opened.len() <= MAX_RECORD_BYTES,
            "Cargo job entry changed or is oversized"
        );
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD_BYTES,
            "Cargo job entry grew beyond its limit"
        );
        Ok(Self {
            identity: Identity::of(&opened),
            content: Content::Regular {
                length: bytes.len() as u64,
                digest: blake3::hash(&bytes).to_hex().to_string(),
            },
        })
    }
    fn matches_bytes(&self, bytes: &[u8]) -> bool {
        self.content
            == Content::Regular {
                length: bytes.len() as u64,
                digest: blake3::hash(bytes).to_hex().to_string(),
            }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    owner_record: String,
    directory: Identity,
    files: BTreeMap<String, Fingerprint>,
}

fn known_file(name: &str) -> bool {
    matches!(
        name,
        "owner.json" | "job.plist" | "control.sock" | "guardian.stderr"
    )
}

fn entries(directory: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let entries = std::fs::read_dir(directory)?
        .take(5)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= 4
            && entries
                .iter()
                .all(|entry| entry.file_name().to_str().is_some_and(known_file)),
        "Cargo job directory has unexpected content"
    );
    Ok(entries)
}

impl Journal {
    fn validate(&self, directory: &Path) -> Result<CargoJobOwner> {
        ensure!(
            self.version == 1
                && self.owner_record.len() as u64 <= MAX_RECORD_BYTES
                && self.files.len() <= 4
                && self.files.contains_key("owner.json")
                && self.files.contains_key("job.plist")
                && self.files.keys().all(|name| known_file(name)),
            "unsupported Cargo job retirement journal"
        );
        let owner: CargoJobOwner = serde_json::from_str(&self.owner_record)?;
        owner.validate(directory)?;
        for (name, fingerprint) in &self.files {
            match &fingerprint.content {
                Content::Regular { length, digest } => ensure!(
                    name != "control.sock"
                        && *length <= MAX_RECORD_BYTES
                        && digest.len() == 64
                        && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid Cargo job retirement fingerprint"
                ),
                Content::Socket => ensure!(
                    name == "control.sock",
                    "invalid Cargo job socket fingerprint"
                ),
            }
        }
        ensure!(
            self.files["owner.json"].matches_bytes(self.owner_record.as_bytes())
                && self.files["job.plist"].matches_bytes(owner.definition.as_bytes()),
            "Cargo job retirement ownership changed"
        );
        Ok(owner)
    }
}

fn read_journal(path: &Path) -> Result<Journal> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1
            && metadata.len() <= MAX_JOURNAL_BYTES,
        "unsafe Cargo job retirement journal"
    );
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_JOURNAL_BYTES,
        "Cargo job retirement journal grew beyond its limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn owner(directory: &Path) -> Result<(CargoJobOwner, Vec<u8>)> {
    let path = marker(directory)?;
    verify_directory(
        path.parent()
            .context("Cargo job retirement has no parent")?,
    )?;
    if pending(directory)? {
        let journal = read_journal(&path)?;
        let owner = journal.validate(directory)?;
        Ok((owner, journal.owner_record.into_bytes()))
    } else {
        super::read_owner(directory)
    }
}

#[derive(Debug)]
pub struct RetirementBusy;

impl std::fmt::Display for RetirementBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Cargo job retirement lock is busy")
    }
}

impl std::error::Error for RetirementBusy {}

pub(super) fn advance(
    directory: &Path,
    owner: &CargoJobOwner,
    bytes: &[u8],
    max_files: usize,
) -> Result<bool> {
    ensure!(
        (1..=4).contains(&max_files),
        "invalid Cargo job retirement work limit"
    );
    let marker = marker(directory)?;
    let parent = marker
        .parent()
        .context("Cargo job retirement has no parent")?;
    verify_directory(parent)?;
    crate::paths::check_local_cleanup_volume(parent)?;
    owner.validate(directory)?;
    let paths = RgoPaths {
        root: parent
            .parent()
            .context("Cargo job has no storage root")?
            .to_owned(),
    };
    let lock_path = paths.state_dir().join("locks/macos-job-retirement.lock");
    let lock = crate::supervision::open_lock_file(&lock_path)?;
    if !FileExt::try_lock_exclusive(&lock)? {
        return Err(RetirementBusy.into());
    }
    crate::supervision::verify_lock_identity(&lock_path, &lock)?;
    let journal = if pending(directory)? {
        read_journal(&marker)?
    } else {
        super::verify_owner(directory, owner, bytes)?;
        let mut files = BTreeMap::new();
        for entry in entries(directory)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid Cargo job filename"))?;
            files.insert(
                name.clone(),
                Fingerprint::read(&entry.path(), name == "control.sock")?,
            );
        }
        let journal = Journal {
            version: 1,
            owner_record: std::str::from_utf8(bytes)?.to_owned(),
            directory: Identity::of(&std::fs::symlink_metadata(directory)?),
            files,
        };
        journal.validate(directory)?;
        let data = serde_json::to_vec(&journal)?;
        ensure!(
            data.len() as u64 <= MAX_JOURNAL_BYTES,
            "Cargo job retirement journal is too large"
        );
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&data)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&marker)?;
        File::open(parent)?.sync_all()?;
        journal
    };
    let saved_owner = journal.validate(directory)?;
    ensure!(
        journal.owner_record.as_bytes() == bytes
            && serde_json::to_vec(&saved_owner)? == serde_json::to_vec(owner)?,
        "Cargo job retirement belongs to another owner"
    );
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => {
            verify_directory(directory)?;
            ensure!(
                Identity::of(&metadata) == journal.directory,
                "retiring Cargo job directory changed"
            );
            let mut entries = entries(directory)?;
            for entry in &entries {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid Cargo job filename"))?;
                ensure!(
                    journal.files.get(&name)
                        == Some(&Fingerprint::read(&entry.path(), name == "control.sock")?),
                    "retiring Cargo job content changed"
                );
            }
            // Fence the one-use rendezvous first; startup also rejects this
            // journal even before the socket name is removed. Header next.
            entries.sort_by_key(|entry| match entry.file_name().to_str() {
                Some("control.sock") => 0,
                Some("owner.json") => 1,
                Some("job.plist") => 2,
                _ => 3,
            });
            let incomplete = entries.len() > max_files;
            for entry in entries.into_iter().take(max_files) {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid Cargo job filename"))?;
                ensure!(
                    journal.files.get(&name)
                        == Some(&Fingerprint::read(&entry.path(), name == "control.sock")?),
                    "retiring Cargo job content changed"
                );
                crate::paths::check_local_cleanup_volume(directory)?;
                std::fs::remove_file(entry.path())?;
            }
            File::open(directory)?.sync_all()?;
            if incomplete {
                return Ok(false);
            }
            std::fs::remove_dir(directory)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    File::open(parent)?.sync_all()?;
    // LaunchOnlyOnce makes launchd remove the actual job incarnation on exit.
    // A label may now belong to a foreign replacement; never boot it out.
    std::fs::remove_file(&marker)?;
    File::open(parent)?.sync_all()?;
    Ok(true)
}
