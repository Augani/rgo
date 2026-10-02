//! Small synchronous IPC transport used by the daemon and its clients.
//!
//! Each connection performs one version handshake and one request/response exchange. This
//! keeps failure recovery simple: a timed-out heartbeat never poisons a long-lived client
//! connection, and the rustc wrapper can use the same transport without an async runtime.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rgo_protocol::{
    CLIENT_TIMEOUT_MILLIS, MAX_FRAME_SIZE, PROTOCOL_VERSION, Request, Response, decode_frame,
    encode_frame,
};

#[cfg(unix)]
mod platform {
    use std::io;
    use std::path::Path;

    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};

    pub type Listener = interprocess::local_socket::Listener;
    pub type Stream = interprocess::local_socket::Stream;

    pub fn bind(path: &Path) -> io::Result<Listener> {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        let name = path.to_fs_name::<GenericFilePath>()?;
        let listener = ListenerOptions::new().name(name).create_sync()?;
        let mut permissions = std::fs::metadata(path)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
        Ok(listener)
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        let name = path.to_fs_name::<GenericFilePath>()?;
        interprocess::local_socket::ConnectOptions::new()
            .name(name)
            .connect_sync()
    }
}

#[cfg(windows)]
mod platform {
    use interprocess::local_socket::{
        ConnectOptions, GenericNamespaced, ListenerOptions, prelude::*,
    };
    use std::io;
    use std::path::Path;

    pub type Listener = interprocess::local_socket::Listener;
    pub type Stream = interprocess::local_socket::Stream;

    pub fn bind(path: &Path) -> io::Result<Listener> {
        let name = path
            .to_string_lossy()
            .into_owned()
            .to_ns_name::<GenericNamespaced>()?;
        ListenerOptions::new().name(name).create_sync()
    }

    pub fn connect(path: &Path, timeout: std::time::Duration) -> io::Result<Stream> {
        let name = path
            .to_string_lossy()
            .into_owned()
            .to_ns_name::<GenericNamespaced>()?;
        ConnectOptions::new()
            .name(name)
            .wait_mode(interprocess::ConnectWaitMode::Timeout(timeout))
            .connect_sync()
    }
}

pub struct Listener(platform::Listener);

pub struct Connection {
    stream: platform::Stream,
    #[cfg(windows)]
    deadline: Option<Instant>,
}

