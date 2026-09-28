use std::path::{Component, Path};

use anyhow::{Result, bail};
use rgo_core::context;
use rgo_core::ipc;
use rgo_protocol::{Request, Response};

use super::{daemon, env};

pub fn run(id: &str, pin: bool) -> Result<()> {
    let e = env()?;
    let existing = context::list(&e.paths)?
        .into_iter()
        .find(|c| c.id() == id || c.sidecar.as_ref().is_some_and(|s| s.workspace_root == id));
    let (path, label) = if let Some(context) = existing {
        let label = context.id();
        (context.dir, label)
    } else if !pin {
        // A full `cargo clean` removes the context, but its durable pin may
        // still need to be released before the next build.
        let parts = Path::new(id).components().collect::<Vec<_>>();
        if parts.len() != 2
            || !parts
                .iter()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("no managed context or durable pin {id:?}; see `rgo ls`");
        }
        let path = e.paths.builds_dir().join(id);
        if !context::is_pinned(&e.paths, &path) {
            bail!("no managed context or durable pin {id:?}; see `rgo ls`");
        }
        (path, id.to_owned())
    } else {
        bail!("no managed context {id:?}; see `rgo ls`");
    };
    if !daemon::ensure_running(&e.paths) {
        bail!("rgo daemon is unavailable; refusing to change pin state");
    }
    let request = if pin {
        Request::Pin {
            build_dir: path.to_string_lossy().into_owned(),
        }
    } else {
        Request::Unpin {
            build_dir: path.to_string_lossy().into_owned(),
        }
    };
    match ipc::request_with_timeout(
        &e.paths.socket_path(),
        request,
        std::time::Duration::from_secs(5),
    )? {
        Response::Ok => {
            println!("{} {}", if pin { "pinned" } else { "unpinned" }, label);
            Ok(())
        }
        Response::Error { code, message } => bail!("pin operation failed ({code}): {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}
