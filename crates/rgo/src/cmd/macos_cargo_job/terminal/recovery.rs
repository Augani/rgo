//! Explicitly registered zsh recovery pilot. Its terminal lease is independent
//! of Cargo job retirement; it never authorizes deleting build storage.

use super::{Configuration, Settings, open_original};
use anyhow::{Context, Result, ensure};
use rgo_core::macos_coalition::boot_session;
use rgo_core::macos_terminal_hosts::{
    Host, Process, ProcessState, Saved, Terminal, directory, generation, lock, private_directory,
    read, saved, write,
};
use rgo_core::paths::RgoPaths;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
    rgo_core::macos_terminal_hosts::validate_saved(host, saved)?;
    ensure!(
        saved.configuration.shell_group == unsafe { libc::getpgid(host.shell.pid) },
        "terminal lease identity changed"
    );
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
    if let Action::Unregister { token } = &action {
        ensure!(
            unsafe { libc::tcgetpgrp(original.as_raw_fd()) } == unsafe { libc::getpgrp() },
            "terminal host operation is not foreground"
        );
        return rgo_core::macos_terminal_hosts::retire(
            &paths,
            token,
            Some((&Process::current_parent()?, &Terminal::read(&original)?)),
        );
    }
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
        let _lock = rgo_core::macos_terminal_hosts::registration_lock(&directory)?;
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
        _ => unreachable!("initialization handled above"),
    }
    Ok(())
}
