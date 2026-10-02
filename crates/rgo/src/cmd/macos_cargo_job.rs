//! Per-invocation launchd Cargo guardian. Private pilot until terminal, recovery,
//! and supported-runtime gates are complete. No rustup proxy is replaced.

#![allow(unsafe_code)] // macOS descriptor passing, peer credentials, and signals.

use anyhow::{Context, Result, bail, ensure};
use rgo_core::macos_coalition::ResourceCoalition;
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const SOCKET_NAME: &str = "control.sock";
const FORWARDED_SIGNALS: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];
static SIGNALS: AtomicU32 = AtomicU32::new(0);

#[derive(Serialize, Deserialize)]
struct Owner {
    label: String,
    domain: String,
    token: String,
    definition: String,
}

#[derive(Serialize, Deserialize)]
struct Invocation {
    executable: Vec<u8>,
    args: Vec<Vec<u8>>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    directory: Vec<u8>,
    root: PathBuf,
    context: PathBuf,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Greeting {
        token: String,
    },
    Prepared {
        coalition: u64,
    },
    Started {
        pid: u32,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Failed {
        detail: String,
    },
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

fn send_stdio(stream: &UnixStream) -> Result<()> {
    let descriptors = [0_i32, 1, 2];
    for descriptor in descriptors {
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
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&descriptors) as u32) };
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&descriptors) as u32);
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(header),
            std::mem::size_of_val(&descriptors),
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

fn receive_stdio(stream: &UnixStream) -> Result<[OwnedFd; 3]> {
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
            && files.len() == 3,
        "Cargo job did not receive exactly three standard descriptors"
    );
    for file in &files {
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "cannot close Cargo descriptor on guardian exec"
        );
    }
    files
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid standard descriptor count"))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

fn verify_owner(directory: &Path, owner: &Owner, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Cargo job directory is not private and owned"
    );
    for name in ["owner.json", "job.plist"] {
        let metadata = std::fs::symlink_metadata(directory.join(name))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Cargo job definition is not a regular file"
        );
    }
    ensure!(
        std::fs::read(directory.join("owner.json"))? == bytes
            && std::fs::read(directory.join("job.plist"))? == owner.definition.as_bytes(),
        "Cargo job ownership changed"
    );
    Ok(())
}

fn read_owner(directory: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Cargo job directory is not private and owned"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("owner.json"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.len() <= 16 * 1024,
        "unsafe or oversized Cargo job owner record"
    );
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "Cargo job owner record grew beyond its limit"
    );
    Ok(bytes)
}

fn cleanup(directory: &Path, owner: &Owner, bytes: &[u8]) -> Result<()> {
    verify_owner(directory, owner, bytes)?;
    let target = format!("{}/{}", owner.domain, owner.label);
    // Remove the one-use rendezvous before bootout: a restarted old definition
    // cannot obtain another invocation or enter a managed context.
    std::fs::remove_dir_all(directory)?;
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .output()?;
    Ok(())
}

struct EarlyCleanup<'a> {
    directory: &'a Path,
    owner: &'a Owner,
    bytes: &'a [u8],
    safe_to_reap: bool,
}

impl Drop for EarlyCleanup<'_> {
    fn drop(&mut self) {
        if self.safe_to_reap {
            let _ = cleanup(self.directory, self.owner, self.bytes);
        }
    }
}

pub(super) struct PreparedJob {
    stream: UnixStream,
}

