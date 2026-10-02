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

#[cfg(debug_assertions)]
mod audit;
mod descriptors;
mod events;
mod native;
mod terminal;
mod wake;

pub(crate) use descriptors::Inherited as InheritedDescriptors;
pub(crate) use native::Attributes as NativeAttributes;
pub use terminal::recovery::Action as TerminalHostAction;

pub fn terminal_host(action: TerminalHostAction, home: Option<&Path>) -> Result<()> {
    terminal::recovery::run(action, home)
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const SOCKET_NAME: &str = "control.sock";
const FORWARDED_SIGNALS: [i32; 9] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGCONT,
    libc::SIGWINCH,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

fn forwarded_mask() -> u32 {
    FORWARDED_SIGNALS
        .into_iter()
        .fold(0_u32, |mask, signal| mask | (1 << signal))
}
static SIGNALS: AtomicU32 = AtomicU32::new(0);
static RELAY_MASK: AtomicU32 = AtomicU32::new(0);
static CANCEL_MASK: AtomicU32 = AtomicU32::new(0);
static SIGNAL_WAKE: AtomicI32 = AtomicI32::new(-1);
const TERMINATING_SIGNALS: [i32; 6] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

fn cancelled_signal() -> Option<i32> {
    let pending = SIGNALS.load(Ordering::Acquire) & CANCEL_MASK.load(Ordering::Acquire);
    TERMINATING_SIGNALS
        .into_iter()
        .find(|signal| pending & (1 << signal) != 0)
}

fn terminate(signal: i32) -> ! {
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        // The primary may have unblocked a signal that the original caller
        // blocked. Report its actual signal death after restoring that mask.
        let mut mask = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, signal);
        libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
        libc::raise(signal);
    }
    std::process::exit(128 + signal);
}

/// Preparation failure must not convert an observed cancellation into another
/// Cargo launch. The failed preparation already dropped/restored its terminal.
pub(super) fn abort_cancelled() {
    if let Some(signal) = cancelled_signal() {
        terminate(signal);
    }
}

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
    #[serde(default)]
    inherited_fds: Option<Vec<i32>>,
    #[serde(default)]
    native_attributes: Option<NativeAttributes>,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Greeting {
        token: String,
        #[serde(default)]
        inherited_fds: bool,
        #[serde(default)]
        native_attributes: bool,
    },
    Prepared {
        coalition: u64,
        #[serde(default)]
        pending_before_exec: bool,
        #[serde(default)]
        forwarded_signals: u32,
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
        (1..=descriptors::MAX_TRANSFER).contains(&descriptors.len()),
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
    let mut control = [0_u64; 32];
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
        (1..=descriptors::MAX_TRANSFER).contains(&expected),
        "invalid descriptor receive count"
    );
    let mut marker = [0_u8; 1];
    let mut io = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0_u64; 32];
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
    #[cfg(debug_assertions)]
    audit_paths: RgoPaths,
}

/// Obtained only after the complete commit packet was sent. Errors from this
/// phase must never launch another Cargo invocation.
pub(super) struct RunningJob {
    stream: UnixStream,
    terminal: Option<terminal::Caller>,
    signals: Option<SignalRelay>,
    #[cfg(debug_assertions)]
    audit_paths: RgoPaths,
    committed: Instant,
}

