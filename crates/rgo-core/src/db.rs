//! Rebuildable SQLite metadata for the local daemon.
//!
//! The filesystem remains authoritative for Cargo state. SQLite stores attribution, leases,
//! pins, and accounting so the daemon can coordinate concurrent clients without scanning the
//! entire managed tree for every heartbeat.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rgo_protocol::{CacheEvent, CacheManifest, CacheStatsReport, LeaseScope, PROTOCOL_VERSION};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::context;
use crate::paths::RgoPaths;

const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, Default)]
pub struct DbStats {
    pub active_leases: u64,
    pub pinned_contexts: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LastGc {
    pub reclaimed_bytes: u64,
    pub finished_at: u64,
    pub error: Option<String>,
}

pub struct StateDb {
    connection: Connection,
}

impl StateDb {
    /// Open the daemon index without creating or mutating it. CLI inspection commands use this
    /// path so all metadata writes remain daemon-owned.
    pub fn open_read_only(paths: &RgoPaths) -> Result<Self> {
        let connection =
            Connection::open_with_flags(paths.db_file(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening {} read-only", paths.db_file().display()))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(2))?;
        Ok(Self { connection })
    }

    pub fn open(paths: &RgoPaths) -> Result<Self> {
        std::fs::create_dir_all(paths.state_dir())
            .with_context(|| format!("creating {}", paths.state_dir().display()))?;
        match Self::open_initialized(&paths.db_file()) {
            Ok(db) => Ok(db),
            Err(first_error) => {
                let corrupt = paths
                    .state_dir()
                    .join(format!("meta.sqlite.corrupt-{}", unix_now()));
                if paths.db_file().exists() {
                    std::fs::rename(paths.db_file(), &corrupt).with_context(|| {
                        format!(
                            "moving corrupt database to {} after: {first_error:#}",
                            corrupt.display()
                        )
                    })?;
                }
                let db = Self::open_initialized(&paths.db_file())
                    .context("rebuilding SQLite metadata database")?;
                tracing::warn!(path = %corrupt.display(), error = %first_error, "rebuilt corrupt rgo database");
                Ok(db)
            }
        }
    }

    fn open_initialized(path: &Path) -> Result<Self> {
        let connection =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS schema_meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS contexts (
                build_dir TEXT PRIMARY KEY NOT NULL,
                workspace_root TEXT,
                manifest_path TEXT,
                last_seen INTEGER NOT NULL DEFAULT 0,
                last_used INTEGER NOT NULL DEFAULT 0,
                physical_bytes INTEGER NOT NULL DEFAULT 0,
                incremental_bytes INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS leases (
                lease_id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope TEXT NOT NULL,
                workspace_root TEXT,
                build_dir TEXT,
                owner_pid INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS leases_expiry ON leases(expires_at);
            CREATE INDEX IF NOT EXISTS leases_build_dir ON leases(build_dir);
            CREATE INDEX IF NOT EXISTS leases_workspace ON leases(workspace_root);
            CREATE TABLE IF NOT EXISTS pins (
                build_dir TEXT PRIMARY KEY NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS gc_runs (
                run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                dry_run INTEGER NOT NULL,
                aggressive INTEGER NOT NULL,
                reclaimed_bytes INTEGER NOT NULL DEFAULT 0,
                skipped_live INTEGER NOT NULL DEFAULT 0,
                skipped_leased INTEGER NOT NULL DEFAULT 0,
                error TEXT
            );
            CREATE TABLE IF NOT EXISTS access_summary (
                build_dir TEXT PRIMARY KEY NOT NULL,
                touch_count INTEGER NOT NULL DEFAULT 0,
                last_touched INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS cache_entries (
                key TEXT PRIMARY KEY NOT NULL,
                manifest_path TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                last_used INTEGER NOT NULL DEFAULT 0,
                hits INTEGER NOT NULL DEFAULT 0,
                misses INTEGER NOT NULL DEFAULT 0,
                bypasses INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS cache_objects (
                digest TEXT PRIMARY KEY NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                last_verified INTEGER NOT NULL DEFAULT 0,
                quarantined INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS cache_references (
                key TEXT NOT NULL,
                digest TEXT NOT NULL,
                PRIMARY KEY(key, digest),
                FOREIGN KEY(key) REFERENCES cache_entries(key) ON DELETE CASCADE,
                FOREIGN KEY(digest) REFERENCES cache_objects(digest) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS cache_events (
                event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                key TEXT,
                outcome TEXT NOT NULL,
                bytes INTEGER NOT NULL DEFAULT 0,
                reason TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS cache_events_key ON cache_events(key, created_at);
            CREATE TABLE IF NOT EXISTS cache_verifications (
                verify_id INTEGER PRIMARY KEY AUTOINCREMENT,
                verified_at INTEGER NOT NULL,
                checked_manifests INTEGER NOT NULL,
                checked_objects INTEGER NOT NULL,
                quarantined INTEGER NOT NULL,
                error TEXT
            );
            ",
        )?;
        let current: Option<i64> = connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| value.parse::<i64>())
            .transpose()
            .context("reading schema version")?;
        if let Some(version) = current {
            if version > SCHEMA_VERSION {
                bail!(
                    "unsupported rgo database schema {version}, expected at most {SCHEMA_VERSION}"
                );
            }
            // Version 1 had all Phase 2 tables. Phase 3 tables are additive and are created
            // above, so advancing the marker is a safe migration.
        }
        connection.execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        connection.execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('protocol_version', ?1)",
            params![PROTOCOL_VERSION.to_string()],
        )?;
        Ok(Self { connection })
    }

    pub fn reconcile(&mut self, paths: &RgoPaths) -> Result<()> {
        let contexts = context::list(paths)?;
        self.reconcile_contexts(&contexts)
    }

    pub fn reconcile_contexts(&mut self, contexts: &[context::BuildContext]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let mut current = HashSet::new();
        for item in contexts {
            let path = normalize(&item.dir);
            current.insert(path.to_string_lossy().into_owned());
            let (workspace_root, manifest_path) = item
                .sidecar
                .as_ref()
                .map(|s| {
                    (
                        Some(s.workspace_root.as_str()),
                        Some(s.manifest_path.as_str()),
                    )
                })
                .unwrap_or((None, None));
            transaction.execute(
                "INSERT INTO contexts(build_dir, workspace_root, manifest_path, last_seen, last_used, physical_bytes, incremental_bytes)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(build_dir) DO UPDATE SET
                   workspace_root=excluded.workspace_root,
                   manifest_path=excluded.manifest_path,
                   last_seen=excluded.last_seen,
                   last_used=excluded.last_used,
                   physical_bytes=excluded.physical_bytes,
                   incremental_bytes=excluded.incremental_bytes",
                params![
                    path.to_string_lossy(),
                    workspace_root,
                    manifest_path,
                    unix_time(item.last_used),
                    unix_time(item.last_used),
                    item.usage.physical_bytes as i64,
                    item.incremental_usage.physical_bytes as i64,
                ],
            )?;
        }
        let mut statement = transaction.prepare("SELECT build_dir FROM contexts")?;
        let old: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for path in old {
            if !current.contains(&path) {
                transaction.execute("DELETE FROM contexts WHERE build_dir = ?1", params![path])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn touch(
        &self,
        build_dir: &Path,
        workspace_root: Option<&str>,
        physical_bytes: Option<u64>,
        incremental_bytes: Option<u64>,
    ) -> Result<()> {
        let path = normalize(build_dir);
        let now = unix_now();
        self.connection.execute(
            "INSERT INTO contexts(build_dir, workspace_root, last_seen, last_used, physical_bytes, incremental_bytes)
             VALUES(?1, ?2, ?3, ?3, COALESCE(?4, 0), COALESCE(?5, 0))
             ON CONFLICT(build_dir) DO UPDATE SET
               workspace_root=COALESCE(excluded.workspace_root, contexts.workspace_root),
               last_seen=excluded.last_seen,
               last_used=excluded.last_used,
               physical_bytes=COALESCE(?4, contexts.physical_bytes),
               incremental_bytes=COALESCE(?5, contexts.incremental_bytes)",
            params![
                path.to_string_lossy(),
                workspace_root,
                now,
                physical_bytes.map(|v| v as i64),
                incremental_bytes.map(|v| v as i64),
            ],
        )?;
        self.connection.execute(
            "INSERT INTO access_summary(build_dir, touch_count, last_touched) VALUES(?1, 1, ?2)
             ON CONFLICT(build_dir) DO UPDATE SET touch_count=touch_count+1, last_touched=excluded.last_touched",
            params![path.to_string_lossy(), now],
        )?;
        Ok(())
    }

    pub fn acquire(&self, scope: &LeaseScope, pid: u32, ttl_secs: u32) -> Result<(u64, u32)> {
        self.expire_leases()?;
        let now = unix_now();
        let expires = now.saturating_add(u64::from(ttl_secs.max(1)));
        let (scope_name, workspace_root, build_dir) = match scope {
            LeaseScope::Workspace { workspace_root } => ("workspace", Some(workspace_root), None),
            LeaseScope::Context { build_dir } => ("context", None, Some(build_dir)),
            LeaseScope::Cache { key } => ("cache", Some(key), None),
        };
        self.connection.execute(
            "INSERT INTO leases(scope, workspace_root, build_dir, owner_pid, created_at, heartbeat_at, expires_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![scope_name, workspace_root, build_dir, i64::from(pid), now, expires],
        )?;
        Ok((self.connection.last_insert_rowid() as u64, ttl_secs.max(1)))
    }

    pub fn bind(
        &self,
        lease_id: u64,
        build_dir: &Path,
        workspace_root: Option<&str>,
    ) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE leases SET build_dir = ?1, workspace_root = COALESCE(?2, workspace_root)
             WHERE lease_id = ?3 AND expires_at > ?4",
            params![
                normalize(build_dir).to_string_lossy(),
                workspace_root,
                lease_id as i64,
                unix_now()
            ],
        )?;
        if changed == 0 {
            bail!("lease {lease_id} is missing or expired");
        }
        Ok(())
    }

    pub fn heartbeat(&self, lease_id: u64, ttl_secs: u32) -> Result<u32> {
        let now = unix_now();
        let expires = now.saturating_add(u64::from(ttl_secs.max(1)));
        let changed = self.connection.execute(
            "UPDATE leases SET heartbeat_at = ?1, expires_at = ?2 WHERE lease_id = ?3 AND expires_at > ?4",
            params![now, expires, lease_id as i64, now],
        )?;
        if changed == 0 {
            bail!("lease {lease_id} is missing or expired");
        }
        Ok(ttl_secs.max(1))
    }

    pub fn release(&self, lease_id: u64) -> Result<()> {
        self.connection.execute(
            "DELETE FROM leases WHERE lease_id = ?1",
            params![lease_id as i64],
        )?;
        Ok(())
    }

    pub fn expire_leases(&self) -> Result<usize> {
        Ok(self.connection.execute(
            "DELETE FROM leases WHERE expires_at <= ?1",
            params![unix_now()],
        )?)
    }

    pub fn protected_paths(&self, contexts: &[context::BuildContext]) -> Result<Vec<PathBuf>> {
        let mut paths = HashSet::new();
        let mut query = self
            .connection
            .prepare("SELECT build_dir, workspace_root FROM leases WHERE expires_at > ?1")?;
        let leases = query.query_map(params![unix_now()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?;
        for lease in leases {
            let (build_dir, workspace_root) = lease?;
            if let Some(dir) = build_dir {
                paths.insert(normalize(Path::new(&dir)));
            }
            if let Some(root) = workspace_root {
                for context in contexts {
                    if context
                        .sidecar
                        .as_ref()
                        .is_some_and(|sidecar| sidecar.workspace_root == root)
                    {
                        paths.insert(normalize(&context.dir));
                    }
                }
            }
        }
        Ok(paths.into_iter().collect())
    }

    pub fn active_cache_keys(&self) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_root FROM leases WHERE scope = 'cache' AND expires_at > ?1 AND workspace_root IS NOT NULL",
        )?;
        statement
            .query_map(params![unix_now()], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(Into::into)
    }

    pub fn pinned_paths(&self) -> Result<Vec<PathBuf>> {
        let mut statement = self.connection.prepare("SELECT build_dir FROM pins")?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| row.map(|path| normalize(Path::new(&path))))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(paths)
    }

    pub fn set_pin(&self, path: &Path, pinned: bool) -> Result<()> {
        let path = normalize(path);
        if pinned {
            self.connection.execute(
                "INSERT OR IGNORE INTO pins(build_dir, created_at) VALUES(?1, ?2)",
                params![path.to_string_lossy(), unix_now()],
            )?;
        } else {
            self.connection.execute(
                "DELETE FROM pins WHERE build_dir = ?1",
                params![path.to_string_lossy()],
            )?;
        }
        Ok(())
    }

    pub fn stats(&self) -> Result<DbStats> {
        Ok(DbStats {
            active_leases: self.connection.query_row(
                "SELECT COUNT(*) FROM leases WHERE expires_at > ?1",
                params![unix_now()],
                |row| row.get::<_, i64>(0),
            )? as u64,
            pinned_contexts: self
                .connection
                .query_row("SELECT COUNT(*) FROM pins", [], |row| row.get::<_, i64>(0))?
                as u64,
        })
    }

    pub fn cache_stats(&self, enabled: bool, cas_bytes: u64) -> Result<CacheStatsReport> {
        let manifests =
            self.connection
                .query_row("SELECT COUNT(*) FROM cache_entries", [], |row| {
                    row.get::<_, i64>(0)
                })? as u64;
        let objects = self.connection.query_row(
            "SELECT COUNT(*) FROM cache_objects WHERE quarantined = 0",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let hits = self.connection.query_row(
            "SELECT COALESCE(SUM(hits), 0) FROM cache_entries",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let misses = self.connection.query_row(
            "SELECT COALESCE(SUM(misses), 0) FROM cache_entries",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let bypasses = self.connection.query_row(
            "SELECT COUNT(*) FROM cache_events WHERE outcome = 'bypass'",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let (last_verify_at, last_verify_error): (u64, Option<String>) = self.connection.query_row(
            "SELECT verified_at, error FROM cache_verifications ORDER BY verify_id DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get(1)?)),
        ).optional()?.unwrap_or_default();
        Ok(CacheStatsReport {
            enabled,
            manifests,
            objects,
            cas_bytes,
            hits,
            misses,
            bypasses,
            last_verify_at,
            last_verify_error,
        })
    }

    pub fn record_cache_event(&self, event: &CacheEvent) -> Result<()> {
        let now = unix_now();
        self.connection.execute(
            "INSERT INTO cache_events(key, outcome, bytes, reason, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![event.key, event.outcome, event.bytes as i64, event.reason, now],
        )?;
        if let Some(key) = &event.key {
            let column = match event.outcome.as_str() {
                "hit" => "hits",
                "miss" => "misses",
                _ => return Ok(()),
            };
            self.connection.execute(
                &format!("UPDATE cache_entries SET {column} = {column} + 1, last_used = ?1 WHERE key = ?2"),
                params![now, key],
            )?;
        }
        Ok(())
    }

    pub fn record_cache_manifest(
        &self,
        manifest: &CacheManifest,
        manifest_path: &Path,
    ) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO cache_entries(key, manifest_path, created_at, last_used)
             VALUES(?1, ?2, ?3, ?3)
             ON CONFLICT(key) DO UPDATE SET manifest_path=excluded.manifest_path",
            params![
                manifest.key,
                manifest_path.to_string_lossy(),
                manifest.created_at as i64
            ],
        )?;
        transaction.execute(
            "DELETE FROM cache_references WHERE key = ?1",
            params![manifest.key],
        )?;
        for object in manifest
            .outputs
            .iter()
            .map(|output| &output.object)
            .chain(manifest.stdout.iter())
            .chain(manifest.stderr.iter())
        {
            transaction.execute(
                "INSERT INTO cache_objects(digest, size, last_verified) VALUES(?1, ?2, ?3)
                 ON CONFLICT(digest) DO UPDATE SET size=excluded.size, last_verified=excluded.last_verified, quarantined=0",
                params![object.digest, object.size as i64, unix_now()],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO cache_references(key, digest) VALUES(?1, ?2)",
                params![manifest.key, object.digest],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn record_cache_verify(&self, report: &rgo_protocol::CacheVerifyReport) -> Result<()> {
        self.connection.execute(
            "INSERT INTO cache_verifications(verified_at, checked_manifests, checked_objects, quarantined, error)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                unix_now(),
                report.checked_manifests as i64,
                report.checked_objects as i64,
                report.quarantined as i64,
                (!report.errors.is_empty()).then(|| report.errors.join("; ")),
            ],
        )?;
        Ok(())
    }

    pub fn cache_explanation(&self, key: &str) -> Result<rgo_protocol::CacheExplanation> {
        let entry = self
            .connection
            .query_row(
                "SELECT key, hits, misses FROM cache_entries WHERE key = ?1",
                params![key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((key, hits, _misses)) = entry {
            let outputs = self
                .connection
                .prepare("SELECT digest FROM cache_references WHERE key = ?1 ORDER BY digest")?
                .query_map(params![&key], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            return Ok(rgo_protocol::CacheExplanation {
                key: key.clone(),
                state: if hits > 0 {
                    "hit".into()
                } else {
                    "miss".into()
                },
                reason: None,
                outputs,
            });
        }
        let event = self.connection.query_row(
            "SELECT outcome, reason FROM cache_events WHERE key = ?1 ORDER BY event_id DESC LIMIT 1",
            params![key],
            |row| Ok((row.get::<_, String>(0)?, row.get(1)?)),
        ).optional()?;
        Ok(rgo_protocol::CacheExplanation {
            key: key.into(),
            state: event
                .as_ref()
                .map_or_else(|| "unknown".into(), |(outcome, _)| outcome.clone()),
            reason: event.and_then(|(_, reason)| reason),
            outputs: Vec::new(),
        })
    }

    pub fn record_gc(
        &self,
        dry_run: bool,
        aggressive: bool,
        report: &rgo_protocol::GcReport,
        error: Option<&str>,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO gc_runs(started_at, finished_at, dry_run, aggressive, reclaimed_bytes, skipped_live, skipped_leased, error)
             VALUES(?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                unix_now(),
                i64::from(dry_run),
                i64::from(aggressive),
                report.reclaimed_bytes as i64,
                report.skipped_live as i64,
                report.skipped_leased as i64,
                error,
            ],
        )?;
        Ok(())
    }

    pub fn last_gc(&self) -> Result<Option<LastGc>> {
        self.connection
            .query_row(
                "SELECT reclaimed_bytes, finished_at, error FROM gc_runs ORDER BY run_id DESC LIMIT 1",
                [],
                |row| {
                    Ok(LastGc {
                        reclaimed_bytes: row.get::<_, i64>(0)? as u64,
                        finished_at: row.get::<_, Option<i64>>(1)?.unwrap_or_default() as u64,
                        error: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_time(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn database_initializes_wal_and_leases_expire() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let (lease, _) = db
            .acquire(
                &LeaseScope::Context {
                    build_dir: "/tmp/build".into(),
                },
                1,
                1,
            )
            .unwrap();
        assert_eq!(db.stats().unwrap().active_leases, 1);
        db.release(lease).unwrap();
        assert_eq!(db.stats().unwrap().active_leases, 0);
        assert_eq!(
            db.connection
                .query_row::<String, _, _>("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap(),
            "wal"
        );
    }

    #[test]
    fn pins_are_idempotent() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        db.set_pin(Path::new("/tmp/build"), true).unwrap();
        db.set_pin(Path::new("/tmp/build"), true).unwrap();
        assert_eq!(db.stats().unwrap().pinned_contexts, 1);
        db.set_pin(Path::new("/tmp/build"), false).unwrap();
        assert_eq!(db.stats().unwrap().pinned_contexts, 0);
    }

    #[test]
    fn corrupt_database_is_moved_and_rebuilt() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        std::fs::write(paths.db_file(), b"not sqlite").unwrap();
        let _db = StateDb::open(&paths).unwrap();
        assert!(paths.db_file().exists());
        assert!(
            std::fs::read_dir(paths.state_dir())
                .unwrap()
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("meta.sqlite.corrupt-"))
        );
    }
}
