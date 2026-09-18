//! Rebuildable SQLite metadata for the local daemon.
//!
//! The filesystem remains authoritative for Cargo state. SQLite stores attribution, leases,
//! pins, and accounting so the daemon can coordinate concurrent clients without scanning the
//! entire managed tree for every heartbeat.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rgo_protocol::{
    CacheEvent, CacheManifest, CacheStatsReport, LeaseScope, PROTOCOL_VERSION, RemoteStatusReport,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::context;
use crate::paths::RgoPaths;

const SCHEMA_VERSION: i64 = 4;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheBuildDecision {
    Producer { lease_id: u64, expires_in_secs: u32 },
    Wait { expires_in_secs: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFetchDecision {
    Started { lease_id: u64 },
    Wait,
    Build(CacheBuildDecision),
}

#[derive(Debug, Clone, Default)]
pub struct SingleFlightStats {
    pub producers: u64,
    pub waiters: u64,
    pub timeouts: u64,
    pub takeovers: u64,
    pub active_builds: u64,
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
                let stamp = unix_now();
                let corrupt = paths
                    .state_dir()
                    .join(format!("meta.sqlite.corrupt-{stamp}"));
                for (source, destination) in [
                    (paths.db_file(), corrupt.clone()),
                    (
                        paths.state_dir().join("meta.sqlite-wal"),
                        paths
                            .state_dir()
                            .join(format!("meta.sqlite-wal.corrupt-{stamp}")),
                    ),
                    (
                        paths.state_dir().join("meta.sqlite-shm"),
                        paths
                            .state_dir()
                            .join(format!("meta.sqlite-shm.corrupt-{stamp}")),
                    ),
                ] {
                    if source.exists() {
                        std::fs::rename(&source, &destination).with_context(|| {
                            format!(
                                "moving corrupt database artifact to {} after: {first_error:#}",
                                destination.display()
                            )
                        })?;
                    }
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
            CREATE TABLE IF NOT EXISTS cache_builds (
                key TEXT PRIMARY KEY NOT NULL,
                state TEXT NOT NULL,
                owner_lease_id INTEGER,
                started_at INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                manifest_path TEXT,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS cache_builds_state ON cache_builds(state);
            CREATE TABLE IF NOT EXISTS single_flight_stats (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                producers INTEGER NOT NULL DEFAULT 0,
                waiters INTEGER NOT NULL DEFAULT 0,
                timeouts INTEGER NOT NULL DEFAULT 0,
                takeovers INTEGER NOT NULL DEFAULT 0
            );
            INSERT OR IGNORE INTO single_flight_stats(id) VALUES (1);
            CREATE TABLE IF NOT EXISTS remote_meta (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                endpoint TEXT NOT NULL DEFAULT '',
                namespace TEXT NOT NULL DEFAULT '',
                config_fingerprint TEXT NOT NULL DEFAULT '',
                last_probe INTEGER NOT NULL DEFAULT 0,
                healthy INTEGER NOT NULL DEFAULT 0,
                last_error TEXT
            );
            INSERT OR IGNORE INTO remote_meta(id) VALUES (1);
            CREATE TABLE IF NOT EXISTS remote_jobs (
                job_id INTEGER PRIMARY KEY AUTOINCREMENT,
                key TEXT NOT NULL,
                kind TEXT NOT NULL,
                digest TEXT,
                status TEXT NOT NULL DEFAULT 'PENDING',
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt_at INTEGER NOT NULL DEFAULT 0,
                bytes INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(key, kind, digest)
            );
            CREATE INDEX IF NOT EXISTS remote_jobs_ready ON remote_jobs(status, next_attempt_at);
            CREATE TABLE IF NOT EXISTS remote_counters (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                hits INTEGER NOT NULL DEFAULT 0,
                misses INTEGER NOT NULL DEFAULT 0,
                authentication_failures INTEGER NOT NULL DEFAULT 0,
                corruptions INTEGER NOT NULL DEFAULT 0,
                uploads INTEGER NOT NULL DEFAULT 0,
                downloads INTEGER NOT NULL DEFAULT 0,
                upload_bytes INTEGER NOT NULL DEFAULT 0,
                download_bytes INTEGER NOT NULL DEFAULT 0,
                retries INTEGER NOT NULL DEFAULT 0
            );
            INSERT OR IGNORE INTO remote_counters(id) VALUES (1);
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
            // Phase 3 and Phase 4 tables are additive, so advancing the marker is a safe
            // migration for databases created by earlier versions.
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
        // Pins are durable marker files inside build dirs; the table is a derived
        // index rebuilt here so pins survive database loss, and a marker deleted
        // out-of-band drops the pin.
        let marked: HashSet<String> = contexts
            .iter()
            .filter(|c| context::is_pinned_dir(&c.dir))
            .map(|c| normalize(&c.dir).to_string_lossy().into_owned())
            .collect();
        for path in &marked {
            transaction.execute(
                "INSERT OR IGNORE INTO pins(build_dir, created_at) VALUES(?1, ?2)",
                params![path, unix_time(SystemTime::now())],
            )?;
        }
        let mut statement = transaction.prepare("SELECT build_dir FROM pins")?;
        let known_pins: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for path in known_pins {
            if !marked.contains(&path) {
                transaction.execute("DELETE FROM pins WHERE build_dir = ?1", params![path])?;
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

    /// Ephemeral producer ownership must never survive a daemon restart. A manifest already
    /// published in CAS remains usable; an in-progress row is simply made retryable.
    pub fn recover_cache_builds(&self) -> Result<()> {
        let now = unix_now();
        self.connection.execute(
            "UPDATE cache_builds
             SET state = 'FAILED', owner_lease_id = NULL, expires_at = ?1,
                 heartbeat_at = ?1, error = 'daemon_restart'
             WHERE state IN ('BUILDING', 'COMMITTING', 'REMOTE_FETCHING')",
            params![now],
        )?;
        self.connection.execute(
            "DELETE FROM leases WHERE scope IN ('cache_build', 'cache_remote')",
            [],
        )?;
        Ok(())
    }

    pub fn acquire_cache_build(
        &self,
        key: &str,
        pid: u32,
        ttl_secs: u32,
        register_waiter: bool,
    ) -> Result<CacheBuildDecision> {
        self.expire_leases()?;
        let now = unix_now();
        let ttl = u64::from(ttl_secs.max(1));
        let transaction = self.connection.unchecked_transaction()?;
        let existing: Option<(String, Option<i64>, i64)> = transaction
            .query_row(
                "SELECT state, owner_lease_id, expires_at FROM cache_builds WHERE key = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let active = if let Some((_, Some(owner), expires)) = &existing {
            let lease_active: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM leases WHERE lease_id = ?1 AND expires_at > ?2",
                params![owner, now],
                |row| row.get(0),
            )?;
            *expires as u64 > now && lease_active > 0
        } else {
            false
        };
        if active {
            if register_waiter {
                transaction.execute(
                    "UPDATE single_flight_stats SET waiters = waiters + 1 WHERE id = 1",
                    [],
                )?;
            }
            transaction.commit()?;
            let expires = existing
                .map(|(_, _, expires)| expires as u64)
                .unwrap_or(now);
            return Ok(CacheBuildDecision::Wait {
                expires_in_secs: expires.saturating_sub(now).min(u64::from(u32::MAX)) as u32,
            });
        }

        let stale_owner = existing
            .as_ref()
            .is_some_and(|(_, owner, _)| owner.is_some() && !active);
        let expires = now.saturating_add(ttl);
        transaction.execute(
            "INSERT INTO leases(scope, owner_pid, created_at, heartbeat_at, expires_at)
             VALUES('cache_build', ?1, ?2, ?2, ?3)",
            params![i64::from(pid), now, expires],
        )?;
        let lease_id = transaction.last_insert_rowid() as u64;
        transaction.execute(
            "INSERT INTO cache_builds(key, state, owner_lease_id, started_at, heartbeat_at, expires_at, error)
             VALUES(?1, 'BUILDING', ?2, ?3, ?3, ?4, NULL)
             ON CONFLICT(key) DO UPDATE SET
               state = 'BUILDING', owner_lease_id = excluded.owner_lease_id,
               started_at = excluded.started_at, heartbeat_at = excluded.heartbeat_at,
               expires_at = excluded.expires_at, error = NULL",
            params![key, lease_id as i64, now, expires],
        )?;
        transaction.execute(
            "UPDATE single_flight_stats SET producers = producers + 1, takeovers = takeovers + ?1 WHERE id = 1",
            params![if stale_owner { 1i64 } else { 0i64 }],
        )?;
        transaction.commit()?;
        Ok(CacheBuildDecision::Producer {
            lease_id,
            expires_in_secs: ttl as u32,
        })
    }

    /// Reserve a key for the daemon's remote fetch worker. The reservation is kept in the same
    /// single-flight table as local producers so waiters never issue network requests themselves.
    pub fn acquire_remote_fetch(
        &self,
        key: &str,
        pid: u32,
        ttl_secs: u32,
    ) -> Result<RemoteFetchDecision> {
        self.expire_leases()?;
        let now = unix_now();
        let ttl = u64::from(ttl_secs.max(1));
        let transaction = self.connection.unchecked_transaction()?;
        let existing: Option<(String, Option<i64>, i64)> = transaction
            .query_row(
                "SELECT state, owner_lease_id, expires_at FROM cache_builds WHERE key = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((state, Some(owner), expires)) = &existing {
            let active: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM leases WHERE lease_id = ?1 AND expires_at > ?2",
                params![owner, now],
                |row| row.get(0),
            )?;
            if *expires as u64 > now && active != 0 {
                let decision = if state == "REMOTE_FETCHING" {
                    RemoteFetchDecision::Wait
                } else {
                    RemoteFetchDecision::Build(CacheBuildDecision::Wait {
                        expires_in_secs: (*expires as u64).saturating_sub(now) as u32,
                    })
                };
                transaction.commit()?;
                return Ok(decision);
            }
        }
        let expires = now.saturating_add(ttl);
        transaction.execute(
            "INSERT INTO leases(scope, owner_pid, created_at, heartbeat_at, expires_at)
             VALUES('cache_remote', ?1, ?2, ?2, ?3)",
            params![i64::from(pid), now, expires],
        )?;
        let lease_id = transaction.last_insert_rowid() as u64;
        transaction.execute(
            "INSERT INTO cache_builds(key, state, owner_lease_id, started_at, heartbeat_at, expires_at, error)
             VALUES(?1, 'REMOTE_FETCHING', ?2, ?3, ?3, ?4, NULL)
             ON CONFLICT(key) DO UPDATE SET state='REMOTE_FETCHING', owner_lease_id=?2,
               started_at=?3, heartbeat_at=?3, expires_at=?4, error=NULL",
            params![key, lease_id as i64, now, expires],
        )?;
        transaction.commit()?;
        Ok(RemoteFetchDecision::Started { lease_id })
    }

    pub fn finish_remote_fetch(
        &self,
        key: &str,
        lease_id: u64,
        manifest_path: &Path,
    ) -> Result<bool> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE cache_builds SET state='READY', owner_lease_id=NULL, manifest_path=?1,
             heartbeat_at=?2, expires_at=?2, error=NULL
             WHERE key=?3 AND state='REMOTE_FETCHING' AND owner_lease_id=?4",
            params![
                manifest_path.to_string_lossy(),
                unix_now(),
                key,
                lease_id as i64
            ],
        )?;
        transaction.execute(
            "DELETE FROM leases WHERE lease_id=?1 AND scope='cache_remote'",
            params![lease_id as i64],
        )?;
        transaction.commit()?;
        Ok(changed != 0)
    }

    /// Make a failed remote fetch immediately available to the normal local producer path.
    pub fn remote_fetch_fallback(&self, key: &str, lease_id: u64, reason: &str) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE cache_builds SET state='BUILDING', owner_lease_id=NULL, expires_at=?1,
             heartbeat_at=?1, error=?2 WHERE key=?3 AND state='REMOTE_FETCHING' AND owner_lease_id=?4",
            params![unix_now(), reason, key, lease_id as i64],
        )?;
        transaction.execute(
            "DELETE FROM leases WHERE lease_id=?1 AND scope='cache_remote'",
            params![lease_id as i64],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_single_flight_timeout(&self) -> Result<()> {
        self.connection.execute(
            "UPDATE single_flight_stats SET timeouts = timeouts + 1 WHERE id = 1",
            [],
        )?;
        Ok(())
    }

    pub fn remote_fetch_active(&self, key: &str) -> Result<bool> {
        let now = unix_now();
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM cache_builds b JOIN leases l ON l.lease_id = b.owner_lease_id
             WHERE b.key = ?1 AND b.state = 'REMOTE_FETCHING' AND l.scope = 'cache_remote'
               AND l.expires_at > ?2",
            params![key, now],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    pub fn prune_failed_cache_builds(&self, retention: Duration) -> Result<usize> {
        let cutoff = unix_now().saturating_sub(retention.as_secs());
        Ok(self.connection.execute(
            "DELETE FROM cache_builds
             WHERE state = 'FAILED' AND owner_lease_id IS NULL AND expires_at < ?1",
            params![cutoff],
        )?)
    }

    pub fn begin_cache_commit(&self, key: &str, lease_id: u64) -> Result<bool> {
        let changed = self.connection.execute(
            "UPDATE cache_builds SET state = 'COMMITTING', heartbeat_at = ?1
             WHERE key = ?2 AND state = 'BUILDING' AND owner_lease_id = ?3
               AND EXISTS(SELECT 1 FROM leases WHERE lease_id = ?3 AND expires_at > ?1)",
            params![unix_now(), key, lease_id as i64],
        )?;
        Ok(changed != 0)
    }

    pub fn finish_cache_commit(
        &self,
        key: &str,
        lease_id: u64,
        manifest_path: &Path,
    ) -> Result<bool> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE cache_builds SET state = 'READY', owner_lease_id = NULL,
                 manifest_path = ?1, expires_at = ?2, heartbeat_at = ?2, error = NULL
             WHERE key = ?3 AND state = 'COMMITTING' AND owner_lease_id = ?4",
            params![
                manifest_path.to_string_lossy(),
                unix_now(),
                key,
                lease_id as i64
            ],
        )?;
        transaction.execute(
            "DELETE FROM leases WHERE lease_id = ?1",
            params![lease_id as i64],
        )?;
        transaction.commit()?;
        Ok(changed != 0)
    }

    pub fn fail_cache_build(&self, key: &str, lease_id: u64, reason: &str) -> Result<bool> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE cache_builds SET state = 'FAILED', owner_lease_id = NULL,
                 expires_at = ?1, heartbeat_at = ?1, error = ?2
             WHERE key = ?3 AND owner_lease_id = ?4",
            params![unix_now(), reason, key, lease_id as i64],
        )?;
        transaction.execute(
            "DELETE FROM leases WHERE lease_id = ?1",
            params![lease_id as i64],
        )?;
        transaction.commit()?;
        Ok(changed != 0)
    }

    pub fn single_flight_stats(&self) -> Result<SingleFlightStats> {
        let (producers, waiters, timeouts, takeovers): (u64, u64, u64, u64) = self.connection.query_row(
            "SELECT producers, waiters, timeouts, takeovers FROM single_flight_stats WHERE id = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)? as u64, row.get::<_, i64>(3)? as u64)),
        )?;
        let active_builds = self.connection.query_row(
            "SELECT COUNT(*) FROM cache_builds WHERE state IN ('BUILDING', 'COMMITTING') AND expires_at > ?1",
            params![unix_now()],
            |row| row.get::<_, i64>(0),
        )? as u64;
        Ok(SingleFlightStats {
            producers,
            waiters,
            timeouts,
            takeovers,
            active_builds,
        })
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
            "SELECT workspace_root FROM leases WHERE scope IN ('cache', 'cache_build') AND expires_at > ?1 AND workspace_root IS NOT NULL",
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
        let flight = self.single_flight_stats()?;
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
            single_flight_producers: flight.producers,
            single_flight_waiters: flight.waiters,
            single_flight_timeouts: flight.timeouts,
            single_flight_takeovers: flight.takeovers,
            active_builds: flight.active_builds,
            remote: self.remote_status(false, None, None)?,
        })
    }

    pub fn remote_status(
        &self,
        enabled: bool,
        endpoint: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<RemoteStatusReport> {
        let meta = self.connection.query_row(
            "SELECT healthy, last_error FROM remote_meta WHERE id = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get(1)?)),
        )?;
        let values: (u64, u64, u64, u64, u64, u64, u64, u64, u64) = self.connection.query_row(
            "SELECT hits, misses, authentication_failures, corruptions, uploads, downloads,
                    upload_bytes, download_bytes, retries FROM remote_counters WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                    row.get::<_, i64>(4)? as u64,
                    row.get::<_, i64>(5)? as u64,
                    row.get::<_, i64>(6)? as u64,
                    row.get::<_, i64>(7)? as u64,
                    row.get::<_, i64>(8)? as u64,
                ))
            },
        )?;
        let queue_depth = self.connection.query_row(
            "SELECT COUNT(*) FROM remote_jobs WHERE status IN ('PENDING', 'RETRY')",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        Ok(RemoteStatusReport {
            enabled,
            configured: endpoint.is_some_and(|value| !value.is_empty())
                && namespace.is_some_and(|value| !value.is_empty()),
            healthy: enabled && meta.0 != 0,
            endpoint: endpoint.map(str::to_owned),
            namespace: namespace.map(str::to_owned),
            protocol_compatible: enabled && meta.0 != 0,
            queue_depth,
            hits: values.0,
            misses: values.1,
            authentication_failures: values.2,
            corruptions: values.3,
            uploads: values.4,
            downloads: values.5,
            upload_bytes: values.6,
            download_bytes: values.7,
            retries: values.8,
            last_error: meta.1,
        })
    }

    pub fn set_remote_config(
        &self,
        endpoint: &str,
        namespace: &str,
        fingerprint: &str,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE remote_meta SET endpoint = ?1, namespace = ?2, config_fingerprint = ?3 WHERE id = 1",
            params![endpoint, namespace, fingerprint],
        )?;
        Ok(())
    }

    pub fn set_remote_probe(&self, healthy: bool, error: Option<&str>) -> Result<()> {
        self.connection.execute(
            "UPDATE remote_meta SET last_probe = ?1, healthy = ?2, last_error = ?3 WHERE id = 1",
            params![unix_now(), i64::from(healthy), error],
        )?;
        Ok(())
    }

    pub fn record_remote_counter(&self, counter: &str, bytes: u64) -> Result<()> {
        let column = match counter {
            "hit" => "hits",
            "miss" => "misses",
            "authentication_failure" => "authentication_failures",
            "corruption" => "corruptions",
            "upload" => "uploads",
            "download" => "downloads",
            "retry" => "retries",
            _ => bail!("unknown remote counter {counter}"),
        };
        self.connection.execute(
            &format!("UPDATE remote_counters SET {column} = {column} + 1 WHERE id = 1"),
            [],
        )?;
        if counter == "upload" {
            self.connection.execute(
                "UPDATE remote_counters SET upload_bytes = upload_bytes + ?1 WHERE id = 1",
                params![bytes as i64],
            )?;
        } else if counter == "download" {
            self.connection.execute(
                "UPDATE remote_counters SET download_bytes = download_bytes + ?1 WHERE id = 1",
                params![bytes as i64],
            )?;
        }
        Ok(())
    }

    pub fn queue_remote_job(&self, key: &str, kind: &str, digest: Option<&str>) -> Result<()> {
        let now = unix_now();
        self.connection.execute(
            "INSERT INTO remote_jobs(key, kind, digest, status, created_at, updated_at)
             VALUES(?1, ?2, ?3, 'PENDING', ?4, ?4)
             ON CONFLICT(key, kind, digest) DO UPDATE SET status = 'PENDING', updated_at = ?4",
            params![key, kind, digest, now],
        )?;
        Ok(())
    }

    pub fn finish_remote_job(&self, key: &str, error: Option<&str>) -> Result<()> {
        self.connection.execute(
            "UPDATE remote_jobs SET status = ?1, attempts = attempts + 1, last_error = ?2,
             updated_at = ?3 WHERE key = ?4 AND status IN ('PENDING', 'RETRY', 'RUNNING')",
            params![
                if error.is_some() { "FAILED" } else { "DONE" },
                error,
                unix_now(),
                key
            ],
        )?;
        Ok(())
    }

    pub fn next_remote_job(&self) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT key FROM remote_jobs
                 WHERE status IN ('PENDING', 'RETRY') AND next_attempt_at <= ?1
                 ORDER BY job_id LIMIT 1",
                params![unix_now()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn claim_remote_job(&self, key: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE remote_jobs SET status='RUNNING', updated_at=?1
             WHERE key=?2 AND status IN ('PENDING', 'RETRY') AND next_attempt_at <= ?1",
            params![unix_now(), key],
        )? != 0)
    }

    pub fn retry_remote_job(&self, key: &str, error: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE remote_jobs SET status = CASE WHEN attempts >= 7 THEN 'FAILED' ELSE 'RETRY' END,
             attempts = attempts + 1,
             next_attempt_at = ?1 + (1 << MIN(attempts, 6)), last_error = ?2, updated_at = ?1
             WHERE key = ?3 AND status IN ('PENDING', 'RETRY', 'RUNNING')",
            params![unix_now(), error, key],
        )?;
        self.record_remote_counter("retry", 0)
    }

    pub fn recover_remote_jobs(&self) -> Result<()> {
        self.connection.execute(
            "UPDATE remote_jobs SET status='RETRY', next_attempt_at=?1 WHERE status='RUNNING'",
            params![unix_now()],
        )?;
        Ok(())
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
        if event.outcome == "timeout" {
            self.record_single_flight_timeout()?;
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
        if let Some((state, reason)) = self
            .connection
            .query_row(
                "SELECT state, error FROM cache_builds WHERE key = ?1 AND state IN ('BUILDING', 'COMMITTING', 'REMOTE_FETCHING', 'FAILED')",
                params![key],
                |row| Ok((row.get::<_, String>(0)?, row.get(1)?)),
            )
            .optional()?
        {
            return Ok(rgo_protocol::CacheExplanation {
                key: key.into(),
                state: state.to_lowercase(),
                reason,
                outputs: Vec::new(),
            });
        }
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
    fn reconcile_rebuilds_pins_from_markers() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let pinned_dir = paths.builds_dir().join("ab").join("cdef");
        let plain_dir = paths.builds_dir().join("ab").join("1234");
        std::fs::create_dir_all(&pinned_dir).unwrap();
        std::fs::create_dir_all(&plain_dir).unwrap();
        context::write_pin_marker(&pinned_dir).unwrap();
        let mut db = StateDb::open(&paths).unwrap();
        db.reconcile(&paths).unwrap();
        assert_eq!(
            db.pinned_paths().unwrap(),
            vec![std::fs::canonicalize(&pinned_dir).unwrap()],
            "pin marker must rebuild the pins table"
        );
        // A marker removed out-of-band drops the pin on the next reconcile.
        context::remove_pin_marker(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert!(db.pinned_paths().unwrap().is_empty());
        // And pins referencing deleted contexts are reaped.
        context::write_pin_marker(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert_eq!(db.pinned_paths().unwrap().len(), 1);
        std::fs::remove_dir_all(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert!(db.pinned_paths().unwrap().is_empty());
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

    #[test]
    fn single_flight_has_one_producer_and_reclaims_expired_owner() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let first = db.acquire_cache_build("key", 1, 30, true).unwrap();
        let lease = match first {
            CacheBuildDecision::Producer { lease_id, .. } => lease_id,
            other => panic!("expected producer, got {other:?}"),
        };
        assert!(matches!(
            db.acquire_cache_build("key", 2, 30, true).unwrap(),
            CacheBuildDecision::Wait { .. }
        ));
        db.connection
            .execute(
                "UPDATE leases SET expires_at = 0 WHERE lease_id = ?1",
                params![lease as i64],
            )
            .unwrap();
        assert!(matches!(
            db.acquire_cache_build("key", 3, 30, true).unwrap(),
            CacheBuildDecision::Producer { .. }
        ));
        let stats = db.single_flight_stats().unwrap();
        assert_eq!(stats.producers, 2);
        assert_eq!(stats.waiters, 1);
        assert_eq!(stats.takeovers, 1);
    }

    #[test]
    fn restart_recovery_does_not_leave_an_active_build() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        assert!(matches!(
            db.acquire_cache_build("key", 1, 30, true).unwrap(),
            CacheBuildDecision::Producer { .. }
        ));
        db.recover_cache_builds().unwrap();
        assert_eq!(db.single_flight_stats().unwrap().active_builds, 0);
        assert!(matches!(
            db.acquire_cache_build("key", 2, 30, true).unwrap(),
            CacheBuildDecision::Producer { .. }
        ));
    }

    #[test]
    fn remote_jobs_are_claimed_and_recovered() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        db.set_remote_config("https://cache.example", "stable", "fingerprint")
            .unwrap();
        db.queue_remote_job("key", "manifest", None).unwrap();
        assert_eq!(db.next_remote_job().unwrap().as_deref(), Some("key"));
        assert!(db.claim_remote_job("key").unwrap());
        db.recover_remote_jobs().unwrap();
        assert_eq!(db.next_remote_job().unwrap().as_deref(), Some("key"));
        let status = db.remote_status(true, Some("https://cache.example"), Some("stable"));
        assert_eq!(status.unwrap().queue_depth, 1);
    }

    #[test]
    fn concurrent_writers_survive_wal_busy_contention() {
        use std::sync::{Arc, Barrier};

        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let paths = Arc::new(paths);
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let paths = Arc::clone(&paths);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                let db = StateDb::open(&paths).unwrap();
                barrier.wait();
                for round in 0..20 {
                    db.touch(
                        &paths
                            .builds_dir()
                            .join(format!("aa/context-{index}-{round}")),
                        None,
                        Some((index + round + 1) as u64),
                        None,
                    )
                    .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let db = StateDb::open(&paths).unwrap();
        assert_eq!(db.stats().unwrap().active_leases, 0);
        assert!(
            db.connection
                .query_row::<i64, _, _>("SELECT COUNT(*) FROM access_summary", [], |row| row.get(0))
                .unwrap()
                >= 160
        );
    }

    #[test]
    fn damaged_wal_and_shm_are_recoverable_without_blocking_reopen() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        drop(StateDb::open(&paths).unwrap());

        let raw = Connection::open(paths.db_file()).unwrap();
        raw.pragma_update(None, "journal_mode", "WAL").unwrap();
        raw.pragma_update(None, "wal_autocheckpoint", 1_000_000_i64)
            .unwrap();
        let transaction = raw.unchecked_transaction().unwrap();
        for index in 0..500 {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO access_summary(build_dir, touch_count, last_touched)
                     VALUES(?1, ?2, ?2)",
                    params![format!("/tmp/recovery-{index}"), index as i64],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        let wal = paths.state_dir().join("meta.sqlite-wal");
        let shm = paths.state_dir().join("meta.sqlite-shm");
        assert!(wal.is_file());
        assert!(shm.is_file());
        std::fs::write(&wal, vec![0_u8; 32]).unwrap();
        std::fs::write(&shm, vec![0_u8; 32]).unwrap();
        drop(raw);

        let reopened = StateDb::open(&paths).unwrap();
        assert!(
            reopened
                .connection
                .query_row::<String, _, _>(
                    "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .is_ok()
        );
    }
}
