//! Rebuildable SQLite metadata for the local daemon.
//!
//! The filesystem remains authoritative for Cargo state. SQLite stores attribution, leases,
//! pins, and accounting so the daemon can coordinate concurrent clients without scanning the
//! entire managed tree for every heartbeat.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rgo_protocol::{
    CacheEvent, CacheManifest, CacheStatsReport, LeaseScope, PROTOCOL_VERSION, RemoteStatusReport,
};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};

use crate::context;
use crate::paths::RgoPaths;

const SCHEMA_VERSION: i64 = 6;
const CACHE_EVENT_HISTORY_LIMIT: i64 = 10_000;
const OPERATION_HISTORY_LIMIT: i64 = 100;
const REMOTE_TERMINAL_HISTORY_LIMIT: i64 = 1_000;
const MAX_INCREMENTAL_VACUUM_PAGES: i64 = 256;
const WAL_SIZE_LIMIT_BYTES: i64 = 8 * 1024 * 1024;
const LEGACY_VACUUM_FREE_MARGIN_BYTES: u64 = 16 * 1024 * 1024;

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
        let lock_path = paths.state_dir().join("meta-open.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock.lock_exclusive()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        match Self::open_initialized(&paths.db_file()) {
            Ok(db) => Ok(db),
            Err(first_error) => {
                // A concurrent opener, inaccessible file, or newer schema is not a corrupt
                // database. Moving a live WAL aside would lose work and strand its owner.
                if !is_sqlite_corruption(&first_error) {
                    return Err(first_error).context("opening rgo metadata database");
                }
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
        connection.busy_timeout(Duration::from_secs(5))?;
        // Pointer maps are only available when enabled before the first table.
        // Existing databases retain their layout; changing NONE requires a full
        // VACUUM, which can require twice the database size in free space.
        let table_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if table_count == 0 {
            connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT_BYTES)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
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
            CREATE TABLE IF NOT EXISTS cache_event_totals (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                pruned_bypasses INTEGER NOT NULL DEFAULT 0
            );
            INSERT OR IGNORE INTO cache_event_totals(id) VALUES (1);
            CREATE TABLE IF NOT EXISTS cache_event_batches (
                name TEXT PRIMARY KEY NOT NULL
            );
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
            // These tables are additive, so advancing the marker is a safe
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
        self.reconcile_contexts(paths, &contexts)
    }

    pub fn reconcile_contexts(
        &mut self,
        paths: &RgoPaths,
        contexts: &[context::BuildContext],
    ) -> Result<()> {
        // Migrate old in-context pins before Cargo can remove the build tree.
        // The stable record becomes the durable intent; the database is only
        // an index of pins whose contexts currently exist.
        for item in contexts {
            if context::is_pinned_dir(&item.dir) {
                context::migrate_legacy_pin(paths, &item.dir)?;
            }
        }
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
        // The table is a derived index of durable pin records and legacy
        // in-context markers. Cargo may delete the latter during `clean`.
        let marked: HashSet<String> = contexts
            .iter()
            .filter(|c| context::is_pinned(paths, &c.dir))
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
            "SELECT workspace_root FROM leases
             WHERE scope IN ('cache', 'cache_build', 'cache_remote')
               AND expires_at > ?1 AND workspace_root IS NOT NULL
             UNION SELECT key FROM remote_jobs
             WHERE status IN ('PENDING', 'RETRY', 'RUNNING')",
        )?;
        statement
            .query_map(params![unix_now()], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(Into::into)
    }

    /// Cache manifests in eviction order. The manifest remains authoritative
    /// for object references; this index only supplies last-use ordering.
    pub fn cache_lru(&self) -> Result<Vec<(String, u64)>> {
        let mut statement = self
            .connection
            .prepare("SELECT key, last_used FROM cache_entries ORDER BY last_used ASC, key ASC")?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn touch_cache_entry(&self, key: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE cache_entries SET last_used = ?1 WHERE key = ?2",
            params![unix_now(), key],
        )?;
        Ok(())
    }

    pub fn forget_cache_entry(&self, key: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM cache_entries WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn forget_unreferenced_cache_object(&self, digest: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM cache_objects WHERE digest = ?1
             AND NOT EXISTS (SELECT 1 FROM cache_references WHERE digest = ?1)",
            params![digest],
        )?;
        Ok(())
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
            "SELECT (SELECT pruned_bypasses FROM cache_event_totals WHERE id = 1)
                    + (SELECT COUNT(*) FROM cache_events WHERE outcome = 'bypass')",
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
            observations_incomplete: false,
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
        record_cache_event_on(&self.connection, event)
    }

    /// A drained file is applied atomically with a deduplication marker. If the
    /// daemon crashes before unlinking the file, retrying it cannot double the
    /// event counters.
    pub fn record_cache_event_batch(&self, name: &str, events: &[CacheEvent]) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO cache_event_batches(name) VALUES(?1)",
            params![name],
        )?;
        if inserted != 0 {
            for event in events {
                record_cache_event_on(&transaction, event)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn forget_cache_event_batch(&self, name: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM cache_event_batches WHERE name = ?1",
            params![name],
        )?;
        Ok(())
    }

    /// Recover the unlink/marker-delete gap after a crashed event drain. Walk
    /// rowids incrementally so a large old table cannot monopolize one daemon
    /// maintenance pass. A still-present drain keeps its deduplication marker.
    pub fn prune_missing_cache_event_batches(
        &self,
        state_dir: &Path,
        cursor: &mut i64,
        limit: usize,
    ) -> Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let mut query = self.connection.prepare(
            "SELECT rowid, name FROM cache_event_batches WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = query
            .query_map(params![*cursor, i64::try_from(limit)?], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        let mut pruned = 0;
        for (rowid, name) in &rows {
            // Old malformed rows cannot correspond to a filename selected by
            // the drain scanner. Never join an untrusted path into state/.
            let valid_name = name.starts_with("cache-events.")
                && name.ends_with(".drain")
                && !name.contains('/')
                && !name.contains('\\')
                && !name.contains(':')
                && !name.contains('\0');
            let missing = if valid_name {
                match std::fs::symlink_metadata(state_dir.join(name)) {
                    Ok(_) => false,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("checking cache event drain {name}"));
                    }
                }
            } else {
                true
            };
            if missing {
                self.connection.execute(
                    "DELETE FROM cache_event_batches WHERE rowid = ?1",
                    params![rowid],
                )?;
                pruned += 1;
            }
            *cursor = *rowid;
        }
        if rows.len() < limit {
            *cursor = 0;
        }
        Ok(pruned)
    }

    /// Keep diagnostic rows finite while retaining cumulative bypasses and the
    /// most recent real GC run used by the age-maintenance clock.
    pub fn prune_operational_history(&self) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let last_event: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(event_id), 0) FROM cache_events",
            [],
            |row| row.get(0),
        )?;
        let event_cutoff = last_event.saturating_sub(CACHE_EVENT_HISTORY_LIMIT);
        let bypasses: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM cache_events WHERE event_id <= ?1 AND outcome = 'bypass'",
            params![event_cutoff],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE cache_event_totals SET pruned_bypasses = pruned_bypasses + ?1 WHERE id = 1",
            params![bypasses],
        )?;
        transaction.execute(
            "DELETE FROM cache_events WHERE event_id <= ?1",
            params![event_cutoff],
        )?;
        transaction.execute(
            "DELETE FROM gc_runs WHERE run_id <=
                (SELECT COALESCE(MAX(run_id), 0) FROM gc_runs) - ?1
                AND run_id != (SELECT COALESCE(MAX(run_id), 0) FROM gc_runs WHERE dry_run = 0)",
            params![OPERATION_HISTORY_LIMIT],
        )?;
        transaction.execute(
            "DELETE FROM cache_verifications WHERE verify_id <=
                (SELECT COALESCE(MAX(verify_id), 0) FROM cache_verifications) - ?1",
            params![OPERATION_HISTORY_LIMIT],
        )?;
        transaction.execute(
            "DELETE FROM remote_jobs WHERE status IN ('DONE', 'FAILED')
                AND job_id <= (SELECT COALESCE(MAX(job_id), 0) FROM remote_jobs) - ?1",
            params![REMOTE_TERMINAL_HISTORY_LIMIT],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Return up to 256 empty pages to the volume per maintenance pass. Older
    /// databases without pointer maps still reuse their free pages internally.
    pub fn reclaim_unused_pages(&self) -> Result<()> {
        let mode: i64 = self
            .connection
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        if mode != 2 {
            return Ok(());
        }
        let free_pages: i64 = self
            .connection
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        if free_pages > 0 {
            self.connection.execute_batch(&format!(
                "PRAGMA incremental_vacuum({MAX_INCREMENTAL_VACUUM_PAGES})"
            ))?;
        }
        Ok(())
    }

    /// Convert an old, non-shrinking database once SQLite has room for its
    /// full rebuild. The caller must exclude other daemon writes (startup or
    /// the operation lock) and reset cursors over implicit rowids afterward.
    pub fn migrate_legacy_auto_vacuum(&self, path: &Path, free_bytes: u64) -> Result<bool> {
        let mode: i64 = self
            .connection
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        if mode != 0 {
            return Ok(false);
        }
        let file_bytes = std::fs::metadata(path)
            .with_context(|| format!("measuring {} before SQLite VACUUM", path.display()))?
            .len();
        let page_count: i64 = self
            .connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = self
            .connection
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let database_bytes =
            file_bytes.max(u64::try_from(page_count)?.saturating_mul(u64::try_from(page_size)?));
        let required_free = database_bytes
            .saturating_mul(2)
            .saturating_add(LEGACY_VACUUM_FREE_MARGIN_BYTES);
        if free_bytes < required_free {
            return Ok(false);
        }
        self.connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        self.connection.execute_batch("VACUUM")?;
        let converted: i64 = self
            .connection
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        anyhow::ensure!(
            converted == 2,
            "SQLite VACUUM did not enable incremental reclamation"
        );
        Ok(true)
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

    pub fn last_real_gc_at(&self) -> Result<u64> {
        Ok(self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(finished_at), 0) FROM gc_runs WHERE dry_run = 0",
                [],
                |row| row.get::<_, i64>(0),
            )?
            .max(0) as u64)
    }
}

