//! Experimental macOS resource-coalition observations for a launchd guardian.
//!
//! These private XNU interfaces are dynamically resolved and never substitute a
//! PID scan or an empty count on error. Durable receipts extend an explicitly
//! registered session's protection after its lock descriptors close. The private
//! Cargo launcher pilot registers isolated launchd jobs; normal activation still
//! needs terminal compatibility, crash recovery, and supported-version evidence.

#![allow(unsafe_code)] // Narrow FFI to dynamically resolved XNU observation APIs.

use anyhow::{Context, Result, bail, ensure};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::paths::RgoPaths;

const MAX_RECEIPT_BYTES: u64 = 4096;
const MAX_COALITIONS: usize = 32;

type PidInfo = unsafe extern "C" fn(i32, i32, u64, *mut libc::c_void, i32) -> i32;
type ResourceUsage = unsafe extern "C" fn(u64, *mut libc::c_void, usize) -> i32;

struct Library(*mut libc::c_void);

impl Library {
    fn open(path: &std::ffi::CStr) -> Result<Self> {
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(
            !handle.is_null(),
            "macOS coalition observation library is unavailable"
        );
        Ok(Self(handle))
    }

    fn symbol(&self, name: &std::ffi::CStr) -> Result<*mut libc::c_void> {
        let symbol = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        ensure!(
            !symbol.is_null(),
            "macOS coalition observation symbol is unavailable"
        );
        Ok(symbol)
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.0) };
    }
}

/// A resource coalition identified by the kernel, rather than a PID lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceCoalition {
    id: u64,
}

/// A registered, same-boot ID is never reused. XNU removes its lookup entry
/// only after termination and zero active references, including every task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceState {
    Active(u64),
    Reaped,
}

impl ResourceCoalition {
    /// Observe a live process's resource coalition. This does not establish that
    /// the coalition contains only rgo's job or that a PID was not reused.
    pub fn for_pid(pid: i32) -> Result<Self> {
        ensure!(pid > 0, "coalition lookup requires a positive process ID");
        let library = Library::open(c"/usr/lib/libproc.dylib")?;
        let symbol = library.symbol(c"proc_pidinfo")?;
        let query: PidInfo = unsafe { std::mem::transmute(symbol) };
        // XNU proc_info_private.h: PROC_PIDCOALITIONINFO = 20; two coalition
        // IDs followed by three reserved u64s. Require the complete response.
        let mut info = [0_u64; 5];
        let bytes = unsafe {
            query(
                pid,
                20,
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of_val(&info) as i32,
            )
        };
        if bytes <= 0 {
            bail!(
                "macOS coalition PID lookup failed: {}",
                std::io::Error::last_os_error()
            );
        }
        ensure!(
            bytes as usize == std::mem::size_of_val(&info),
            "macOS coalition PID response has an unsupported size: {bytes}"
        );
        ensure!(info[0] != 0, "macOS process has no resource coalition ID");
        Ok(Self { id: info[0] })
    }

    pub fn id(self) -> u64 {
        self.id
    }

    /// Count all currently active tasks, including detached descendants. XNU
    /// reads these two counters while holding its coalition lock. A failed or
    /// inconsistent query remains an error; absence never means safe to delete.
    pub fn active_tasks(self) -> Result<u64> {
        match self.state()? {
            ResourceState::Active(count) => Ok(count),
            ResourceState::Reaped => bail!("macOS resource coalition {} was reaped", self.id),
        }
    }

