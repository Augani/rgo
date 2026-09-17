use anyhow::{Result, bail};
use rgo_core::context;
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, env};

pub fn run(id: &str, pin: bool) -> Result<()> {
    let e = env()?;
    let Some(context) = context::list(&e.paths)?
        .into_iter()
        .find(|c| c.id() == id || c.sidecar.as_ref().is_some_and(|s| s.workspace_root == id))
    else {
        bail!("no managed context {id:?}; see `rgo ls`");
    };
    if !daemon::ensure_running(&e.paths) {
        bail!("rgo daemon is unavailable; refusing to change pin state");
    }
    let request = if pin {
        Request::Pin {
            build_dir: context.dir.to_string_lossy().into_owned(),
        }
    } else {
        Request::Unpin {
            build_dir: context.dir.to_string_lossy().into_owned(),
        }
    };
    match ipc::request_with_timeout(
        &e.paths.socket_path(),
        request,
        std::time::Duration::from_secs(5),
    )? {
        Response::Ok => {
            println!(
                "{} {}",
                if pin { "pinned" } else { "unpinned" },
                context.id()
            );
            Ok(())
        }
        Response::Error { code, message } => bail!("pin operation failed ({code}): {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}
