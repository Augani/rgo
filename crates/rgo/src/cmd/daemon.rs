use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use rgo_core::config::Config;
use rgo_core::ipc;
use rgo_core::paths::RgoPaths;
use rgo_protocol::{Request, Response};

use super::env;

pub fn run(foreground: bool, home: Option<PathBuf>) -> Result<()> {
    if foreground {
        close_inherited_descriptors()?;
    }
    let e = if let Some(root) = home {
        ensure!(root.is_absolute(), "daemon --home must be an absolute path");
        let paths = RgoPaths { root };
        let cfg = Config::load(&paths.config_file())?.resolve(&paths.root)?;
        super::Env { paths, cfg }
    } else {
        env()?
    };
    if !foreground {
        let exe = std::env::current_exe().context("locating rgo executable")?;
        spawn_background_daemon(&exe, &e.paths.root).context("starting background daemon")?;
        return Ok(());
    }
    rgo_core::daemon::run(e.paths, e.cfg)
}

/// A daemon outlives the command that started it. On Unix, an inherited
/// non-stdio pipe descriptor can keep that command's output reader open even
/// after the command exits. No daemon socket is opened before this point.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(unsafe_code)]
fn close_inherited_descriptors() -> Result<()> {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let mut descriptors = Vec::new();
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("enumerating inherited descriptors in {directory}"))?
    {
        let entry = entry?;
        if let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|fd| *fd > 2)
        {
            descriptors.push(fd);
        }
    }
    for descriptor in descriptors {
        // The descriptor used to enumerate /dev/fd may already be closed by
        // ReadDir's Drop. close(2) safely reports EBADF for that entry.
        unsafe { libc::close(descriptor) };
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn close_inherited_descriptors() -> Result<()> {
    Ok(())
}

/// Best-effort daemon startup used by commands that must coordinate state. A failed start is
/// deliberately reported as `false`: builds can continue without coordination, while GC and
/// other destructive operations refuse to run.
pub fn ensure_running(paths: &RgoPaths) -> bool {
    if daemon_responds(paths) {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if spawn_background_daemon(&exe, &paths.root).is_err() {
        return false;
    }
    // Spawning a Rust binary from a cold disk or an overloaded CI host can
    // exceed one second. This path is for explicit commands/opted-in
    // maintenance, never the compiler wrapper's 150 ms fail-open probe.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        if daemon_responds_with_timeout(paths, Duration::from_millis(250)) {
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
fn spawn_background_daemon(exe: &std::path::Path, root: &std::path::Path) -> Result<()> {
    Command::new(exe)
        .args(["daemon", "--foreground", "--home"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)] // CreateProcessW is required to disable inherited handles.
fn spawn_background_daemon(exe: &std::path::Path, root: &std::path::Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };

    // std::process::Command inherits every inheritable Windows handle. A caller
    // capturing `rgo gc` can otherwise wait forever for EOF because the daemon
    // inherited its output pipe, even after the command has exited. The daemon
    // has file logging and needs no inherited handles or standard streams.
    let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command_line: Vec<u16> = "rgo daemon --foreground --home "
        .encode_utf16()
        .chain(quote_windows_arg(root.as_os_str()))
        .chain(Some(0))
        .collect();
    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &mut startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(windows)]
fn quote_windows_arg(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut quoted = vec![b'"' as u16];
    let mut backslashes = 0;
    for word in value.encode_wide() {
        if word == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if word == b'"' as u16 {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            quoted.push(word);
            backslashes = 0;
            continue;
        }
        quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        backslashes = 0;
        quoted.push(word);
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    quoted.push(b'"' as u16);
    quoted
}

/// Read-only health probe used by `rgo doctor`. Unlike `ensure_running`, this never starts a
/// daemon or mutates the user's coordination state.
pub fn is_available(paths: &RgoPaths) -> bool {
    daemon_responds(paths)
}

fn daemon_responds(paths: &RgoPaths) -> bool {
    daemon_responds_with_timeout(paths, Duration::from_secs(2))
}

fn daemon_responds_with_timeout(paths: &RgoPaths, timeout: Duration) -> bool {
    // Health must not depend on a full storage scan. A status computation can
    // fail because one managed path is unreadable while the daemon is alive;
    // treating that as a dead daemon would start a duplicate and hide the
    // actual error from `rgo status`.
    match ipc::request_with_timeout(&paths.socket_path(), Request::QueryRemoteStatus, timeout) {
        Ok(Response::RemoteStatus(_)) => true,
        Ok(other) => {
            tracing::debug!(response = ?other, "daemon health check returned unexpected response");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "daemon health check failed");
            false
        }
    }
}