impl PreparedJob {
    /// Prepare only. Cargo cannot start until `run` sends its one-byte commit.
    pub(super) fn prepare(
        executable: &Path,
        args: &[OsString],
        paths: &RgoPaths,
        context: &Path,
    ) -> Result<Self> {
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
        let directory = temporary.path();
        let listener = in_directory(directory, || Ok(UnixListener::bind(SOCKET_NAME)?))?;
        listener.set_nonblocking(true)?;
        let mut entropy = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let token = blake3::hash(&entropy).to_hex().to_string();
        let label = format!("com.rgo.cargo.{}", &token[..32]);
        let domain = format!("gui/{}", unsafe { libc::geteuid() });
        let rgo = std::env::current_exe()?;
        let definition = format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>macos-cargo-job</string><string>--directory</string><string>{}</string><string>--token</string><string>{token}</string></array><key>RunAtLoad</key><true/><key>AbandonProcessGroup</key><true/><key>StandardOutPath</key><string>/dev/null</string><key>StandardErrorPath</key><string>{}</string></dict></plist>",
            xml(rgo.to_str().context("guardian executable is not UTF-8")?),
            xml(directory
                .to_str()
                .context("guardian directory is not UTF-8")?),
            xml(directory
                .join("guardian.stderr")
                .to_str()
                .context("guardian diagnostic path is not UTF-8")?)
        );
        let owner = Owner {
            label,
            domain,
            token,
            definition,
        };
        let bytes = serde_json::to_vec(&owner)?;
        for (name, data) in [
            ("owner.json", bytes.as_slice()),
            ("job.plist", owner.definition.as_bytes()),
        ] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        File::open(directory)?.sync_all()?;
        let bootstrap = Command::new("/bin/launchctl")
            .arg("bootstrap")
            .arg(&owner.domain)
            .arg(directory.join("job.plist"))
            .output()?;
        ensure!(
            bootstrap.status.success(),
            "Cargo job bootstrap failed: {}",
            String::from_utf8_lossy(&bootstrap.stderr)
        );
        // The guardian owns this directory now, including after the caller exits.
        let _directory = temporary.keep();
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => {
                    let diagnostic = std::fs::read_to_string(_directory.join("guardian.stderr"))
                        .unwrap_or_default();
                    // No request or commit was sent, so this job cannot have
                    // started Cargo or registered a managed context.
                    let _ = cleanup(&_directory, &owner, &bytes);
                    bail!("waiting for Cargo guardian: {error}; {diagnostic}");
                }
            }
        };
        // Darwin accept inherits the listener's O_NONBLOCK state. Only the
        // accept loop is nonblocking; framed startup reads have a deadline.
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(STARTUP_TIMEOUT))?;
        stream.set_write_timeout(Some(STARTUP_TIMEOUT))?;
        check_peer(&stream)?;
        ensure!(
            matches!(read_frame::<Message>(&mut stream)?, Message::Greeting { token } if token == owner.token),
            "Cargo guardian authentication failed"
        );
        write_frame(&mut stream, &invocation)?;
        send_stdio(&stream)?;
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
        Ok(Self { stream })
    }

    pub(super) fn run(mut self, mut session: SessionGuard) -> Result<()> {
        let signals = SignalRelay::new()?;
        session.release_local_for_macos_guardian();
        self.stream.set_read_timeout(None)?;
        self.stream.write_all(b"S")?; // From here failure cannot retry Cargo.
        let mut reader = self.stream.try_clone()?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let message = read_frame::<Message>(&mut reader);
                let terminal = !matches!(message, Ok(Message::Started { .. }));
                if sender.send(message).is_err() || terminal {
                    break;
                }
            }
        });
        loop {
            let pending = SIGNALS.swap(0, Ordering::Relaxed);
            for signal in FORWARDED_SIGNALS {
                if pending & (1 << signal) != 0 {
                    self.stream.write_all(&[signal as u8])?;
                }
            }
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(Message::Started { .. })) => {}
                Ok(Ok(Message::Exited { code, signal })) => {
                    drop(session);
                    drop(signals);
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
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

extern "C" fn capture_signal(signal: i32) {
    if (0..32).contains(&signal) {
        SIGNALS.fetch_or(1 << signal, Ordering::Relaxed);
    }
}

struct SignalRelay(Vec<(i32, libc::sigaction)>);

impl SignalRelay {
    fn new() -> Result<Self> {
        SIGNALS.store(0, Ordering::Relaxed);
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

/// Private launchd entry point. Only the authenticated one-use socket supplies
/// an invocation; restarting its old definition cannot start another Cargo.
pub fn guardian(directory: &Path, token: &str) -> Result<()> {
    ensure!(
        token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Cargo job token"
    );
    let bytes = read_owner(directory)?;
    let owner: Owner = serde_json::from_slice(&bytes)?;
    ensure!(
        owner.token == token
            && owner.label == format!("com.rgo.cargo.{}", &token[..32])
            && owner.domain == format!("gui/{}", unsafe { libc::geteuid() }),
        "Cargo job owner does not match its invocation"
    );
    verify_owner(directory, &owner, &bytes)?;
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
    let [stdin, stdout, stderr] = receive_stdio(&stream)?;
    let paths = RgoPaths { root: request.root };
    ensure!(
        paths.root.is_absolute() && request.context.is_absolute(),
        "Cargo job scope must be absolute"
    );
    let mut session = supervision::lock_cargo_session(&paths, Some(&request.context))?;
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
        let mut commit = [0_u8];
        stream.read_exact(&mut commit)?;
        ensure!(commit == *b"S", "Cargo job was not committed");
        stream.set_read_timeout(None)?;
        stream.set_nonblocking(true)?;
        session.retain_across_exec()?;
        let mut child = Command::new(OsString::from_vec(request.executable))
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
            .process_group(0)
            .spawn()
            .context("starting real Cargo in its launchd coalition")?;
        let _ = write_frame(&mut stream, &Message::Started { pid: child.id() });
        let mut primary_exited = false;
        let mut disconnected = false;
        loop {
            if !primary_exited {
                if let Some(status) = child.try_wait()? {
                    send_exit(&mut stream, status);
                    primary_exited = true;
                }
            }
            if !disconnected {
                let mut commands = [0_u8; 32];
                match stream.read(&mut commands) {
                    Ok(0) => {
                        disconnected = true;
                        if !primary_exited {
                            signal_group(&child, libc::SIGKILL);
                        }
                    }
                    Ok(length) => {
                        for signal in &commands[..length] {
                            let signal = i32::from(*signal);
                            if !primary_exited && FORWARDED_SIGNALS.contains(&signal) {
                                signal_group(&child, signal);
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        disconnected = true;
                        if !primary_exited {
                            signal_group(&child, libc::SIGKILL);
                        }
                    }
                }
            }
            // The guardian itself is the remaining task. Query failures retain
            // its lock; they cannot retire a receipt or release managed storage.
            if primary_exited && coalition.active_tasks().is_ok_and(|count| count == 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(if primary_exited { 100 } else { 20 }));
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
    cleanup(directory, &owner, &bytes)?;
    early_cleanup.safe_to_reap = false;
    result
}
