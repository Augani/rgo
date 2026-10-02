//! Explicitly registered zsh recovery pilot. Its terminal lease is independent
//! of Cargo job retirement; it never authorizes deleting build storage.

use super::{Configuration, Settings, open_original};
use anyhow::{Context, Result, ensure};
use fs4::fs_std::FileExt;
use rgo_core::macos_coalition::boot_session;
use rgo_core::paths::RgoPaths;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 8192;
const HOST_ENV: &str = "RGO_TERMINAL_HOST";

#[derive(clap::Subcommand)]
pub enum Action {
    InitZsh,
    Register {
        #[arg(long)]
        shell_pid: i32,
    },
    Finish {
        #[arg(long)]
        token: String,
        #[arg(long)]
        generation: u64,
    },
    Unregister {
        #[arg(long)]
        token: String,
    },
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Process {
    pid: i32,
    started_seconds: u64,
    started_microseconds: u64,
    session: i32,
}

#[derive(PartialEq, Eq)]
enum ProcessState {
    Live,
    Gone,
    Reused,
}

impl Process {
    fn observe(pid: i32) -> Result<Option<Self>> {
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

    fn current_parent() -> Result<Self> {
        Self::observe(unsafe { libc::getppid() })?.context("terminal shell disappeared")
    }

    fn state(&self) -> Result<ProcessState> {
        match Self::observe(self.pid)? {
            Some(current) if current == *self => Ok(ProcessState::Live),
            Some(_) => Ok(ProcessState::Reused),
            None => Ok(ProcessState::Gone),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Terminal {
    device: u64,
    inode: u64,
    special_device: u64,
}

impl Terminal {
    fn read(file: &File) -> Result<Self> {
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Host {
    version: u32,
    token: String,
    boot: String,
    shell: Process,
    terminal: Terminal,
}

fn generation(directory: &Path) -> Result<u64> {
    // The shell replaces this small JSON integer atomically using builtins.
    // It must not launch a new foreground process from its preexec hook.
    read(&directory.join("generation"))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    token: String,
    boot: String,
    shell: Process,
    caller: Process,
    terminal: Terminal,
    generation: u64,
    configuration: Configuration,
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "terminal host directory is not private and owned"
    );
    Ok(())
}

fn directory(paths: &RgoPaths, token: &str) -> Result<PathBuf> {
    ensure!(
        token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid terminal host token"
    );
    let path = paths.state_dir().join("terminal-hosts").join(token);
    private_directory(path.parent().context("terminal host parent is missing")?)?;
    private_directory(&path)?;
    Ok(path)
}

fn lock(directory: &Path) -> Result<File> {
    private_directory(directory)?;
    let path = directory.join("host.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
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
    FileExt::try_lock_exclusive(&file)?;
    let current = std::fs::symlink_metadata(path)?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "terminal host lock changed"
    );
    Ok(file)
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
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
    Ok(serde_json::from_slice(&bytes)?)
}

fn saved(directory: &Path) -> Result<Option<Saved>> {
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

fn write(directory: &Path, name: &str, value: &impl Serialize, durable: bool) -> Result<()> {
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
    temporary.persist(directory.join(name))?;
    if durable {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn validate(host: &Host, token: &str, file: &File) -> Result<()> {
    ensure!(
        host.version == 1
            && host.token == token
            && host.boot == boot_session()?
            && host.shell == Process::current_parent()?
            && host.terminal == Terminal::read(file)?,
        "terminal host belongs to another shell, boot, or terminal"
    );
    ensure!(
        unsafe { libc::tcgetpgrp(file.as_raw_fd()) } == unsafe { libc::getpgrp() },
        "terminal host operation is not foreground"
    );
    Ok(())
}

fn validate_saved(host: &Host, saved: &Saved) -> Result<()> {
    ensure!(
        saved.version == 1
            && saved.token == host.token
            && saved.boot == host.boot
            && saved.shell == host.shell
            && saved.terminal == host.terminal
            && saved.caller.session == host.shell.session
            && saved.generation > 0
            && saved.configuration.owner_group > 0
            && saved.configuration.shell_group == unsafe { libc::getpgid(host.shell.pid) },
        "terminal lease identity changed"
    );
    saved.configuration.settings.native()?;
    Ok(())
}

pub(super) struct Lease {
    directory: PathBuf,
    token: String,
    caller: Process,
}

impl Lease {
    pub(super) fn for_caller(paths: &RgoPaths, file: &File) -> Result<Option<Self>> {
        let Some(token) = std::env::var_os(HOST_ENV) else {
            return Ok(None);
        };
        let token = token
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid terminal host token"))?;
        let directory = directory(paths, &token)?;
        let _lock = lock(&directory)?;
        let host: Host = read(&directory.join("host.json"))?;
        validate(&host, &token, file)?;
        let caller = Process::observe(std::process::id().try_into()?)?
            .context("terminal caller disappeared")?;
        ensure!(
            caller.session == host.shell.session && generation(&directory)? > 0,
            "terminal caller has no shell generation"
        );
        if let Some(previous) = saved(&directory)? {
            validate_saved(&host, &previous)?;
            ensure!(
                previous.caller == caller,
                "terminal lease awaits its owning shell's acknowledgment"
            );
        }
        Ok(Some(Self {
            directory,
            token,
            caller,
        }))
    }

    pub(super) fn arm(&self, file: &File, configuration: &Configuration) -> Result<()> {
        let _lock = lock(&self.directory)?;
        let host: Host = read(&self.directory.join("host.json"))?;
        validate(&host, &self.token, file)?;
        if let Some(previous) = saved(&self.directory)? {
            validate_saved(&host, &previous)?;
            ensure!(
                previous.caller == self.caller,
                "terminal lease awaits its owning shell's acknowledgment"
            );
        }
        ensure!(
            unsafe { libc::getpgrp() } == configuration.owner_group
                && unsafe { libc::getpgid(host.shell.pid) } == configuration.shell_group,
            "terminal lease process groups changed"
        );
        let generation = generation(&self.directory)?;
        ensure!(generation > 0, "terminal caller has no shell generation");
        write(
            &self.directory,
            "lease.json",
            &Saved {
                version: 1,
                token: self.token.clone(),
                boot: host.boot,
                shell: host.shell,
                caller: self.caller.clone(),
                terminal: host.terminal,
                generation,
                configuration: configuration.clone(),
            },
            false,
        )
    }

    pub(super) fn disarm(&self) -> Result<()> {
        let _lock = lock(&self.directory)?;
        if let Some(saved) = saved(&self.directory)? {
            ensure!(
                saved.token == self.token && saved.caller == self.caller,
                "terminal lease owner changed"
            );
            std::fs::remove_file(self.directory.join("lease.json"))?;
        }
        Ok(())
    }
}

fn quote(path: &Path) -> Result<String> {
    Ok(format!(
        "'{}'",
        path.to_str()
            .context("terminal host path is not UTF-8")?
            .replace('\'', "'\\''")
    ))
}

pub(crate) fn run(action: Action, home: Option<&Path>) -> Result<()> {
    let paths = match home {
        Some(root) => RgoPaths {
            root: root.to_owned(),
        },
        None => RgoPaths::discover()?,
    };
    ensure!(
        paths.root.is_absolute(),
        "terminal host root must be absolute"
    );
    if matches!(action, Action::InitZsh) {
        let script = include_str!("recovery/zsh.sh");
        let (guard, body) = script
            .split_once("# rgo initialization variables\n")
            .context("terminal hook template is invalid")?;
        print!("{guard}");
        println!(
            "typeset -g __rgo_terminal_executable={}",
            quote(&std::env::current_exe()?)?
        );
        println!("typeset -g __rgo_terminal_root={}", quote(&paths.root)?);
        print!("{body}");
        return Ok(());
    }
    let original = open_original()?.context("terminal host requires its controlling terminal")?;
    if let Action::Register { shell_pid } = action {
        let shell = Process::current_parent()?;
        ensure!(
            shell.pid == shell_pid,
            "terminal registration parent is not the requested shell"
        );
        paths.ensure_layout()?;
        let parent = paths.state_dir().join("terminal-hosts");
        match std::fs::create_dir(&parent) {
            Ok(()) => std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        private_directory(&parent)?;
        let mut entropy = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let token = blake3::hash(&entropy).to_hex().to_string();
        let directory = parent.join(&token);
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let _lock = lock(&directory)?;
        write(
            &directory,
            "host.json",
            &Host {
                version: 1,
                token: token.clone(),
                boot: boot_session()?,
                shell,
                terminal: Terminal::read(&original)?,
            },
            true,
        )?;
        write(&directory, "generation", &0_u64, false)?;
        println!("typeset -gx {HOST_ENV}='{token}'");
        println!("typeset -g __rgo_terminal_token='{token}'");
        println!("typeset -g __rgo_terminal_directory={}", quote(&directory)?);
        println!("typeset -gi __rgo_terminal_generation=0");
        return Ok(());
    }
    let (token, generation) = match &action {
        Action::Finish { token, generation } => (token, *generation),
        Action::Unregister { token } => (token, 0),
        _ => unreachable!("initialization handled above"),
    };
    let directory = directory(&paths, token)?;
    let _lock = lock(&directory)?;
    let host: Host = read(&directory.join("host.json"))?;
    validate(&host, token, &original)?;
    match action {
        Action::Finish { .. } => {
            ensure!(
                generation == self::generation(&directory)?,
                "terminal finish generation changed"
            );
            if let Some(saved) = saved(&directory)? {
                validate_saved(&host, &saved)?;
                let state = saved.caller.state()?;
                if state != ProcessState::Live {
                    let current = Settings::read(original.as_raw_fd())?;
                    let raw = saved.configuration.settings.raw()?;
                    if state == ProcessState::Gone
                        && saved.generation == generation
                        && (current.same_effective_mode(&raw)
                            || current.same_effective_mode(&raw.zsh_resumed()))
                    {
                        saved.configuration.settings.apply(original.as_raw_fd())?;
                    }
                    // Changed modes/generations and reused PIDs never authorize
                    // restoration. Retire only this shell's identified lease.
                    std::fs::remove_file(directory.join("lease.json"))?;
                }
            }
        }
        Action::Unregister { .. } => {
            if let Some(saved) = saved(&directory)? {
                validate_saved(&host, &saved)?;
                ensure!(
                    saved.caller.state()? != ProcessState::Live,
                    "terminal recovery is still owned by a live caller"
                );
            }
            let entries = std::fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
            ensure!(
                entries.len() <= 4
                    && entries.iter().all(|entry| matches!(
                        entry.file_name().to_str(),
                        Some("host.json" | "host.lock" | "lease.json" | "generation")
                    )),
                "terminal host has unknown content"
            );
            for entry in entries {
                let metadata = std::fs::symlink_metadata(entry.path())?;
                ensure!(
                    metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
                    "terminal host content changed"
                );
                std::fs::remove_file(entry.path())?;
            }
            std::fs::remove_dir(directory)?;
        }
        _ => unreachable!("initialization handled above"),
    }
    Ok(())
}
