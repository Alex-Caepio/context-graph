//! Framework-agnostic native implementations of the memory & continuity hooks.
//! Memory hook failures degrade silently/cleanly (exit 0), never blocking user turns.
//!
//! Includes PR #112 settled-mtime preflight caches (`HOOK_PREFLIGHT_RACY_NS`) with
//! Graph IR write-epoch invalidation (`snapshot::collect_graph_ir_revisions`).

use crate::process;
use context_graph_core::{
    BankService, OpenOptions, Result, Roots, RuntimeConfig, RuntimeContext, RuntimeError, Store,
    fs, git::GitFrame, graph, snapshot, storage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as object};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const ROLES: &[&str] = &[
    "working-memory-read",
    "working-memory-write",
    "local-context",
    "context-continuity",
];

const HOOK_PREFLIGHT_RACY_NS: i128 = 2_000_000_000;
const HOOK_PREFLIGHT_CACHE_FILE: &str = "memory-bank/local/hook-refresh-preflight.json";
const HOOK_WRITE_PREFLIGHT_CACHE_FILE: &str = "memory-bank/local/hook-write-preflight.json";
const HOOK_CONTINUITY_PREFLIGHT_CACHE_FILE: &str = "memory-bank/local/hook-continuity-preflight.json";

fn session_id_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^[A-Za-z0-9._:-]{1,128}$").expect("constant session regex")
    })
}

fn time_to_unix_ns(time: SystemTime) -> Option<i128> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => i128::try_from(delta.as_nanos()).ok(),
        Err(err) => i128::try_from(err.duration().as_nanos())
            .ok()
            .and_then(|ns| ns.checked_neg()),
    }
}

fn now_unix_ns() -> i128 {
    time_to_unix_ns(SystemTime::now()).unwrap_or(0)
}

fn settled_mtime(mtime_ns: i128, now_ns: i128) -> bool {
    mtime_ns > 0 && now_ns > HOOK_PREFLIGHT_RACY_NS && mtime_ns <= now_ns - HOOK_PREFLIGHT_RACY_NS
}

fn max_dir_mtime_ns(dir: &Path, remaining: &mut usize) -> i128 {
    if !dir.is_dir() || *remaining == 0 {
        return 0;
    }
    let mut max_ns = dir
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(time_to_unix_ns)
        .unwrap_or(0);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return max_ns;
    };
    for entry in entries.flatten() {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            return i128::MAX;
        }
        if let Some(ns) = meta.modified().ok().and_then(time_to_unix_ns) {
            max_ns = max_ns.max(ns);
        }
        if meta.is_dir() && path.file_name().is_none_or(|n| n != "local") {
            max_ns = max_ns.max(max_dir_mtime_ns(&path, remaining));
        }
    }
    max_ns
}