    /// Interpret ESRCH only for a previously observed ID. Other syscall,
    /// library, response, or counter failures remain errors. Receipts from the
    /// earlier observer policy must continue to use strict `active_tasks`.
    pub fn state(self) -> Result<ResourceState> {
        let library = Library::open(c"/usr/lib/libSystem.B.dylib")?;
        let symbol = library.symbol(c"coalition_info_resource_usage")?;
        let query: ResourceUsage = unsafe { std::mem::transmute(symbol) };
        // The first two coalition_resource_usage fields are tasks_started and
        // tasks_exited. XNU copies only min(caller size, its full struct size),
        // so no later SDK-dependent resource fields are needed here.
        let mut counters = [0_u64; 2];
        let result = unsafe {
            query(
                self.id,
                counters.as_mut_ptr().cast(),
                std::mem::size_of_val(&counters),
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if result == -1 && error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(ResourceState::Reaped);
            }
            bail!(
                "macOS resource coalition {} query failed: {}",
                self.id,
                error
            );
        }
        counters[0]
            .checked_sub(counters[1])
            .map(ResourceState::Active)
            .context("macOS coalition task counters are inconsistent")
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    boot_session: String,
    context: Option<PathBuf>,
    coalitions: Vec<u64>,
}

/// Kernel boot identity used to reject observations and receipts from a prior boot.
pub fn boot_session() -> Result<String> {
    let mut buffer = [0_u8; 128];
    let mut length = buffer.len();
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("reading macOS boot session");
    }
    ensure!(
        length == 37 && buffer[36] == 0,
        "unsupported macOS boot session response"
    );
    let value = std::str::from_utf8(&buffer[..36])?;
    ensure!(valid_boot_session(value), "invalid macOS boot session UUID");
    Ok(value.to_owned())
}

pub(crate) fn valid_boot_session(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn receipt_path(paths: &RgoPaths, context: Option<&Path>) -> Result<PathBuf> {
    let scope = if let Some(context) = context {
        use std::os::unix::ffi::OsStrExt;
        let relative = context.strip_prefix(paths.builds_dir())?;
        let mut parts = relative.components();
        ensure!(
            matches!(parts.next(), Some(std::path::Component::Normal(_)))
                && matches!(parts.next(), Some(std::path::Component::Normal(_)))
                && parts.next().is_none(),
            "invalid coalition receipt context"
        );
        format!(
            "context-{}",
            blake3::hash(relative.as_os_str().as_bytes()).to_hex()
        )
    } else {
        "global".to_owned()
    };
    Ok(paths
        .state_dir()
        .join("locks")
        .join(format!("macos-coalition-{scope}.json")))
}

fn receipt_lock(paths: &RgoPaths) -> Result<std::fs::File> {
    paths.ensure_layout()?;
    let path = paths
        .state_dir()
        .join("locks/macos-coalition-receipts.lock");
    let file = crate::supervision::open_lock_file(&path)?;
    FileExt::lock_exclusive(&file)?;
    crate::supervision::verify_lock_identity(&path, &file)?;
    Ok(file)
}

fn read_receipt(path: &Path, context: Option<&Path>) -> Result<Option<Receipt>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening macOS coalition receipt"),
    };
    crate::supervision::verify_lock_identity(path, &file)?;
    ensure!(
        file.metadata()?.len() <= MAX_RECEIPT_BYTES,
        "macOS coalition receipt is oversized"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECEIPT_BYTES,
        "macOS coalition receipt grew beyond its limit"
    );
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        matches!(receipt.version, 1 | 2) && receipt.context.as_deref() == context,
        "unsupported or mismatched macOS coalition receipt"
    );
    ensure!(
        valid_boot_session(&receipt.boot_session),
        "invalid coalition receipt boot session"
    );
    ensure!(
        !receipt.coalitions.is_empty()
            && receipt.coalitions.len() <= MAX_COALITIONS
            && receipt.coalitions.iter().all(|id| *id != 0),
        "invalid coalition receipt identities"
    );
    Ok(Some(receipt))
}

