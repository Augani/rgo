//! Owned macOS terminal-host records and retirement; no build-data authorization.

#![allow(unsafe_code)] // Confined process identity and termios syscalls.

use crate::macos_coalition::boot_session;
use crate::paths::RgoPaths;
use anyhow::{Context, Result, ensure};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, ReadDir};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = 8192;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    input: u64,
    output: u64,
    control: u64,
    local: u64,
    characters: Vec<u8>,
    input_speed: u64,
    output_speed: u64,
}

impl Settings {
    pub fn read(fd: i32) -> Result<Self> {
        let mut value: libc::termios = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::tcgetattr(fd, &mut value) } == 0,
            "reading terminal settings failed"
        );
        Ok(Self::from_native(&value))
    }

    fn from_native(value: &libc::termios) -> Self {
        Self {
            input: value.c_iflag,
            output: value.c_oflag,
            control: value.c_cflag,
            local: value.c_lflag,
            characters: value.c_cc.to_vec(),
            input_speed: value.c_ispeed,
            output_speed: value.c_ospeed,
        }
    }

    pub fn native(&self) -> Result<libc::termios> {
        ensure!(
            self.characters.len() == libc::NCCS,
            "unsupported terminal character count"
        );
        let mut value: libc::termios = unsafe { std::mem::zeroed() };
        value.c_iflag = self.input;
        value.c_oflag = self.output;
        value.c_cflag = self.control;
        value.c_lflag = self.local;
        value.c_cc.copy_from_slice(&self.characters);
        value.c_ispeed = self.input_speed;
        value.c_ospeed = self.output_speed;
        Ok(value)
    }

    pub fn raw(&self) -> Result<Self> {
        let mut value = self.native()?;
        unsafe { libc::cfmakeraw(&mut value) };
        Ok(Self::from_native(&value))
    }

    pub fn apply(&self, fd: i32) -> Result<()> {
        ensure!(
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.native()?) } == 0,
            "setting terminal mode failed"
        );
        Ok(())
    }

    pub fn same_effective_mode(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        // PENDIN is a kernel input-queue state, not a persistent mode setting.
        left.local &= !libc::PENDIN;
        right.local &= !libc::PENDIN;
        left == right
    }

    pub fn zsh_resumed(&self) -> Self {
        let mut settings = self.clone();
        settings.local |= libc::ICANON | libc::ECHO;
        settings.local &= !libc::FLUSHO;
        settings
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub settings: Settings,
    pub owner_group: i32,
    pub shell_group: i32,
    pub foreground: bool,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub pid: i32,
    pub started_seconds: u64,
    pub started_microseconds: u64,
    pub session: i32,
}

#[derive(PartialEq, Eq)]
pub enum ProcessState {
    Live,
    Gone,
    Reused,
}