impl PreparedJob {
    /// Prepare only. Cargo cannot start until `commit` sends the full record.
    pub(super) fn prepare(
        executable: &Path,
        args: &[OsString],
        paths: &RgoPaths,
        context: &Path,
        inherited: &InheritedDescriptors,
        attributes: NativeAttributes,
    ) -> Result<Self> {
        let started = Instant::now();
        let deadline = started + STARTUP_TIMEOUT;
        let signals = events::NativeSignals::capture()?;
        if let Err(error) = signals.validate_for_managed() {
            // Keep the same private observation barrier on a rejected caller,
            // before opening a terminal or registering any guardian job.
            #[cfg(debug_assertions)]
            audit::prepared(paths)?;
            return Err(error);
        }
        let terminal = terminal::Caller::open(paths)?;
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
            signals,
            inherited_fds: Some(inherited.targets.clone()),
            native_attributes: Some(attributes),
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
        let Message::Greeting {
            token,
            inherited_fds,
            native_attributes,
        } = read_frame::<Message>(&mut stream).context("reading Cargo guardian greeting")?
        else {
            bail!("Cargo guardian authentication failed");
        };
        ensure!(token == owner.token, "Cargo guardian authentication failed");
        ensure!(
            inherited_fds,
            "Cargo guardian cannot preserve inherited descriptors"
        );
        ensure!(
            native_attributes,
            "Cargo guardian cannot preserve native attributes"
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
        descriptors.extend_from_slice(&inherited.targets);
        send_descriptors(&stream, &descriptors)?;
        let coalition = match read_frame::<Message>(&mut stream)
            .context("reading Cargo guardian prepared reply")?
        {
            Message::Prepared {
                coalition,
                pending_before_exec,
                forwarded_signals,
            } => {
                // A guardian launched during an upgrade may be older. Refuse
                // this handoff before activating the terminal or committing.
                ensure!(
                    pending_before_exec,
                    "Cargo guardian cannot queue signals before exec"
                );
                ensure!(
                    forwarded_signals & forwarded_mask() == forwarded_mask(),
                    "Cargo guardian cannot forward required signals"
                );
                coalition
            }
            Message::Failed { detail } => {
                bail!("Cargo guardian cannot admit the invocation: {detail}")
            }
            _ => bail!("Cargo guardian returned an invalid startup reply"),
        };
        ensure!(
            coalition != ResourceCoalition::for_pid(std::process::id().try_into()?)?.id(),
            "Cargo guardian inherited its caller's coalition"
        );
        // Owning terminal state and signal restoration in one object ensures
        // every subsequent preparation error restores the terminal first.
        let mut job = Self {
            stream,
            terminal,
            signals: Some(SignalRelay::new()?),
            #[cfg(debug_assertions)]
            audit_paths: paths.clone(),
        };
        if let Some(terminal) = &mut job.terminal {
            terminal.attach(
                receive_descriptors(&job.stream, 1)
                    .context("receiving Cargo guardian terminal")?
                    .pop()
                    .context("PTY descriptor is missing")?,
            );
            job.stream.write_all(b"T")?;
            write_frame(&mut job.stream, &terminal.configuration()?)?;
            ensure!(
                matches!(
                    read_frame::<Message>(&mut job.stream)
                        .context("reading Cargo guardian terminal configuration reply")?,
                    Message::Configured
                ),
                "Cargo terminal was not configured"
            );
            terminal.activate()?;
        }
        #[cfg(debug_assertions)]
        audit::prepared(paths)?;
        tracing::debug!(
            files_ms = files_ready.as_secs_f64() * 1000.0,
            bootstrap_ms = (bootstrapped - files_ready).as_secs_f64() * 1000.0,
            connection_ms = (connected - bootstrapped).as_secs_f64() * 1000.0,
            admission_ms = (started.elapsed() - connected).as_secs_f64() * 1000.0,
            "macOS Cargo guardian preparation"
        );
        Ok(job)
    }

    /// Every error leaves the commit incomplete, so the caller may restore
    /// state and use checkout Cargo after acknowledging any cancellation.
    pub(super) fn commit(mut self, session: &mut SessionGuard) -> Result<RunningJob> {
        ensure!(
            cancelled_signal().is_none(),
            "Cargo preparation was cancelled"
        );
        self.stream.set_read_timeout(None)?;
        let pending = {
            let relay = self
                .signals
                .as_mut()
                .context("Cargo signal relay is missing")?;
            relay.committing()?;
            relay.queued_commit()
        };
        ensure!(cancelled_signal().is_none(), "Cargo commit was cancelled");
        session.release_local_for_macos_guardian();
        let committed = Instant::now();
        let mut commit = [0_u8; 5];
        commit[0] = b'Q';
        commit[1..].copy_from_slice(&pending.to_be_bytes());
        // A failed write cannot have completed this fixed-length packet. The
        // guardian waits for all bytes before spawning; teardown closes the
        // stream, restores terminal state, and requeues unsent notifications.
        #[cfg(debug_assertions)]
        let offset = audit::commit_prefix(&self.audit_paths, &mut self.stream, &commit)?;
        #[cfg(not(debug_assertions))]
        let offset = 0;
        self.stream.write_all(&commit[offset..])?;
        // No fallible operation follows the completed packet in this phase.
        if let Some(relay) = &mut self.signals {
            relay.pending_commit = 0;
        }
        Ok(RunningJob {
            stream: self.stream,
            terminal: self.terminal,
            signals: self.signals,
            #[cfg(debug_assertions)]
            audit_paths: self.audit_paths,
            committed,
        })
    }
}

impl RunningJob {
    pub(super) fn run(mut self, session: SessionGuard) -> Result<()> {
        let committed = self.committed;
        #[cfg(debug_assertions)]
        audit::caller_committed(&self.audit_paths)?;
        let mut reader = self.stream.try_clone()?;
        let wake = wake::Wake::get()?;
        let (sender, receiver) = std::sync::mpsc::channel();
        #[cfg(debug_assertions)]
        let event_audit = std::env::var_os("RGO_MACOS_SUPERVISOR_EVENT_AUDIT").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        #[cfg(debug_assertions)]
        let reader_paths = self.audit_paths.clone();
        std::thread::spawn(move || {
            loop {
                let message = read_frame::<Message>(&mut reader);
                #[cfg(debug_assertions)]
                if matches!(message, Ok(Message::Stopped)) {
                    audit::event(&reader_paths, event_audit, format_args!("reader stopped"));
                }
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
                    #[cfg(debug_assertions)]
                    audit::event(
                        &self.audit_paths,
                        event_audit,
                        format_args!("caller foreground={foreground}"),
                    );
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
            let pending = SIGNALS.swap(0, Ordering::AcqRel) & RELAY_MASK.load(Ordering::Acquire);
            for signal in FORWARDED_SIGNALS {
                if pending & (1 << signal) != 0 {
                    #[cfg(debug_assertions)]
                    audit::event(
                        &self.audit_paths,
                        event_audit,
                        format_args!("caller signal={signal}"),
                    );
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
                    #[cfg(debug_assertions)]
                    audit::event(
                        &self.audit_paths,
                        event_audit,
                        format_args!("caller stopped"),
                    );
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
                        terminate(signal);
                    }
                    std::process::exit(code.unwrap_or(129));
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

struct SignalRelay {
    dispositions: Vec<(i32, libc::sigaction)>,
    mask: libc::sigset_t,
    pending_commit: u32,
}

impl SignalRelay {
    fn forwarded_set() -> libc::sigset_t {
        let mut mask = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
            for signal in FORWARDED_SIGNALS {
                libc::sigaddset(&mut mask, signal);
            }
        }
        mask
    }

    fn action() -> libc::sigaction {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = capture_signal as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action
    }

    fn new() -> Result<Self> {
        SIGNALS.store(0, Ordering::Relaxed);
        RELAY_MASK.store(0, Ordering::Relaxed);
        CANCEL_MASK.store(0, Ordering::Relaxed);
        SIGNAL_WAKE.store(wake::Wake::get()?.writer(), Ordering::Relaxed);
        let mut mask = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) } == 0,
            "cannot observe caller signal mask"
        );
        let mut relay = Self {
            dispositions: Vec::new(),
            mask,
            pending_commit: 0,
        };
        for signal in FORWARDED_SIGNALS {
            let action = Self::action();
            let mut previous = unsafe { std::mem::zeroed() };
            ensure!(
                unsafe { libc::sigaction(signal, &action, &mut previous) } == 0,
                "cannot relay Cargo signal"
            );
            relay.dispositions.push((signal, previous));
            if previous.sa_sigaction == libc::SIG_IGN
                && signal != libc::SIGCONT
                && signal != libc::SIGWINCH
            {
                // During preparation, discard ignored signals at delivery.
                // Runtime group forwarding begins at the commit handoff.
                ensure!(
                    unsafe { libc::sigaction(signal, &previous, std::ptr::null_mut()) } == 0,
                    "cannot preserve ignored Cargo signal"
                );
            } else {
                RELAY_MASK.fetch_or(1 << signal, Ordering::Release);
            }
            if previous.sa_sigaction != libc::SIG_IGN
                && unsafe { libc::sigismember(&mask, signal) } == 0
            {
                CANCEL_MASK.fetch_or(1 << signal, Ordering::Release);
            }
        }
        // Fork does not inherit pending signals. Capture blocked notifications
        // here and deliver them to the separate Cargo group, which retains the
        // caller's original mask. Only originally deliverable termination is
        // a preparation cancellation. CONT/window kernel effects are still
        // observed even when ignored; Cargo keeps its own native actions.
        let forwarded = Self::forwarded_set();
        ensure!(
            unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &forwarded, std::ptr::null_mut()) } == 0,
            "cannot observe Cargo group notifications"
        );
        Ok(relay)
    }

