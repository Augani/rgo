//! Per-invocation launchd Cargo guardian. Private pilot until terminal, recovery,
//! and supported-runtime gates are complete. No rustup proxy is replaced.

#![allow(unsafe_code)] // macOS descriptor passing, peer credentials, and signals.

use anyhow::{Context, Result, bail, ensure};
use rgo_core::macos_coalition::ResourceCoalition;
use rgo_core::macos_jobs::{self, CargoJobOwner as Owner};
use rgo_core::paths::RgoPaths;
use rgo_core::supervision::{self, SessionGuard};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

mod events;
mod terminal;
mod wake;

pub use terminal::recovery::Action as TerminalHostAction;

pub fn terminal_host(action: TerminalHostAction, home: Option<&Path>) -> Result<()> {
    terminal::recovery::run(action, home)
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const SOCKET_NAME: &str = "control.sock";
const FORWARDED_SIGNALS: [i32; 7] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGCONT,
    libc::SIGWINCH,
];
static SIGNALS: AtomicU32 = AtomicU32::new(0);
static SIGNAL_WAKE: AtomicI32 = AtomicI32::new(-1);

#[derive(Serialize, Deserialize)]
struct Invocation {
    executable: Vec<u8>,
    args: Vec<Vec<u8>>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    directory: Vec<u8>,
    root: PathBuf,
    context: PathBuf,
    #[serde(default)]
    terminal: bool,
    #[serde(default)]
    framed_control: bool,
    #[serde(default)]
    signals: events::NativeSignals,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Greeting {
        token: String,
    },
    Prepared {
        coalition: u64,
    },
    Configured,
    Started {
        pid: u32,
    },
    Stopped,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Failed {
        detail: String,
    },
}

#[derive(Serialize, Deserialize)]
enum Control {
    Signal(i32),
    Terminal(terminal::Configuration),
    Foreground(bool),
}

#[derive(Default)]
struct Controls(Vec<u8>);

impl Controls {
    fn read(&mut self, stream: &mut UnixStream, framed: bool) -> Result<Option<Vec<Control>>> {
        let mut bytes = [0_u8; 4096];
        let length = match stream.read(&mut bytes) {
            Ok(0) => return Ok(None),
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(Some(Vec::new()));
            }
            Err(error) => return Err(error.into()),
        };
        if !framed {
            return Ok(Some(
                bytes[..length]
                    .iter()
                    .map(|signal| Control::Signal(i32::from(*signal)))
                    .collect(),
            ));
        }
        self.0.extend_from_slice(&bytes[..length]);
        ensure!(self.0.len() <= 8192, "Cargo control buffer is too large");
        let mut commands = Vec::new();
        while self.0.len() >= 4 {
            let length = u32::from_be_bytes(self.0[..4].try_into()?) as usize;
            ensure!(length <= 4096, "Cargo control frame is too large");
            if self.0.len() < 4 + length {
                break;
            }
            commands.push(serde_json::from_slice(&self.0[4..4 + length])?);
            self.0.drain(..4 + length);
        }
        Ok(Some(commands))
    }
}

fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= rgo_protocol::MAX_FRAME_SIZE,
        "Cargo job request is too large"
    );
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length <= rgo_protocol::MAX_FRAME_SIZE,
        "Cargo job frame is too large"
    );
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn check_peer(stream: &UnixStream) -> Result<()> {
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    ensure!(
        result == 0 && uid == unsafe { libc::geteuid() },
        "Cargo job peer is not this user"
    );
    Ok(())
}

fn send_descriptors(stream: &UnixStream, descriptors: &[i32]) -> Result<()> {
    ensure!(
        (1..=4).contains(&descriptors.len()),
        "invalid descriptor transfer count"
    );
    for &descriptor in descriptors {
        ensure!(
            unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0,
            "Cargo job cannot transfer a closed standard descriptor"
        );
    }
    let mut marker = *b"F";
    let mut io = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0_u64; 16];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut io;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(descriptors) as u32) };
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(descriptors) as u32);
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(header),
            std::mem::size_of_val(descriptors),
        );
    }
    loop {
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) };
        if sent == 1 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if sent < 0 && error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error).context("sending Cargo standard descriptors");
    }
}