impl Process {
    pub fn observe(pid: i32) -> Result<Option<Self>> {
        ensure!(pid > 0, "invalid terminal process identity");
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info);
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            )
        };
        if read <= 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("observing terminal process");
        }
        let session = unsafe { libc::getsid(pid) };
        ensure!(
            read as usize == size
                && info.pbi_pid == pid as u32
                && info.pbi_uid == unsafe { libc::geteuid() }
                && session > 0
                && info.pbi_start_tvsec > 0
                && info.pbi_start_tvusec < 1_000_000,
            "unsupported terminal process identity"
        );
        Ok(Some(Self {
            pid,
            started_seconds: info.pbi_start_tvsec,
            started_microseconds: info.pbi_start_tvusec,
            session,
        }))
    }

    pub fn current_parent() -> Result<Self> {
        Self::observe(unsafe { libc::getppid() })?.context("terminal shell disappeared")
    }

    pub fn state(&self) -> Result<ProcessState> {
        match Self::observe(self.pid)? {
            Some(current) if current == *self => Ok(ProcessState::Live),
            Some(_) => Ok(ProcessState::Reused),
            None => Ok(ProcessState::Gone),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Terminal {
    pub device: u64,
    pub inode: u64,
    pub special_device: u64,
}

impl Terminal {
    pub fn read(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        ensure!(
            unsafe { libc::isatty(file.as_raw_fd()) } != 0,
            "terminal lease requires a terminal"
        );
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            special_device: metadata.rdev(),
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub version: u32,
    pub token: String,
    pub boot: String,
    pub shell: Process,
    pub terminal: Terminal,
}

pub fn generation(directory: &Path) -> Result<u64> {
    // The shell replaces this small JSON integer atomically using builtins.
    // It must not launch a new foreground process from its preexec hook.
    read(&directory.join("generation"))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    pub version: u32,
    pub token: String,
    pub boot: String,
    pub shell: Process,
    pub caller: Process,
    pub terminal: Terminal,
    pub generation: u64,
    pub configuration: Configuration,
}

pub fn private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "terminal host directory is not private and owned"
    );
    Ok(())
}

pub fn directory(paths: &RgoPaths, token: &str) -> Result<PathBuf> {
    validate_token(token)?;
    let path = paths.state_dir().join("terminal-hosts").join(token);
    private_directory(path.parent().context("terminal host parent is missing")?)?;
    ensure!(
        !has_retirement(&path)?,
        "terminal host retirement is pending"
    );
    private_directory(&path)?;
    Ok(path)
}

pub fn lock(directory: &Path) -> Result<File> {
    ensure!(
        !has_retirement(directory)?,
        "terminal host retirement is pending"
    );
    let file = host_lock(directory, false)?;
    ensure!(
        !has_retirement(directory)?,
        "terminal host retirement is pending"
    );
    Ok(file)
}

/// Create a lock only while publishing a new, empty registration directory.
pub fn registration_lock(directory: &Path) -> Result<File> {
    ensure!(
        !has_retirement(directory)? && std::fs::read_dir(directory)?.next().is_none(),
        "terminal registration directory is not new"
    );
    host_lock(directory, true)
}

fn host_lock(directory: &Path, create: bool) -> Result<File> {
    private_directory(directory)?;
    let path = directory.join("host.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "unsafe terminal host lock"
    );
    // A stopped caller can retain this lock. Never freeze the owning shell
    // waiting for it; uncertainty disables the private recovery pilot.
    ensure!(
        FileExt::try_lock_exclusive(&file)?,
        "terminal host lock is busy"
    );
    let current = std::fs::symlink_metadata(path)?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "terminal host lock changed"
    );
    Ok(file)
}

pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let (bytes, _) = read_bytes(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_bytes(path: &Path) -> Result<(Vec<u8>, Identity)> {
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
            && metadata.len() <= MAX_BYTES,
        "unsafe terminal host record"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "terminal host record grew beyond its limit"
    );
    Ok((bytes, Identity::from_metadata(&metadata)))
}

