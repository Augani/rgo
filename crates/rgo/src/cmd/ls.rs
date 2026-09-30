use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use rgo_core::context;
use rgo_core::db::StateDb;

use super::{display_workspace_path, env, human};

pub fn run() -> Result<()> {
    let e = env()?;
    let contexts = context::list(&e.paths)?;
    let (pinned, leased) = match StateDb::open_read_only(&e.paths) {
        Ok(db) => (db.pinned_paths()?, db.protected_paths(&contexts)?),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let now = SystemTime::now();
    println!(
        "{:<18} {:>10} {:>10} {:>10}  {:<3} {:<3} WORKSPACE",
        "ID", "SIZE", "INCR", "IDLE", "PIN", "USE"
    );

    // Contexts whose workspaces share a Git common dir (linked worktrees of the
    // same repository) are listed together under a repo header.
    let keys: Vec<Option<PathBuf>> = contexts
        .iter()
        .map(|c| {
            c.sidecar
                .as_ref()
                .and_then(|s| context::git_common_dir(Path::new(&s.workspace_root)))
        })
        .collect();
    let mut clusters: HashMap<&PathBuf, Vec<usize>> = HashMap::new();
    for (index, key) in keys.iter().enumerate() {
        if let Some(key) = key {
            clusters.entry(key).or_default().push(index);
        }
    }
    clusters.retain(|_, members| members.len() > 1);

    let print_row = |c: &context::BuildContext| {
        let ws = match &c.sidecar {
            Some(s) if c.is_orphan() => {
                format!(
                    "{} (orphan)",
                    display_workspace_path(Path::new(&s.workspace_root))
                )
            }
            Some(s) if c.workspace_unavailable() => {
                format!(
                    "{} (workspace unavailable; protected)",
                    display_workspace_path(Path::new(&s.workspace_root))
                )
            }
            Some(s) => display_workspace_path(Path::new(&s.workspace_root)),
            None => "? (unattributed; protected)".to_owned(),
        };
        let idle = humantime::format_duration(std::time::Duration::from_secs(
            c.idle_for(now).as_secs() / 60 * 60,
        ));
        println!(
            "{:<18} {:>10} {:>10} {:>10}  {:<3} {:<3} {}",
            c.id(),
            human(c.usage.physical_bytes),
            human(c.incremental_usage.physical_bytes),
            idle,
            if c.is_pinned(&e.paths) || pinned.iter().any(|p| same_path(p, &c.dir)) {
                "PIN"
            } else {
                ""
            },
            if leased.iter().any(|p| same_path(p, &c.dir)) {
                "LIVE"
            } else {
                ""
            },
            ws
        );
    };

    let mut emitted = vec![false; contexts.len()];
    for index in 0..contexts.len() {
        if emitted[index] {
            continue;
        }
        if let Some(key) = &keys[index] {
            if let Some(members) = clusters.get(key) {
                println!("── {} ({} contexts)", repo_label(key), members.len());
                for &member in members {
                    print_row(&contexts[member]);
                    emitted[member] = true;
                }
                continue;
            }
        }
        print_row(&contexts[index]);
        emitted[index] = true;
    }
    for path in context::durable_pin_contexts(&e.paths)? {
        if contexts.iter().any(|context| context.dir == path) {
            continue;
        }
        let relative = path.strip_prefix(e.paths.builds_dir())?;
        let id = context::context_id(relative);
        println!(
            "{:<18} {:>10} {:>10} {:>10}  {:<3} {:<3} (context absent; pin retained)",
            id, "-", "-", "-", "PIN", ""
        );
    }
    Ok(())
}

/// `<repo>/.git` displays as `<repo>`; anything else shows the resolved path.
fn repo_label(common: &Path) -> String {
    match common.file_name() {
        Some(name) if name == ".git" => display_workspace_path(common.parent().unwrap_or(common)),
        _ => display_workspace_path(common),
    }
}

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf())
        == std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf())
}
