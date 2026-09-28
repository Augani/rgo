#![allow(unsafe_code)] // This module contains the bounded Win32 overlapped-I/O FFI boundary.

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

pub fn read(handle: BorrowedHandle<'_>, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    transfer(
        handle,
        buf.len(),
        deadline,
        |raw, overlapped, count| unsafe {
            ReadFile(raw, buf.as_mut_ptr(), count, ptr::null_mut(), overlapped)
        },
    )
}

pub fn write(handle: BorrowedHandle<'_>, buf: &[u8], deadline: Instant) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    transfer(
        handle,
        buf.len(),
        deadline,
        |raw, overlapped, count| unsafe {
            WriteFile(raw, buf.as_ptr(), count, ptr::null_mut(), overlapped)
        },
    )
}

fn transfer(
    handle: BorrowedHandle<'_>,
    len: usize,
    deadline: Instant,
    start: impl FnOnce(HANDLE, *mut OVERLAPPED, u32) -> i32,
) -> io::Result<usize> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let raw = handle.as_raw_handle() as HANDLE;
    // Overlapped pipe I/O requires a manual-reset event. The kernel resets it
    // when the operation starts and signals it when completion is available.
    let event_raw = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event_raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let event = unsafe { OwnedHandle::from_raw_handle(event_raw) };
    let mut overlapped = OVERLAPPED {
        hEvent: event.as_raw_handle() as HANDLE,
        ..OVERLAPPED::default()
    };
    let count = u32::try_from(len).unwrap_or(u32::MAX);
    if start(raw, &mut overlapped, count) != 0 {
        let mut transferred = 0;
        if unsafe { GetOverlappedResult(raw, &overlapped, &mut transferred, 0) } != 0 {
            return Ok(transferred as usize);
        }
        return Err(io::Error::last_os_error());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
        return Err(error);
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    let wait_ms = remaining.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    let wait = unsafe { WaitForSingleObject(event.as_raw_handle() as HANDLE, wait_ms) };
    if wait == WAIT_OBJECT_0 {
        let mut transferred = 0;
        if unsafe { GetOverlappedResult(raw, &overlapped, &mut transferred, 0) } != 0 {
            return Ok(transferred as usize);
        }
        return Err(io::Error::last_os_error());
    }

    // The kernel may complete concurrently with cancellation. Drain the completion
    // before dropping `overlapped`, `event`, or the caller-owned byte buffer.
    let wait_error = (wait == WAIT_FAILED).then(io::Error::last_os_error);
    unsafe { CancelIoEx(raw, &overlapped) };
    let mut transferred = 0;
    unsafe { GetOverlappedResult(raw, &overlapped, &mut transferred, 1) };
    if wait == WAIT_TIMEOUT {
        Err(io::ErrorKind::TimedOut.into())
    } else if let Some(error) = wait_error {
        Err(error)
    } else {
        Err(io::ErrorKind::Other.into())
    }
}
