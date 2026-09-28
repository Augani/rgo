//! Bounded overlapped I/O for the Windows named-pipe handles supplied by interprocess.
//!
//! `PIPE_NOWAIT` is not overlapped I/O and makes short frame writes return zero on
//! Windows. Use event-backed operations instead, canceling and draining a pending
//! operation before its buffer and OVERLAPPED value leave scope.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{read, write};
