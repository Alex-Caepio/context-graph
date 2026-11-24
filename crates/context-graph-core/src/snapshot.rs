//! Working-memory snapshot store and IR-epoch / citation invalidation.
//!
//! Ported from PR #112 `infra-pilots/src/run_working_memory.rs` so standalone
//! working snapshots and hook preflight caches automatically invalidate when
//! a workspace `ir_meta.write_epoch` advances or a cited source changes.

use crate::{
    Result, RuntimeError,
    bank::publication::PublicationFiles,
    fs,
};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const SNAPSHOT_BYTES: usize = 8192;
pub const MAX_TTL_MS: u64 = 86_400_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub service: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotValidation {
    pub citations: Vec<Citation>,
    #[serde(default)]
    pub graph_ir_revisions: BTreeMap<String, i64>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    crate::query::sha256(bytes)
}

/// Reads `ir_meta.write_epoch` from a Graph IR SQLite file without following symlinks.
pub fn read_graph_ir_epoch(db_path: &Path) -> Option<i64> {
    let meta = std::fs::symlink_metadata(db_path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let value: String = conn
        .query_row(
            "SELECT value FROM ir_meta WHERE key='write_epoch'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    value.parse::<i64>().ok()
}

/// Collects `canonical_db_path -> write_epoch` revisions across the given roots.
pub fn collect_graph_ir_revisions<'a>(
    roots: impl IntoIterator<Item = &'a Path>,
) -> BTreeMap<String, i64> {
    const CANDIDATES: [&str; 4] = [
        "graph_ir.sqlite3",
        "memory-bank/local/graph_ir.sqlite3",
        "ir.sqlite",
        "memory-bank/local/ir.sqlite",
    ];
    let mut revisions = BTreeMap::new();
    for root in roots {
        for rel in CANDIDATES {
            let db_path = root.join(rel);
            let Ok(canonical) = fs::canonicalize(&db_path) else {
                continue;
            };
            let key = canonical.to_string_lossy().replace('\\', "/");
            if revisions.contains_key(&key) {
                continue;
            }
            if let Some(epoch) = read_graph_ir_epoch(&canonical) {
                revisions.insert(key, epoch);
            }
        }
    }
    revisions
}

/// Disposable, versioned working-snapshot backend with IR-epoch & citation validation.
pub trait SnapshotStore: Send {
    fn load(&mut self, key: &str) -> Result<Option<Vec<u8>>>;
    fn publish(&mut self, key: &str, bytes: &[u8], ttl: Duration) -> Result<()>;

    fn load_validated(
        &mut self,
        key: &str,
        expected_validation: &SnapshotValidation,
    ) -> Result<Option<Vec<u8>>> {
        let _ = expected_validation;
        self.load(key)
    }

    fn publish_validated(
        &mut self,
        key: &str,
        bytes: &[u8],
        ttl: Duration,
        validation: &SnapshotValidation,
    ) -> Result<()> {
        let _ = validation;
        self.publish(key, bytes, ttl)
    }
}

pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub fn open(directory: &Path) -> Result<Self> {
        let _ = PublicationFiles::open_private_root(directory)?;
        let mut custody = PublicationFiles::new(directory)?;
        let name = "working-memory.sqlite3";
        #[cfg(unix)]
        let before = custody.metadata_image_identity(name)?;
        #[cfg(not(unix))]
        let before = custody.image_identity(name)?;
        if before.is_some_and(|image| image.3 != 1) {
            return Err(RuntimeError::policy("working_memory_cache_file"));
        }
        let connection = Connection::open_with_flags(
            directory.join(name),
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        #[cfg(unix)]
        let opened = custody
            .metadata_image_identity(name)?
            .ok_or_else(|| RuntimeError::policy("working_memory_cache_file"))?;
        #[cfg(not(unix))]
        let opened = custody
            .image_identity(name)?
            .ok_or_else(|| RuntimeError::policy("working_memory_cache_file"))?;
        if opened.3 != 1 || before.is_some_and(|before| before != opened) {
            return Err(RuntimeError::policy("working_memory_cache_file_changed"));
        }
        #[cfg(unix)]
        custody.secure_private_file_metadata(name, opened)?;
        #[cfg(not(unix))]
        custody.secure_private_file(name)?;
        custody.check()?;
        connection.busy_timeout(Duration::from_millis(100))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS snapshots(key TEXT PRIMARY KEY, value BLOB NOT NULL, expires_ms INTEGER NOT NULL);\
             CREATE TABLE IF NOT EXISTS snapshot_validations(key TEXT PRIMARY KEY, validation TEXT NOT NULL, expires_ms INTEGER NOT NULL);",
        )?;
        Ok(Self { connection })
    }
}