fn record_cache_event_on(connection: &Connection, event: &CacheEvent) -> Result<()> {
    let now = unix_now();
    connection.execute(
        "INSERT INTO cache_events(key, outcome, bytes, reason, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![event.key, event.outcome, event.bytes as i64, event.reason, now],
    )?;
    if let Some(key) = &event.key {
        let column = match event.outcome.as_str() {
            "hit" => Some("hits"),
            "miss" => Some("misses"),
            _ => None,
        };
        if let Some(column) = column {
            connection.execute(
                &format!("UPDATE cache_entries SET {column} = {column} + 1, last_used = ?1 WHERE key = ?2"),
                params![now, key],
            )?;
        }
    }
    if event.outcome == "timeout" {
        connection.execute(
            "UPDATE single_flight_stats SET timeouts = timeouts + 1 WHERE id = 1",
            [],
        )?;
    }
    Ok(())
}

fn is_sqlite_corruption(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(
                info,
                _
            )) if matches!(info.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
        )
    })
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
    fn database_initializes_wal_and_releases_leases() {
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
                60,
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
        assert_eq!(
            db.connection
                .query_row::<i64, _, _>("PRAGMA auto_vacuum", [], |r| r.get(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.connection
                .query_row::<i64, _, _>("PRAGMA journal_size_limit", [], |r| r.get(0))
                .unwrap(),
            WAL_SIZE_LIMIT_BYTES
        );
    }

    #[test]
    fn incremental_reclamation_shrinks_new_databases_and_preserves_legacy_layout() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let transaction = db.connection.unchecked_transaction().unwrap();
        for index in 0..400 {
            transaction
                .execute(
                    "INSERT INTO cache_events(key, outcome, created_at) VALUES(?1, 'miss', 1)",
                    params![format!("{index}-{}", "x".repeat(4_096))],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        db.connection
            .execute("DELETE FROM cache_events", [])
            .unwrap();
        let free_before: i64 = db
            .connection
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert!(free_before > MAX_INCREMENTAL_VACUUM_PAGES);
        db.reclaim_unused_pages().unwrap();
        let free_after: i64 = db
            .connection
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert!(free_after < free_before);
        assert!(free_before - free_after <= MAX_INCREMENTAL_VACUUM_PAGES);
        drop(db);

        let legacy_root = tempdir().unwrap();
        let legacy_paths = RgoPaths {
            root: legacy_root.path().join("rgo"),
        };
        legacy_paths.ensure_layout().unwrap();
        let legacy = Connection::open(legacy_paths.db_file()).unwrap();
        legacy
            .execute_batch("CREATE TABLE old_metadata(value TEXT)")
            .unwrap();
        let transaction = legacy.unchecked_transaction().unwrap();
        transaction
            .execute("INSERT INTO old_metadata(value) VALUES('sentinel')", [])
            .unwrap();
        for _ in 0..400 {
            transaction
                .execute(
                    "INSERT INTO old_metadata(value) VALUES(?1)",
                    params!["x".repeat(4_096)],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(legacy);
        let legacy = StateDb::open(&legacy_paths).unwrap();
        assert_eq!(
            legacy
                .connection
                .query_row::<i64, _, _>("PRAGMA auto_vacuum", [], |row| row.get(0))
                .unwrap(),
            0
        );
        let pages_before: i64 = legacy
            .connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        legacy
            .connection
            .execute("DELETE FROM old_metadata WHERE value != 'sentinel'", [])
            .unwrap();
        assert!(
            !legacy
                .migrate_legacy_auto_vacuum(&legacy_paths.db_file(), 0)
                .unwrap()
        );
        let writer = Connection::open(legacy_paths.db_file()).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        legacy.connection.busy_timeout(Duration::ZERO).unwrap();
        assert!(
            legacy
                .migrate_legacy_auto_vacuum(&legacy_paths.db_file(), u64::MAX)
                .is_err()
        );
        assert_eq!(
            legacy
                .connection
                .query_row::<i64, _, _>("PRAGMA auto_vacuum", [], |row| row.get(0))
                .unwrap(),
            0
        );
        legacy.reclaim_unused_pages().unwrap();
        writer.execute_batch("ROLLBACK").unwrap();
        legacy
            .connection
            .busy_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(
            legacy
                .migrate_legacy_auto_vacuum(&legacy_paths.db_file(), u64::MAX)
                .unwrap()
        );
        assert_eq!(
            legacy
                .connection
                .query_row::<i64, _, _>("PRAGMA auto_vacuum", [], |row| row.get(0))
                .unwrap(),
            2
        );
        assert!(
            legacy
                .connection
                .query_row::<i64, _, _>("PRAGMA page_count", [], |row| row.get(0))
                .unwrap()
                < pages_before
        );
        assert_eq!(
            legacy
                .connection
                .query_row::<i64, _, _>("SELECT COUNT(*) FROM old_metadata", [], |row| row.get(0))
                .unwrap(),
            1
        );
        legacy.reclaim_unused_pages().unwrap();
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
    fn reconcile_migrates_legacy_pins_and_preserves_them_across_cargo_clean() {
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
        // Cargo can remove the whole build tree. Its in-context marker is no
        // longer authoritative once the stable record has been migrated.
        context::remove_pin_marker(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert_eq!(db.pinned_paths().unwrap().len(), 1);
        std::fs::remove_dir_all(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert!(db.pinned_paths().unwrap().is_empty());
        std::fs::create_dir(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert_eq!(db.pinned_paths().unwrap().len(), 1);
        assert!(context::is_pinned(&paths, &pinned_dir));
        context::remove_durable_pin(&paths, &pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert!(db.pinned_paths().unwrap().is_empty());
        // An already-started launcher may write its compatibility marker
        // after unpin. The newer unpin decision must stay authoritative.
        context::write_pin_marker(&pinned_dir).unwrap();
        db.reconcile(&paths).unwrap();
        assert!(db.pinned_paths().unwrap().is_empty());
        assert!(!context::is_pinned(&paths, &pinned_dir));
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
    fn unsupported_schema_is_reported_without_quarantining_live_metadata() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        drop(StateDb::open(&paths).unwrap());
        let raw = Connection::open(paths.db_file()).unwrap();
        raw.execute(
            "UPDATE schema_meta SET value = '999' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        drop(raw);

        let error = StateDb::open(&paths).err().unwrap();
        assert!(format!("{error:#}").contains("unsupported rgo database schema"));
        assert!(paths.db_file().exists());
        assert!(
            std::fs::read_dir(paths.state_dir())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".corrupt-"))
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
    fn operational_history_is_bounded_without_losing_bypasses_or_gc_clock() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let transaction = db.connection.unchecked_transaction().unwrap();
        for index in 1..=10_005i64 {
            transaction
                .execute(
                    "INSERT INTO cache_events(key, outcome, created_at) VALUES(?1, ?2, 1)",
                    params![
                        format!("key-{index}"),
                        if index % 2 == 0 { "bypass" } else { "miss" }
                    ],
                )
                .unwrap();
        }
        for index in 1..=105i64 {
            transaction
                .execute(
                    "INSERT INTO gc_runs(started_at, finished_at, dry_run, aggressive)
                     VALUES(77, 77, ?1, 0)",
                    params![if index == 1 { 0 } else { 1 }],
                )
                .unwrap();
        }
        for index in 1..=1_002i64 {
            transaction
                .execute(
                    "INSERT INTO remote_jobs(key, kind, status, created_at, updated_at)
                     VALUES(?1, 'manifest', 'DONE', 1, 1)",
                    params![format!("remote-{index}")],
                )
                .unwrap();
        }
        transaction
            .execute(
                "INSERT INTO remote_jobs(key, kind, status, created_at, updated_at)
                 VALUES('active', 'manifest', 'PENDING', 1, 1)",
                [],
            )
            .unwrap();
        transaction.commit().unwrap();

        db.prune_operational_history().unwrap();
        db.prune_operational_history().unwrap();
        assert_eq!(
            db.connection
                .query_row("SELECT COUNT(*) FROM cache_events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            CACHE_EVENT_HISTORY_LIMIT
        );
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 5_002);
        assert_eq!(db.last_real_gc_at().unwrap(), 77);
        assert_eq!(
            db.connection
                .query_row("SELECT COUNT(*) FROM gc_runs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            OPERATION_HISTORY_LIMIT + 1
        );
        assert_eq!(
            db.connection
                .query_row(
                    "SELECT COUNT(*) FROM remote_jobs WHERE status = 'PENDING'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn retried_event_batch_does_not_double_count() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let events = vec![
            CacheEvent {
                key: None,
                outcome: "bypass".into(),
                bytes: 0,
                reason: Some("unsafe invocation".into()),
            },
            CacheEvent {
                key: Some("key".into()),
                outcome: "timeout".into(),
                bytes: 0,
                reason: None,
            },
        ];
        db.record_cache_event_batch("drain-1", &events).unwrap();
        db.record_cache_event_batch("drain-1", &events).unwrap();
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 1);
        assert_eq!(db.single_flight_stats().unwrap().timeouts, 1);
        assert_eq!(
            db.connection
                .query_row("SELECT COUNT(*) FROM cache_events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn missing_event_batch_markers_are_pruned_incrementally_after_unlink() {
        let root = tempdir().unwrap();
        let paths = RgoPaths {
            root: root.path().join("rgo"),
        };
        paths.ensure_layout().unwrap();
        let db = StateDb::open(&paths).unwrap();
        let event = CacheEvent {
            key: None,
            outcome: "bypass".into(),
            bytes: 0,
            reason: None,
        };
        let live_name = "cache-events.0.drain";
        let live_path = paths.state_dir().join(live_name);
        std::fs::write(&live_path, b"pending").unwrap();
        db.record_cache_event_batch(live_name, std::slice::from_ref(&event))
            .unwrap();
        for index in 1..=70 {
            db.record_cache_event_batch(&format!("cache-events.{index}.drain"), &[])
                .unwrap();
        }

        let mut cursor = 0;
        let mut pruned = 0;
        for _ in 0..10 {
            pruned += db
                .prune_missing_cache_event_batches(&paths.state_dir(), &mut cursor, 10)
                .unwrap();
        }
        assert_eq!(pruned, 70);
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 1);
        db.record_cache_event_batch(live_name, std::slice::from_ref(&event))
            .unwrap();
        assert_eq!(db.cache_stats(false, 0).unwrap().bypasses, 1);
        std::fs::remove_file(live_path).unwrap();
        cursor = 0;
        assert_eq!(
            db.prune_missing_cache_event_batches(&paths.state_dir(), &mut cursor, 10)
                .unwrap(),
            1
        );
        let remaining: i64 = db
            .connection
            .query_row("SELECT COUNT(*) FROM cache_event_batches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
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
                // Race initialization too. A failed open must fail the test instead of
                // leaving every successful worker waiting at a barrier forever.
                barrier.wait();
                let db = StateDb::open(&paths).unwrap();
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
        assert!(
            std::fs::read_dir(paths.state_dir())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".corrupt-")),
            "ordinary concurrent opens must not quarantine metadata"
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
