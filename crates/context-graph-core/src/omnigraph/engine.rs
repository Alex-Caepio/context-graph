//! Embedded SQLite Omnigraph Graph Engine with optional HTTP sidecar forwarding.
//!
//! Works 100% offline with zero external daemon dependencies, storing branches,
//! commit DAG snapshots, `ContextAtom`s, `FileRef`s, `SymbolRef`s (`MentionsSymbol`),
//! `TouchesFile`, `RelatesTo` (`contradicts`, `supersedes`, `supports`, ...),
//! 3-way `branch_diff` / `keep` / `merge` conflict resolution, NDJSON
//! export/import, and `canonical_sync_ledger` digest pruning.

use super::{
    CANONICAL_KNOWLEDGE_BRANCH, VALID_ATOM_KINDS, hexid, normalize_relation_name, now_utc_iso_ms,
    schema,
    symbols::{extract_symbol_mentions},
    transfer::{
        self, EdgeRecord, ParsedGraph, filter_graph_for_export, parse_ndjson_records,
        prepare_import_ndjson, serialize_graph_ndjson, validate_branch_token,
    },
};
use crate::{
    BankService, BrainRepository, Result, RuntimeConfig, RuntimeContext, RuntimeError,
    bank::{codec, publication::PublicationFiles},
    domain::json,
    index,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Map, Value, json as obj};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

const OMNIGRAPH_DB_REL: &str = "memory-bank/local/omnigraph.sqlite3";
const SYNC_LEDGER_REL: &str = "memory-bank/local/cache/omnigraph/sync_ledger.sqlite3";

#[derive(Clone, Debug)]
struct LedgerEntry {
    atom_sha256: String,
    files_sha256: String,
    supersedes_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey {
    edge_type: String,
    src: String,
    dst: String,
    relation: String,
}

impl EdgeKey {
    fn encode(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.edge_type, self.src, self.dst, self.relation
        )
    }

    fn decode(raw: &str) -> Option<Self> {
        let mut parts = raw.splitn(4, '\u{1f}');
        Some(Self {
            edge_type: parts.next()?.to_string(),
            src: parts.next()?.to_string(),
            dst: parts.next()?.to_string(),
            relation: parts.next()?.to_string(),
        })
    }
}

struct RelateSpec<'a> {
    src: &'a str,
    dst: &'a str,
    relation: &'a str,
    note: Option<&'a str>,
    confidence: f64,
}

struct DiffComputation {
    src_head: String,
    merged_atoms: BTreeMap<String, Value>,
    summary: Value,
}

#[derive(Clone, Debug)]
pub struct MergeKeepOptions<'a> {
    pub project_id: &'a str,
    pub source: &'a str,
    pub target: Option<&'a str>,
    pub strategy: &'a str,
    pub atom_ids: &'a [String],
    pub dry_run: bool,
    pub delete_branch: bool,
}

pub struct OmnigraphEngine {
    db_path: PathBuf,
    state_dir: PathBuf,
    project_root: PathBuf,
    conn: Mutex<Connection>,
}

impl OmnigraphEngine {
    /// Opens (or initializes) the project's local Omnigraph SQLite store under private custody.
    pub fn open(context: &RuntimeContext) -> Result<Self> {
        let db_path = context.roots.state.join(OMNIGRAPH_DB_REL);
        let dir = db_path
            .parent()
            .ok_or_else(|| RuntimeError::invalid("Invalid omnigraph database path"))?;
        let _ = PublicationFiles::open_private_root(dir)?;
        let mut custody = PublicationFiles::new(dir)?;
        let name = db_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RuntimeError::invalid("Invalid omnigraph file name"))?;

        #[cfg(unix)]
        let before = custody.metadata_image_identity(name)?;
        #[cfg(not(unix))]
        let before = custody.image_identity(name)?;
        if before.is_some_and(|image| image.3 != 1) {
            return Err(RuntimeError::policy("Omnigraph database must be a regular file"));
        }

        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        #[cfg(unix)]
        let opened = custody
            .metadata_image_identity(name)?
            .ok_or_else(|| RuntimeError::policy("Omnigraph database unavailable"))?;
        #[cfg(not(unix))]
        let opened = custody
            .image_identity(name)?
            .ok_or_else(|| RuntimeError::policy("Omnigraph database unavailable"))?;
        if opened.3 != 1 || before.is_some_and(|b| b != opened) {
            return Err(RuntimeError::policy("Omnigraph database custody check failed"));
        }
        #[cfg(unix)]
        custody.secure_private_file_metadata(name, opened)?;
        #[cfg(not(unix))]
        custody.secure_private_file(name)?;
        custody.check()?;
        conn.busy_timeout(Duration::from_millis(5000))?;