impl Connection {
    /// Bound daemon-side resource usage for clients that connect and then stop sending bytes.
    pub fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        #[cfg(unix)]
        {
            use interprocess::local_socket::traits::Stream as _;
            self.stream.set_recv_timeout(Some(timeout))?;
            self.stream.set_send_timeout(Some(timeout))?;
        }
        #[cfg(windows)]
        {
            // Windows named pipes do not expose socket timeouts. Each read or
            // write uses a cancelable overlapped operation against this deadline.
            self.deadline = Some(Instant::now() + timeout);
        }
        Ok(())
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return self.stream.read(buf);
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;
            let deadline = self.deadline.ok_or(io::ErrorKind::InvalidInput)?;
            let platform::Stream::NamedPipe(pipe) = &self.stream;
            rgo_winpipe::read(pipe.inner().as_handle(), buf, deadline)
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return self.stream.write(buf);
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;
            let deadline = self.deadline.ok_or(io::ErrorKind::InvalidInput)?;
            let platform::Stream::NamedPipe(pipe) = &self.stream;
            let written = rgo_winpipe::write(pipe.inner().as_handle(), buf, deadline)?;
            if written > 0 {
                // We bypassed interprocess's Write impl, so preserve its
                // named-pipe linger-on-drop behavior for unread response bytes.
                pipe.inner().mark_dirty();
            }
            Ok(written)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl Listener {
    pub fn bind(path: &Path) -> Result<Self> {
        Ok(Self(
            platform::bind(path).with_context(|| format!("binding {}", path.display()))?,
        ))
    }

    pub fn accept(&self) -> io::Result<Connection> {
        use interprocess::local_socket::traits::Listener as _;
        let stream = self.0.accept()?;
        Ok(Connection {
            stream,
            #[cfg(windows)]
            deadline: None,
        })
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        use interprocess::local_socket::traits::Listener as _;
        self.0.set_nonblocking(
            interprocess::local_socket::ListenerNonblockingMode::from_bool(
                nonblocking,
                nonblocking,
            ),
        )
    }
}

pub fn connect(path: &Path, timeout: Duration) -> Result<Connection> {
    #[cfg(unix)]
    let stream = platform::connect(path);
    #[cfg(windows)]
    let stream = platform::connect(path, timeout);
    let stream = stream.with_context(|| format!("connecting to {}", path.display()))?;
    #[cfg(unix)]
    {
        use interprocess::local_socket::traits::Stream as _;
        stream
            .set_recv_timeout(Some(timeout))
            .context("setting IPC receive timeout")?;
        stream
            .set_send_timeout(Some(timeout))
            .context("setting IPC send timeout")?;
    }
    Ok(Connection {
        stream,
        #[cfg(windows)]
        deadline: Some(Instant::now() + timeout),
    })
}

pub fn request(path: &Path, message: Request) -> Result<Response> {
    request_with_timeout(path, message, Duration::from_millis(CLIENT_TIMEOUT_MILLIS))
}

pub fn request_with_timeout(path: &Path, message: Request, timeout: Duration) -> Result<Response> {
    request_with_response_timeout(path, message, timeout, timeout)
}

/// Keep connection and handshake failures prompt without timing out a long
/// coordinated operation while the daemon is still computing its response.
pub fn request_with_response_timeout(
    path: &Path,
    message: Request,
    connection_timeout: Duration,
    response_timeout: Duration,
) -> Result<Response> {
    request_for_protocol(
        path,
        message,
        connection_timeout,
        response_timeout,
        PROTOCOL_VERSION,
    )
}

/// Activation recovery may only shut down a recorded older daemon. Ordinary
/// operations retain strict protocol equality, including GC admission.
pub fn shutdown_recorded_daemon(
    path: &Path,
    recorded_version: Option<u32>,
    timeout: Duration,
) -> Result<Response> {
    let current = request_with_timeout(path, Request::Shutdown, timeout);
    if current.is_ok() {
        return current;
    }
    // Shutdown's wire shape has been unchanged since protocol 6. Never use an
    // unrecorded or future protocol to issue maintenance or recovery requests.
    match recorded_version {
        Some(version) if (6..PROTOCOL_VERSION).contains(&version) => {
            request_for_protocol(path, Request::Shutdown, timeout, timeout, version)
        }
        _ => current,
    }
}

fn request_for_protocol(
    path: &Path,
    message: Request,
    connection_timeout: Duration,
    response_timeout: Duration,
    protocol_version: u32,
) -> Result<Response> {
    let mut connection = connect(path, connection_timeout)?;
    write_message(
        &mut connection,
        &Request::Hello {
            version: protocol_version,
            client: client_name(),
        },
    )?;
    match read_message::<Response>(&mut connection)? {
        Response::Hello { version } if version == protocol_version => {}
        Response::Hello { version } => {
            bail!("daemon protocol mismatch: server={version}, client={protocol_version}")
        }
        Response::Error { code, message } => bail!("daemon handshake failed ({code}): {message}"),
        other => bail!("invalid daemon handshake response: {other:?}"),
    }
    write_message(&mut connection, &message)?;
    if response_timeout != connection_timeout {
        connection
            .set_timeout(response_timeout)
            .context("setting IPC response timeout")?;
    }
    read_message(&mut connection)
}

pub fn read_message<T: for<'de> serde::Deserialize<'de>>(connection: &mut Connection) -> Result<T> {
    let frame = read_frame(connection)?;
    decode_frame(&frame).map_err(|e| anyhow::anyhow!(e.to_string()))
}

pub fn write_message<T: serde::Serialize>(connection: &mut Connection, message: &T) -> Result<()> {
    let frame = encode_frame(message).context("encoding IPC frame")?;
    connection.write_all(&frame).context("writing IPC frame")?;
    connection.flush().context("flushing IPC frame")
}

fn read_frame(connection: &mut Connection) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    connection
        .read_exact(&mut header)
        .context("reading IPC frame length")?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        bail!("empty IPC frame");
    }
    if len > MAX_FRAME_SIZE {
        bail!("IPC frame exceeds {} bytes", MAX_FRAME_SIZE);
    }
    let mut frame = Vec::with_capacity(len + 4);
    frame.extend_from_slice(&header);
    frame.resize(len + 4, 0);
    connection
        .read_exact(&mut frame[4..])
        .context("reading IPC frame payload")?;
    Ok(frame)
}

fn client_name() -> String {
    format!("{}:{}", std::env::consts::OS, std::process::id())
}