    fn committing(&self) -> Result<()> {
        for (signal, previous) in &self.dispositions {
            if previous.sa_sigaction == libc::SIG_IGN
                && *signal != libc::SIGCONT
                && *signal != libc::SIGWINCH
            {
                // Do not replay anything captured in the transient install
                // window during preparation. From this handoff, forward group
                // notifications: Cargo/applications may change their own
                // actions after exec. The child's native action decides what
                // happens when the guardian delivers the notification.
                let bit = 1 << signal;
                SIGNALS.fetch_and(!bit, Ordering::AcqRel);
                RELAY_MASK.fetch_or(bit, Ordering::Release);
                let action = Self::action();
                ensure!(
                    unsafe { libc::sigaction(*signal, &action, std::ptr::null_mut()) } == 0,
                    "cannot relay running Cargo group signal"
                );
            }
        }
        Ok(())
    }

    fn queued_commit(&mut self) -> u32 {
        let blocked = self
            .dispositions
            .iter()
            .fold(0_u32, |mask, (signal, previous)| {
                if previous.sa_sigaction != libc::SIG_IGN
                    && unsafe { libc::sigismember(&self.mask, *signal) } == 1
                {
                    mask | (1 << signal)
                } else {
                    mask
                }
            });
        // These bits remain owned by the relay until the complete commit is
        // written. A failed write must still restore them on teardown.
        self.pending_commit = SIGNALS.fetch_and(!blocked, Ordering::AcqRel) & blocked;
        self.pending_commit
    }
}

impl Drop for SignalRelay {
    fn drop(&mut self) {
        // Keep the captured bits/mask until preparation's fallback decision.
        // An error must still acknowledge a cancellation after handler restore.
        let forwarded = Self::forwarded_set();
        unsafe { libc::sigprocmask(libc::SIG_BLOCK, &forwarded, std::ptr::null_mut()) };
        SIGNAL_WAKE.store(-1, Ordering::Relaxed);
        let pending = SIGNALS.load(Ordering::Acquire) | self.pending_commit;
        for (signal, previous) in &self.dispositions {
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
                if previous.sa_sigaction != libc::SIG_IGN
                    && libc::sigismember(&self.mask, *signal) == 1
                    && pending & (1 << signal) != 0
                {
                    // Restore captured-but-unforwarded blocked notifications
                    // to this thread's kernel pending set. A preparation error
                    // then execs fallback Cargo in this same PID. Thread-local
                    // raise avoids delivery to a still-unblocked reader thread.
                    libc::raise(*signal);
                }
            }
        }
        unsafe {
            libc::sigprocmask(libc::SIG_SETMASK, &self.mask, std::ptr::null_mut());
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
            inherited_fds: true,
            native_attributes: true,
        },
    )?;
    let request = read_frame::<Invocation>(&mut stream)?;
    let targets = request.inherited_fds.as_deref().unwrap_or_default();
    descriptors::validate(targets)?;
    let standard_count = if request.terminal { 4 } else { 3 };
    let mut descriptors = receive_descriptors(&stream, standard_count + targets.len())?;
    // An older caller can still send a mask outside the current relay policy.
    // Refuse it before preparing a PTY, publishing a receipt or accepting Q/S.
    let validation = request.signals.validate_for_managed().and_then(|()| {
        ensure!(
            request.inherited_fds.is_some(),
            "Cargo caller did not capture inherited descriptors"
        );
        request
            .native_attributes
            .context("Cargo caller did not capture native attributes")?
            .validate_for_guardian()
    });
    if let Err(error) = validation {
        let _ = write_frame(
            &mut stream,
            &Message::Failed {
                detail: format!("{error:#}"),
            },
        );
        return Err(error);
    }
    let attributes = request
        .native_attributes
        .context("Cargo native attributes are missing")?;
    let inherited = descriptors::Restored::prepare(targets, descriptors.split_off(standard_count))?;
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
    #[cfg(debug_assertions)]
    let event_audit = request.environment.iter().any(|(key, value)| {
        key.as_slice() == b"RGO_MACOS_SUPERVISOR_EVENT_AUDIT" && value.as_slice() == b"1"
    });
    #[cfg(debug_assertions)]
    audit::event(
        &paths,
        event_audit,
        format_args!("guardian native={:?}", request.signals),
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
                pending_before_exec: true,
                forwarded_signals: forwarded_mask(),
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
        let pending = match commit[0] {
            b'S' => 0, // Older callers have no queued-signal packet.
            b'Q' => {
                let mut bytes = [0_u8; 4];
                stream.read_exact(&mut bytes)?;
                let pending = u32::from_be_bytes(bytes);
                ensure!(
                    pending & !forwarded_mask() == 0 && request.signals.accepts_pending(pending),
                    "Cargo commit contains unsupported pending signals"
                );
                pending
            }
            _ => bail!("Cargo job was not committed"),
        };
        #[cfg(debug_assertions)]
        audit::committed(
            &paths,
            request.environment.iter().any(|(key, value)| {
                key.as_slice() == b"RGO_MACOS_SUPERVISOR_AUDIT" && value.as_slice() == b"1"
            }),
        )?;
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
        // Restore only in the actual child, after descriptor staging. The
        // guardian retains its launchd limits and mask for lifecycle cleanup.
        unsafe {
            command.pre_exec(move || {
                inherited.restore_in_child()?;
                attributes.restore_in_child()?;
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
                request.signals.restore_in_child(pending)
            });
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
                        #[cfg(debug_assertions)]
                        audit::event(
                            &paths,
                            event_audit,
                            format_args!("guardian child-stopped pid={}", child.id()),
                        );
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
                                    #[cfg(debug_assertions)]
                                    audit::event(
                                        &paths,
                                        event_audit,
                                        format_args!("guardian signal={signal}"),
                                    );
                                    signal_group(&child, signal)
                                }
                                Control::Terminal(configuration) => {
                                    if let Some(terminal) = &mut terminal {
                                        terminal.configure(configuration, false)?;
                                    }
                                }
                                Control::Foreground(foreground) if !primary_exited => {
                                    #[cfg(debug_assertions)]
                                    audit::event(
                                        &paths,
                                        event_audit,
                                        format_args!("guardian foreground={foreground}"),
                                    );
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
    // Concurrent guardians can complete together. Only a busy retirement lock
    // is transient; edited metadata and every other failure remain errors.
    // Cargo already returned and the receipt retired, so this cannot delay or
    // repeat managed work. Daemon maintenance keeps its nonblocking policy.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match macos_jobs::cleanup(directory, &owner, &bytes) {
            Ok(()) => break,
            Err(error) if error.is::<macos_jobs::RetirementBusy>() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    early_cleanup.safe_to_reap = false;
    result
}
