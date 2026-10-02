//! Controlling-terminal relay for a Cargo process in its guardian's session.

#![allow(unsafe_code)] // Confined termios, PTY, session, and descriptor operations.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const BUFFER_LIMIT: usize = 64 * 1024;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Settings {
    input: u64,
    output: u64,
    control: u64,
    local: u64,
    characters: Vec<u8>,
    input_speed: u64,
    output_speed: u64,
}

impl Settings {
    fn read(fd: i32) -> Result<Self> {
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

    fn native(&self) -> Result<libc::termios> {
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

    fn raw(&self) -> Result<Self> {
        let mut value = self.native()?;
        unsafe { libc::cfmakeraw(&mut value) };
        Ok(Self::from_native(&value))
    }

    fn apply(&self, fd: i32) -> Result<()> {
        ensure!(
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.native()?) } == 0,
            "setting terminal mode failed"
        );
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Configuration {
    settings: Settings,
    owner_group: i32,
    shell_group: i32,
    pub(super) foreground: bool,
}

fn window(fd: i32) -> Result<libc::winsize> {
    let mut size = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0,
        "reading terminal size failed"
    );
    Ok(size)
}

fn set_nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "setting PTY nonblocking failed"
    );
    Ok(())
}

fn read_once(file: &mut File, buffer: &mut Vec<u8>) -> Result<bool> {
    let mut bytes = [0_u8; 8192];
    let available = (BUFFER_LIMIT - buffer.len()).min(bytes.len());
    if available == 0 {
        return Ok(false);
    }
    match file.read(&mut bytes[..available]) {
        Ok(0) => Ok(false),
        Ok(length) => {
            buffer.extend_from_slice(&bytes[..length]);
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn write_once(file: &mut File, buffer: &mut Vec<u8>) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    match file.write(buffer) {
        Ok(0) => anyhow::bail!("terminal relay stopped accepting output"),
        Ok(length) => {
            buffer.drain(..length);
            Ok(())
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) struct Caller {
    original: File,
    master: Option<File>,
    configuration: Configuration,
    raw: Settings,
    raw_active: bool,
    last_foreground: Option<bool>,
    input: Vec<u8>,
    output: Vec<u8>,
}

impl Caller {
    pub(super) fn open() -> Result<Option<Self>> {
        let Some(first) = (0..=2).find(|fd| unsafe { libc::isatty(*fd) } != 0) else {
            return Ok(None);
        };
        let mut name = [0_i8; 1024];
        ensure!(
            unsafe { libc::ttyname_r(first, name.as_mut_ptr(), name.len()) } == 0,
            "identifying original terminal failed"
        );
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_bytes()
            .to_vec();
        let original = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(std::ffi::OsString::from_vec(name))?;
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open("/dev/tty")?;
        let foreground = unsafe { libc::tcgetpgrp(original.as_raw_fd()) };
        ensure!(
            foreground > 0 && foreground == unsafe { libc::tcgetpgrp(control.as_raw_fd()) },
            "standard terminal is not the controlling terminal"
        );
        let identity = original.metadata()?.rdev();
        for fd in 0..=2 {
            if unsafe { libc::isatty(fd) } != 0 {
                let mut metadata = unsafe { std::mem::zeroed() };
                ensure!(
                    unsafe { libc::fstat(fd, &mut metadata) } == 0
                        && metadata.st_rdev as u64 == identity,
                    "standard streams use different terminals"
                );
            }
        }
        let settings = Settings::read(original.as_raw_fd())?;
        let raw = settings.raw()?;
        Ok(Some(Self {
            original,
            master: None,
            configuration: Configuration {
                settings,
                owner_group: unsafe { libc::getpgrp() },
                shell_group: unsafe { libc::getpgid(libc::getppid()) },
                foreground: false,
            },
            raw,
            raw_active: false,
            last_foreground: None,
            input: Vec::new(),
            output: Vec::new(),
        }))
    }

    pub(super) fn fd(&self) -> i32 {
        self.original.as_raw_fd()
    }
    pub(super) fn attach(&mut self, master: OwnedFd) {
        self.master = Some(File::from(master));
    }
    pub(super) fn configuration(&mut self) -> Result<Configuration> {
        if self.raw_active {
            return Ok(self.configuration.clone());
        }
        self.configuration.settings = Settings::read(self.fd())?;
        self.raw = self.configuration.settings.raw()?;
        self.configuration.foreground = self.foreground();
        Ok(self.configuration.clone())
    }

    pub(super) fn foreground(&self) -> bool {
        unsafe { libc::tcgetpgrp(self.fd()) == self.configuration.owner_group }
    }

    pub(super) fn activate(&mut self) -> Result<()> {
        ensure!(
            Settings::read(self.fd())? == self.configuration.settings,
            "terminal changed during preparation"
        );
        if self.foreground() {
            self.raw.apply(self.fd())?;
            self.raw_active = true;
        }
        self.last_foreground = Some(self.foreground());
        self.resize()
    }

    pub(super) fn restore(&mut self) {
        if self.raw_active && Settings::read(self.fd()).is_ok_and(|current| current == self.raw) {
            let _ = self.configuration.settings.apply(self.fd());
        }
        self.raw_active = false;
        self.last_foreground = None;
    }

    pub(super) fn resize(&self) -> Result<()> {
        let size = window(self.fd())?;
        ensure!(
            unsafe {
                libc::ioctl(
                    self.master
                        .as_ref()
                        .context("PTY master is missing")?
                        .as_raw_fd(),
                    libc::TIOCSWINSZ,
                    &size,
                )
            } == 0,
            "resizing Cargo terminal failed"
        );
        Ok(())
    }

    pub(super) fn changed_foreground(&mut self) -> Result<Option<bool>> {
        let foreground = self.foreground();
        if self.last_foreground == Some(foreground) {
            return Ok(None);
        }
        if foreground && !self.raw_active {
            // Re-capture shell settings after a stop, then notify the guardian
            // before re-entering relay mode (the caller handles that ordering).
            self.configuration()?;
        } else if !foreground {
            self.restore();
        }
        self.last_foreground = Some(foreground);
        Ok(Some(foreground))
    }

    pub(super) fn pump(&mut self, read_input: bool, timeout: i32) -> Result<()> {
        let master = self.master.as_mut().context("PTY master is missing")?;
        let mut descriptors = [
            libc::pollfd {
                fd: self.original.as_raw_fd(),
                events: (if read_input && self.input.len() < BUFFER_LIMIT {
                    libc::POLLIN
                } else {
                    0
                }) | if self.output.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            },
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: (if self.output.len() < BUFFER_LIMIT {
                    libc::POLLIN
                } else {
                    0
                }) | if self.input.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            read_once(&mut self.original, &mut self.input)?;
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            read_once(master, &mut self.output)?;
        }
        write_once(master, &mut self.input)?;
        write_once(&mut self.original, &mut self.output)?;
        Ok(())
    }

    pub(super) fn drain_output(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            let read = read_once(
                self.master.as_mut().context("PTY master is missing")?,
                &mut self.output,
            )?;
            write_once(&mut self.original, &mut self.output)?;
            if !read && self.output.is_empty() {
                return Ok(());
            }
            // A surviving descendant may write indefinitely. The guardian
            // takes over the master after the control socket disconnects.
            if std::time::Instant::now() >= deadline && self.output.is_empty() {
                return Ok(());
            }
            self.pump(false, 10)?;
        }
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        self.restore();
    }
}

struct IgnoredHangup(libc::sigaction);
impl IgnoredHangup {
    fn new() -> Result<Self> {
        let mut previous = unsafe { std::mem::zeroed() };
        let mut ignored: libc::sigaction = unsafe { std::mem::zeroed() };
        ignored.sa_sigaction = libc::SIG_IGN;
        unsafe { libc::sigemptyset(&mut ignored.sa_mask) };
        ensure!(
            unsafe { libc::sigaction(libc::SIGHUP, &ignored, &mut previous) } == 0,
            "cannot protect terminal guardian from PTY hangup"
        );
        Ok(Self(previous))
    }
}
impl Drop for IgnoredHangup {
    fn drop(&mut self) {
        unsafe { libc::sigaction(libc::SIGHUP, &self.0, std::ptr::null_mut()) };
    }
}

pub(super) struct Guardian {
    original: File,
    master: File,
    slave: File,
    configuration: Option<Configuration>,
    output: Vec<u8>,
    _hangup: IgnoredHangup,
}

impl Guardian {
    pub(super) fn new(original: OwnedFd) -> Result<Self> {
        let original = File::from(original);
        let pid = unsafe { libc::getpid() };
        let session = unsafe { libc::getsid(0) };
        ensure!(session > 0, "cannot identify guardian session");
        if session != pid {
            if unsafe { libc::getpgrp() } == pid {
                let parent = unsafe { libc::getppid() };
                let group = unsafe { libc::getpgid(parent) };
                ensure!(
                    group > 0 && group != pid && unsafe { libc::getsid(parent) } == session,
                    "cannot establish guardian session anchor"
                );
                ensure!(
                    unsafe { libc::setpgid(0, group) } == 0,
                    "moving guardian process group failed"
                );
            }
            ensure!(
                unsafe { libc::setsid() } == pid,
                "creating guardian terminal session failed"
            );
        }
        let mut settings = Settings::read(original.as_raw_fd())?.native()?;
        let mut size = window(original.as_raw_fd())?;
        let mut master = -1;
        let mut slave = -1;
        ensure!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    &mut settings,
                    &mut size,
                )
            } == 0,
            "creating Cargo terminal failed"
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            ensure!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "setting PTY descriptor ownership failed"
            );
        }
        set_nonblocking(master.as_raw_fd())?;
        let hangup = IgnoredHangup::new()?;
        ensure!(
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY.into(), 0) } == 0,
            "adopting Cargo controlling terminal failed"
        );
        Ok(Self {
            original,
            master,
            slave,
            configuration: None,
            output: Vec::new(),
            _hangup: hangup,
        })
    }

    pub(super) fn master_fd(&self) -> i32 {
        self.master.as_raw_fd()
    }
    pub(super) fn slave_fd(&self) -> i32 {
        self.slave.as_raw_fd()
    }
    pub(super) fn replace_stdio(&self, descriptors: &mut [OwnedFd; 3]) -> Result<()> {
        for descriptor in descriptors {
            if unsafe { libc::isatty(descriptor.as_raw_fd()) } != 0 {
                *descriptor = self.slave.try_clone()?.into();
            }
        }
        Ok(())
    }

    pub(super) fn configure(&mut self, configuration: Configuration, initial: bool) -> Result<()> {
        ensure!(
            configuration.owner_group > 0,
            "invalid terminal owner group"
        );
        if initial {
            ensure!(
                Settings::read(self.original.as_raw_fd())? == configuration.settings,
                "original terminal changed during handshake"
            );
        }
        // Shell-mode updates do not overwrite a running application's inner
        // terminal modes. Only the initial PTY receives the shell configuration.
        if initial {
            configuration.settings.apply(self.slave_fd())?;
        }
        self.configuration = Some(configuration);
        Ok(())
    }

    pub(super) fn foreground(&self, group: i32) -> Result<()> {
        let mut blocked = unsafe { std::mem::zeroed() };
        let mut previous = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTTOU);
        }
        ensure!(
            unsafe { libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) } == 0,
            "blocking terminal group signal failed"
        );
        let result = unsafe { libc::tcsetpgrp(self.slave_fd(), group) };
        let error = std::io::Error::last_os_error();
        let restored =
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
        ensure!(restored == 0, "restoring guardian signal mask failed");
        if result != 0 {
            return Err(error).context("setting Cargo terminal foreground group");
        }
        Ok(())
    }

    pub(super) fn restore_original(&self) {
        if let Some(configuration) = &self.configuration {
            let foreground = unsafe { libc::tcgetpgrp(self.original.as_raw_fd()) };
            if (foreground == configuration.owner_group || foreground == configuration.shell_group)
                && configuration.settings.raw().is_ok_and(|raw| {
                    Settings::read(self.original.as_raw_fd()).is_ok_and(|current| current == raw)
                })
            {
                let _ = configuration.settings.apply(self.original.as_raw_fd());
            }
        }
    }

    pub(super) fn pump_output(&mut self) -> Result<()> {
        read_once(&mut self.master, &mut self.output)?;
        write_once(&mut self.original, &mut self.output)
    }

    pub(super) fn output_events(&self) -> [libc::pollfd; 2] {
        [
            libc::pollfd {
                fd: self.master_fd(),
                events: if self.output.len() < BUFFER_LIMIT {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: self.original.as_raw_fd(),
                events: if self.output.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            },
        ]
    }

    pub(super) fn output_pending(&self) -> bool {
        !self.output.is_empty()
    }
}

impl Drop for Guardian {
    fn drop(&mut self) {
        self.restore_original();
    }
}