impl SnapshotStore for SqliteStore {
    fn load(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut statement = self.connection.prepare(
            "SELECT value FROM snapshots WHERE key=?1 AND expires_ms>?2 AND length(value)<=?3",
        )?;
        let mut rows = statement.query(params![key, now_ms(), SNAPSHOT_BYTES])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    fn publish(&mut self, key: &str, bytes: &[u8], ttl: Duration) -> Result<()> {
        if bytes.len() > SNAPSHOT_BYTES {
            return Err(RuntimeError::invalid("working_memory_snapshot_bound"));
        }
        let now = now_ms();
        let expires = now.saturating_add(ttl.as_millis().min(u128::from(MAX_TTL_MS)) as u64);
        self.connection
            .execute("DELETE FROM snapshots WHERE expires_ms<=?1", [now])?;
        self.connection.execute(
            "INSERT OR REPLACE INTO snapshots(key,value,expires_ms) VALUES(?1,?2,?3)",
            params![key, bytes, expires],
        )?;
        Ok(())
    }

    fn load_validated(
        &mut self,
        key: &str,
        expected_validation: &SnapshotValidation,
    ) -> Result<Option<Vec<u8>>> {
        let now = now_ms();
        let mut statement = self.connection.prepare(
            "SELECT s.value, v.validation FROM snapshots s \
             LEFT JOIN snapshot_validations v ON v.key = s.key AND v.expires_ms > ?2 \
             WHERE s.key=?1 AND s.expires_ms>?2 AND length(s.value)<=?3",
        )?;
        let mut rows = statement.query(params![key, now, SNAPSHOT_BYTES])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.get(0)?;
        let validation_raw: Option<String> = row.get(1)?;
        drop(rows);
        drop(statement);
        let valid = validation_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<SnapshotValidation>(raw).ok())
            .is_some_and(|stored| &stored == expected_validation);
        if !valid {
            let _ = self
                .connection
                .execute("DELETE FROM snapshots WHERE key=?1", [key]);
            let _ = self
                .connection
                .execute("DELETE FROM snapshot_validations WHERE key=?1", [key]);
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    fn publish_validated(
        &mut self,
        key: &str,
        bytes: &[u8],
        ttl: Duration,
        validation: &SnapshotValidation,
    ) -> Result<()> {
        if bytes.len() > SNAPSHOT_BYTES {
            return Err(RuntimeError::invalid("working_memory_snapshot_bound"));
        }
        let now = now_ms();
        let expires_ms = now.saturating_add(ttl.as_millis().min(u128::from(MAX_TTL_MS)) as u64);
        let validation_raw = serde_json::to_string(validation)?;
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM snapshots WHERE expires_ms<=?1", [now])?;
        tx.execute(
            "DELETE FROM snapshot_validations WHERE expires_ms<=?1",
            [now],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO snapshots(key,value,expires_ms) VALUES(?1,?2,?3)",
            params![key, bytes, expires_ms],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO snapshot_validations(key,validation,expires_ms) VALUES(?1,?2,?3)",
            params![key, validation_raw, expires_ms],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::uuid4;

    #[test]
    fn sqlite_snapshot_store_invalidates_on_graph_ir_write_epoch_change() {
        let root = std::env::temp_dir().join(format!("cg-snapshot-{}", uuid4()));
        let local = root.join("memory-bank/local");
        std::fs::create_dir_all(&local).unwrap();
        let ir_db = local.join("graph_ir.sqlite3");
        {
            let conn = Connection::open(&ir_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE ir_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO ir_meta(key, value) VALUES('write_epoch', '1');",
            )
            .unwrap();
        }
        let rev1 = collect_graph_ir_revisions([root.as_path()]);
        assert_eq!(rev1.len(), 1);
        assert_eq!(rev1.values().copied().next(), Some(1));

        let mut store = SqliteStore::open(&local).unwrap();
        let validation1 = SnapshotValidation {
            citations: vec![Citation {
                service: "core".into(),
                path: "src/lib.rs".into(),
                sha256: sha256_hex(b"hello"),
            }],
            graph_ir_revisions: rev1,
        };
        store
            .publish_validated("k1", b"cached-view", Duration::from_secs(60), &validation1)
            .unwrap();
        assert_eq!(
            store.load_validated("k1", &validation1).unwrap().as_deref(),
            Some(b"cached-view".as_slice())
        );

        // Bump IR epoch -> cached view is invalidated and evicted.
        {
            let conn = Connection::open(&ir_db).unwrap();
            conn.execute(
                "UPDATE ir_meta SET value='2' WHERE key='write_epoch'",
                [],
            )
            .unwrap();
        }
        let validation2 = SnapshotValidation {
            citations: validation1.citations.clone(),
            graph_ir_revisions: collect_graph_ir_revisions([root.as_path()]),
        };
        assert_ne!(validation1, validation2);
        assert!(store.load_validated("k1", &validation2).unwrap().is_none());
        // Row was deleted when invalidated.
        assert!(store.load("k1").unwrap().is_none());

        // Rejects oversized payloads.
        let oversized = vec![b'x'; SNAPSHOT_BYTES + 1];
        assert!(
            store
                .publish_validated("k2", &oversized, Duration::from_secs(60), &validation2)
                .is_err()
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }
}