fn receive_descriptors(stream: &UnixStream, expected: usize) -> Result<Vec<OwnedFd>> {
    ensure!(
        (1..=4).contains(&expected),
        "invalid descriptor receive count"
    );
    let mut marker = [0_u8; 1];
    let mut io = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0_u64; 16];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut io;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as u32;
    let received = loop {
        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        if received < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        break received;
    };
    if received < 0 {
        return Err(std::io::Error::last_os_error()).context("receiving Cargo descriptors");
    }
    // Adopt every delivered descriptor before validation, so errors close them.
    let mut files = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*header)
                    .cmsg_len
                    .checked_sub(libc::CMSG_LEN(0))
                    .context("invalid descriptor header")? as usize;
                ensure!(
                    payload % std::mem::size_of::<i32>() == 0,
                    "invalid descriptor payload"
                );
                for index in 0..payload / std::mem::size_of::<i32>() {
                    let descriptor =
                        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>().add(index));
                    ensure!(descriptor >= 0, "invalid received descriptor");
                    files.push(OwnedFd::from_raw_fd(descriptor));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    ensure!(
        received == 1
            && marker == *b"F"
            && message.msg_flags & libc::MSG_CTRUNC == 0
            && files.len() == expected,
        "Cargo job received an unexpected descriptor count"
    );
    for file in &files {
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "cannot close Cargo descriptor on guardian exec"
        );
    }
    Ok(files)
}

// Relative Unix socket names avoid macOS's short sockaddr path limit even when
// the owned storage root has a long path. Restore the actual directory handle,
// which also works if the caller's working directory was renamed meanwhile.
fn in_directory<T>(directory: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let saved = File::open(".")?;
    std::env::set_current_dir(directory)?;
    let result = operation();
    ensure!(
        unsafe { libc::fchdir(saved.as_raw_fd()) } == 0,
        "restoring Cargo working directory failed"
    );
    result
}

struct EarlyCleanup<'a> {
    directory: &'a Path,
    owner: &'a Owner,
    bytes: &'a [u8],
    safe_to_reap: bool,
}

fn sync_pair(first: File, second: File) -> Result<()> {
    // Independent flushes can overlap, but both must succeed before proceeding
    // to namespace durability or bootstrap. Never skip a required flush.
    std::thread::scope(|scope| {
        let pending = std::thread::Builder::new().spawn_scoped(scope, move || first.sync_all())?;
        let last = second.sync_all();
        let first = pending
            .join()
            .map_err(|_| anyhow::anyhow!("Cargo metadata sync worker panicked"))?;
        first?;
        last?;
        Ok(())
    })
}

impl Drop for EarlyCleanup<'_> {
    fn drop(&mut self) {
        if self.safe_to_reap {
            let _ = macos_jobs::cleanup(self.directory, self.owner, self.bytes);
        }
    }
}

pub(super) struct PreparedJob {
    stream: UnixStream,
    terminal: Option<terminal::Caller>,
    signals: Option<SignalRelay>,
}