pub fn saved(directory: &Path) -> Result<Option<Saved>> {
    match read(&directory.join("lease.json")) {
        Ok(saved) => Ok(Some(saved)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn write(directory: &Path, name: &str, value: &impl Serialize, durable: bool) -> Result<()> {
    ensure!(
        matches!(name, "host.json" | "lease.json" | "generation"),
        "unknown terminal record name"
    );
    atomic_write(directory, name, value, durable, true)
}

fn atomic_write(
    directory: &Path,
    name: &str,
    value: &impl Serialize,
    durable: bool,
    replace: bool,
) -> Result<()> {
    private_directory(directory)?;
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "terminal host record is too large"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    // Leases/counters need atomic process-crash visibility, not power-loss
    // persistence: their boot and process identities expire after a reboot.
    if durable {
        temporary.as_file().sync_all()?;
    }
    if replace {
        temporary.persist(directory.join(name))?;
    } else {
        temporary.persist_noclobber(directory.join(name))?;
    }
    if durable {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<()> {
    ensure!(
        token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid terminal host token"
    );
    Ok(())
}

fn validate_process(process: &Process) -> Result<()> {
    ensure!(
        process.pid > 0
            && process.session > 0
            && process.started_seconds > 0
            && process.started_microseconds < 1_000_000,
        "invalid stored terminal process identity"
    );
    Ok(())
}

fn validate_host(host: &Host, token: &str) -> Result<()> {
    validate_token(token)?;
    ensure!(
        host.version == 1
            && host.token == token
            && crate::macos_coalition::valid_boot_session(&host.boot),
        "unsupported terminal host identity"
    );
    validate_process(&host.shell)
}

pub fn validate_saved(host: &Host, saved: &Saved) -> Result<()> {
    ensure!(
        saved.version == 1
            && saved.token == host.token
            && saved.boot == host.boot
            && saved.shell == host.shell
            && saved.terminal == host.terminal
            && saved.caller.session == host.shell.session
            && saved.generation > 0
            && saved.configuration.owner_group > 0
            && saved.configuration.shell_group > 0,
        "terminal lease identity changed"
    );
    validate_process(&saved.caller)?;
    saved.configuration.settings.native()?;
    Ok(())
}

fn retirement_name(token: &str) -> String {
    format!(".retiring-{token}.json")
}

fn has_retirement(directory: &Path) -> Result<bool> {
    let token = directory
        .file_name()
        .and_then(|value| value.to_str())
        .context("terminal host token is missing")?;
    validate_token(token)?;
    let parent = directory
        .parent()
        .context("terminal host parent is missing")?;
    match std::fs::symlink_metadata(parent.join(retirement_name(token))) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    identity: Identity,
    length: u64,
    digest: String,
}

impl Fingerprint {
    fn read(path: &Path) -> Result<Self> {
        let (bytes, identity) = read_bytes(path)?;
        Ok(Self::from_bytes(&bytes, identity))
    }
    fn parse<T: serde::de::DeserializeOwned>(path: &Path) -> Result<(T, Self)> {
        let (bytes, identity) = read_bytes(path)?;
        Ok((
            serde_json::from_slice(&bytes)?,
            Self::from_bytes(&bytes, identity),
        ))
    }
    fn from_bytes(bytes: &[u8], identity: Identity) -> Self {
        Self {
            identity,
            length: bytes.len() as u64,
            digest: blake3::hash(bytes).to_hex().to_string(),
        }
    }
    fn matches(&self, path: &Path) -> Result<bool> {
        let current = Self::read(path)?;
        Ok(current.identity == self.identity
            && current.length == self.length
            && current.digest == self.digest)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Retirement {
    version: u32,
    host: Host,
    caller: Option<Process>,
    directory: Identity,
    files: BTreeMap<String, Fingerprint>,
}

fn known_file(name: &str) -> bool {
    matches!(
        name,
        "host.json" | "host.lock" | "generation" | "lease.json"
    )
}

fn entries(directory: &Path) -> Result<Vec<std::fs::DirEntry>> {
    // Bound enumeration before collecting: added content must not make this
    // maintenance step grow to an unbounded directory inventory.
    let entries = std::fs::read_dir(directory)?
        .take(5)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= 4
            && entries
                .iter()
                .all(|entry| entry.file_name().to_str().is_some_and(known_file)),
        "terminal host has unknown content"
    );
    Ok(entries)
}

fn authorize(
    host: &Host,
    caller: Option<&Process>,
    authorized: Option<(&Process, &Terminal)>,
    current_boot: &str,
) -> Result<()> {
    if let Some((shell, terminal)) = authorized {
        ensure!(
            host.boot == current_boot && host.shell == *shell && host.terminal == *terminal,
            "terminal retirement belongs to another shell, boot, or terminal"
        );
    } else if host.boot == current_boot {
        ensure!(
            host.shell.state()? != ProcessState::Live,
            "terminal shell remains live"
        );
    }
    if host.boot == current_boot {
        if let Some(caller) = caller {
            ensure!(
                caller.state()? != ProcessState::Live,
                "terminal recovery remains owned by a live caller"
            );
        }
    }
    Ok(())
}

fn retirement_lock(paths: &RgoPaths) -> Result<File> {
    let path = paths
        .state_dir()
        .join("locks/macos-terminal-retirement.lock");
    let lock = crate::supervision::open_lock_file(&path)?;
    ensure!(
        FileExt::try_lock_exclusive(&lock)?,
        "terminal retirement lock is busy"
    );
    crate::supervision::verify_lock_identity(&path, &lock)?;
    Ok(lock)
}

/// Retire only identified terminal metadata. `authorized` acknowledges owned
/// undo from the same shell; maintenance instead requires its original shell
/// and caller to be gone. No terminal is opened and no mode is changed here.
pub fn retire(
    paths: &RgoPaths,
    token: &str,
    authorized: Option<(&Process, &Terminal)>,
) -> Result<()> {
    ensure!(
        retire_step(paths, token, authorized, 4)?,
        "terminal retirement remains incomplete"
    );
    Ok(())
}

/// Advance a persisted retirement by at most `max_files` unlinks. Returning
/// false leaves the synced journal for the next maintenance pass or process.
pub fn retire_step(
    paths: &RgoPaths,
    token: &str,
    authorized: Option<(&Process, &Terminal)>,
    max_files: usize,
) -> Result<bool> {
    ensure!(
        (1..=4).contains(&max_files),
        "invalid terminal retirement work limit"
    );
    validate_token(token)?;
    let parent = paths.state_dir().join("terminal-hosts");
    private_directory(&parent)?;
    crate::paths::check_local_cleanup_volume(&parent)?;
    let _retirement_lock = retirement_lock(paths)?;
    let directory = parent.join(token);
    let marker = parent.join(retirement_name(token));
    let current_boot = boot_session()?;
    let mut host_lock_guard = None;
    let retirement = match std::fs::symlink_metadata(&marker) {
        Ok(_) => read::<Retirement>(&marker)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            private_directory(&directory)?;
            // Registration always creates its lock before publishing the host.
            // An unrecognized directory must not gain a new lock as a side effect.
            host_lock_guard = Some(host_lock(&directory, false)?);
            let (host, host_fingerprint): (Host, _) =
                Fingerprint::parse(&directory.join("host.json"))?;
            validate_host(&host, token)?;
            let saved = match Fingerprint::parse::<Saved>(&directory.join("lease.json")) {
                Ok(value) => Some(value),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            if let Some(saved) = &saved {
                validate_saved(&host, &saved.0)?;
            }
            let caller = saved.as_ref().map(|value| value.0.caller.clone());
            authorize(&host, caller.as_ref(), authorized, &current_boot)?;
            let (_, generation_fingerprint): (u64, _) =
                Fingerprint::parse(&directory.join("generation"))?;
            let mut files = BTreeMap::new();
            for entry in entries(&directory)? {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid terminal filename"))?;
                files.insert(name, Fingerprint::read(&entry.path())?);
            }
            ensure!(
                files.contains_key("host.json")
                    && files.contains_key("host.lock")
                    && files.contains_key("generation"),
                "incomplete terminal host record"
            );
            ensure!(
                files["host.lock"].length == 0,
                "terminal host lock content changed"
            );
            // The authorization must describe the exact bytes about to be
            // journaled, including an absent lease, rather than an earlier read.
            ensure!(
                files["host.json"] == host_fingerprint
                    && files["generation"] == generation_fingerprint
                    && files.get("lease.json") == saved.as_ref().map(|value| &value.1),
                "terminal host changed during retirement preparation"
            );
            let retirement = Retirement {
                version: 1,
                host,
                caller,
                directory: Identity::from_metadata(&std::fs::symlink_metadata(&directory)?),
                files,
            };
            // This journal survives individual removals and remains outside the
            // directory until its disappearance has been durably acknowledged.
            atomic_write(&parent, &retirement_name(token), &retirement, true, false)?;
            retirement
        }
        Err(error) => return Err(error.into()),
    };
    validate_host(&retirement.host, token)?;
    ensure!(
        retirement.version == 1
            && retirement.files.len() <= 4
            && retirement.files.contains_key("host.json")
            && retirement.files.contains_key("host.lock")
            && retirement.files.contains_key("generation")
            && retirement.files.keys().all(|name| known_file(name)),
        "unsupported terminal retirement record"
    );
    ensure!(
        retirement.files.contains_key("lease.json") == retirement.caller.is_some()
            && retirement.files["host.lock"].length == 0,
        "terminal retirement lease or lock changed"
    );
    if let Some(caller) = &retirement.caller {
        validate_process(caller)?;
        ensure!(
            caller.session == retirement.host.shell.session,
            "terminal retirement caller session changed"
        );
    }
    for fingerprint in retirement.files.values() {
        ensure!(
            fingerprint.length <= MAX_BYTES
                && fingerprint.digest.len() == 64
                && fingerprint
                    .digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "invalid terminal retirement fingerprint"
        );
    }
    authorize(
        &retirement.host,
        retirement.caller.as_ref(),
        authorized,
        &current_boot,
    )?;
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            private_directory(&directory)?;
            ensure!(
                Identity::from_metadata(&metadata) == retirement.directory,
                "retiring terminal directory changed"
            );
            if host_lock_guard.is_none() {
                match host_lock(&directory, false) {
                    Ok(lock) => host_lock_guard = Some(lock),
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            // Preflight every remaining file before removing any. Missing
            // entries are resumed removals; additions and edits are preserved.
            let mut entries = entries(&directory)?;
            for entry in &entries {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid terminal filename"))?;
                let fingerprint = retirement
                    .files
                    .get(&name)
                    .context("retiring terminal content was added")?;
                ensure!(
                    fingerprint.matches(&entry.path())?,
                    "retiring terminal content changed"
                );
            }
            // Retire the original header first: subsequent passes must use the
            // durable journal. Keep the coordinating lock until the final step.
            entries.sort_by_key(|entry| match entry.file_name().to_str() {
                Some("host.json") => 0,
                Some("lease.json") => 1,
                Some("generation") => 2,
                _ => 3,
            });
            let incomplete = entries.len() > max_files;
            for entry in entries.into_iter().take(max_files) {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid terminal filename"))?;
                ensure!(
                    retirement.files[&name].matches(&entry.path())?,
                    "retiring terminal content changed"
                );
                crate::paths::check_local_cleanup_volume(&directory)?;
                std::fs::remove_file(entry.path())?;
            }
            File::open(&directory)?.sync_all()?;
            if incomplete {
                return Ok(false);
            }
            std::fs::remove_dir(&directory)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // An earlier rmdir can be visible but not yet durable after interruption.
    // Sync its parent even when the directory was already absent on entry.
    File::open(&parent)?.sync_all()?;
    drop(host_lock_guard);
    std::fs::remove_file(marker)?;
    File::open(parent)?.sync_all()?;
    Ok(true)
}

#[derive(Default)]
pub struct RecoveryScanner {
    entries: Option<ReadDir>,
}

impl RecoveryScanner {
    pub fn scan(&mut self, paths: &RgoPaths, max_entries: usize) -> Result<usize> {
        let parent = paths.state_dir().join("terminal-hosts");
        if self.entries.is_none() {
            match std::fs::symlink_metadata(&parent) {
                Ok(_) => private_directory(&parent)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(error) => return Err(error.into()),
            }
            self.entries = Some(std::fs::read_dir(&parent)?);
        }
        let started = Instant::now();
        let mut retired = 0;
        for _ in 0..max_entries {
            let Some(entry) = self.entries.as_mut().expect("initialized iterator").next() else {
                self.entries = None;
                break;
            };
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let token = name
                .strip_prefix(".retiring-")
                .and_then(|name| name.strip_suffix(".json"))
                .unwrap_or(name);
            if validate_token(token).is_err() {
                continue;
            }
            match retire_step(paths, token, None, 2) {
                Ok(true) => retired += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(%error, path = %entry.path().display(), "retaining unresolved terminal host")
                }
            }
            if started.elapsed() >= Duration::from_millis(50) {
                break;
            }
        }
        Ok(retired)
    }
}
