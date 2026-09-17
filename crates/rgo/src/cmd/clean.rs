use anyhow::{Result, bail};
use rgo_core::context;
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, env, human};

pub fn run(id: &str) -> Result<()> {
    let e = env()?;
    let Some(c) = context::list(&e.paths)?
        .into_iter()
        .find(|c| c.id() == id || c.sidecar.as_ref().is_some_and(|s| s.workspace_root == id))
    else {
        bail!("no managed context {id:?}; see `rgo ls`");
    };
    if !daemon::ensure_running(&e.paths) {
        bail!("rgo daemon is unavailable; refusing coordinated clean");
    }
    let response = ipc::request_with_timeout(
        &e.paths.socket_path(),
        Request::Clean {
            build_dir: c.dir.to_string_lossy().into_owned(),
        },
        std::time::Duration::from_secs(30),
    )?;
    let n = match response {
        Response::Gc(report) => report.reclaimed_bytes,
        Response::Error { code, message } => bail!("clean failed ({code}): {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    };
    println!("removed {} ({})", c.id(), human(n));
    Ok(())
}