impl PreparedJob {
    /// Prepare only. Cargo cannot start until `run` sends its one-byte commit.
    pub(super) fn prepare(
        executable: &Path,
        args: &[OsString],
        paths: &RgoPaths,
        context: &Path,
    ) -> Result<Self> {
        let started = Instant::now();
        let deadline = started + STARTUP_TIMEOUT;
        let mut terminal = terminal::Caller::open(paths)?;
        paths.ensure_layout()?;
        let invocation = Invocation {
            executable: executable.as_os_str().as_bytes().to_vec(),
            args: args.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
            environment: std::env::vars_os()
                .map(|(key, value)| (key.as_bytes().to_vec(), value.as_bytes().to_vec()))
                .collect(),
            directory: std::env::current_dir()?.as_os_str().as_bytes().to_vec(),
            root: paths.root.clone(),
            context: context.to_owned(),
            terminal: terminal.is_some(),
            framed_control: true,
            signals: events::NativeSignals::capture()?,
        };
        // Serialize before registering any job, including the frame size check.
        ensure!(
            serde_json::to_vec(&invocation)?.len() <= rgo_protocol::MAX_FRAME_SIZE,
            "Cargo job request is too large"
        );
        let temporary = tempfile::Builder::new()
            .prefix("macos-cargo-job-")
            .tempdir_in(paths.state_dir())?;
        // The parent state directory is already private; enforce the same
        // ownership boundary on the new per-job directory before publishing it.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
        // Preserve unknown or partial content on later preparation failure.
        // TempDir's recursive destructor cannot establish that ownership.
        let directory = temporary.keep();
        let listener = in_directory(&directory, || Ok(UnixListener::bind(SOCKET_NAME)?))?;
        listener.set_nonblocking(true)?;
        let mut entropy = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let token = blake3::hash(&entropy).to_hex().to_string();
        let owner = Owner::new(
            &directory,
            std::env::current_exe()?,
            context.to_owned(),
            token,
        )?;
        let bytes = serde_json::to_vec(&owner)?;
        let mut records = Vec::new();
        for (name, data) in [
            ("owner.json", bytes.as_slice()),
            ("job.plist", owner.definition.as_bytes()),
        ] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))?;
            file.write_all(data)?;
            records.push(file);
        }
        let [owner_record, definition]: [File; 2] = records
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cargo job metadata records are missing"))?;
        sync_pair(owner_record, definition)?;
        sync_pair(File::open(&directory)?, File::open(paths.state_dir())?)?;
        let files_ready = started.elapsed();
        // The guardian owns this directory now, including after the caller exits.
        let mut early_cleanup = EarlyCleanup {
            directory: &directory,
            owner: &owner,
            bytes: &bytes,
            safe_to_reap: true,
        };
        macos_jobs::bootstrap(&directory, &owner, &bytes, deadline)?;
        let bootstrapped = started.elapsed();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    let mut readiness = libc::pollfd {
                        fd: listener.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(i32::MAX as u128) as i32;
                    let ready = unsafe { libc::poll(&mut readiness, 1, remaining.max(1)) };
                    if ready < 0
                        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        return Err(std::io::Error::last_os_error())
                            .context("waiting for Cargo guardian readiness");
                    }
                }
                Err(error) => {
                    let diagnostic = std::fs::read_to_string(directory.join("guardian.stderr"))
                        .unwrap_or_default();
                    // No request or commit was sent, so this job cannot have
                    // started Cargo or registered a managed context.
                    bail!("waiting for Cargo guardian: {error}; {diagnostic}");
                }
            }
        };
        // Darwin accept inherits the listener's O_NONBLOCK state. Only the
        // accept loop is nonblocking; framed startup reads have a deadline.
        stream.set_nonblocking(false)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Cargo guardian startup timed out");
        stream.set_read_timeout(Some(remaining))?;
        stream.set_write_timeout(Some(remaining))?;
        check_peer(&stream)?;
        ensure!(
            matches!(read_frame::<Message>(&mut stream)?, Message::Greeting { token } if token == owner.token),
            "Cargo guardian authentication failed"
        );
        let connected = started.elapsed();
        // From the first request byte the guardian may publish a receipt. It
        // alone decides when that receipt and loaded job can be retired.
        early_cleanup.safe_to_reap = false;
        write_frame(&mut stream, &invocation)?;
        let mut descriptors = vec![0, 1, 2];
        if let Some(terminal) = &terminal {
            descriptors.push(terminal.fd());
        }
        send_descriptors(&stream, &descriptors)?;
        let coalition = match read_frame::<Message>(&mut stream)? {
            Message::Prepared { coalition } => coalition,
            Message::Failed { detail } => {
                bail!("Cargo guardian cannot admit the invocation: {detail}")
            }
            _ => bail!("Cargo guardian returned an invalid startup reply"),
        };
        ensure!(
            coalition != ResourceCoalition::for_pid(std::process::id().try_into()?)?.id(),
            "Cargo guardian inherited its caller's coalition"
        );
        let signals = SignalRelay::new()?;
        if let Some(terminal) = &mut terminal {
            terminal.attach(
                receive_descriptors(&stream, 1)?
                    .pop()
                    .context("PTY descriptor is missing")?,
            );
            stream.write_all(b"T")?;
            write_frame(&mut stream, &terminal.configuration()?)?;
            ensure!(
                matches!(read_frame::<Message>(&mut stream)?, Message::Configured),
                "Cargo terminal was not configured"
            );
            terminal.activate()?;
        }
        tracing::debug!(
            files_ms = files_ready.as_secs_f64() * 1000.0,
            bootstrap_ms = (bootstrapped - files_ready).as_secs_f64() * 1000.0,
            connection_ms = (connected - bootstrapped).as_secs_f64() * 1000.0,
            admission_ms = (started.elapsed() - connected).as_secs_f64() * 1000.0,
            "macOS Cargo guardian preparation"
        );
        Ok(Self {
            stream,
            terminal,
            signals: Some(signals),
        })
    }

    pub(super) fn run(mut self, mut session: SessionGuard) -> Result<()> {
        session.release_local_for_macos_guardian();
        self.stream.set_read_timeout(None)?;
        let committed = Instant::now();
        self.stream.write_all(b"S")?; // From here failure cannot retry Cargo.
        let mut reader = self.stream.try_clone()?;
        let wake = wake::Wake::get()?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let message = read_frame::<Message>(&mut reader);
                let terminal = !matches!(message, Ok(Message::Started { .. } | Message::Stopped));
                if sender.send(message).is_err() {
                    break;
                }
                wake.notify();
                if terminal {
                    break;
                }
            }
        });
        loop {
            wake.drain()?;
            if let Some(terminal) = &mut self.terminal {
                if let Some(foreground) = terminal.changed_foreground()? {
                    if foreground {
                        write_frame(
                            &mut self.stream,
                            &Control::Terminal(terminal.configuration()?),
                        )?;
                    }
                    write_frame(&mut self.stream, &Control::Foreground(foreground))?;
                    if foreground {
                        terminal.activate()?;
                    }
                }
            }
            let pending = SIGNALS.swap(0, Ordering::AcqRel);
            for signal in FORWARDED_SIGNALS {
                if pending & (1 << signal) != 0 {
                    match (signal, self.terminal.as_ref()) {
                        (libc::SIGWINCH, Some(terminal)) => terminal.resize()?,
                        _ => write_frame(&mut self.stream, &Control::Signal(signal))?,
                    }
                }
            }
            match receiver.try_recv() {
                Ok(Ok(Message::Started { .. })) => {
                    tracing::debug!(
                        elapsed_ms = committed.elapsed().as_secs_f64() * 1000.0,
                        "macOS Cargo guardian started primary"
                    );
                }
                Ok(Ok(Message::Stopped)) => {
                    if let Some(terminal) = &mut self.terminal {
                        terminal.restore();
                    }
                    // Reflect an observed primary stop in the shell's actual
                    // job. SIGCONT after `fg`/`bg` is relayed on the next loop.
                    unsafe { libc::raise(libc::SIGSTOP) };
                }
                Ok(Ok(Message::Exited { code, signal })) => {
                    tracing::debug!(
                        elapsed_ms = committed.elapsed().as_secs_f64() * 1000.0,
                        "macOS Cargo guardian primary result"
                    );
                    if let Some(terminal) = &mut self.terminal {
                        terminal.drain_output()?;
                        terminal.restore();
                    }
                    drop(session);
                    drop(self.signals.take());
                    if let Some(signal) = signal {
                        unsafe {
                            libc::signal(signal, libc::SIG_DFL);
                            libc::raise(signal);
                        }
                    }
                    std::process::exit(code.unwrap_or(128 + signal.unwrap_or(1)));
                }
                Ok(Ok(Message::Failed { detail })) => {
                    bail!("Cargo guardian failed after commit: {detail}")
                }
                Ok(Ok(_)) => bail!("Cargo guardian returned an invalid running reply"),
                Ok(Err(error)) => {
                    return Err(error).context("Cargo guardian disconnected after commit");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if let Some(terminal) = &mut self.terminal {
                        // Only relay after the handoff has been configured.
                        // The live foreground group may change between the
                        // top-of-loop check and this poll.
                        let foreground = terminal.relaying_input();
                        // zsh's `fg` does not send SIGCONT to an already
                        // running job. Discover that terminal handoff while
                        // backgrounded; foreground replies/signals wake us.
                        terminal.pump(
                            foreground,
                            if foreground { -1 } else { 100 },
                            Some(wake.reader()),
                        )?;
                    } else {
                        wake.wait()?;
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

extern "C" fn capture_signal(signal: i32) {
    let saved_errno = unsafe { *libc::__error() };
    if (0..32).contains(&signal) {
        SIGNALS.fetch_or(1 << signal, Ordering::Release);
        let fd = SIGNAL_WAKE.load(Ordering::Relaxed);
        if fd >= 0 {
            // Only async-signal-safe operations; the nonblocking pipe remains
            // open for this process's lifetime, including handler teardown.
            unsafe { libc::write(fd, b"S".as_ptr().cast(), 1) };
        }
    }
    unsafe { *libc::__error() = saved_errno };
}

struct SignalRelay(Vec<(i32, libc::sigaction)>);

impl SignalRelay {
    fn new() -> Result<Self> {
        SIGNALS.store(0, Ordering::Relaxed);
        SIGNAL_WAKE.store(wake::Wake::get()?.writer(), Ordering::Relaxed);
        let mut relay = Self(Vec::new());
        for signal in FORWARDED_SIGNALS {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = capture_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            let mut previous = unsafe { std::mem::zeroed() };
            ensure!(
                unsafe { libc::sigaction(signal, &action, &mut previous) } == 0,
                "cannot relay Cargo signal"
            );
            relay.0.push((signal, previous));
        }
        Ok(relay)
    }
}

impl Drop for SignalRelay {
    fn drop(&mut self) {
        SIGNAL_WAKE.store(-1, Ordering::Relaxed);
        for (signal, previous) in &self.0 {
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

fn signal_group(child: &std::process::Child, signal: i32) {
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}

fn send_exit(stream: &mut UnixStream, status: ExitStatus) {
    let _ = write_frame(
        stream,
        &Message::Exited {
            code: status.code(),
            signal: status.signal(),
        },
    );
}

enum ChildChange {
    Exited(ExitStatus),
    Stopped,
    None,
}

fn poll_child(child: &std::process::Child) -> Result<ChildChange> {
    let mut status = 0;
    let changed = unsafe {
        libc::waitpid(
            child.id().try_into()?,
            &mut status,
            libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
        )
    };
    if changed < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(ChildChange::None);
        }
        return Err(error).context("observing Cargo process state");
    }
    Ok(if changed == 0 || libc::WIFCONTINUED(status) {
        ChildChange::None
    } else if libc::WIFSTOPPED(status) {
        ChildChange::Stopped
    } else {
        ChildChange::Exited(ExitStatus::from_raw(status))
    })
}

/// Private launchd entry point. Only the authenticated one-use socket supplies
/// an invocation; restarting its old definition cannot start another Cargo.
pub fn guardian(directory: &Path, token: &str, context: Option<&Path>) -> Result<()> {
    ensure!(
        token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Cargo job token"
    );
    let (owner, bytes) = macos_jobs::read_owner(directory)?;
    ensure!(
        owner.token == token
            && context == Some(owner.context.as_path())
            && owner.label == format!("com.rgo.cargo.{}", &token[..32])
            && owner.domain == format!("gui/{}", unsafe { libc::geteuid() }),
        "Cargo job owner does not match its invocation"
    );
    macos_jobs::verify_owner(directory, &owner, &bytes)?;
    let mut early_cleanup = EarlyCleanup {
        directory,
        owner: &owner,
        bytes: &bytes,
        safe_to_reap: true,
    };
    let mut stream = in_directory(directory, || Ok(UnixStream::connect(SOCKET_NAME)?))?;
    check_peer(&stream)?;
    stream.set_read_timeout(Some(STARTUP_TIMEOUT))?;
    stream.set_write_timeout(Some(STARTUP_TIMEOUT))?;
    write_frame(
        &mut stream,
        &Message::Greeting {
            token: token.to_owned(),
        },
    )?;
    let request = read_frame::<Invocation>(&mut stream)?;
    let mut descriptors = receive_descriptors(&stream, if request.terminal { 4 } else { 3 })?;
    let mut terminal = if request.terminal {
        Some(terminal::Guardian::new(
            descriptors.pop().context("original terminal is missing")?,
        )?)
    } else {
        None
    };
    let mut descriptors: [OwnedFd; 3] = descriptors
        .try_into()
        .map_err(|_| anyhow::anyhow!("standard descriptors are missing"))?;
    if let Some(terminal) = &terminal {
        terminal.replace_stdio(&mut descriptors)?;
    }
    let [stdin, stdout, stderr] = descriptors;
    let paths = RgoPaths { root: request.root };
    ensure!(
        paths.root.is_absolute()
            && directory.parent() == Some(paths.state_dir().as_path())
            && request.context == owner.context,
        "Cargo job scope must be absolute"
    );
    let mut session = supervision::lock_cargo_session(&paths, Some(&request.context))?;
    // Recovery can fence a startup waiting for this scope. Revalidate after
    // acquiring it, before publishing any receipt or accepting a commit.
    macos_jobs::verify_owner(directory, &owner, &bytes)?;
    early_cleanup.safe_to_reap = false;
    let admission = session.record_macos_coalition();
    if let Err(error) = admission {
        let _ = write_frame(
            &mut stream,
            &Message::Failed {
                detail: format!("{error:#}"),
            },
        );
        // A sync failure can leave a receipt behind. Retire it before reaping,
        // or retain the loaded job so zero tasks can still be queried later.
        if session.finish_macos_coalition().is_ok() {
            early_cleanup.safe_to_reap = true;
        }
        return Err(error);
    }
    let coalition = ResourceCoalition::for_pid(std::process::id().try_into()?)?;
    session.release_local_for_macos_guardian();
    let result = (|| -> Result<()> {
        write_frame(
            &mut stream,
            &Message::Prepared {
                coalition: coalition.id(),
            },
        )?;
        let mut initially_foreground = false;
        if let Some(terminal) = &mut terminal {
            send_descriptors(&stream, &[terminal.master_fd()])?;
            let mut marker = [0_u8];
            stream.read_exact(&mut marker)?;
            ensure!(
                marker == *b"T",
                "Cargo terminal configuration was not supplied"
            );
            let configuration = read_frame::<terminal::Configuration>(&mut stream)?;
            initially_foreground = configuration.foreground;
            terminal.configure(configuration, true)?;
            write_frame(&mut stream, &Message::Configured)?;
        }
        let mut commit = [0_u8];
        stream.read_exact(&mut commit)?;
        ensure!(commit == *b"S", "Cargo job was not committed");
        stream.set_read_timeout(None)?;
        stream.set_nonblocking(true)?;
        session.retain_across_exec()?;
        let mut events = events::ChildEvents::new()?;
        let slave = terminal.as_ref().map(terminal::Guardian::slave_fd);
        let mut command = Command::new(OsString::from_vec(request.executable));
        command
            .args(request.args.into_iter().map(OsString::from_vec))
            .current_dir(PathBuf::from(OsString::from_vec(request.directory)))
            .env_clear()
            .envs(
                request
                    .environment
                    .into_iter()
                    .map(|(key, value)| (OsString::from_vec(key), OsString::from_vec(value))),
            )
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0);
        // A pre_exec callback forces Command off Darwin's posix_spawn path.
        // Exec already resets caught handlers. Pipes need no callback when
        // the remaining inherited mask/ignored dispositions match the caller.
        if slave.is_some() || request.signals != events::NativeSignals::capture()? {
            unsafe {
                command.pre_exec(move || {
                    if let Some(slave) = slave.filter(|_| initially_foreground) {
                        // Command has established the child's process group. Make
                        // it foreground before exec can read its controlling PTY.
                        let mut blocked = std::mem::zeroed();
                        libc::sigemptyset(&mut blocked);
                        libc::sigaddset(&mut blocked, libc::SIGTTOU);
                        if libc::sigprocmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()) != 0
                            || libc::tcsetpgrp(slave, libc::getpgrp()) != 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    request.signals.restore_in_child()
                });
            }
        }
        let child = command
            .spawn()
            .context("starting real Cargo in its launchd coalition")?;
        // Command retains its configured Stdio after spawn. Closing those
        // parent copies lets pipe readers observe EOF at primary completion.
        drop(command);
        let _ = write_frame(&mut stream, &Message::Started { pid: child.id() });
        let mut primary_exited = false;
        let mut disconnected = false;
        let mut controls = Controls::default();
        loop {
            if !primary_exited {
                match poll_child(&child)? {
                    ChildChange::Exited(status) => {
                        send_exit(&mut stream, status);
                        primary_exited = true;
                        if let Some(terminal) = &terminal {
                            terminal.foreground(unsafe { libc::getpgrp() })?;
                        }
                    }
                    ChildChange::Stopped => {
                        let _ = write_frame(&mut stream, &Message::Stopped);
                    }
                    ChildChange::None => {}
                }
            }
            if !disconnected {
                match controls.read(&mut stream, request.framed_control) {
                    Ok(None) | Err(_) => {
                        disconnected = true;
                        if let Some(terminal) = &terminal {
                            terminal.restore_original();
                        }
                        if !primary_exited {
                            signal_group(&child, libc::SIGKILL);
                        }
                    }
                    Ok(Some(commands)) => {
                        for command in commands {
                            match command {
                                Control::Signal(signal)
                                    if !primary_exited && FORWARDED_SIGNALS.contains(&signal) =>
                                {
                                    signal_group(&child, signal)
                                }
                                Control::Terminal(configuration) => {
                                    if let Some(terminal) = &mut terminal {
                                        terminal.configure(configuration, false)?;
                                    }
                                }
                                Control::Foreground(foreground) if !primary_exited => {
                                    if let Some(terminal) = &terminal {
                                        terminal.foreground(if foreground {
                                            child.id().try_into()?
                                        } else {
                                            unsafe { libc::getpgrp() }
                                        })?;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            if let Some(terminal) = terminal.as_mut().filter(|_| disconnected) {
                terminal.pump_output()?;
            }
            // The guardian itself is the remaining task. Query failures retain
            // its lock; they cannot retire a receipt or release managed storage.
            if primary_exited
                && (terminal.is_none() || disconnected)
                && !terminal
                    .as_ref()
                    .is_some_and(terminal::Guardian::output_pending)
                && coalition.active_tasks().is_ok_and(|count| count == 1)
            {
                break;
            }
            let output = terminal
                .as_ref()
                .filter(|_| disconnected)
                .map(terminal::Guardian::output_events);
            events.wait(
                &stream,
                disconnected,
                if primary_exited { 100 } else { -1 },
                output.as_ref().map_or(&[], |events| events.as_slice()),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = write_frame(
            &mut stream,
            &Message::Failed {
                detail: format!("{error:#}"),
            },
        );
    }
    session.finish_macos_coalition()?;
    early_cleanup.safe_to_reap = true;
    macos_jobs::cleanup(directory, &owner, &bytes)?;
    early_cleanup.safe_to_reap = false;
    result
}