#[derive(Clone, Debug, Default)]
pub struct HookOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl HookOutput {
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            code: 0,
            stdout: value.into().into_bytes(),
            stderr: Vec::new(),
        }
    }

    pub fn envelope(value: Value) -> Result<Self> {
        if let Some(render) = value.get("_cli") {
            let code = render["code"].as_i64().unwrap_or(0) as i32;
            let stdout = render["stdout"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            let stderr = render["stderr"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            return Ok(Self { code, stdout, stderr });
        }
        if value["stdio_inherited"] == true {
            return Ok(Self {
                code: value["exit_code"].as_i64().unwrap_or(0) as i32,
                ..Default::default()
            });
        }
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        Ok(Self {
            code: 0,
            stdout: bytes,
            stderr: Vec::new(),
        })
    }

    pub fn write(self) -> std::io::Result<i32> {
        {
            let mut out = std::io::stdout().lock();
            out.write_all(&self.stdout)?;
            out.flush()?;
        }
        {
            let mut err = std::io::stderr().lock();
            err.write_all(&self.stderr)?;
            err.flush()?;
        }
        Ok(self.code)
    }
}

pub fn passed(host: &str, role: &str) -> HookOutput {
    if host == "cursor" && role == "working-memory-read" {
        HookOutput::text("{\"continue\": true}\n")
    } else {
        HookOutput::default()
    }
}

fn valid_abs_path(value: Option<&str>, limit: usize) -> Option<PathBuf> {
    let value = value?;
    if value.chars().count() > limit
        || value.contains(['\n', '\0'])
        || !Path::new(value).is_absolute()
    {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn is_plugin_dir(dir: &Path) -> bool {
    context_graph_core::mcp::is_plugin_dir(dir)
}

fn parse_payload(raw: &[u8]) -> Value {
    serde_json::from_slice::<Value>(raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| object!({}))
}

fn resolve_hook_roots(root: &Path, payload: &Value) -> Result<Option<(Roots, Option<String>)>> {
    let mut candidate = root.to_path_buf();
    if let Some(cwd) = valid_abs_path(payload["cwd"].as_str(), 4096).filter(|p| p.is_dir())
        && (candidate == Path::new(".") || is_plugin_dir(&candidate))
    {
        candidate = cwd;
    }
    if candidate == Path::new(".") {
        candidate = std::env::current_dir()?;
    }
    let canonical = fs::canonicalize(&candidate)?;
    if is_plugin_dir(&canonical) {
        return Ok(None);
    }
    let state = std::env::var_os("ACCELERATOR_STATE_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| canonical.clone());
    let roots = Roots::discover(&state, &canonical)?;
    let task = if let Ok(t) = std::env::var("CONTEXT_TASK_ID")
        && !t.is_empty()
    {
        Some(t)
    } else {
        GitFrame::discover(&roots.project)?
            .and_then(|f| f.working_identity().ok().flatten())
    };
    Ok(Some((roots, task)))
}

fn invoke_self_memory(
    roots: &Roots,
    operation: &str,
    extra: &[String],
    stdin_bytes: &[u8],
    timeout: Duration,
) -> Result<process::ProcessOutput> {
    let exe = std::env::current_exe()?;
    let mut args = vec![
        OsString::from("memory"),
        OsString::from("--root"),
        roots.state.as_os_str().to_os_string(),
    ];
    if roots.shared {
        args.push(OsString::from("--project"));
        args.push(roots.project.as_os_str().to_os_string());
    }
    args.push(OsString::from(operation));
    for item in extra {
        args.push(OsString::from(item));
    }
    let mut env = BTreeMap::new();
    if roots.shared || roots.attached() {
        env.insert(
            OsString::from("ACCELERATOR_STATE_DIR"),
            Some(roots.state.as_os_str().to_os_string()),
        );
        env.insert(
            OsString::from("ACCELERATOR_PROJECT_DIR"),
            Some(roots.project.as_os_str().to_os_string()),
        );
    }
    process::run_with_env(
        &exe.into_os_string(),
        &args,
        &roots.project,
        Some(stdin_bytes),
        timeout,
        4 * 1024 * 1024,
        &env,
    )
}

fn budget_from_env() -> Duration {
    let secs = std::env::var("CONTEXT_HOOK_BUDGET")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(5.0);
    Duration::from_secs_f64(secs)
}

fn git_head_and_index_mtime(project: &Path) -> (String, i128) {
    if let Ok(Some(frame)) = GitFrame::discover(project) {
        let idx_mtime = std::fs::symlink_metadata(frame.admin.join("index"))
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(time_to_unix_ns)
            .unwrap_or(0);
        let head_txt = std::fs::read_to_string(frame.admin.join("HEAD")).unwrap_or_default();
        let trimmed = head_txt.trim();
        let resolved = if let Some(ref_name) = trimmed.strip_prefix("ref:") {
            let ref_rel = ref_name.trim();
            frame
                .common_dir()
                .ok()
                .and_then(|common| {
                    std::fs::read_to_string(common.join(ref_rel))
                        .ok()
                        .or_else(|| std::fs::read_to_string(common.join("packed-refs")).ok())
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        (format!("{trimmed}:{}", resolved.trim()), idx_mtime)
    } else {
        (String::new(), 0)
    }
}

fn transcript_stamp(payload: &Value, require_settled_now: Option<i128>) -> Option<String> {
    let Some(path) = valid_abs_path(payload["transcript_path"].as_str(), 4096) else {
        return Some(String::new());
    };
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let mt = meta.modified().ok().and_then(time_to_unix_ns)?;
    if let Some(now_ns) = require_settled_now
        && !settled_mtime(mt, now_ns)
    {
        return None;
    }
    Some(format!("{}:{mt}", meta.len()))
}

fn file_stamp_if_present(path: &Path, now_ns: i128) -> Option<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Some("missing".into()),
        Err(_) => return None,
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let mt = meta.modified().ok().and_then(time_to_unix_ns)?;
    if !settled_mtime(mt, now_ns) {
        return None;
    }
    Some(format!("{}:{mt}", meta.len()))
}

fn load_json_cache<T: for<'de> Deserialize<'de>>(roots: &Roots, relative: &str) -> Option<T> {
    let path = roots.state.join(relative);
    let bytes = fs::read_regular(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_json_cache<T: Serialize>(roots: &Roots, relative: &str, record: &T) {
    let path = roots.state.join(relative);
    if let Ok(bytes) = serde_json::to_vec(record) {
        let _ = fs::atomic_write(&path, &bytes);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RefreshPreflightRecord {
    task_id: String,
    index_gen: String,
    git_head: String,
    host: String,
    query_digest: String,
    db_mtime_ns: i128,
    db_len: u64,
    pending_deltas: i64,
    task_updated_at: String,
    tree_max_mtime_ns: i128,
    transcript_stamp: String,
    graph_ir_revisions: BTreeMap<String, i64>,
    stdout: Vec<u8>,
}

fn refresh_preflight_snapshot(
    roots: &Roots,
    task: Option<&str>,
    host: &str,
    payload: &Value,
    query: &str,
    now_ns: i128,
) -> Option<RefreshPreflightRecord> {
    if std::env::var("HARNESS_DISABLE_HOOK_PREFLIGHT").as_deref() == Ok("1") {
        return None;
    }
    let task_id = task.filter(|t| !t.is_empty())?.to_owned();
    if query.trim().is_empty() || query.starts_with("INTERNAL_NATIVE_MEMORY_RESULT ") {
        return None;
    }
    if host == "cursor"
        && roots.state == roots.tooling
        && !roots.tooling.join(".cursor/rules/working-memory.mdc").is_file()
    {
        return None;
    }
    let db_path = storage::default_database(roots).ok()?;
    let db_meta = std::fs::symlink_metadata(&db_path).ok()?;
    if db_meta.file_type().is_symlink() || !db_meta.is_file() {
        return None;
    }
    let db_mtime_ns = db_meta.modified().ok().and_then(time_to_unix_ns)?;
    if !settled_mtime(db_mtime_ns, now_ns) {
        return None;
    }

    let mut budget = 1500usize;
    let chunks_mtime = max_dir_mtime_ns(&roots.state.join("memory-bank/chunks"), &mut budget);
    let brain_mtime = max_dir_mtime_ns(&roots.brain_root(), &mut budget);
    let mut tree_max_mtime_ns = chunks_mtime.max(brain_mtime).max(db_mtime_ns);
    let (git_head, git_idx_mtime) = git_head_and_index_mtime(&roots.project);
    tree_max_mtime_ns = tree_max_mtime_ns.max(git_idx_mtime);
    if tree_max_mtime_ns == i128::MAX || !settled_mtime(tree_max_mtime_ns, now_ns) {
        return None;
    }

    let store = Store::open_read_only(&db_path, Duration::from_millis(250))
        .or_else(|_| Store::open(&db_path, OpenOptions::default()))
        .ok()?;
    let index_gen = storage::index_generation(&store.conn).ok()?;
    let pending_deltas: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM turn_deltas WHERE task_id=?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let task_updated_at: String = store
        .conn
        .query_row(
            "SELECT COALESCE(MAX(updated_at), '') FROM working_tasks WHERE task_id=?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let transcript_stamp = transcript_stamp(payload, None).unwrap_or_default();
    let graph_ir_revisions =
        snapshot::collect_graph_ir_revisions([roots.state.as_path(), roots.project.as_path()]);

    Some(RefreshPreflightRecord {
        task_id,
        index_gen,
        git_head,
        host: host.to_string(),
        query_digest: context_graph_core::query::sha256(query.as_bytes()),
        db_mtime_ns,
        db_len: db_meta.len(),
        pending_deltas,
        task_updated_at,
        tree_max_mtime_ns,
        transcript_stamp,
        graph_ir_revisions,
        stdout: Vec::new(),
    })
}

fn refresh_preflight_matches(
    cached: &RefreshPreflightRecord,
    current: &RefreshPreflightRecord,
) -> bool {
    cached.task_id == current.task_id
        && cached.index_gen == current.index_gen
        && cached.git_head == current.git_head
        && cached.host == current.host
        && cached.query_digest == current.query_digest
        && cached.db_mtime_ns == current.db_mtime_ns
        && cached.db_len == current.db_len
        && cached.pending_deltas == current.pending_deltas
        && cached.task_updated_at == current.task_updated_at
        && cached.tree_max_mtime_ns == current.tree_max_mtime_ns
        && cached.transcript_stamp == current.transcript_stamp
        && cached.graph_ir_revisions == current.graph_ir_revisions
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WritePreflightRecord {
    task_id: String,
    host: String,
    recovery: bool,
    delivered: bool,
    flush_after: String,
    index_gen: String,
    git_head: String,
    db_mtime_ns: i128,
    db_len: u64,
    pending_deltas: i64,
    task_updated_at: String,
    tree_max_mtime_ns: i128,
    transcript_stamp: String,
    payload_sha256: String,
    graph_ir_revisions: BTreeMap<String, i64>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct WritePreflightArgs<'a> {
    task: Option<&'a str>,
    host: &'a str,
    payload: &'a Value,
    raw: &'a [u8],
    recovery: bool,
    delivered: bool,
    flush_after: &'a str,
    now_ns: i128,
}

fn write_preflight_snapshot(roots: &Roots, args: &WritePreflightArgs<'_>) -> Option<WritePreflightRecord> {
    if std::env::var("HARNESS_DISABLE_HOOK_PREFLIGHT").as_deref() == Ok("1") {
        return None;
    }
    let task_id = args.task.filter(|t| !t.is_empty())?.to_owned();
    let now_ns = args.now_ns;
    let db_path = storage::default_database(roots).ok()?;
    let db_meta = std::fs::symlink_metadata(&db_path).ok()?;
    if db_meta.file_type().is_symlink() || !db_meta.is_file() {
        return None;
    }
    let db_mtime_ns = db_meta.modified().ok().and_then(time_to_unix_ns)?;
    if !settled_mtime(db_mtime_ns, now_ns) {
        return None;
    }
    let mut budget = 1500usize;
    let chunks_mtime = max_dir_mtime_ns(&roots.state.join("memory-bank/chunks"), &mut budget);
    let brain_mtime = max_dir_mtime_ns(&roots.brain_root(), &mut budget);
    let (git_head, git_idx_mtime) = git_head_and_index_mtime(&roots.project);
    let tree_max_mtime_ns = chunks_mtime
        .max(brain_mtime)
        .max(db_mtime_ns)
        .max(git_idx_mtime);
    if tree_max_mtime_ns == i128::MAX || !settled_mtime(tree_max_mtime_ns, now_ns) {
        return None;
    }
    let store = Store::open_read_only(&db_path, Duration::from_millis(250))
        .or_else(|_| Store::open(&db_path, OpenOptions::default()))
        .ok()?;
    let index_gen = storage::index_generation(&store.conn).ok()?;
    let pending_deltas: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM turn_deltas WHERE task_id=?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let task_updated_at: String = store
        .conn
        .query_row(
            "SELECT COALESCE(MAX(updated_at), '') FROM working_tasks WHERE task_id=?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let transcript_stamp = transcript_stamp(args.payload, Some(now_ns))?;
    let graph_ir_revisions =
        snapshot::collect_graph_ir_revisions([roots.state.as_path(), roots.project.as_path()]);

    Some(WritePreflightRecord {
        task_id,
        host: args.host.to_string(),
        recovery: args.recovery,
        delivered: args.delivered,
        flush_after: args.flush_after.to_string(),
        index_gen,
        git_head,
        db_mtime_ns,
        db_len: db_meta.len(),
        pending_deltas,
        task_updated_at,
        tree_max_mtime_ns,
        transcript_stamp,
        payload_sha256: context_graph_core::query::sha256(args.raw),
        graph_ir_revisions,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn write_preflight_matches(cached: &WritePreflightRecord, current: &WritePreflightRecord) -> bool {
    cached.task_id == current.task_id
        && cached.host == current.host
        && cached.recovery == current.recovery
        && cached.delivered == current.delivered
        && cached.flush_after == current.flush_after
        && cached.index_gen == current.index_gen
        && cached.git_head == current.git_head
        && cached.db_mtime_ns == current.db_mtime_ns
        && cached.db_len == current.db_len
        && cached.pending_deltas == current.pending_deltas
        && cached.task_updated_at == current.task_updated_at
        && cached.tree_max_mtime_ns == current.tree_max_mtime_ns
        && cached.transcript_stamp == current.transcript_stamp
        && cached.payload_sha256 == current.payload_sha256
        && cached.graph_ir_revisions == current.graph_ir_revisions
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ContinuityPreflightRecord {
    host: String,
    owner: String,
    env_digest: String,
    git_head: String,
    config_stamp: String,
    handoff_tree_digest: String,
    handoff_max_mtime_ns: i128,
    transcript_stamp: String,
    payload_sha256: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn handoff_settled_fingerprint(handoff: &Path, now_ns: i128) -> Option<(i128, String)> {
    let meta = match std::fs::symlink_metadata(handoff) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Some((0, "missing".into())),
        Err(_) => return None,
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    let mut max_mtime = meta.modified().ok().and_then(time_to_unix_ns)?;
    let mut files = Vec::<String>::new();
    let mut stack = vec![handoff.to_path_buf()];
    let mut remaining = 512usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return None;
        };
        for entry in entries.flatten() {
            if remaining == 0 {
                return None;
            }
            remaining -= 1;
            let path = entry.path();
            let Ok(m) = std::fs::symlink_metadata(&path) else {
                return None;
            };
            if m.file_type().is_symlink() {
                return None;
            }
            let mt = m.modified().ok().and_then(time_to_unix_ns)?;
            max_mtime = max_mtime.max(mt);
            let rel = path
                .strip_prefix(handoff)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if m.is_dir() {
                files.push(format!("D:{rel}:{mt}"));
                stack.push(path);
            } else {
                files.push(format!("F:{rel}:{}:{mt}", m.len()));
            }
        }
    }
    if !settled_mtime(max_mtime, now_ns) {
        return None;
    }
    files.sort();
    Some((
        max_mtime,
        context_graph_core::query::sha256(files.join("\n").as_bytes()),
    ))
}

fn continuity_preflight_snapshot(
    roots: &Roots,
    host: &str,
    payload: &Value,
    raw: &[u8],
    now_ns: i128,
) -> Option<ContinuityPreflightRecord> {
    if std::env::var("HARNESS_DISABLE_HOOK_PREFLIGHT").as_deref() == Ok("1") {
        return None;
    }
    let handoff = roots.state.join(".context-handoff");
    let (handoff_max_mtime_ns, handoff_tree_digest) = handoff_settled_fingerprint(&handoff, now_ns)?;
    let (git_head, git_idx_mtime) = git_head_and_index_mtime(&roots.project);
    if git_idx_mtime > 0 && !settled_mtime(git_idx_mtime, now_ns) {
        return None;
    }
    let config_stamp =
        file_stamp_if_present(&roots.brain_root().join("config/runtime.json"), now_ns)?;
    let transcript_stamp = transcript_stamp(payload, Some(now_ns))?;
    let owner = std::env::var("PROJECT_BRAIN_OWNER").unwrap_or_else(|_| "local".into());
    let env_digest = context_graph_core::query::sha256(
        format!(
            "{}:{}:{}",
            std::env::var("CONTEXT_CONTINUITY_DISABLED").unwrap_or_default(),
            std::env::var("CONTEXT_CONTINUITY_CAPTURE_DISABLED").unwrap_or_default(),
            std::env::var("CONTEXT_CONTINUITY_RESTORE_DISABLED").unwrap_or_default(),
        )
        .as_bytes(),
    );

    Some(ContinuityPreflightRecord {
        host: host.to_string(),
        owner,
        env_digest,
        git_head,
        config_stamp,
        handoff_tree_digest,
        handoff_max_mtime_ns,
        transcript_stamp,
        payload_sha256: context_graph_core::query::sha256(raw),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn continuity_preflight_matches(
    cached: &ContinuityPreflightRecord,
    current: &ContinuityPreflightRecord,
) -> bool {
    cached.host == current.host
        && cached.owner == current.owner
        && cached.env_digest == current.env_digest
        && cached.git_head == current.git_head
        && cached.config_stamp == current.config_stamp
        && cached.handoff_tree_digest == current.handoff_tree_digest
        && cached.handoff_max_mtime_ns == current.handoff_max_mtime_ns
        && cached.transcript_stamp == current.transcript_stamp
        && cached.payload_sha256 == current.payload_sha256
}

fn working_memory_read(
    roots: &Roots,
    task: Option<&str>,
    host: &str,
    payload: &Value,
    raw: &[u8],
) -> Result<HookOutput> {
    if std::env::var("CONTEXT_CAPSULE_DELIVERED").as_deref() == Ok("1") {
        return Ok(passed(host, "working-memory-read"));
    }
    let mut query = payload["prompt"].as_str().unwrap_or("").to_string();
    if query.chars().count() > 24000 {
        let head: String = query.chars().take(16000).collect();
        let tail: String = query
            .chars()
            .rev()
            .take(8000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        query = format!("{head}\n...\n{tail}");
    }
    if query.starts_with("INTERNAL_NATIVE_MEMORY_RESULT ") {
        return Ok(passed(host, "working-memory-read"));
    }

    let now_ns = now_unix_ns();
    let preflight = refresh_preflight_snapshot(roots, task, host, payload, &query, now_ns);
    if let Some(current) = &preflight
        && let Some(cached) = load_json_cache::<RefreshPreflightRecord>(roots, HOOK_PREFLIGHT_CACHE_FILE)
        && refresh_preflight_matches(&cached, current)
    {
        return Ok(HookOutput {
            code: 0,
            stdout: cached.stdout,
            stderr: Vec::new(),
        });
    }

    let budget = budget_from_env();
    let native_handle = task.map(|t| {
        let roots = roots.clone();
        let host_str = host.to_string();
        let t = t.to_string();
        let raw_bytes = raw.to_vec();
        std::thread::spawn(move || {
            invoke_self_memory(
                &roots,
                "native-prompt",
                &["--host".into(), host_str, "--task-id".into(), t],
                &raw_bytes,
                budget,
            )
        })
    });

    let mut args = vec!["--host".into(), host.into()];
    if host == "cursor" {
        args.extend([
            "--delivery-profile".into(),
            "cursor-rule".into(),
            "--json".into(),
        ]);
    }
    if !query.is_empty()
        && let Some(task_id) = task
    {
        args.extend([
            "--query".into(),
            query,
            "--task-id".into(),
            task_id.into(),
            "--ephemeral".into(),
            "--sanitize".into(),
        ]);
        if let Some(session) = payload["session_id"]
            .as_str()
            .filter(|id| session_id_re().is_match(id))
        {
            args.extend(["--session-id".into(), session.into()]);
            if let Some(transcript) = valid_abs_path(payload["transcript_path"].as_str(), 4096) {
                args.extend([
                    "--transcript".into(),
                    transcript.to_string_lossy().into_owned(),
                ]);
            }
        }
    }

    let result = invoke_self_memory(roots, "refresh", &args, &[], budget);
    let mut prompt_notice = String::new();
    let mut all_ok = true;
    if let Some(handle) = native_handle
        && let Ok(Ok(out)) = handle.join()
        && out.code == 0
        && String::from_utf8_lossy(&out.stdout).trim()
            == "Previous native memory result was not saved completely."
    {
        prompt_notice = "Previous native memory result was not saved completely.\n".into();
        all_ok = false;
    }

    if host == "cursor" {
        return Ok(passed(host, "working-memory-read"));
    }

    let mut rendered = prompt_notice;
    match result {
        Ok(output) if output.code == 0 && !output.stdout.is_empty() => {
            rendered.push_str(&String::from_utf8_lossy(&output.stdout));
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
        }
        Ok(output) if output.code != 0 => {
            all_ok = false;
            let detail = String::from_utf8_lossy(&output.stderr)
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .chars()
                .take(160)
                .collect::<String>();
            if !detail.is_empty() {
                rendered.push_str(&format!(
                    "Memory refresh unavailable ({detail}). Working memory was NOT consulted this turn.\n"
                ));
            }
        }
        Err(err) if err.code.as_deref() == Some("native-timeout") => {
            all_ok = false;
            rendered.push_str("Memory refresh exceeded hook budget. Read canonical sources directly.\n");
        }
        _ => {
            all_ok = false;
        }
    }

    let output = HookOutput::text(rendered);
    if all_ok && let Some(mut record) = preflight {
        record.stdout = output.stdout.clone();
        save_json_cache(roots, HOOK_PREFLIGHT_CACHE_FILE, &record);
    }
    Ok(output)
}

fn working_memory_write(
    roots: &Roots,
    task: Option<&str>,
    host: &str,
    payload: &Value,
    raw: &[u8],
) -> Result<HookOutput> {
    let Some(task_id) = task else {
        return Ok(HookOutput::default());
    };
    let budget = budget_from_env();
    let recovery = if host == "cursor" {
        payload["loop_count"].as_i64().is_some_and(|c| c > 0)
    } else {
        payload["stop_hook_active"] == true
    };
    let flush = std::env::var("CONTEXT_FLUSH_AFTER").unwrap_or_else(|_| "5".into());
    let delivered = std::env::var("CONTEXT_CAPSULE_DELIVERED").as_deref() == Ok("1");
    let now_ns = now_unix_ns();
    let preflight = write_preflight_snapshot(
        roots,
        &WritePreflightArgs {
            task: Some(task_id),
            host,
            payload,
            raw,
            recovery,
            delivered,
            flush_after: &flush,
            now_ns,
        },
    );
    if let Some(current) = &preflight
        && let Some(cached) =
            load_json_cache::<WritePreflightRecord>(roots, HOOK_WRITE_PREFLIGHT_CACHE_FILE)
        && write_preflight_matches(&cached, current)
    {
        return Ok(HookOutput {
            code: 0,
            stdout: cached.stdout,
            stderr: cached.stderr,
        });
    }

    let mut all_ok = true;
    if !recovery {
        let mut args = vec![
            "--task-id".into(),
            task_id.into(),
            "--flush-after".into(),
            flush,
        ];
        if let Some(transcript) =
            valid_abs_path(payload["transcript_path"].as_str(), 4096).filter(|p| p.is_file())
        {
            args.extend([
                "--transcript".into(),
                transcript.to_string_lossy().into_owned(),
            ]);
        }
        if !invoke_self_memory(roots, "turn", &args, &[], budget).is_ok_and(|o| o.code == 0) {
            all_ok = false;
        }
    }
    if delivered {
        if all_ok && let Some(record) = preflight {
            save_json_cache(roots, HOOK_WRITE_PREFLIGHT_CACHE_FILE, &record);
        }
        return Ok(HookOutput::default());
    }
    let out = invoke_self_memory(
        roots,
        "native-stop",
        &["--host".into(), host.into(), "--task-id".into(), task_id.into()],
        raw,
        budget,
    );
    let response = match out {
        Ok(res) if res.code == 0 => HookOutput {
            code: 0,
            stdout: res.stdout,
            stderr: Vec::new(),
        },
        _ => {
            all_ok = false;
            HookOutput::default()
        }
    };
    if all_ok && let Some(mut record) = preflight {
        record.stdout = response.stdout.clone();
        record.stderr = response.stderr.clone();
        save_json_cache(roots, HOOK_WRITE_PREFLIGHT_CACHE_FILE, &record);
    }
    Ok(response)
}

fn local_context(roots: &Roots, task: Option<&str>) -> Result<HookOutput> {
    let config = RuntimeConfig::load(roots)?;
    let mode = config.mode(None)?;
    let actor = std::env::var("PROJECT_BRAIN_OWNER").unwrap_or_else(|_| "local".into());
    let context = RuntimeContext::new(roots.clone(), actor, mode.clone())?;

    let mut lines = vec!["Context Graph ready.".to_string()];
    if let Ok(Some(frame)) = GitFrame::discover(&roots.project)
        && let Ok(out) = context_graph_core::git::run(
            &frame.top,
            &context_graph_core::git::strings(&["branch", "--show-current"]),
            None,
            Duration::from_secs(2),
            16384,
        )
        && out.code == 0
    {
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !branch.is_empty() {
            lines.push(format!(
                "Branch: {branch}{}",
                task.map(|t| format!(" (task: {t})")).unwrap_or_default()
            ));
        }
    }

    let db_path = storage::default_database(roots)?;
    if db_path.is_file() {
        if let Ok(store) = Store::open(&db_path, OpenOptions::default()) {
            let docs: i64 = store
                .conn
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap_or(0);
            let gstats = graph::stats(&store.conn).unwrap_or_else(|_| object!({}));
            let nodes = gstats["nodes"].as_u64().unwrap_or(0);
            let edges = gstats["edges"].as_u64().unwrap_or(0);
            lines.push(format!(
                "Context governance: mode={mode}, indexed_docs={docs}, graph={nodes} nodes / {edges} edges."
            ));
        }
    } else {
        lines.push(format!(
            "Context governance: mode={mode}, index=not yet built (run context-graph memory index)."
        ));
    }

    if roots.state.join("memory-bank/INDEX.md").is_file() {
        let svc = BankService::new(context);
        let chunks = context_graph_core::bank::codec::paths(&svc)
            .map(|p| p.len())
            .unwrap_or(0);
        lines.push(format!("Memory Bank: {chunks} chunk(s) in memory-bank/INDEX.md."));
    }

    let handoff_dir = roots.state.join(".context-handoff");
    if fs::admit_directory(&handoff_dir).unwrap_or(false)
        && let Ok(entries) = std::fs::read_dir(&handoff_dir)
    {
        let handoffs = entries
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .count();
        if handoffs > 0 {
            lines.push(format!("Portable handoffs: {handoffs} recorded in .context-handoff/."));
        }
    }

    Ok(HookOutput::text(format!("{}\n", lines.join("\n"))))
}

fn context_continuity(roots: &Roots, host: &str, payload: &Value, raw: &[u8]) -> Result<HookOutput> {
    let now_ns = now_unix_ns();
    let preflight = continuity_preflight_snapshot(roots, host, payload, raw, now_ns);
    if let Some(current) = &preflight
        && let Some(cached) =
            load_json_cache::<ContinuityPreflightRecord>(roots, HOOK_CONTINUITY_PREFLIGHT_CACHE_FILE)
        && continuity_preflight_matches(&cached, current)
    {
        return Ok(HookOutput {
            code: 0,
            stdout: cached.stdout,
            stderr: cached.stderr,
        });
    }

    let config = RuntimeConfig::load(roots)?;
    let mode = config.mode(None)?;
    let actor = std::env::var("PROJECT_BRAIN_OWNER").unwrap_or_else(|_| "local".into());
    let ctx = RuntimeContext::new(roots.clone(), actor, mode)?;
    let value = context_graph_core::chat_continuity::execute(
        &ctx,
        host,
        "hook",
        raw,
        &object!({}),
    )?;
    let out = HookOutput::envelope(value)?;
    if out.code == 0 && let Some(mut record) = preflight {
        record.stdout = out.stdout.clone();
        record.stderr = out.stderr.clone();
        save_json_cache(roots, HOOK_CONTINUITY_PREFLIGHT_CACHE_FILE, &record);
    }
    Ok(out)
}

pub fn execute(root: &Path, host: &str, role: &str, raw: &[u8]) -> Result<HookOutput> {
    if !["claude", "codex", "cursor"].contains(&host) {
        return Err(RuntimeError::invalid("Unsupported hook host"));
    }
    if !ROLES.contains(&role) {
        return Err(RuntimeError::invalid(format!("Unknown hook role: {role}")));
    }
    if std::env::var("CONTEXT_MEMORY_RECOVERY").as_deref() == Ok("1") {
        return Ok(passed(host, role));
    }
    let payload = parse_payload(raw);
    let resolved = match resolve_hook_roots(root, &payload) {
        Ok(Some(v)) => v,
        _ => return Ok(passed(host, role)),
    };
    let (roots, task) = resolved;
    let res = match role {
        "working-memory-read" => working_memory_read(&roots, task.as_deref(), host, &payload, raw),
        "working-memory-write" => working_memory_write(&roots, task.as_deref(), host, &payload, raw),
        "local-context" => local_context(&roots, task.as_deref()),
        "context-continuity" => context_continuity(&roots, host, &payload, raw),
        _ => Ok(passed(host, role)),
    };
    // Memory hooks never refuse/block the host turn.
    Ok(res.unwrap_or_else(|_| passed(host, role)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_graph_core::{domain::uuid4, index::refresh_index};

    #[test]
    fn refresh_preflight_respects_racy_ns_and_invalidates_on_graph_ir_epoch() {
        let root = std::env::temp_dir().join(format!("cg-hooks-preflight-{}", uuid4()));
        std::fs::create_dir_all(root.join("memory-bank/local")).unwrap();
        std::fs::create_dir_all(root.join("memory-bank/chunks")).unwrap();
        std::fs::create_dir_all(root.join("project-brain")).unwrap();

        let roots = Roots::installed(&root).unwrap();
        let ctx = RuntimeContext::new(roots.clone(), "local", "governed").unwrap();
        let db = storage::default_database(&roots).unwrap();
        let mut store = Store::open(&db, OpenOptions::default()).unwrap();
        refresh_index(&mut store, &ctx, false).unwrap();

        let ir_db = roots.state.join("memory-bank/local/graph_ir.sqlite3");
        {
            let conn = Store::open(&ir_db, OpenOptions::default()).unwrap();
            conn.conn
                .execute_batch(
                    "CREATE TABLE ir_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO ir_meta(key, value) VALUES('write_epoch', '10');",
                )
                .unwrap();
        }

        let payload = object!({"prompt": "check billing"});
        // Racy timestamp right at creation -> skipped
        let now_racy = now_unix_ns();
        assert!(refresh_preflight_snapshot(&roots, Some("TASK-1"), "claude", &payload, "check billing", now_racy).is_none());

        // Settled timestamp (>2s past racy boundary) -> captured with write_epoch=10
        let now_settled = now_racy + HOOK_PREFLIGHT_RACY_NS + 1_000_000;
        let rec1 = refresh_preflight_snapshot(&roots, Some("TASK-1"), "claude", &payload, "check billing", now_settled).unwrap();
        assert!(refresh_preflight_matches(&rec1, &rec1));

        // Bump IR epoch -> invalidates preflight match
        {
            let conn = Store::open(&ir_db, OpenOptions::default()).unwrap();
            conn.conn
                .execute("UPDATE ir_meta SET value='11' WHERE key='write_epoch'", [])
                .unwrap();
        }
        let rec2 = refresh_preflight_snapshot(&roots, Some("TASK-1"), "claude", &payload, "check billing", now_settled).unwrap();
        assert!(!refresh_preflight_matches(&rec1, &rec2));

        let _ = std::fs::remove_dir_all(&root);
    }
}