        Self::init_schema(&conn)?;
        Ok(Self {
            db_path,
            state_dir: context.roots.state.clone(),
            project_root: context.roots.project.clone(),
            conn: Mutex::new(conn),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS og_branches (
                name TEXT PRIMARY KEY,
                head_commit_id TEXT NOT NULL,
                base_branch TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS og_commits (
                commit_id TEXT PRIMARY KEY,
                branch TEXT NOT NULL,
                parent_commit_id TEXT,
                second_parent_commit_id TEXT,
                summary TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS og_atoms (
                branch TEXT NOT NULL,
                atom_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                summary TEXT NOT NULL,
                body TEXT NOT NULL,
                tags_json TEXT,
                source_branch TEXT NOT NULL,
                source_session_id TEXT,
                confidence REAL NOT NULL,
                pin_priority INTEGER NOT NULL,
                verified INTEGER NOT NULL,
                origin_layer TEXT,
                authority TEXT,
                privacy TEXT,
                brain_record_id TEXT,
                memory_id TEXT,
                record_revision INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(branch, atom_id)
            );
            CREATE TABLE IF NOT EXISTS og_files (
                branch TEXT NOT NULL,
                path TEXT NOT NULL,
                project_id TEXT NOT NULL,
                language TEXT,
                last_touched_at TEXT NOT NULL,
                PRIMARY KEY(branch, path)
            );
            CREATE TABLE IF NOT EXISTS og_symbols (
                branch TEXT NOT NULL,
                fqn TEXT NOT NULL,
                project_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                PRIMARY KEY(branch, fqn)
            );
            CREATE TABLE IF NOT EXISTS og_sessions (
                branch TEXT NOT NULL,
                session_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                title TEXT NOT NULL,
                provider TEXT NOT NULL,
                workflow TEXT NOT NULL,
                status TEXT NOT NULL,
                goal TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(branch, session_id)
            );
            CREATE TABLE IF NOT EXISTS og_edges (
                branch TEXT NOT NULL,
                edge_type TEXT NOT NULL,
                src TEXT NOT NULL,
                dst TEXT NOT NULL,
                relation TEXT NOT NULL DEFAULT '',
                data_json TEXT NOT NULL,
                PRIMARY KEY(branch, edge_type, src, dst, relation)
            );
            CREATE TABLE IF NOT EXISTS og_atom_versions (
                commit_id TEXT NOT NULL,
                atom_id TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY(commit_id, atom_id)
            );
            CREATE TABLE IF NOT EXISTS og_edge_versions (
                commit_id TEXT NOT NULL,
                edge_key TEXT NOT NULL,
                data_json TEXT NOT NULL,
                PRIMARY KEY(commit_id, edge_key)
            );",
        )?;
        let main_exists: Option<String> = conn
            .query_row("SELECT name FROM og_branches WHERE name='main'", [], |r| r.get(0))
            .optional()?;
        if main_exists.is_none() {
            let ts = now_utc_iso_ms();
            let init_commit = "commit-init".to_string();
            conn.execute(
                "INSERT INTO og_commits(commit_id,branch,parent_commit_id,second_parent_commit_id,summary,created_at) VALUES(?1,'main',NULL,NULL,'initialize main',?2)",
                params![init_commit, ts],
            )?;
            conn.execute(
                "INSERT INTO og_branches(name,head_commit_id,base_branch,created_at) VALUES('main',?1,NULL,?2)",
                params![init_commit, ts],
            )?;
        }
        Ok(())
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| RuntimeError::unavailable("Omnigraph lock poisoned"))
    }

    fn record_commit(
        conn: &Connection,
        branch: &str,
        second_parent: Option<&str>,
        summary: &str,
        ts: &str,
    ) -> Result<String> {
        let parent: Option<String> = conn
            .query_row(
                "SELECT head_commit_id FROM og_branches WHERE name=?1",
                [branch],
                |r| r.get(0),
            )
            .optional()?;
        let commit_id = format!("c-{}", &hexid()[..16]);
        conn.execute(
            "INSERT INTO og_commits(commit_id,branch,parent_commit_id,second_parent_commit_id,summary,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![commit_id, branch, parent, second_parent, summary, ts],
        )?;
        for atom in Self::read_branch_atoms_conn(conn, branch, None)? {
            let aid = atom["atom_id"].as_str().unwrap_or("");
            let encoded = serde_json::to_string(&atom)?;
            conn.execute(
                "INSERT INTO og_atom_versions(commit_id,atom_id,payload_json) VALUES(?1,?2,?3)",
                params![commit_id, aid, encoded],
            )?;
        }
        for (ekey, val) in Self::read_branch_edges_conn(conn, branch)? {
            let encoded = serde_json::to_string(&val)?;
            conn.execute(
                "INSERT INTO og_edge_versions(commit_id,edge_key,data_json) VALUES(?1,?2,?3)",
                params![commit_id, ekey.encode(), encoded],
            )?;
        }
        conn.execute(
            "UPDATE og_branches SET head_commit_id=?1 WHERE name=?2",
            params![commit_id, branch],
        )?;
        Ok(commit_id)
    }

    fn ensure_branch_conn(conn: &Connection, branch: &str, from: Option<&str>, ts: &str) -> Result<String> {
        let branch = validate_branch_token(branch)?;
        if let Some(head) = conn
            .query_row(
                "SELECT head_commit_id FROM og_branches WHERE name=?1",
                [&branch],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(head);
        }
        let base = validate_branch_token(from.unwrap_or("main"))?;
        let base_head: String = conn
            .query_row(
                "SELECT head_commit_id FROM og_branches WHERE name=?1",
                [&base],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| RuntimeError::not_found(format!("Base Omnigraph branch '{base}' not found")))?;

        conn.execute(
            "INSERT INTO og_atoms SELECT ?1,atom_id,project_id,kind,title,summary,body,tags_json,source_branch,source_session_id,confidence,pin_priority,verified,origin_layer,authority,privacy,brain_record_id,memory_id,record_revision,created_at,updated_at FROM og_atoms WHERE branch=?2",
            params![branch, base],
        )?;
        conn.execute(
            "INSERT INTO og_files SELECT ?1,path,project_id,language,last_touched_at FROM og_files WHERE branch=?2",
            params![branch, base],
        )?;
        conn.execute(
            "INSERT INTO og_symbols SELECT ?1,fqn,project_id,file_path,kind,name FROM og_symbols WHERE branch=?2",
            params![branch, base],
        )?;
        conn.execute(
            "INSERT INTO og_sessions SELECT ?1,session_id,project_id,title,provider,workflow,status,goal,created_at,updated_at FROM og_sessions WHERE branch=?2",
            params![branch, base],
        )?;
        conn.execute(
            "INSERT INTO og_edges SELECT ?1,edge_type,src,dst,relation,data_json FROM og_edges WHERE branch=?2",
            params![branch, base],
        )?;
        conn.execute(
            "INSERT INTO og_branches(name,head_commit_id,base_branch,created_at) VALUES(?1,?2,?3,?4)",
            params![branch, base_head, base, ts],
        )?;
        Ok(base_head)
    }

    fn read_branch_atoms_conn(
        conn: &Connection,
        branch: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<Value>> {
        let mut stmt = conn.prepare(
            "SELECT atom_id,project_id,kind,title,summary,body,tags_json,source_branch,source_session_id,\
             confidence,pin_priority,verified,origin_layer,authority,privacy,brain_record_id,memory_id,\
             record_revision,created_at,updated_at \
             FROM og_atoms WHERE branch=?1 ORDER BY pin_priority DESC, updated_at DESC, atom_id ASC",
        )?;
        let rows = stmt.query_map([branch], |row| {
            let tags_raw: Option<String> = row.get(6)?;
            let tags = tags_raw
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or(Value::Null);
            Ok(obj!({
                "atom_id": row.get::<_, String>(0)?,
                "project_id": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "title": row.get::<_, String>(3)?,
                "summary": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "tags": tags,
                "source_branch": row.get::<_, String>(7)?,
                "source_session_id": row.get::<_, Option<String>>(8)?,
                "confidence": row.get::<_, f64>(9)?,
                "pin_priority": row.get::<_, i32>(10)?,
                "verified": row.get::<_, i64>(11)? != 0,
                "origin_layer": row.get::<_, Option<String>>(12)?,
                "authority": row.get::<_, Option<String>>(13)?,
                "privacy": row.get::<_, Option<String>>(14)?,
                "brain_record_id": row.get::<_, Option<String>>(15)?,
                "memory_id": row.get::<_, Option<String>>(16)?,
                "record_revision": row.get::<_, Option<i64>>(17)?,
                "created_at": row.get::<_, String>(18)?,
                "updated_at": row.get::<_, String>(19)?,
            }))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let item = row?;
            if let Some(pid) = project_id.filter(|p| !p.is_empty())
                && item["project_id"].as_str() != Some(pid)
            {
                continue;
            }
            out.push(item);
        }
        Ok(out)
    }

    fn read_branch_edges_conn(conn: &Connection, branch: &str) -> Result<BTreeMap<EdgeKey, Value>> {
        let mut stmt = conn.prepare(
            "SELECT edge_type,src,dst,relation,data_json FROM og_edges WHERE branch=?1",
        )?;
        let rows = stmt.query_map([branch], |row| {
            let data_raw: String = row.get(4)?;
            let data = serde_json::from_str::<Value>(&data_raw).unwrap_or_else(|_| obj!({}));
            Ok((
                EdgeKey {
                    edge_type: row.get(0)?,
                    src: row.get(1)?,
                    dst: row.get(2)?,
                    relation: row.get(3)?,
                },
                data,
            ))
        })?;
        let mut map = BTreeMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    }

    fn atom_files_conn(conn: &Connection, branch: &str, atom_id: &str) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(
            "SELECT dst FROM og_edges WHERE branch=?1 AND edge_type='TouchesFile' AND src=?2 ORDER BY dst ASC",
        )?;
        let rows = stmt.query_map(params![branch, atom_id], |r| r.get::<_, String>(0))?;
        let mut files = Vec::new();
        for r in rows {
            files.push(r?);
        }
        Ok(files)
    }

    fn atom_symbols_conn(conn: &Connection, branch: &str, atom_id: &str) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(
            "SELECT dst FROM og_edges WHERE branch=?1 AND edge_type='MentionsSymbol' AND src=?2 ORDER BY dst ASC",
        )?;
        let rows = stmt.query_map(params![branch, atom_id], |r| r.get::<_, String>(0))?;
        let mut syms = Vec::new();
        for r in rows {
            syms.push(r?);
        }
        Ok(syms)
    }

    fn upsert_atom_conn(conn: &Connection, branch: &str, atom: &Value) -> Result<()> {
        let atom_id = atom["atom_id"]
            .as_str()
            .or_else(|| atom["id"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RuntimeError::invalid("Atom missing `atom_id`"))?;
        let project_id = atom["project_id"].as_str().unwrap_or("");
        let kind_raw = atom["kind"].as_str().unwrap_or("insight").to_ascii_lowercase();
        let kind = if VALID_ATOM_KINDS.contains(&kind_raw.as_str()) {
            kind_raw.as_str()
        } else {
            "insight"
        };
        let title = atom["title"].as_str().unwrap_or(atom_id);
        let summary = atom["summary"].as_str().unwrap_or(title);
        let body = atom["body"].as_str().unwrap_or(summary);
        let tags_json = atom
            .get("tags")
            .filter(|v| v.is_array())
            .map(serde_json::to_string)
            .transpose()?;
        let source_branch = atom["source_branch"].as_str().unwrap_or(branch);
        let source_session_id = atom["source_session_id"].as_str();
        let confidence = atom["confidence"].as_f64().unwrap_or(0.9).clamp(0.0, 1.0);
        let pin_priority = atom["pin_priority"].as_i64().unwrap_or(20);
        let verified = if atom["verified"].as_bool().unwrap_or(false) { 1i64 } else { 0i64 };
        let origin_layer = atom["origin_layer"].as_str();
        let authority = atom["authority"].as_str();
        let privacy = atom["privacy"].as_str();
        let brain_record_id = atom["brain_record_id"].as_str();
        let memory_id = atom["memory_id"].as_str();
        let record_revision = atom["record_revision"].as_i64();
        let now = now_utc_iso_ms();
        let created_at = atom["created_at"].as_str().filter(|s| !s.is_empty()).unwrap_or(&now);
        let updated_at = atom["updated_at"].as_str().filter(|s| !s.is_empty()).unwrap_or(&now);

        conn.execute(
            "INSERT OR REPLACE INTO og_atoms(\
                branch,atom_id,project_id,kind,title,summary,body,tags_json,source_branch,source_session_id,\
                confidence,pin_priority,verified,origin_layer,authority,privacy,brain_record_id,memory_id,\
                record_revision,created_at,updated_at\
            ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                branch, atom_id, project_id, kind, title, summary, body, tags_json, source_branch,
                source_session_id, confidence, pin_priority, verified, origin_layer, authority,
                privacy, brain_record_id, memory_id, record_revision, created_at, updated_at
            ],
        )?;
        Ok(())
    }

    fn set_atom_files_conn(
        conn: &Connection,
        branch: &str,
        project_id: &str,
        atom_id: &str,
        files: &[String],
        ts: &str,
    ) -> Result<()> {
        conn.execute(
            "DELETE FROM og_edges WHERE branch=?1 AND edge_type='TouchesFile' AND src=?2",
            params![branch, atom_id],
        )?;
        for file in files {
            let clean = file
                .split('#')
                .next()
                .unwrap_or(file)
                .trim()
                .trim_start_matches("./");
            if clean.is_empty() {
                continue;
            }
            let lang = super::detect_language(clean);
            conn.execute(
                "INSERT OR REPLACE INTO og_files(branch,path,project_id,language,last_touched_at) VALUES(?1,?2,?3,?4,?5)",
                params![branch, clean, project_id, lang, ts],
            )?;
            let data = obj!({ "access": "cited", "line_start": Value::Null, "line_end": Value::Null });
            conn.execute(
                "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,'TouchesFile',?2,?3,'touches_file',?4)",
                params![branch, atom_id, clean, serde_json::to_string(&data)?],
            )?;
        }
        Ok(())
    }

    fn set_atom_symbols_conn(
        &self,
        conn: &Connection,
        branch: &str,
        project_id: &str,
        atom_id: &str,
        files: &[String],
        text: &str,
    ) -> Result<usize> {
        conn.execute(
            "DELETE FROM og_edges WHERE branch=?1 AND edge_type='MentionsSymbol' AND src=?2",
            params![branch, atom_id],
        )?;
        let syms = extract_symbol_mentions(Some(&self.state_dir), Some(&self.project_root), files, text);
        for sym in &syms {
            let Some(fqn) = sym["fqn"].as_str() else {
                continue;
            };
            let file_path = sym["file_path"].as_str().unwrap_or("");
            let kind = sym["kind"].as_str().unwrap_or("symbol");
            let name = sym["name"].as_str().unwrap_or(fqn);
            conn.execute(
                "INSERT OR REPLACE INTO og_symbols(branch,fqn,project_id,file_path,kind,name) VALUES(?1,?2,?3,?4,?5,?6)",
                params![branch, fqn, project_id, file_path, kind, name],
            )?;
            let data = obj!({ "role": "named", "file_path": file_path, "name": name, "kind": kind });
            conn.execute(
                "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,'MentionsSymbol',?2,?3,'mentions_symbol',?4)",
                params![branch, atom_id, fqn, serde_json::to_string(&data)?],
            )?;
        }
        Ok(syms.len())
    }

    fn insert_relates_to_conn(
        conn: &Connection,
        branch: &str,
        spec: &RelateSpec<'_>,
        ts: &str,
    ) -> Result<()> {
        let rel = normalize_relation_name(spec.relation).ok_or_else(|| {
            RuntimeError::invalid(format!("Invalid RelatesTo relation '{}'", spec.relation))
        })?;
        let data = obj!({
            "relation": rel,
            "note": spec.note,
            "confidence": spec.confidence,
            "created_at": ts,
        });
        conn.execute(
            "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,'RelatesTo',?2,?3,?4,?5)",
            params![branch, spec.src, spec.dst, rel, serde_json::to_string(&data)?],
        )?;
        Ok(())
    }

    pub fn status(&self, project_id: &str) -> Result<Value> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare("SELECT name,head_commit_id,base_branch,created_at FROM og_branches ORDER BY name ASC")?;
        let branches: Vec<Value> = stmt
            .query_map([], |row| {
                Ok(obj!({
                    "name": row.get::<_, String>(0)?,
                    "head_commit_id": row.get::<_, String>(1)?,
                    "base_branch": row.get::<_, Option<String>>(2)?,
                    "created_at": row.get::<_, String>(3)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let atom_count: i64 = if project_id.is_empty() {
            conn.query_row("SELECT COUNT(*) FROM og_atoms", [], |r| r.get(0))?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM og_atoms WHERE project_id=?1",
                [project_id],
                |r| r.get(0),
            )?
        };
        let edge_count: i64 = conn.query_row("SELECT COUNT(*) FROM og_edges", [], |r| r.get(0))?;
        let symbol_count: i64 = conn.query_row("SELECT COUNT(*) FROM og_symbols", [], |r| r.get(0))?;
        Ok(obj!({
            "ok": true,
            "mode": "embedded-sqlite",
            "graph_id": schema::HARNESS_GRAPH_ID,
            "om_version": schema::OMNIGRAPH_VERSION,
            "db_path": self.db_path.to_string_lossy(),
            "branches": branches,
            "counts": {
                "atoms": atom_count,
                "edges": edge_count,
                "symbols": symbol_count,
            }
        }))
    }

    pub fn create_branch(&self, name: &str, from: Option<&str>) -> Result<Value> {
        let name = validate_branch_token(name)?;
        let from_branch = from.map(validate_branch_token).transpose()?;
        let conn = self.lock_conn()?;
        if conn
            .query_row("SELECT 1 FROM og_branches WHERE name=?1", [&name], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Err(RuntimeError::conflict(format!("Omnigraph branch '{name}' already exists")));
        }
        let ts = now_utc_iso_ms();
        let head = Self::ensure_branch_conn(&conn, &name, from_branch.as_deref(), &ts)?;
        Ok(obj!({
            "ok": true,
            "action": "create",
            "name": name,
            "from": from_branch.as_deref().unwrap_or("main"),
            "from_commit_id": head,
        }))
    }

    pub fn delete_branch(&self, name: &str) -> Result<Value> {
        let name = validate_branch_token(name)?;
        if name == "main" {
            return Err(RuntimeError::invalid("Cannot delete the `main` branch."));
        }
        let conn = self.lock_conn()?;
        let existed = conn.execute("DELETE FROM og_branches WHERE name=?1", [&name])?;
        if existed == 0 {
            return Err(RuntimeError::not_found(format!("Omnigraph branch '{name}' not found")));
        }
        for sql in [
            "DELETE FROM og_atoms WHERE branch=?1",
            "DELETE FROM og_files WHERE branch=?1",
            "DELETE FROM og_symbols WHERE branch=?1",
            "DELETE FROM og_sessions WHERE branch=?1",
            "DELETE FROM og_edges WHERE branch=?1",
        ] {
            conn.execute(sql, [&name])?;
        }
        Ok(obj!({ "ok": true, "action": "delete", "name": name }))
    }

    /// Publishes or updates a `ContextAtom` on `branch`, indexing its `TouchesFile`,
    /// `MentionsSymbol` (`SymbolRef`), and optional `RelatesTo` edges.
    pub fn publish_atom(&self, body: &Value) -> Result<Value> {
        let branch = body["branch"]
            .as_str()
            .or_else(|| body["source_branch"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("main");
        let branch = validate_branch_token(branch)?;
        let atom_id = body["atom_id"]
            .as_str()
            .or_else(|| body["id"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("atom-{}", &hexid()[..12]));
        let project_id = body["project_id"].as_str().unwrap_or("default");
        let kind_raw = body["kind"].as_str().unwrap_or("finding").to_ascii_lowercase();
        let kind = if VALID_ATOM_KINDS.contains(&kind_raw.as_str()) {
            kind_raw
        } else {
            "insight".to_string()
        };
        let title = body["title"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&atom_id)
            .to_string();
        let summary = body["summary"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&title)
            .to_string();
        let body_text = body["body"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&summary)
            .to_string();
        let ts = body["updated_at"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(now_utc_iso_ms);
        let created_at = body["created_at"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| ts.clone());
        let files: Vec<String> = body["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        let atom = obj!({
            "atom_id": atom_id,
            "project_id": project_id,
            "kind": kind,
            "title": title,
            "summary": summary,
            "body": body_text,
            "tags": body.get("tags").cloned().unwrap_or(Value::Null),
            "source_branch": branch,
            "source_session_id": body.get("session_id").or_else(|| body.get("source_session_id")).cloned().unwrap_or(Value::Null),
            "confidence": body["confidence"].as_f64().unwrap_or(0.9).clamp(0.0, 1.0),
            "pin_priority": body["pin_priority"].as_i64().unwrap_or(20),
            "verified": body["verified"].as_bool().unwrap_or(false),
            "origin_layer": body.get("origin_layer").cloned().unwrap_or(Value::Null),
            "authority": body.get("authority").cloned().unwrap_or(Value::Null),
            "privacy": body.get("privacy").cloned().unwrap_or(Value::Null),
            "brain_record_id": body.get("brain_record_id").cloned().unwrap_or(Value::Null),
            "memory_id": body.get("memory_id").cloned().unwrap_or(Value::Null),
            "record_revision": body.get("record_revision").cloned().unwrap_or(Value::Null),
            "created_at": created_at,
            "updated_at": ts,
        });

        let conn = self.lock_conn()?;
        let _ = Self::ensure_branch_conn(&conn, &branch, Some("main"), &ts)?;
        Self::upsert_atom_conn(&conn, &branch, &atom)?;
        Self::set_atom_files_conn(&conn, &branch, project_id, &atom_id, &files, &ts)?;
        let sym_count = self.set_atom_symbols_conn(
            &conn,
            &branch,
            project_id,
            &atom_id,
            &files,
            &format!("{title}\n{summary}\n{body_text}"),
        )?;

        if let Some(sid) = body["session_id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            let sess_data = obj!({ "role": "publisher", "published_at": ts });
            conn.execute(
                "INSERT OR REPLACE INTO og_sessions(branch,session_id,project_id,title,provider,workflow,status,goal,created_at,updated_at) VALUES(?1,?2,?3,?2,'context-graph','session','active',NULL,?4,?4)",
                params![branch, sid, project_id, ts],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,'PublishedBy',?2,?3,'published_by',?4)",
                params![branch, atom_id, sid, serde_json::to_string(&sess_data)?],
            )?;
        }

        if let Some(relations) = body["relations"].as_array() {
            for rel in relations {
                let Some(target) = rel["target_id"].as_str().or_else(|| rel["to"].as_str()) else {
                    continue;
                };
                let rname = rel["relation"].as_str().unwrap_or("supports");
                let note = rel["note"].as_str();
                let conf = rel["confidence"].as_f64().unwrap_or(1.0);
                Self::insert_relates_to_conn(
                    &conn,
                    &branch,
                    &RelateSpec {
                        src: &atom_id,
                        dst: target,
                        relation: rname,
                        note,
                        confidence: conf,
                    },
                    &ts,
                )?;
            }
        }
        if let Some(rel_obj) = body.get("relates_to").and_then(Value::as_object)
            && let Some(target) = rel_obj.get("target_id").and_then(Value::as_str)
        {
            let rname = rel_obj.get("relation").and_then(Value::as_str).unwrap_or("supports");
            let note = rel_obj.get("note").and_then(Value::as_str);
            let conf = rel_obj.get("confidence").and_then(Value::as_f64).unwrap_or(1.0);
            Self::insert_relates_to_conn(
                &conn,
                &branch,
                &RelateSpec {
                    src: &atom_id,
                    dst: target,
                    relation: rname,
                    note,
                    confidence: conf,
                },
                &ts,
            )?;
        }

        let commit_id = Self::record_commit(&conn, &branch, None, &format!("publish {atom_id}"), &ts)?;
        let syms = Self::atom_symbols_conn(&conn, &branch, &atom_id)?;
        let mut enriched = atom;
        enriched["files"] = obj!(files);
        enriched["symbols"] = obj!(syms);

        Ok(obj!({
            "ok": true,
            "branch": branch,
            "atom": enriched,
            "symbols_linked": sym_count,
            "commit_id": commit_id,
        }))
    }

    pub fn share_into_session(&self, body: &Value) -> Result<Value> {
        let target_session_id = body["target_session_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RuntimeError::invalid("`target_session_id` is required"))?;
        let branch = validate_branch_token(body["branch"].as_str().unwrap_or("main"))?;
        let atom_ids: Vec<String> = body["atom_ids"]
            .as_array()
            .ok_or_else(|| RuntimeError::invalid("`atom_ids` must be a non-empty array"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if atom_ids.is_empty() {
            return Err(RuntimeError::invalid("`atom_ids` must not be empty"));
        }
        let ts = now_utc_iso_ms();
        let project_id = body["project_id"].as_str().unwrap_or("default");
        let conn = self.lock_conn()?;
        let _ = Self::ensure_branch_conn(&conn, &branch, Some("main"), &ts)?;
        conn.execute(
            "INSERT OR REPLACE INTO og_sessions(branch,session_id,project_id,title,provider,workflow,status,goal,created_at,updated_at) VALUES(?1,?2,?3,?2,'context-graph','shared','active',NULL,?4,?4)",
            params![branch, target_session_id, project_id, ts],
        )?;
        let mut shared = 0usize;
        for aid in &atom_ids {
            let exists: Option<String> = conn
                .query_row(
                    "SELECT atom_id FROM og_atoms WHERE branch=?1 AND atom_id=?2",
                    params![branch, aid],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                continue;
            }
            let data = obj!({ "consumed_at": ts, "injection_mode": "explicit", "relevance": 1.0 });
            conn.execute(
                "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,'ConsumedBy',?2,?3,'consumed_by',?4)",
                params![branch, aid, target_session_id, serde_json::to_string(&data)?],
            )?;
            shared += 1;
        }
        let commit_id = Self::record_commit(
            &conn,
            &branch,
            None,
            &format!("share {} atoms to {target_session_id}", shared),
            &ts,
        )?;
        Ok(obj!({
            "ok": true,
            "branch": branch,
            "target_session_id": target_session_id,
            "shared": shared,
            "commit_id": commit_id,
        }))
    }

    fn open_ledger_conn(&self) -> Result<Connection> {
        let ledger_path = self.state_dir.join(SYNC_LEDGER_REL);
        if let Some(parent) = ledger_path.parent() {
            let _ = PublicationFiles::open_private_root(parent)?;
        }
        let conn = Connection::open_with_flags(
            &ledger_path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS canonical_sync_ledger(
                endpoint TEXT NOT NULL,
                graph_id TEXT NOT NULL,
                branch TEXT NOT NULL,
                project_id TEXT NOT NULL,
                atom_id TEXT NOT NULL,
                atom_sha256 TEXT NOT NULL,
                files_sha256 TEXT NOT NULL,
                supersedes_sha256 TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                PRIMARY KEY(endpoint, graph_id, branch, project_id, atom_id)
            );",
        )?;
        Ok(conn)
    }

    fn sorted_list_digest(values: &[String]) -> String {
        let sorted: BTreeSet<&str> = values.iter().map(String::as_str).collect();
        let encoded = serde_json::to_string(&sorted).unwrap_or_default();
        crate::query::sha256(encoded.as_bytes())
    }

    fn atom_digest(atom: &Value) -> String {
        let mut clone = atom.clone();
        if let Some(m) = clone.as_object_mut() {
            m.remove("created_at");
            m.remove("updated_at");
        }
        let canonical = json::canonical(&clone).unwrap_or_else(|_| clone.to_string());
        crate::query::sha256(canonical.as_bytes())
    }

    /// Collects Project Brain records and Memory Bank chunks from the current
    /// workspace and syncs them into `knowledge/canonical` using the ledger.
    pub fn sync_canonical_workspace(
        &self,
        context: &RuntimeContext,
        project_id: &str,
        prune: bool,
    ) -> Result<Value> {
        let config = RuntimeConfig::load(&context.roots)?;
        let allowed_privacy = config.strings("allowed_privacy")?;
        let mut items = Vec::new();
        let mut prune_layers = Vec::new();

        if context.mode == "governed"
            && let Ok(brain) = BrainRepository::new(context.clone())
            && let Ok(records) = brain.records(false)
        {
            prune_layers.push("project-brain".to_string());
            for image in records {
                let r = &image.record;
                let privacy = r["privacy"].as_str().unwrap_or("");
                if !allowed_privacy.iter().any(|p| p == privacy) {
                    continue;
                }
                let status = r["status"].as_str().unwrap_or("");
                if matches!(status, "archived" | "superseded" | "rejected") {
                    continue;
                }
                let Some(id) = r["id"].as_str() else {
                    continue;
                };
                let kind = match r["type"].as_str().unwrap_or("finding") {
                    "decision" => "decision",
                    "incident" | "bug" => "warning",
                    "task" => "contract",
                    _ => "finding",
                };
                let mut files: Vec<String> = r["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                for s in r["sources"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    let clean = s.split('#').next().unwrap_or(s).to_string();
                    if !files.contains(&clean) {
                        files.push(clean);
                    }
                }
                let title = r["title"].as_str().unwrap_or(id);
                let body = r["goal"]
                    .as_str()
                    .or_else(|| r["summary"].as_str())
                    .unwrap_or(title);
                let authority = r["authority"].as_str().unwrap_or("observed");
                items.push(obj!({
                    "atom_id": format!("brain-{id}"),
                    "project_id": project_id,
                    "kind": kind,
                    "title": title,
                    "summary": title,
                    "body": body,
                    "files": files,
                    "origin_layer": "project-brain",
                    "authority": authority,
                    "verified": authority == "verified",
                    "privacy": privacy,
                    "brain_record_id": id,
                    "record_revision": r["revision"].as_i64().unwrap_or(1),
                }));
            }
        }

        if crate::fs::admit_directory(&context.roots.state.join("memory-bank/chunks")).unwrap_or(false)
        {
            prune_layers.push("memory-bank".to_string());
            let bank = BankService::new(context.clone());
            if let Ok(paths) = codec::paths(&bank)
                && let Ok(mut files_custody) = PublicationFiles::new(&context.roots.state)
            {
                for chunk_path in paths {
                    let Ok(chunk) = codec::read(&mut files_custody, &chunk_path) else {
                        continue;
                    };
                    let m = &chunk.metadata;
                    if m["status"] != "active" || index::memory_temporal_reason(m).is_some() {
                        continue;
                    }
                    let Some(mid) = m["id"].as_str() else {
                        continue;
                    };
                    let auto_promoted = m["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.iter().any(|t| t == "auto-promoted"));
                    let sources: Vec<String> = m["sources"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|s| s.split('#').next().unwrap_or(s).to_string())
                        .collect();
                    let supersedes: Vec<String> = m["supersedes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|id| format!("mem-{id}"))
                        .collect();
                    let kind = match m["type"].as_str().unwrap_or("convention") {
                        "decision" => "decision",
                        "anti-pattern" | "gotcha" => "warning",
                        "convention" | "contract" => "contract",
                        _ => "insight",
                    };
                    let title = m["title"].as_str().unwrap_or(mid);
                    let snippet = chunk.body.lines().take(12).collect::<Vec<_>>().join("\n");
                    items.push(obj!({
                        "atom_id": format!("mem-{mid}"),
                        "project_id": project_id,
                        "kind": kind,
                        "title": title,
                        "summary": title,
                        "body": if snippet.trim().is_empty() { title } else { snippet.trim() },
                        "files": sources,
                        "tags": m["tags"],
                        "supersedes_atom_ids": supersedes,
                        "origin_layer": "memory-bank",
                        "authority": if auto_promoted { "observed" } else { "verified" },
                        "verified": !auto_promoted,
                        "memory_id": mid,
                    }));
                }
            }
        }

        let layers = if prune { prune_layers } else { Vec::new() };
        self.sync_canonical_items(project_id, &items, &layers)
    }

    pub fn sync_canonical_items(
        &self,
        project_id: &str,
        items: &[Value],
        prune_layers: &[String],
    ) -> Result<Value> {
        let branch = CANONICAL_KNOWLEDGE_BRANCH;
        let ts = now_utc_iso_ms();
        let conn = self.lock_conn()?;
        let _ = Self::ensure_branch_conn(&conn, branch, Some("main"), &ts)?;
        let ledger = self.open_ledger_conn()?;

        let endpoint_key = "";
        let graph_key = schema::HARNESS_GRAPH_ID;
        let mut ledger_rows = BTreeMap::new();
        {
            let mut stmt = ledger.prepare(
                "SELECT atom_id, atom_sha256, files_sha256, supersedes_sha256 FROM canonical_sync_ledger WHERE endpoint=?1 AND graph_id=?2 AND branch=?3 AND project_id=?4",
            )?;
            let rows = stmt.query_map(
                params![endpoint_key, graph_key, branch, project_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        LedgerEntry {
                            atom_sha256: r.get(1)?,
                            files_sha256: r.get(2)?,
                            supersedes_sha256: r.get(3)?,
                        },
                    ))
                },
            )?;
            for row in rows {
                let (aid, entry) = row?;
                ledger_rows.insert(aid, entry);
            }
        }

        let existing_atoms = Self::read_branch_atoms_conn(&conn, branch, Some(project_id))?;
        let mut existing_ids: BTreeSet<String> = existing_atoms
            .iter()
            .filter_map(|a| a["atom_id"].as_str().map(str::to_string))
            .collect();

        let mut atom_ids = Vec::new();
        let mut mutated_atoms = 0usize;
        let mut skipped_atoms = 0usize;
        let mut skipped_files = 0usize;
        let mut symbols_linked = 0usize;
        let mut pending_supersedes: Vec<(String, Vec<String>)> = Vec::new();

        for item in items {
            let Some(atom_id) = item["atom_id"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let title: String = item["title"]
                .as_str()
                .or_else(|| item["summary"].as_str())
                .unwrap_or(&atom_id)
                .chars()
                .take(240)
                .collect();
            let raw_kind = item["kind"]
                .as_str()
                .unwrap_or("finding")
                .to_ascii_lowercase();
            let kind = if VALID_ATOM_KINDS.contains(&raw_kind.as_str()) {
                raw_kind
            } else {
                "insight".to_string()
            };
            let summary = item["summary"].as_str().unwrap_or(&title).to_string();
            let body = item["body"].as_str().unwrap_or(&summary).to_string();
            let files: Vec<String> = item["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let supersedes: Vec<String> = item["supersedes_atom_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();

            let normalized_atom = obj!({
                "atom_id": atom_id,
                "project_id": project_id,
                "kind": kind,
                "title": title,
                "summary": summary,
                "body": body,
                "tags": item.get("tags").cloned().unwrap_or(Value::Null),
                "source_branch": branch,
                "source_session_id": item.get("source_session_id").or_else(|| item.get("session_id")).cloned().unwrap_or(Value::Null),
                "confidence": item["confidence"].as_f64().unwrap_or(0.9).clamp(0.0, 1.0),
                "pin_priority": item["pin_priority"].as_i64().unwrap_or(20),
                "verified": item["verified"].as_bool().unwrap_or_else(|| item["authority"].as_str() == Some("verified")),
                "origin_layer": item.get("origin_layer").cloned().unwrap_or(Value::Null),
                "authority": item.get("authority").cloned().unwrap_or(Value::Null),
                "privacy": item.get("privacy").cloned().unwrap_or(Value::Null),
                "brain_record_id": item.get("brain_record_id").cloned().unwrap_or(Value::Null),
                "memory_id": item.get("memory_id").cloned().unwrap_or(Value::Null),
                "record_revision": item.get("record_revision").cloned().unwrap_or(Value::Null),
                "created_at": ts,
                "updated_at": ts,
            });
            let atom_sha = Self::atom_digest(&normalized_atom);
            let files_sha = Self::sorted_list_digest(&files);
            let sup_sha = Self::sorted_list_digest(&supersedes);

            let prev = ledger_rows.get(&atom_id);
            let on_branch = existing_ids.contains(&atom_id);
            let atom_unchanged = on_branch && prev.is_some_and(|e| e.atom_sha256 == atom_sha);
            if atom_unchanged {
                skipped_atoms += 1;
            } else {
                Self::upsert_atom_conn(&conn, branch, &normalized_atom)?;
                mutated_atoms += 1;
            }

            let prev_files: BTreeSet<String> = Self::atom_files_conn(&conn, branch, &atom_id)?
                .into_iter()
                .collect();
            let wanted_files: BTreeSet<String> = files.iter().cloned().collect();
            if on_branch
                && prev_files == wanted_files
                && prev.is_some_and(|e| e.files_sha256 == files_sha)
            {
                skipped_files += 1;
            } else {
                Self::set_atom_files_conn(&conn, branch, project_id, &atom_id, &files, &ts)?;
            }

            symbols_linked += self.set_atom_symbols_conn(
                &conn,
                branch,
                project_id,
                &atom_id,
                &files,
                &format!("{title}\n{summary}\n{body}"),
            )?;
            existing_ids.insert(atom_id.clone());

            if !supersedes.is_empty()
                && (!atom_unchanged || prev.is_none_or(|e| e.supersedes_sha256 != sup_sha))
            {
                pending_supersedes.push((atom_id.clone(), supersedes));
            }
            ledger.execute(
                "INSERT OR REPLACE INTO canonical_sync_ledger(endpoint,graph_id,branch,project_id,atom_id,atom_sha256,files_sha256,supersedes_sha256,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![endpoint_key, graph_key, branch, project_id, atom_id, atom_sha, files_sha, sup_sha, ts],
            )?;
            atom_ids.push(atom_id);
        }

        for (src, targets) in pending_supersedes {
            for dst in targets {
                if src != dst && existing_ids.contains(&dst) {
                    Self::insert_relates_to_conn(
                        &conn,
                        branch,
                        &RelateSpec {
                            src: &src,
                            dst: &dst,
                            relation: "supersedes",
                            note: Some("Superseded in canonical knowledge sync"),
                            confidence: 1.0,
                        },
                        &ts,
                    )?;
                }
            }
        }

        let mut deleted_atom_ids = Vec::new();
        if !prune_layers.is_empty() {
            let current: BTreeSet<String> = atom_ids.iter().cloned().collect();
            for row in &existing_atoms {
                let Some(aid) = row["atom_id"].as_str() else {
                    continue;
                };
                if current.contains(aid) {
                    continue;
                }
                let origin = row["origin_layer"].as_str().unwrap_or("");
                let in_pruned = (origin == "project-brain" && prune_layers.iter().any(|l| l == "project-brain"))
                    || (origin == "memory-bank" && prune_layers.iter().any(|l| l == "memory-bank"))
                    || (aid.starts_with("brain-") && prune_layers.iter().any(|l| l == "project-brain"))
                    || (aid.starts_with("mem-") && prune_layers.iter().any(|l| l == "memory-bank"));
                if !in_pruned {
                    continue;
                }
                conn.execute(
                    "DELETE FROM og_atoms WHERE branch=?1 AND atom_id=?2",
                    params![branch, aid],
                )?;
                conn.execute(
                    "DELETE FROM og_edges WHERE branch=?1 AND (src=?2 OR (edge_type='RelatesTo' AND dst=?2))",
                    params![branch, aid],
                )?;
                ledger.execute(
                    "DELETE FROM canonical_sync_ledger WHERE endpoint=?1 AND graph_id=?2 AND branch=?3 AND project_id=?4 AND atom_id=?5",
                    params![endpoint_key, graph_key, branch, project_id, aid],
                )?;
                deleted_atom_ids.push(aid.to_string());
            }
        }

        let commit_id = if mutated_atoms > 0 || !deleted_atom_ids.is_empty() {
            Some(Self::record_commit(
                &conn,
                branch,
                None,
                &format!("sync {} items (+{}, -{})", atom_ids.len(), mutated_atoms, deleted_atom_ids.len()),
                &ts,
            )?)
        } else {
            conn.query_row(
                "SELECT head_commit_id FROM og_branches WHERE name=?1",
                [branch],
                |r| r.get(0),
            )
            .optional()?
        };

        Ok(obj!({
            "ok": true,
            "branch": branch,
            "synced": atom_ids.len(),
            "mutated_atoms": mutated_atoms,
            "skipped_atoms": skipped_atoms,
            "skipped_files": skipped_files,
            "symbols": symbols_linked,
            "atom_ids": atom_ids,
            "deleted": deleted_atom_ids.len(),
            "deleted_atom_ids": deleted_atom_ids,
            "commit_id": commit_id,
        }))
    }

    /// Queries atoms touching `path` on `main` and `knowledge/canonical` (plus optional `branch`),
    /// attaching their `MentionsSymbol` labels and `contradicts` relations.
    pub fn file_context(&self, project_id: &str, path: &str, branch: Option<&str>) -> Result<Value> {
        let path = path.trim().trim_start_matches("./");
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path.split('/').any(|p| p == "..")
        {
            return Err(RuntimeError::invalid("Choose a relative project file path"));
        }
        let conn = self.lock_conn()?;
        let mut branches = Vec::new();
        if let Some(b) = branch.map(str::trim).filter(|s| !s.is_empty() && *s != "auto") {
            branches.push(validate_branch_token(b)?);
        }
        for b in ["main", CANONICAL_KNOWLEDGE_BRANCH] {
            if !branches.iter().any(|existing| existing == b) {
                branches.push(b.to_string());
            }
        }

        let prefix = format!("{path}::");
        let mut atoms = Vec::new();
        let mut seen = BTreeSet::new();
        let mut contradictions = Vec::new();

        for b in &branches {
            let mut stmt = conn.prepare(
                "SELECT a.atom_id,a.project_id,a.kind,a.title,a.summary,a.body,a.tags_json,a.source_branch,\
                 a.source_session_id,a.confidence,a.pin_priority,a.verified,a.origin_layer,a.authority,\
                 a.privacy,a.brain_record_id,a.memory_id,a.record_revision,a.created_at,a.updated_at \
                 FROM og_atoms a \
                 JOIN og_edges e ON e.branch=a.branch AND e.src=a.atom_id \
                 WHERE a.branch=?1 AND e.edge_type='TouchesFile' AND e.dst=?2 \
                 ORDER BY a.pin_priority DESC, a.updated_at DESC",
            )?;
            let rows = stmt.query_map(params![b, path], |row| {
                let tags = row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or(Value::Null);
                Ok(obj!({
                    "atom_id": row.get::<_, String>(0)?,
                    "project_id": row.get::<_, String>(1)?,
                    "kind": row.get::<_, String>(2)?,
                    "title": row.get::<_, String>(3)?,
                    "summary": row.get::<_, String>(4)?,
                    "body": row.get::<_, String>(5)?,
                    "tags": tags,
                    "source_branch": row.get::<_, String>(7)?,
                    "source_session_id": row.get::<_, Option<String>>(8)?,
                    "confidence": row.get::<_, f64>(9)?,
                    "pin_priority": row.get::<_, i32>(10)?,
                    "verified": row.get::<_, i64>(11)? != 0,
                    "origin_layer": row.get::<_, Option<String>>(12)?,
                    "authority": row.get::<_, Option<String>>(13)?,
                    "privacy": row.get::<_, Option<String>>(14)?,
                    "brain_record_id": row.get::<_, Option<String>>(15)?,
                    "memory_id": row.get::<_, Option<String>>(16)?,
                    "record_revision": row.get::<_, Option<i64>>(17)?,
                    "created_at": row.get::<_, String>(18)?,
                    "updated_at": row.get::<_, String>(19)?,
                }))
            })?;
            for row in rows {
                let mut atom = row?;
                if !project_id.is_empty() && atom["project_id"].as_str() != Some(project_id) {
                    continue;
                }
                let Some(aid) = atom["atom_id"].as_str().map(str::to_string) else {
                    continue;
                };
                if !seen.insert(aid.clone()) {
                    continue;
                }
                let syms: Vec<String> = Self::atom_symbols_conn(&conn, b, &aid)?
                    .into_iter()
                    .filter(|fqn| fqn.starts_with(&prefix))
                    .map(|fqn| fqn.strip_prefix(&prefix).unwrap_or(&fqn).to_string())
                    .collect();
                let line = format_atom_line_with_authority(&atom);
                atom["capsule_line"] = obj!(line.trim_end());
                atom["symbols"] = obj!(syms);
                atoms.push(atom);
            }
        }

        if !seen.is_empty() {
            let mut seen_contra = BTreeSet::new();
            for b in &branches {
                let mut cstmt = conn.prepare(
                    "SELECT e.src, COALESCE(s.title, e.src), e.dst, COALESCE(t.title, e.dst), e.data_json \
                     FROM og_edges e \
                     LEFT JOIN og_atoms s ON s.branch=e.branch AND s.atom_id=e.src \
                     LEFT JOIN og_atoms t ON t.branch=e.branch AND t.atom_id=e.dst \
                     WHERE e.branch=?1 AND e.edge_type='RelatesTo' AND e.relation='contradicts'",
                )?;
                let crows = cstmt.query_map([b], |r| {
                    let data_raw: String = r.get(4)?;
                    let data = serde_json::from_str::<Value>(&data_raw).unwrap_or_else(|_| obj!({}));
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        data,
                    ))
                })?;
                for crow in crows {
                    let (src, stitle, dst, ttitle, data) = crow?;
                    if !seen.contains(&src) && !seen.contains(&dst) {
                        continue;
                    }
                    let pair_key = format!("{src}->{dst}");
                    if !seen_contra.insert(pair_key) {
                        continue;
                    }
                    contradictions.push(obj!({
                        "source_id": src,
                        "source_title": stitle,
                        "target_id": dst,
                        "target_title": ttitle,
                        "note": data.get("note").cloned().unwrap_or(Value::Null),
                        "branch": b,
                    }));
                }
            }
        }

        Ok(obj!({
            "ok": true,
            "path": path,
            "atoms": atoms,
            "contradictions": contradictions,
        }))
    }

    pub fn atoms_query(
        &self,
        project_id: &str,
        branch: Option<&str>,
        kind: Option<&str>,
        q: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        let branch = validate_branch_token(branch.unwrap_or("main"))?;
        let conn = self.lock_conn()?;
        let all = Self::read_branch_atoms_conn(&conn, &branch, (!project_id.is_empty()).then_some(project_id))?;
        let q_lower = q.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        let kind_filter = kind.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());

        let mut filtered = Vec::new();
        for mut atom in all {
            if let Some(ref k) = kind_filter
                && atom["kind"].as_str() != Some(k.as_str())
            {
                continue;
            }
            if let Some(ref ql) = q_lower {
                let hay = format!(
                    "{} {} {}",
                    atom["title"].as_str().unwrap_or(""),
                    atom["summary"].as_str().unwrap_or(""),
                    atom["body"].as_str().unwrap_or("")
                )
                .to_ascii_lowercase();
                if !hay.contains(ql) {
                    continue;
                }
            }
            let aid = atom["atom_id"].as_str().unwrap_or("").to_string();
            atom["files"] = obj!(Self::atom_files_conn(&conn, &branch, &aid)?);
            atom["symbols"] = obj!(Self::atom_symbols_conn(&conn, &branch, &aid)?);
            filtered.push(atom);
        }
        let total = filtered.len();
        let bounded_limit = limit.clamp(1, 500);
        let page: Vec<Value> = filtered.into_iter().skip(offset).take(bounded_limit).collect();

        Ok(obj!({
            "ok": true,
            "branch": branch,
            "total": total,
            "offset": offset,
            "limit": bounded_limit,
            "atoms": page,
        }))
    }

    fn commit_ancestors(conn: &Connection, start: &str) -> Result<BTreeMap<String, usize>> {
        let mut dists = BTreeMap::new();
        let mut queue = VecDeque::new();
        dists.insert(start.to_string(), 0usize);
        queue.push_back((start.to_string(), 0usize));
        while let Some((cid, d)) = queue.pop_front() {
            let parents: Option<(Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT parent_commit_id, second_parent_commit_id FROM og_commits WHERE commit_id=?1",
                    [&cid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((p1, p2)) = parents {
                for p in [p1, p2].into_iter().flatten() {
                    if !dists.contains_key(&p) {
                        dists.insert(p.clone(), d + 1);
                        queue.push_back((p, d + 1));
                    }
                }
            }
        }
        Ok(dists)
    }

    fn merge_base_commit(conn: &Connection, c_src: &str, c_dst: &str) -> Result<Option<String>> {
        let src_anc = Self::commit_ancestors(conn, c_src)?;
        let dst_anc = Self::commit_ancestors(conn, c_dst)?;
        let best = src_anc
            .into_iter()
            .filter_map(|(cid, ds)| dst_anc.get(&cid).map(|dd| (ds + dd, cid)))
            .min()
            .map(|(_, cid)| cid);
        Ok(best)
    }

    fn snapshot_atoms_at_commit(
        conn: &Connection,
        commit_id: &str,
        project_id: Option<&str>,
    ) -> Result<BTreeMap<String, Value>> {
        let mut stmt = conn.prepare(
            "SELECT atom_id, payload_json FROM og_atom_versions WHERE commit_id=?1",
        )?;
        let rows = stmt.query_map([commit_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (aid, raw) = row?;
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if let Some(pid) = project_id.filter(|p| !p.is_empty())
                    && val["project_id"].as_str() != Some(pid)
                {
                    continue;
                }
                out.insert(aid, val);
            }
        }
        Ok(out)
    }

    fn snapshot_edges_at_commit(conn: &Connection, commit_id: &str) -> Result<BTreeMap<EdgeKey, Value>> {
        let mut stmt = conn.prepare(
            "SELECT edge_key, data_json FROM og_edge_versions WHERE commit_id=?1",
        )?;
        let rows = stmt.query_map([commit_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (kraw, vraw) = row?;
            if let (Some(key), Ok(val)) = (EdgeKey::decode(&kraw), serde_json::from_str::<Value>(&vraw)) {
                out.insert(key, val);
            }
        }
        Ok(out)
    }

    fn branch_head(conn: &Connection, branch: &str) -> Result<String> {
        conn.query_row(
            "SELECT head_commit_id FROM og_branches WHERE name=?1",
            [branch],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| RuntimeError::not_found(format!("Omnigraph branch '{branch}' not found")))
    }

    fn compute_diff_and_conflicts(
        conn: &Connection,
        project_id: &str,
        source: &str,
        target: &str,
    ) -> Result<DiffComputation> {
        let pid_opt = (!project_id.is_empty()).then_some(project_id);
        let src_head = Self::branch_head(conn, source)?;
        let dst_head = Self::branch_head(conn, target)?;
        let merge_base = Self::merge_base_commit(conn, &src_head, &dst_head)?;

        let base_atoms = match merge_base.as_deref() {
            Some(c) => Self::snapshot_atoms_at_commit(conn, c, pid_opt)?,
            None => BTreeMap::new(),
        };
        let src_atoms: BTreeMap<String, Value> = Self::read_branch_atoms_conn(conn, source, pid_opt)?
            .into_iter()
            .filter_map(|a| {
                let id = a["atom_id"].as_str().map(str::to_string)?;
                Some((id, a))
            })
            .collect();
        let dst_atoms: BTreeMap<String, Value> = Self::read_branch_atoms_conn(conn, target, pid_opt)?
            .into_iter()
            .filter_map(|a| {
                let id = a["atom_id"].as_str().map(str::to_string)?;
                Some((id, a))
            })
            .collect();

        let all_ids: BTreeSet<String> = base_atoms
            .keys()
            .chain(src_atoms.keys())
            .chain(dst_atoms.keys())
            .cloned()
            .collect();

        let mut added_atoms = Vec::new();
        let mut updated_atoms = Vec::new();
        let mut deleted_atoms = Vec::new();
        let mut conflicts = Vec::new();
        let mut merged_atoms = dst_atoms.clone();

        for aid in all_ids {
            let b = base_atoms.get(&aid);
            let s = src_atoms.get(&aid);
            let t = dst_atoms.get(&aid);

            match (b, s, t) {
                // Only source touched
                (_, Some(sv), None) if b.is_none() => {
                    added_atoms.push(sv.clone());
                    merged_atoms.insert(aid, sv.clone());
                }
                (Some(bv), Some(sv), Some(tv)) if tv == bv && sv != bv => {
                    updated_atoms.push(sv.clone());
                    merged_atoms.insert(aid, sv.clone());
                }
                (Some(bv), None, Some(tv)) if tv == bv => {
                    deleted_atoms.push(tv.clone());
                    merged_atoms.remove(&aid);
                }
                // Identical changes on both
                (_, Some(sv), Some(tv)) if sv == tv => {}
                (_, None, None) => {}
                // Target-only changes: keep target in merge without conflict
                (None, None, Some(_)) => {}
                (Some(bv), Some(sv), Some(_tv)) if sv == bv => {}
                (Some(bv), Some(sv), None) if sv == bv => {}
                // Both modified or modify-vs-delete differently -> conflict!
                (_, s_opt, t_opt) => {
                    let kind = if s_opt.is_some() && t_opt.is_some() {
                        "divergent_update"
                    } else {
                        "modify_delete"
                    };
                    conflicts.push(obj!({
                        "atom_id": aid,
                        "kind": kind,
                        "source_branch": source,
                        "target_branch": target,
                        "base_atom": b.cloned().unwrap_or(Value::Null),
                        "source_atom": s_opt.cloned().unwrap_or(Value::Null),
                        "target_atom": t_opt.cloned().unwrap_or(Value::Null),
                    }));
                }
            }
        }

        let base_edges = match merge_base.as_deref() {
            Some(c) => Self::snapshot_edges_at_commit(conn, c)?,
            None => BTreeMap::new(),
        };
        let src_edges = Self::read_branch_edges_conn(conn, source)?;
        let dst_edges = Self::read_branch_edges_conn(conn, target)?;
        let mut added_edges = Vec::new();
        let mut deleted_edges = Vec::new();
        let all_ekeys: BTreeSet<EdgeKey> = base_edges
            .keys()
            .chain(src_edges.keys())
            .chain(dst_edges.keys())
            .cloned()
            .collect();
        for ek in all_ekeys {
            match (base_edges.get(&ek), src_edges.get(&ek), dst_edges.get(&ek)) {
                (None, Some(sv), None) => added_edges.push(obj!({
                    "edge_type": ek.edge_type, "from": ek.src, "to": ek.dst, "relation": ek.relation, "data": sv
                })),
                (Some(_), None, Some(tv)) => deleted_edges.push(obj!({
                    "edge_type": ek.edge_type, "from": ek.src, "to": ek.dst, "relation": ek.relation, "data": tv
                })),
                _ => {}
            }
        }

        let outcome = if !conflicts.is_empty() {
            "conflict"
        } else if added_atoms.is_empty() && updated_atoms.is_empty() && deleted_atoms.is_empty() && added_edges.is_empty() && deleted_edges.is_empty() {
            "up_to_date"
        } else {
            "clean"
        };

        let summary = obj!({
            "source": source,
            "target": target,
            "merge_base": merge_base,
            "source_commit_id": src_head,
            "target_commit_id": dst_head,
            "outcome": outcome,
            "added_atoms": added_atoms,
            "updated_atoms": updated_atoms,
            "deleted_atoms": deleted_atoms,
            "added_edges": added_edges,
            "deleted_edges": deleted_edges,
            "conflicts": conflicts,
        });

        Ok(DiffComputation {
            src_head,
            merged_atoms,
            summary,
        })
    }

    pub fn branch_diff(&self, project_id: &str, source: &str, target: Option<&str>) -> Result<Value> {
        let source = validate_branch_token(source)?;
        let target = validate_branch_token(target.unwrap_or("main"))?;
        let conn = self.lock_conn()?;
        Ok(Self::compute_diff_and_conflicts(&conn, project_id, &source, &target)?.summary)
    }

    /// Copies one atom verbatim (`keep_source` / `keep_target` style) from `from` onto `to`
    /// with optimistic `expected_updated_at` / `replaced_updated_at` staleness checks.
    pub fn keep_atom(
        &self,
        atom_id: &str,
        from: &str,
        to: &str,
        expected_updated_at: Option<&str>,
        replaced_updated_at: Option<&str>,
    ) -> Result<Value> {
        let from = validate_branch_token(from)?;
        let to = validate_branch_token(to)?;
        if from == to {
            return Err(RuntimeError::invalid("Keep copies an atom from one branch to another."));
        }
        let conn = self.lock_conn()?;
        let src_atom = Self::read_branch_atoms_conn(&conn, &from, None)?
            .into_iter()
            .find(|a| a["atom_id"].as_str() == Some(atom_id))
            .ok_or_else(|| RuntimeError::not_found(format!("Atom '{atom_id}' not found on '{from}'.")))?;

        if let Some(expected) = expected_updated_at.map(str::trim).filter(|s| !s.is_empty())
            && src_atom["updated_at"].as_str() != Some(expected)
        {
            return Err(RuntimeError::conflict(format!(
                "Atom '{atom_id}' changed on '{from}' after merge preview; re-preview before keeping."
            )));
        }
        let dst_atom = Self::read_branch_atoms_conn(&conn, &to, None)?
            .into_iter()
            .find(|a| a["atom_id"].as_str() == Some(atom_id));
        if let Some(replaced) = replaced_updated_at.map(str::trim).filter(|s| !s.is_empty())
            && dst_atom.as_ref().and_then(|a| a["updated_at"].as_str()) != Some(replaced)
        {
            return Err(RuntimeError::conflict(format!(
                "Atom '{atom_id}' changed on '{to}' after merge preview; re-preview before keeping."
            )));
        }

        // Copy atom verbatim (preserving exact `source_branch` and `updated_at`) + its edges so branches converge!
        Self::upsert_atom_conn(&conn, &to, &src_atom)?;
        conn.execute(
            "DELETE FROM og_edges WHERE branch=?1 AND edge_type IN ('TouchesFile','MentionsSymbol') AND src=?2",
            params![to, atom_id],
        )?;
        for (ek, val) in Self::read_branch_edges_conn(&conn, &from)? {
            if ek.src == atom_id && matches!(ek.edge_type.as_str(), "TouchesFile" | "MentionsSymbol") {
                conn.execute(
                    "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![to, ek.edge_type, ek.src, ek.dst, ek.relation, serde_json::to_string(&val)?],
                )?;
            }
        }
        let ts = now_utc_iso_ms();
        let commit_id = Self::record_commit(
            &conn,
            &to,
            None,
            &format!("keep {atom_id} from {from}"),
            &ts,
        )?;
        Ok(obj!({
            "ok": true,
            "action": "keep",
            "atom_id": atom_id,
            "from": from,
            "to": to,
            "atom": src_atom,
            "commit_id": commit_id,
        }))
    }

    /// Unified 3-way merge & conflict resolution (`strategy`: `"merge"` | `"keep_source"` | `"keep_target"`).
    pub fn merge_keep(&self, opts: &MergeKeepOptions<'_>) -> Result<Value> {
        let project_id = opts.project_id;
        let source = validate_branch_token(opts.source)?;
        let target = validate_branch_token(opts.target.unwrap_or("main"))?;
        if source == target {
            return Err(RuntimeError::invalid("Source and target branches must differ"));
        }

        let strat = opts.strategy.trim().to_ascii_lowercase();
        if !matches!(strat.as_str(), "merge" | "keep_source" | "keep_target") {
            return Err(RuntimeError::invalid(
                "Invalid strategy; expected one of: merge, keep_source, keep_target",
            ));
        }
        let atom_ids = opts.atom_ids;
        let dry_run = opts.dry_run;
        let delete_branch = opts.delete_branch;

        // First compute conflicts & apply per-atom resolution when requested.
        let initial_diff = self.branch_diff(project_id, &source, Some(&target))?;
        let conflicts = initial_diff["conflicts"].as_array().cloned().unwrap_or_default();

        if !conflicts.is_empty() && strat == "merge" {
            return Ok(obj!({
                "ok": false,
                "action": "merge",
                "source": source,
                "target": target,
                "outcome": "conflict",
                "dry_run": dry_run,
                "conflicts": conflicts,
            }));
        }

        if !conflicts.is_empty() && !dry_run {
            let filter_set: BTreeSet<&str> = atom_ids.iter().map(String::as_str).collect();
            for c in &conflicts {
                let Some(aid) = c["atom_id"].as_str() else {
                    continue;
                };
                if !filter_set.is_empty() && !filter_set.contains(aid) {
                    continue;
                }
                match strat.as_str() {
                    "keep_source" => {
                        let _ = self.keep_atom(aid, &source, &target, None, None)?;
                    }
                    "keep_target" => {
                        let _ = self.keep_atom(aid, &target, &source, None, None)?;
                    }
                    _ => {}
                }
            }
        }

        let conn = self.lock_conn()?;
        let DiffComputation {
            src_head,
            merged_atoms,
            summary: mut diff_after,
        } = Self::compute_diff_and_conflicts(&conn, project_id, &source, &target)?;
        let remaining_conflicts = diff_after["conflicts"].as_array().cloned().unwrap_or_default();
        if !remaining_conflicts.is_empty() {
            return Ok(obj!({
                "ok": false,
                "action": "merge",
                "source": source,
                "target": target,
                "outcome": "conflict",
                "dry_run": dry_run,
                "conflicts": remaining_conflicts,
            }));
        }
        if dry_run {
            diff_after["ok"] = obj!(true);
            diff_after["dry_run"] = obj!(true);
            return Ok(diff_after);
        }

        // Apply merged state onto target and record 2-parent merge commit.
        let ts = now_utc_iso_ms();
        let pid_opt = (!project_id.is_empty()).then_some(project_id);
        let existing_dst_ids: BTreeSet<String> = Self::read_branch_atoms_conn(&conn, &target, pid_opt)?
            .into_iter()
            .filter_map(|a| a["atom_id"].as_str().map(str::to_string))
            .collect();
        for old_id in existing_dst_ids {
            if !merged_atoms.contains_key(&old_id) {
                conn.execute(
                    "DELETE FROM og_atoms WHERE branch=?1 AND atom_id=?2",
                    params![target, old_id],
                )?;
                conn.execute(
                    "DELETE FROM og_edges WHERE branch=?1 AND src=?2",
                    params![target, old_id],
                )?;
            }
        }
        for atom in merged_atoms.values() {
            Self::upsert_atom_conn(&conn, &target, atom)?;
        }
        // Carry over non-conflicting TouchesFile/MentionsSymbol/RelatesTo edges from source for kept atoms
        for (ek, val) in Self::read_branch_edges_conn(&conn, &source)? {
            if merged_atoms.contains_key(&ek.src) {
                let exists: Option<String> = conn
                    .query_row(
                        "SELECT data_json FROM og_edges WHERE branch=?1 AND edge_type=?2 AND src=?3 AND dst=?4 AND relation=?5",
                        params![target, ek.edge_type, ek.src, ek.dst, ek.relation],
                        |r| r.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    conn.execute(
                        "INSERT INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,?2,?3,?4,?5,?6)",
                        params![target, ek.edge_type, ek.src, ek.dst, ek.relation, serde_json::to_string(&val)?],
                    )?;
                }
            }
        }
        let merge_commit = Self::record_commit(
            &conn,
            &target,
            Some(&src_head),
            &format!("merge {source} into {target} ({strat})"),
            &ts,
        )?;
        let mut deleted = false;
        if delete_branch && source != "main" && source != CANONICAL_KNOWLEDGE_BRANCH {
            drop(conn);
            let _ = self.delete_branch(&source)?;
            deleted = true;
        }
        Ok(obj!({
            "ok": true,
            "action": "merge",
            "strategy": strat,
            "source": source,
            "target": target,
            "outcome": "merged",
            "branch_deleted": deleted,
            "commit_id": merge_commit,
            "conflicts": [],
        }))
    }

    pub fn export_ndjson(
        &self,
        project_id: &str,
        branch: Option<&str>,
        scope: &str,
        include_inherited: bool,
    ) -> Result<Value> {
        let branch = validate_branch_token(branch.unwrap_or("main"))?;
        let conn = self.lock_conn()?;
        let mut parsed = ParsedGraph::default();
        for atom in Self::read_branch_atoms_conn(&conn, &branch, None)? {
            if let Some(id) = atom["atom_id"].as_str().map(str::to_string) {
                parsed.atoms.insert(id, atom);
            }
        }
        {
            let mut stmt = conn.prepare("SELECT path,project_id,language,last_touched_at FROM og_files WHERE branch=?1")?;
            let rows = stmt.query_map([&branch], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    obj!({
                        "path": r.get::<_, String>(0)?,
                        "project_id": r.get::<_, String>(1)?,
                        "language": r.get::<_, Option<String>>(2)?,
                        "last_touched_at": r.get::<_, String>(3)?,
                    }),
                ))
            })?;
            for row in rows {
                let (k, v) = row?;
                parsed.files.insert(k, v);
            }
        }
        {
            let mut stmt = conn.prepare("SELECT fqn,project_id,file_path,kind,name FROM og_symbols WHERE branch=?1")?;
            let rows = stmt.query_map([&branch], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    obj!({
                        "fqn": r.get::<_, String>(0)?,
                        "project_id": r.get::<_, String>(1)?,
                        "file_path": r.get::<_, String>(2)?,
                        "kind": r.get::<_, String>(3)?,
                        "name": r.get::<_, String>(4)?,
                    }),
                ))
            })?;
            for row in rows {
                let (k, v) = row?;
                parsed.symbols.insert(k, v);
            }
        }
        for (idx, (ek, val)) in Self::read_branch_edges_conn(&conn, &branch)?.into_iter().enumerate() {
            let map: Map<String, Value> = val.as_object().cloned().unwrap_or_default();
            parsed.edges.push(EdgeRecord {
                edge_type: ek.edge_type,
                id: format!("edge-{idx}"),
                from: ek.src,
                to: ek.dst,
                data: map,
            });
        }
        let filtered = filter_graph_for_export(&parsed, project_id, scope, None, &branch, include_inherited);
        let ndjson = serialize_graph_ndjson(&filtered);
        Ok(obj!({
            "ok": true,
            "format": transfer::BUNDLE_FORMAT_V1,
            "branch": branch,
            "atoms": filtered.atoms.len(),
            "files": filtered.files.len(),
            "symbols": filtered.symbols.len(),
            "edges": filtered.edges.len(),
            "ndjson": ndjson,
        }))
    }

    pub fn import_ndjson(&self, project_id: &str, target_branch: Option<&str>, ndjson: &str) -> Result<Value> {
        let branch = validate_branch_token(target_branch.unwrap_or("main"))?;
        let parsed = parse_ndjson_records(ndjson)?;
        let ts = now_utc_iso_ms();
        let (normalized, effective_proj) =
            prepare_import_ndjson(&parsed, project_id, &branch, None, &ts);
        let conn = self.lock_conn()?;
        let _ = Self::ensure_branch_conn(&conn, &branch, Some("main"), &ts)?;
        for (aid, atom) in &normalized.atoms {
            let mut copy = atom.clone();
            copy["atom_id"] = obj!(aid);
            Self::upsert_atom_conn(&conn, &branch, &copy)?;
        }
        for (path, f) in &normalized.files {
            conn.execute(
                "INSERT OR REPLACE INTO og_files(branch,path,project_id,language,last_touched_at) VALUES(?1,?2,?3,?4,?5)",
                params![branch, path, f["project_id"].as_str().unwrap_or(&effective_proj), f["language"].as_str(), f["last_touched_at"].as_str().unwrap_or(&ts)],
            )?;
        }
        for (fqn, s) in &normalized.symbols {
            conn.execute(
                "INSERT OR REPLACE INTO og_symbols(branch,fqn,project_id,file_path,kind,name) VALUES(?1,?2,?3,?4,?5,?6)",
                params![branch, fqn, s["project_id"].as_str().unwrap_or(&effective_proj), s["file_path"].as_str().unwrap_or(""), s["kind"].as_str().unwrap_or("symbol"), s["name"].as_str().unwrap_or(fqn)],
            )?;
        }
        for edge in &normalized.edges {
            let rel = edge.relation_label();
            conn.execute(
                "INSERT OR REPLACE INTO og_edges(branch,edge_type,src,dst,relation,data_json) VALUES(?1,?2,?3,?4,?5,?6)",
                params![branch, edge.edge_type, edge.from, edge.to, rel, serde_json::to_string(&Value::Object(edge.data.clone()))?],
            )?;
        }
        let commit_id = Self::record_commit(
            &conn,
            &branch,
            None,
            &format!("import {} atoms", normalized.atoms.len()),
            &ts,
        )?;
        Ok(obj!({
            "ok": true,
            "branch": branch,
            "atoms": normalized.atoms.len(),
            "files": normalized.files.len(),
            "symbols": normalized.symbols.len(),
            "edges": normalized.edges.len(),
            "commit_id": commit_id,
        }))
    }
}

/// Authority-annotated capsule bullet formatting (matching PR #112 `helpers.rs`).
pub fn format_atom_line_with_authority(atom: &Value) -> String {
    let kind = atom["kind"].as_str().unwrap_or("insight");
    let title = atom["title"].as_str().unwrap_or("Untitled");
    let summary = atom["summary"].as_str().unwrap_or("");
    let src = atom["source_branch"].as_str().unwrap_or("main");
    let verified = atom["verified"].as_bool().unwrap_or(false);
    let origin = atom["origin_layer"].as_str().map(str::trim).unwrap_or("");
    let brain_id = atom["brain_record_id"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let mem_id = atom["memory_id"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let is_imported = origin == "imported" || src.starts_with("imported/");

    let tag = if !is_imported && verified && origin == "memory-bank" {
        match mem_id {
            Some(mid) => format!("[memory-bank {kind} · {mid}]"),
            None => format!("[memory-bank {kind}]"),
        }
    } else if !is_imported && verified && origin == "project-brain" {
        match brain_id {
            Some(bid) => {
                let short: String = bid.chars().take(8).collect();
                format!("[project-brain {kind} · {short}]")
            }
            None => format!("[project-brain {kind}]"),
        }
    } else if !is_imported
        && verified
        && src == CANONICAL_KNOWLEDGE_BRANCH
        && (mem_id.is_some() || brain_id.is_some())
    {
        if let Some(mid) = mem_id {
            format!("[memory-bank {kind} · {mid}]")
        } else {
            let short: String = brain_id.unwrap_or("").chars().take(8).collect();
            format!("[project-brain {kind} · {short}]")
        }
    } else {
        format!("[unverified reference — verify against code & Brain] [{kind}]")
    };

    if summary.trim().is_empty() {
        format!("- {tag} {title} (from `{src}`)\n")
    } else {
        format!("- {tag} {title} (from `{src}`): {summary}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Roots, domain::uuid4};

    fn test_ctx(root: &Path) -> RuntimeContext {
        std::fs::create_dir_all(root.join("memory-bank/chunks")).unwrap();
        std::fs::create_dir_all(root.join("memory-bank/local")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/OrderService.php"),
            "<?php\nclass OrderService {\n    public function issueInvoice(): void {}\n}\n",
        )
        .unwrap();
        RuntimeContext::new(Roots::installed(root).unwrap(), "local", "governed").unwrap()
    }

    #[test]
    fn canonical_sync_symbol_links_and_pruning_roundtrip() {
        let root = std::env::temp_dir().join(format!("cg-om-sync-{}", uuid4()));
        let ctx = test_ctx(&root);
        let eng = OmnigraphEngine::open(&ctx).unwrap();

        let item1 = obj!({
            "atom_id": "brain-111",
            "kind": "decision",
            "title": "Issue invoice rule",
            "body": "OrderService::issueInvoice validates VAT before billing.",
            "files": ["src/OrderService.php"],
            "origin_layer": "project-brain",
            "authority": "verified",
            "verified": true,
            "brain_record_id": "111",
        });
        let item2 = obj!({
            "atom_id": "mem-MEM-20261101-00000001",
            "kind": "warning",
            "title": "Legacy retry note",
            "body": "Avoid direct calls.",
            "files": ["src/OrderService.php"],
            "origin_layer": "memory-bank",
            "authority": "verified",
            "verified": true,
            "memory_id": "MEM-20261101-00000001",
        });

        let r1 = eng
            .sync_canonical_items("proj", &[item1.clone(), item2], &["project-brain".into(), "memory-bank".into()])
            .unwrap();
        assert_eq!(r1["synced"], 2);
        assert_eq!(r1["mutated_atoms"], 2);
        assert!(r1["symbols"].as_u64().unwrap() >= 2);

        // File context exposes both atoms + extracted symbols
        let fctx = eng.file_context("proj", "src/OrderService.php", None).unwrap();
        let atoms = fctx["atoms"].as_array().unwrap();
        assert_eq!(atoms.len(), 2);
        let syms = atoms[0]["symbols"].as_array().unwrap();
        assert!(syms.iter().any(|s| s == "OrderService::issueInvoice"));

        // Second sync with only item1 prunes retired memory-bank atom via prune_layers
        let r2 = eng
            .sync_canonical_items("proj", &[item1], &["project-brain".into(), "memory-bank".into()])
            .unwrap();
        assert_eq!(r2["skipped_atoms"], 1);
        assert_eq!(r2["deleted"], 1);
        assert_eq!(r2["deleted_atom_ids"][0], "mem-MEM-20261101-00000001");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn three_way_branch_diff_detects_conflict_and_keep_resolves_it() {
        let root = std::env::temp_dir().join(format!("cg-om-merge-{}", uuid4()));
        let ctx = test_ctx(&root);
        let eng = OmnigraphEngine::open(&ctx).unwrap();

        eng.publish_atom(&obj!({
            "atom_id": "atom-retry",
            "project_id": "proj",
            "branch": "main",
            "kind": "finding",
            "title": "Initial retry timeout",
            "body": "Timeout is 5s.",
            "updated_at": "2026-11-24T10:00:00Z"
        }))
        .unwrap();
        eng.create_branch("session/s1", Some("main")).unwrap();

        // Both branches edit atom-retry independently -> divergent_update
        eng.publish_atom(&obj!({
            "atom_id": "atom-retry",
            "project_id": "proj",
            "branch": "session/s1",
            "kind": "finding",
            "title": "Retry timeout 10s",
            "body": "Increased on session branch.",
            "updated_at": "2026-11-24T10:01:00Z"
        }))
        .unwrap();
        eng.publish_atom(&obj!({
            "atom_id": "atom-retry",
            "project_id": "proj",
            "branch": "main",
            "kind": "finding",
            "title": "Retry timeout 15s",
            "body": "Increased on main.",
            "updated_at": "2026-11-24T10:02:00Z"
        }))
        .unwrap();

        let diff = eng.branch_diff("proj", "session/s1", Some("main")).unwrap();
        assert_eq!(diff["outcome"], "conflict");
        assert_eq!(diff["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(diff["conflicts"][0]["kind"], "divergent_update");

        // Merge without keep fails cleanly with conflict
        let blocked = eng
            .merge_keep(&MergeKeepOptions {
                project_id: "proj",
                source: "session/s1",
                target: Some("main"),
                strategy: "merge",
                atom_ids: &[],
                dry_run: false,
                delete_branch: false,
            })
            .unwrap();
        assert_eq!(blocked["ok"], false);
        assert_eq!(blocked["outcome"], "conflict");

        // Resolve via keep_source
        let merged = eng
            .merge_keep(&MergeKeepOptions {
                project_id: "proj",
                source: "session/s1",
                target: Some("main"),
                strategy: "keep_source",
                atom_ids: &[],
                dry_run: false,
                delete_branch: false,
            })
            .unwrap();
        assert_eq!(merged["ok"], true);
        assert_eq!(merged["outcome"], "merged");

        let main_atoms = eng
            .atoms_query("proj", Some("main"), None, None, 50, 0)
            .unwrap();
        assert_eq!(main_atoms["atoms"][0]["title"], "Retry timeout 10s");

        // Export & import NDJSON roundtrip
        let exported = eng.export_ndjson("proj", Some("main"), "all", true).unwrap();
        assert_eq!(exported["atoms"], 1);
        let imported = eng
            .import_ndjson("proj", Some("shared/copied"), exported["ndjson"].as_str().unwrap())
            .unwrap();
        assert_eq!(imported["atoms"], 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
