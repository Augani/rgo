//! Small synchronous IPC transport used by the daemon and its clients.
//!
//! Each connection performs one version handshake and one request/response exchange. This
//! keeps failure recovery simple: a timed-out heartbeat never poisons a long-lived client
//! connection, and the rustc wrapper can use the same transport without an async runtime.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rgo_protocol::{
    CLIENT_TIMEOUT_MILLIS, MAX_FRAME_SIZE, PROTOCOL_VERSION, Request, Response, decode_frame,
    encode_frame,
};

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    pub type Listener = UnixListener;
    pub type Stream = UnixStream;

    pub fn bind(path: &Path) -> io::Result<Listener> {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path)?;
        let mut permissions = std::fs::metadata(path)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
        Ok(listener)
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path)
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::path::Path;
    use uds_windows::{UnixListener, UnixStream};

    pub type Listener = UnixListener;
    pub type Stream = UnixStream;

    pub fn bind(path: &Path) -> io::Result<Listener> {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        UnixListener::bind(path)
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path)
    }
}

pub struct Listener(platform::Listener);

pub struct Connection {
    stream: platform::Stream,
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
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
        let (stream, _) = self.0.accept()?;
        Ok(Connection { stream })
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.0.set_nonblocking(nonblocking)
    }
}

pub fn connect(path: &Path, timeout: Duration) -> Result<Connection> {
    let stream =
        platform::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(Connection { stream })
}

pub fn request(path: &Path, message: Request) -> Result<Response> {
    request_with_timeout(path, message, Duration::from_millis(CLIENT_TIMEOUT_MILLIS))
}

pub fn request_with_timeout(path: &Path, message: Request, timeout: Duration) -> Result<Response> {
    let mut connection = connect(path, timeout)?;
    write_message(
        &mut connection,
        &Request::Hello {
            version: PROTOCOL_VERSION,
            client: client_name(),
        },
    )?;
    match read_message::<Response>(&mut connection)? {
        Response::Hello { version } if version == PROTOCOL_VERSION => {}
        Response::Hello { version } => {
            bail!("daemon protocol mismatch: server={version}, client={PROTOCOL_VERSION}")
        }
        Response::Error { code, message } => bail!("daemon handshake failed ({code}): {message}"),
        other => bail!("invalid daemon handshake response: {other:?}"),
    }
    write_message(&mut connection, &message)?;
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