/// Register the calling process's immutable kernel membership while its Cargo
/// session guard is held. Records are outside the evictable context and are
/// synced before admission. The caller cannot delete the receipt on exit: a
/// detached child may still belong to this coalition.
pub(crate) fn register_current(paths: &RgoPaths, context: Option<&Path>) -> Result<()> {
    let coalition = ResourceCoalition::for_pid(std::process::id().try_into()?)?;
    ensure!(
        coalition.active_tasks()? > 0,
        "calling process's coalition is empty"
    );
    let boot_session = boot_session()?;
    let path = receipt_path(paths, context)?;
    let _lock = receipt_lock(paths)?;
    let previous = read_receipt(&path, context)?;
    let mut receipt = match previous {
        Some(receipt) if receipt.boot_session == boot_session => {
            ensure!(
                receipt.version == 2,
                "older coalition receipt remains protected"
            );
            receipt
        }
        _ => Receipt {
            version: 2,
            boot_session,
            context: context.map(Path::to_owned),
            coalitions: Vec::new(),
        },
    };
    if receipt.coalitions.contains(&coalition.id()) {
        return Ok(());
    }
    // These v2 IDs were positively observed before publication. Reaped IDs
    // cannot acquire a new task. Other unresolved observations remain retained.
    receipt.coalitions.retain(|id| {
        ResourceCoalition { id: *id }.state().map_or(
            true,
            |state| matches!(state, ResourceState::Active(count) if count != 0),
        )
    });
    ensure!(
        receipt.coalitions.len() < MAX_COALITIONS,
        "macOS coalition receipt is full"
    );
    receipt.coalitions.push(coalition.id());
    write_receipt(paths, &path, &receipt)
}

fn write_receipt(paths: &RgoPaths, path: &Path, receipt: &Receipt) -> Result<()> {
    let bytes = serde_json::to_vec(&receipt)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECEIPT_BYTES,
        "macOS coalition receipt is oversized"
    );
    let staging = paths.state_dir().join("locks/macos-coalition-write.tmp");
    match std::fs::remove_file(&staging) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&staging, path)?;
    std::fs::File::open(path.parent().context("coalition receipt has no parent")?)?.sync_all()?;
    Ok(())
}

/// A dedicated guardian is the last task in its own coalition and has fenced
/// all future managed work. Retire before its one-use job exit reaps the
/// coalition. Its caller holds the context guard through durable completion.
pub(crate) fn retire_current(paths: &RgoPaths, context: Option<&Path>) -> Result<()> {
    let current = ResourceCoalition::for_pid(std::process::id().try_into()?)?;
    ensure!(
        current.active_tasks()? == 1,
        "macOS guardian still has descendants"
    );
    let boot_session = boot_session()?;
    let path = receipt_path(paths, context)?;
    let _lock = receipt_lock(paths)?;
    let mut receipt = read_receipt(&path, context)?.context("macOS guardian receipt is missing")?;
    ensure!(
        receipt.version == 2
            && receipt.boot_session == boot_session
            && receipt.coalitions.contains(&current.id()),
        "macOS guardian receipt does not contain its kernel identity"
    );
    receipt.coalitions.retain(|id| *id != current.id());
    if receipt.coalitions.is_empty() {
        std::fs::remove_file(&path)?;
        std::fs::File::open(path.parent().context("coalition receipt has no parent")?)?
            .sync_all()?;
        Ok(())
    } else {
        write_receipt(paths, &path, &receipt)
    }
}

/// Called only after GC has acquired its exclusive lifecycle guards, which
/// fence new registered sessions through deletion. A busy or unqueryable
/// coalition protects its scope even after every inherited descriptor closed.
pub(crate) fn permits_gc(paths: &RgoPaths, context: Option<&Path>) -> Result<bool> {
    let _lock = receipt_lock(paths)?;
    let boot_session = boot_session()?;
    let mut scopes = vec![None];
    if context.is_some() {
        scopes.push(context);
    }
    for scope in scopes {
        let path = receipt_path(paths, scope)?;
        let Some(receipt) = read_receipt(&path, scope)? else {
            continue;
        };
        if receipt.boot_session == boot_session {
            for id in receipt.coalitions {
                let coalition = ResourceCoalition { id };
                let busy = if receipt.version == 2 {
                    matches!(coalition.state()?, ResourceState::Active(count) if count != 0)
                } else {
                    coalition.active_tasks()? != 0
                };
                if busy {
                    return Ok(false);
                }
            }
        }
        // A validated record from an earlier boot cannot name a current task.
        // On this boot v1 requires a zero count; v2 also recognizes a reaped,
        // previously observed ID. Every other error retains the receipt.
        std::fs::remove_file(&path)?;
        std::fs::File::open(path.parent().context("coalition receipt has no parent")?)?
            .sync_all()?;
    }
    Ok(true)
}
