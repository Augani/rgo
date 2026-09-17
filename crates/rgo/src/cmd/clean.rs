use std::time::SystemTime;

use anyhow::{Result, bail};
use rgo_core::context;
use rgo_core::gc::{self, Action, LIVE_WINDOW, Plan, Tier};

use super::{env, human};

pub fn run(id: &str) -> Result<()> {
    let e = env()?;
    let now = SystemTime::now();
    let Some(c) = context::list(&e.paths)?
        .into_iter()
        .find(|c| c.id() == id || c.sidecar.as_ref().is_some_and(|s| s.workspace_root == id))
    else {
        bail!("no managed context {id:?}; see `rgo ls`");
    };
    if c.recently_locked(LIVE_WINDOW, now) {
        bail!(
            "{} looks like it is being built right now; refusing",
            c.id()
        );
    }
    let plan = Plan {
        actions: vec![Action {
            tier: Tier::StaleContext,
            path: c.dir.clone(),
            bytes: c.usage.physical_bytes,
            reason: "requested".into(),
        }],
        ..Default::default()
    };
    let n = gc::execute(&e.paths, &plan, false)?;
    println!("removed {} ({})", c.id(), human(n));
    Ok(())
}
