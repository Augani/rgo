//! Small, bounded diagnostic sink for the long-running daemon.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;

use fs4::fs_std::FileExt;
use tracing_subscriber::fmt::MakeWriter;

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const TRUNCATED_EVENT: &[u8] = b"\n[daemon log event truncated]\n";
const ROTATED_LOG: &[u8] = b"[older daemon diagnostics discarded to bound storage]\n";

#[derive(Clone)]
pub struct BoundedLog {
    path: PathBuf,
    max_bytes: u64,
}

impl BoundedLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            max_bytes: MAX_LOG_BYTES,
        }
    }

    pub fn append(&self, entry: &[u8]) -> io::Result<()> {
        if entry.is_empty() {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log path has no parent"))?;
        // The daemon creates and validates this private directory before it
        // begins serving. Logging must not create an unvalidated storage root.
        if !parent.is_dir() {
            return Ok(());
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join("daemon.log.lock"))?;
        lock.lock_exclusive()?;
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let entry = if entry.len() as u64 > self.max_bytes {
            &entry[..self.max_bytes as usize]
        } else {
            entry
        };
        let rotate = log.metadata()?.len() > self.max_bytes.saturating_sub(entry.len() as u64);
        if rotate {
            log.set_len(0)?;
            if (ROTATED_LOG.len() + entry.len()) as u64 <= self.max_bytes {
                log.write_all(ROTATED_LOG)?;
            }
        }
        log.write_all(entry)
    }
}

pub struct LogEvent {
    sink: BoundedLog,
    bytes: Vec<u8>,
    truncated: bool,
}

impl Write for LogEvent {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let available = MAX_EVENT_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..available.min(bytes.len())]);
        self.truncated |= bytes.len() > available;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit()
    }
}

impl LogEvent {
    fn commit(&mut self) -> io::Result<()> {
        if self.bytes.is_empty() && !self.truncated {
            return Ok(());
        }
        if self.truncated {
            self.bytes.extend_from_slice(TRUNCATED_EVENT);
        }
        let result = self.sink.append(&self.bytes);
        self.bytes.clear();
        self.truncated = false;
        result
    }
}

impl Drop for LogEvent {
    fn drop(&mut self) {
        let _ = self.commit();
    }
}

impl<'a> MakeWriter<'a> for BoundedLog {
    type Writer = LogEvent;

    fn make_writer(&'a self) -> Self::Writer {
        LogEvent {
            sink: self.clone(),
            bytes: Vec::new(),
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_diagnostics_roll_over_without_growing_the_log() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("daemon.log");
        let sink = BoundedLog {
            path: path.clone(),
            max_bytes: 32,
        };
        sink.append(b"first diagnostic\n").unwrap();
        sink.append(b"second diagnostic\n").unwrap();
        sink.append(b"newest diagnostic\n").unwrap();
        let contents = std::fs::read(path).unwrap();
        assert!(contents.len() <= 32);
        assert!(contents.ends_with(b"newest diagnostic\n"));
    }
}
