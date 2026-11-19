//! Unified Multi-Relation Context Graph across Project Brain, Memory Bank,
//! working tasks, portable handoffs, session merges, and repository files.
use crate::{
    BrainRepository, Result, Roots, RuntimeContext, RuntimeError,
    bank::codec,
    domain::{self, json},
    fs,
    storage::Store,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as object};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

pub const EDGE_KINDS: &[&str] = &[
    "cites",
    "touches",
    "conflicts",
    "supersedes",
    "promoted_from",
    "consolidated_from",
    "handoff_of",
    "session_merge",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Out,
    In,
    Both,
}

impl Direction {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "both" => Ok(Self::Both),
            "out" | "forward" | "dependencies" => Ok(Self::Out),
            "in" | "reverse" | "impact" | "dependents" => Ok(Self::In),
            other => Err(RuntimeError::invalid(format!(
                "Invalid graph traversal direction: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub title: String,
    pub layer: String,
    pub privacy: String,
    pub authority: String,
    pub lifecycle: String,
    pub freshness: String,
    pub updated_at: String,
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphEdge {
    pub src: String,
    pub dst: String,
    pub kind: String,
    pub weight: f64,
    pub metadata: Value,
}

#[derive(Clone, Debug)]
pub struct GraphQuery {
    pub root: Option<String>,
    pub max_depth: usize,
    pub kinds: Vec<String>,
    pub direction: Direction,
    pub include_archived: bool,
    pub limit: usize,
}

impl Default for GraphQuery {
    fn default() -> Self {
        Self {
            root: None,
            max_depth: 2,
            kinds: Vec::new(),
            direction: Direction::Both,
            include_archived: false,
            limit: 200,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphQueryResult {
    pub root: Option<String>,
    pub resolved_root: Option<String>,
    pub direction: String,
    pub max_depth: usize,
    pub include_archived: bool,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub provenance_paths: Vec<Vec<String>>,
    pub summary: Value,
}

#[derive(Default)]
struct GraphBuilder {
    nodes: BTreeMap<String, GraphNode>,
    edges: BTreeMap<(String, String, String), GraphEdge>,
    path_owner: BTreeMap<String, String>,
    external_owner: BTreeMap<String, String>,
}

impl GraphBuilder {
    fn upsert_node(&mut self, node: GraphNode) {
        if !node.path.is_empty() {
            self.path_owner
                .entry(node.path.clone())
                .or_insert_with(|| node.id.clone());
        }
        if let Some(ext) = node.metadata.get("external_id").and_then(Value::as_str)
            && !ext.is_empty()
        {
            self.external_owner
                .entry(ext.to_string())
                .or_insert_with(|| node.id.clone());
        }
        if let Some(existing) = self.nodes.get_mut(&node.id) {
            if existing.kind == "reference" && node.kind != "reference" {
                *existing = node;
            } else {
                if existing.title.is_empty() && !node.title.is_empty() {
                    existing.title = node.title;
                }
                if existing.path.is_empty() && !node.path.is_empty() {
                    existing.path = node.path;
                }
                if existing.freshness == "unknown" && node.freshness != "unknown" {
                    existing.freshness = node.freshness;
                }
            }
            return;
        }
        self.nodes.insert(node.id.clone(), node);
    }

    fn ensure_file_node(&mut self, project: &Path, rel_path: &str, stale_override: bool) -> String {
        let clean = normalize_rel(rel_path);
        let id = format!("file:{clean}");
        let disk_path = project.join(&clean);
        let on_disk = fs::admit_descendant(project, &disk_path).is_ok()
            && std::fs::symlink_metadata(&disk_path)
                .map(|m| m.is_file() && !fs::linked(&m))
                .unwrap_or(false);
        let freshness = if !on_disk {
            "missing"
        } else if stale_override {
            "stale"
        } else {
            "current"
        };
        if let Some(existing) = self.nodes.get_mut(&id) {
            if existing.freshness == "current" && freshness != "current" {
                existing.freshness = freshness.into();
            }
            return id;
        }
        self.upsert_node(GraphNode {
            id: id.clone(),
            kind: "file".into(),
            path: clean.clone(),
            title: clean,
            layer: "codebase".into(),
            privacy: "team".into(),
            authority: "observed".into(),
            lifecycle: if on_disk { "active" } else { "missing" }.into(),
            freshness: freshness.into(),
            updated_at: String::new(),
            metadata: object!({}),
        });
        id
    }

    fn ensure_stub_node(&mut self, id: &str, kind_hint: &str) {
        if self.nodes.contains_key(id) {
            return;
        }
        self.upsert_node(GraphNode {
            id: id.to_string(),
            kind: kind_hint.to_string(),
            path: String::new(),
            title: id.to_string(),
            layer: match kind_hint {
                "memory-chunk" => "semantic",
                "handoff" | "session" | "working-task" => "episodic",
                "file" => "codebase",
                _ => "procedural",
            }
            .into(),
            privacy: "team".into(),
            authority: "observed".into(),
            lifecycle: "referenced".into(),
            freshness: "unknown".into(),
            updated_at: String::new(),
            metadata: object!({"stub": true}),
        });
    }

    fn add_edge(&mut self, src: String, dst: String, kind: &str, weight: f64, metadata: Value) {
        if src.is_empty() || dst.is_empty() || src == dst {
            return;
        }
        let key = (src.clone(), dst.clone(), kind.to_string());
        self.edges.entry(key).or_insert(GraphEdge {
            src,
            dst,
            kind: kind.to_string(),
            weight,
            metadata,
        });
    }
}

fn normalize_rel(path: &str) -> String {
    let trimmed = path.trim().replace('\\', "/");
    trimmed.strip_prefix("./").unwrap_or(&trimmed).to_string()
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                vec![]
            } else {
                vec![trimmed.into()]
            }
        }
        _ => vec![],
    }
}

fn event_uuid_mentions(text: &str) -> Vec<String> {
    let Ok(pattern) = regex::Regex::new(r"\[event\s+([0-9a-fA-F-]{36})\]") else {
        return vec![];
    };
    pattern
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .filter(|id| domain::is_uuid4(id))
        .collect()
}

fn citation_freshness(roots: &Roots, owner_value: &Value, sources: &[String]) -> (String, BTreeSet<String>) {
    if sources.is_empty() {
        return ("unknown".into(), BTreeSet::new());
    }
    let mut stale_paths = BTreeSet::new();
    if let Ok((changed, missing)) = domain::source_changes(roots, &owner_value["source_fingerprints"]) {
        for item in changed.into_iter().chain(missing) {
            stale_paths.insert(normalize_rel(&item));
        }
    }
    let project = &roots.project;
    let mut any_missing = false;
    for source in sources {
        let rel = normalize_rel(source);
        let full = project.join(&rel);
        if fs::admit_descendant(project, &full).is_err()
            || !std::fs::symlink_metadata(&full)
                .map(|m| m.is_file() && !fs::linked(&m))
                .unwrap_or(false)
        {
            any_missing = true;
            stale_paths.insert(rel);
        }
    }
    let overall = if any_missing {
        "missing"
    } else if !stale_paths.is_empty() {
        "stale"
    } else {
        "current"
    };
    (overall.into(), stale_paths)
}

fn rel_from_root(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|p| p.to_str())
        .map(|s| s.replace('\\', "/"))
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
}

fn resolve_target_id(hint: &str, path_owner: &BTreeMap<String, String>, external_owner: &BTreeMap<String, String>) -> String {
    let h = hint.trim();
    if h.starts_with("brain:")
        || h.starts_with("bank:")
        || h.starts_with("file:")
        || h.starts_with("task:")
        || h.starts_with("handoff:")
        || h.starts_with("session:")
    {
        return h.into();
    }
    if domain::is_uuid4(h) {
        return format!("brain:{h}");
    }
    if codec::id_pattern().is_ok_and(|re| re.is_match(h)) {
        return format!("bank:{h}");
    }
    if let Some(id) = external_owner.get(h) {
        return id.clone();
    }
    if let Some(id) = path_owner.get(&normalize_rel(h)) {
        return id.clone();
    }
    format!("task:{h}")
}

fn collect_handoff_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !fs::admit_directory(dir)? {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(dir)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if fs::linked(&meta) {
            continue;
        }
        if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Rebuild the `graph_nodes` and `graph_edges` tables inside an active write
/// transaction over `conn`.
pub fn rebuild_in_transaction(conn: &Connection, context: &RuntimeContext) -> Result<()> {
    let mut builder = GraphBuilder::default();
    let project = &context.roots.project;
    let state_root = &context.roots.state;

    // 1. Project Brain records (active + archive).
    if let Ok(brain) = BrainRepository::new(context.clone())
        && let Ok(inventory) = brain.inventory(true)
    {
        for image in inventory.records {
            let record = &image.record;
            let uuid = record["id"].as_str().unwrap_or("").trim();
            let rel = rel_from_root(state_root, &image.path);
            let node_id = if !uuid.is_empty() {
                format!("brain:{uuid}")
            } else {
                format!("brain:{rel}")
            };
            let kind = record["type"].as_str().unwrap_or("record").to_string();
            let sources = string_list(record, "sources");
            let (freshness, stale_set) = citation_freshness(&context.roots, record, &sources);
            let status = if image.archived {
                "archived".to_string()
            } else {
                record["status"].as_str().unwrap_or("active").to_string()
            };
            let mut meta = object!({
                "external_id": record["external_id"],
                "status": record["status"],
                "archived": image.archived,
                "owner": record["owner"],
                "revision": record["revision"],
                "sources": sources,
            });
            if let Some(ext) = record["external_id"].as_str()
                && !ext.is_empty()
            {
                builder
                    .external_owner
                    .entry(ext.to_string())
                    .or_insert_with(|| node_id.clone());
            }
            if !record["conflicts"].is_null() {
                meta["conflicts"] = record["conflicts"].clone();
            }
            builder.upsert_node(GraphNode {
                id: node_id.clone(),
                kind: kind.clone(),
                path: rel,
                title: record["title"].as_str().unwrap_or("").into(),
                layer: if kind == "handoff" {
                    "episodic"
                } else {
                    "procedural"
                }
                .into(),
                privacy: record["privacy"].as_str().unwrap_or("team").into(),
                authority: record["authority"].as_str().unwrap_or("observed").into(),
                lifecycle: status,
                freshness,
                updated_at: record["updated_at"].as_str().unwrap_or("").into(),
                metadata: meta,
            });

            for src in &sources {
                let norm = normalize_rel(src);
                let stale = stale_set.contains(&norm);
                let file_id = builder.ensure_file_node(project, &norm, stale);
                builder.add_edge(
                    node_id.clone(),
                    file_id,
                    "cites",
                    1.0,
                    object!({"freshness": if stale { "stale_or_missing" } else { "current" }}),
                );
            }
            for file in string_list(record, "files") {
                let norm = normalize_rel(&file);
                let file_id = builder.ensure_file_node(project, &norm, false);
                builder.add_edge(node_id.clone(), file_id, "touches", 0.9, object!({}));
            }
            for conflict in string_list(record, "conflicts") {
                let target = if domain::is_uuid4(&conflict) {
                    format!("brain:{conflict}")
                } else {
                    conflict
                };
                builder.ensure_stub_node(&target, "record");
                builder.add_edge(node_id.clone(), target, "conflicts", 0.9, object!({}));
            }
            for old in string_list(record, "replaces")
                .into_iter()
                .chain(string_list(record, "supersedes"))
            {
                let target = if domain::is_uuid4(&old) {
                    format!("brain:{old}")
                } else {
                    old
                };
                builder.ensure_stub_node(&target, "record");
                builder.add_edge(node_id.clone(), target, "supersedes", 1.0, object!({}));
            }
            for new_owner in string_list(record, "superseded_by") {
                let new_id = if domain::is_uuid4(&new_owner) {
                    format!("brain:{new_owner}")
                } else {
                    new_owner
                };
                builder.ensure_stub_node(&new_id, "record");
                builder.add_edge(new_id, node_id.clone(), "supersedes", 1.0, object!({}));
            }
            let mut event_refs = string_list(record, "source_events");
            event_refs.extend(event_uuid_mentions(&image.body));
            for event_uuid in event_refs {
                let target = if domain::is_uuid4(&event_uuid) {
                    format!("brain:{event_uuid}")
                } else {
                    event_uuid
                };
                builder.ensure_stub_node(&target, "event");
                builder.add_edge(
                    node_id.clone(),
                    target,
                    "consolidated_from",
                    0.95,
                    object!({}),
                );
            }
            if kind == "handoff" {
                for task_ref in string_list(record, "task_id")
                    .into_iter()
                    .chain(string_list(record, "external_id"))
                {
                    let target = resolve_target_id(
                        &task_ref,
                        &builder.path_owner,
                        &builder.external_owner,
                    );
                    if target != node_id {
                        builder.ensure_stub_node(&target, "task");
                        builder.add_edge(node_id.clone(), target, "handoff_of", 1.0, object!({}));
                    }
                }
            }
            for src_session in string_list(record, "merged_from")
                .into_iter()
                .chain(string_list(record, "source_sessions"))
            {
                let target = if src_session.contains(':') {
                    src_session
                } else {
                    format!("session:{src_session}")
                };
                builder.ensure_stub_node(&target, "session");
                builder.add_edge(
                    node_id.clone(),
                    target,
                    "session_merge",
                    0.85,
                    object!({}),
                );
            }
        }
    }

    // 2. Memory Bank chunks.
    let chunks_dir = state_root.join("memory-bank/chunks");
    if fs::admit_directory(&chunks_dir).unwrap_or(false)
        && let Ok(mut files) = crate::bank::publication::PublicationFiles::new(state_root)
        && let Ok(entries) = std::fs::read_dir(&chunks_dir)
    {
        let mut names = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.ends_with(".md"))
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            let rel = format!("memory-bank/chunks/{name}");
            let Ok(chunk) = codec::read(&mut files, &rel) else {
                continue;
            };
            let mem_id = chunk.metadata["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&name);
            let node_id = format!("bank:{mem_id}");
            let sources = string_list(&chunk.metadata, "sources");
            let (freshness, stale_set) = citation_freshness(&context.roots, &chunk.metadata, &sources);
            let status = chunk.metadata["status"]
                .as_str()
                .unwrap_or("active")
                .to_string();
            builder.upsert_node(GraphNode {
                id: node_id.clone(),
                kind: "memory-chunk".into(),
                path: rel,
                title: chunk.metadata["title"].as_str().unwrap_or("").into(),
                layer: "semantic".into(),
                privacy: "team".into(),
                authority: "verified".into(),
                lifecycle: status,
                freshness,
                updated_at: chunk.metadata["last_verified"].as_str().unwrap_or("").into(),
                metadata: object!({
                    "chunk_id": mem_id,
                    "chunk_type": chunk.metadata["type"],
                    "tags": chunk.metadata.get("tags").cloned().unwrap_or(object!([])),
                    "sources": sources,
                }),
            });

            for src in &sources {
                let norm = normalize_rel(src);
                let stale = stale_set.contains(&norm);
                let file_id = builder.ensure_file_node(project, &norm, stale);
                builder.add_edge(
                    node_id.clone(),
                    file_id,
                    "cites",
                    1.0,
                    object!({"freshness": if stale { "stale_or_missing" } else { "current" }}),
                );
            }
            for old in string_list(&chunk.metadata, "supersedes") {
                let target = if old.starts_with("bank:") {
                    old
                } else {
                    format!("bank:{old}")
                };
                builder.ensure_stub_node(&target, "memory-chunk");
                builder.add_edge(node_id.clone(), target, "supersedes", 1.0, object!({}));
            }
            for new_id in string_list(&chunk.metadata, "superseded_by") {
                let src = if new_id.starts_with("bank:") {
                    new_id
                } else {
                    format!("bank:{new_id}")
                };
                builder.ensure_stub_node(&src, "memory-chunk");
                builder.add_edge(src, node_id.clone(), "supersedes", 1.0, object!({}));
            }
            for promo in string_list(&chunk.metadata, "promoted_from")
                .into_iter()
                .chain(string_list(&chunk.metadata, "source_records"))
            {
                let target = if domain::is_uuid4(&promo) {
                    format!("brain:{promo}")
                } else {
                    promo
                };
                builder.ensure_stub_node(&target, "record");
                builder.add_edge(
                    node_id.clone(),
                    target,
                    "promoted_from",
                    1.0,
                    object!({}),
                );
            }
            for conflict in string_list(&chunk.metadata, "conflicts") {
                let target = resolve_target_id(&conflict, &builder.path_owner, &builder.external_owner);
                builder.ensure_stub_node(&target, "record");
                builder.add_edge(node_id.clone(), target, "conflicts", 0.9, object!({}));
            }
        }
    }

    // 3. Promotion proposals (project-brain/control/promotions/*.json).
    let promotions_dir = state_root.join("project-brain/control/promotions");
    if fs::admit_directory(&promotions_dir).unwrap_or(false)
        && let Ok(entries) = std::fs::read_dir(&promotions_dir)
    {
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "json") {
                continue;
            }
            let Ok(raw) = fs::read_regular(&path) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&raw) else {
                continue;
            };
            let Ok(proposal) = json::parse(text) else {
                continue;
            };
            let Some(dest_id) = proposal["destination_memory_id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let chunk_node = format!("bank:{dest_id}");
            builder.ensure_stub_node(&chunk_node, "memory-chunk");
            let mut rec_ids = string_list(&proposal, "source_records");
            if let Some(items) = proposal["source_records"].as_array() {
                for item in items {
                    if let Some(id) = item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        rec_ids.push(id.into());
                    }
                }
            }
            for rec in rec_ids {
                let rec_node = if domain::is_uuid4(&rec) {
                    format!("brain:{rec}")
                } else {
                    rec
                };
                builder.ensure_stub_node(&rec_node, "record");
                builder.add_edge(
                    chunk_node.clone(),
                    rec_node,
                    "promoted_from",
                    1.0,
                    object!({
                        "promotion_id": proposal["id"],
                        "status": proposal["status"],
                    }),
                );
            }
        }
    }

    // 4. Working tasks + bindings in SQLite.
    let mut bindings = BTreeMap::<String, String>::new();
    {
        let mut stmt = conn.prepare("SELECT external_id,task_uuid FROM task_bindings")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (ext, uuid) = row?;
            bindings.insert(ext.clone(), uuid.clone());
            builder
                .external_owner
                .entry(ext)
                .or_insert_with(|| format!("brain:{uuid}"));
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT task_id,goal,files,sources,updated_at FROM working_tasks",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (task_id, goal, files_json, sources_json, updated_at) in rows {
            let node_id = if let Some(uuid) = bindings.get(&task_id) {
                format!("brain:{uuid}")
            } else {
                let id = format!("task:{task_id}");
                builder.upsert_node(GraphNode {
                    id: id.clone(),
                    kind: "working-task".into(),
                    path: String::new(),
                    title: goal,
                    layer: "episodic".into(),
                    privacy: "team".into(),
                    authority: "observed".into(),
                    lifecycle: "active".into(),
                    freshness: "current".into(),
                    updated_at,
                    metadata: object!({"external_id": task_id}),
                });
                id
            };
            if let Ok(files) = serde_json::from_str::<Vec<String>>(&files_json) {
                for f in files {
                    let norm = normalize_rel(&f);
                    let fid = builder.ensure_file_node(project, &norm, false);
                    builder.add_edge(node_id.clone(), fid, "touches", 0.9, object!({}));
                }
            }
            if let Ok(sources) = serde_json::from_str::<Vec<String>>(&sources_json) {
                for s in sources {
                    let norm = normalize_rel(&s);
                    let fid = builder.ensure_file_node(project, &norm, false);
                    builder.add_edge(node_id.clone(), fid, "cites", 1.0, object!({}));
                }
            }
        }
    }

    // 5. Portable handoffs & session merges (.context-handoff/, .context-handoff/merges/, project-brain/handoffs/).
    let mut handoff_paths = Vec::new();
    collect_handoff_files(&state_root.join(".context-handoff"), &mut handoff_paths)?;
    collect_handoff_files(&state_root.join(".context-handoff/merges"), &mut handoff_paths)?;
    collect_handoff_files(&state_root.join("project-brain/handoffs"), &mut handoff_paths)?;
    for hpath in handoff_paths {
        let rel = rel_from_root(state_root, &hpath);
        let is_merge = rel.contains(".context-handoff/merges/");
        let stem = hpath
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("handoff")
            .to_string();
        if hpath.extension().is_some_and(|e| e == "json") {
            let Ok(raw) = fs::read_regular(&hpath) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&raw) else {
                continue;
            };
            let Ok(val) = json::parse(text) else {
                continue;
            };
            let id = val["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&stem);
            let node_id = if is_merge {
                format!("session:{id}")
            } else {
                format!("handoff:{id}")
            };
            builder.upsert_node(GraphNode {
                id: node_id.clone(),
                kind: if is_merge { "session-merge" } else { "handoff" }.into(),
                path: rel,
                title: val["title"]
                    .as_str()
                    .or_else(|| val["goal"].as_str())
                    .or_else(|| val["summary"].as_str())
                    .unwrap_or(id)
                    .chars()
                    .take(120)
                    .collect(),
                layer: "episodic".into(),
                privacy: val["privacy"].as_str().unwrap_or("team").into(),
                authority: "observed".into(),
                lifecycle: "active".into(),
                freshness: "current".into(),
                updated_at: val["updated_at"]
                    .as_str()
                    .or_else(|| val["created_at"].as_str())
                    .unwrap_or("")
                    .into(),
                metadata: val.clone(),
            });
            for t in string_list(&val, "task_id") {
                let target = resolve_target_id(&t, &builder.path_owner, &builder.external_owner);
                builder.ensure_stub_node(&target, "task");
                builder.add_edge(node_id.clone(), target, "handoff_of", 1.0, object!({}));
            }
            for s in string_list(&val, "merged_from")
                .into_iter()
                .chain(string_list(&val, "source_sessions"))
                .chain(string_list(&val, "source_tasks"))
                .chain(string_list(&val, "source_handoffs"))
            {
                let target = resolve_target_id(&s, &builder.path_owner, &builder.external_owner);
                let normalized_target = if target.starts_with("task:") && !bindings.contains_key(&s) && !builder.nodes.contains_key(&target) {
                    format!("session:{s}")
                } else {
                    target
                };
                builder.ensure_stub_node(&normalized_target, "session");
                builder.add_edge(
                    node_id.clone(),
                    normalized_target,
                    "session_merge",
                    0.85,
                    object!({}),
                );
            }
            for f in string_list(&val, "files") {
                let fid = builder.ensure_file_node(project, &normalize_rel(&f), false);
                builder.add_edge(node_id.clone(), fid, "touches", 0.9, object!({}));
            }
            for s in string_list(&val, "sources") {
                let fid = builder.ensure_file_node(project, &normalize_rel(&s), false);
                builder.add_edge(node_id.clone(), fid, "cites", 1.0, object!({}));
            }
        } else if hpath.extension().is_some_and(|e| e == "md") {
            let Ok(raw) = fs::read_regular(&hpath) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&raw) else {
                continue;
            };
            if let Ok((meta, _body)) = domain::parse_markdown_record(text) {
                let id = meta["id"].as_str().unwrap_or(&stem);
                let node_id = format!("handoff:{id}");
                builder.upsert_node(GraphNode {
                    id: node_id.clone(),
                    kind: "handoff".into(),
                    path: rel,
                    title: meta["title"]
                        .as_str()
                        .or_else(|| meta["goal"].as_str())
                        .unwrap_or(id)
                        .into(),
                    layer: "episodic".into(),
                    privacy: meta["privacy"].as_str().unwrap_or("team").into(),
                    authority: "observed".into(),
                    lifecycle: "active".into(),
                    freshness: "current".into(),
                    updated_at: meta["created_at"].as_str().unwrap_or("").into(),
                    metadata: meta.clone(),
                });
                for t in string_list(&meta, "task_id") {
                    let target =
                        resolve_target_id(&t, &builder.path_owner, &builder.external_owner);
                    builder.ensure_stub_node(&target, "task");
                    builder.add_edge(node_id.clone(), target, "handoff_of", 1.0, object!({}));
                }
                for f in string_list(&meta, "files") {
                    let fid = builder.ensure_file_node(project, &normalize_rel(&f), false);
                    builder.add_edge(node_id.clone(), fid, "touches", 0.9, object!({}));
                }
            }
        }
    }

    // 6. Remaining indexed document links (policies, specs, docs) from SQLite.
    {
        let mut stmt = conn.prepare(
            "SELECT d.path,d.layer,d.kind,d.title,m.privacy,m.authority,m.lifecycle,m.updated_at FROM documents d JOIN document_metadata m ON d.path=m.path",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (path, layer, kind, title, privacy, authority, lifecycle, updated_at) in rows {
            let norm = normalize_rel(&path);
            if builder.path_owner.contains_key(&norm) {
                continue;
            }
            let id = format!("doc:{norm}");
            builder.upsert_node(GraphNode {
                id,
                kind,
                path: norm,
                title,
                layer,
                privacy,
                authority,
                lifecycle,
                freshness: "current".into(),
                updated_at,
                metadata: object!({}),
            });
        }
        let mut link_stmt = conn.prepare("SELECT path,ref_path,ref_kind FROM document_links")?;
        let link_rows = link_stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (path, ref_path, ref_kind) in link_rows {
            let src_norm = normalize_rel(&path);
            let Some(src_id) = builder.path_owner.get(&src_norm).cloned() else {
                continue;
            };
            let dst_norm = normalize_rel(&ref_path);
            let edge_kind = match ref_kind.as_str() {
                "file" => "touches",
                _ => "cites",
            };
            let dst_id = builder.ensure_file_node(project, &dst_norm, false);
            builder.add_edge(src_id, dst_id, edge_kind, 1.0, object!({}));
        }
    }

    // Rewrite tables cleanly inside the open SQLite transaction.
    conn.execute("DELETE FROM graph_edges", [])?;
    conn.execute("DELETE FROM graph_nodes", [])?;
    {
        let mut insert_node = conn.prepare(
            "INSERT INTO graph_nodes(id,kind,path,title,layer,privacy,authority,lifecycle,freshness,updated_at,metadata_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        )?;
        for node in builder.nodes.values() {
            insert_node.execute(params![
                node.id,
                node.kind,
                node.path,
                node.title,
                node.layer,
                node.privacy,
                node.authority,
                node.lifecycle,
                node.freshness,
                node.updated_at,
                serde_json::to_string(&node.metadata)?,
            ])?;
        }
    }
    {
        let mut insert_edge = conn.prepare(
            "INSERT OR REPLACE INTO graph_edges(src,dst,kind,weight,metadata_json) VALUES(?1,?2,?3,?4,?5)",
        )?;
        for edge in builder.edges.values() {
            insert_edge.execute(params![
                edge.src,
                edge.dst,
                edge.kind,
                edge.weight,
                serde_json::to_string(&edge.metadata)?,
            ])?;
        }
    }
    Ok(())
}

/// Explicitly synchronize `graph_nodes` and `graph_edges` from workspace state.
pub fn sync_graph(store: &mut Store, context: &RuntimeContext) -> Result<Value> {
    if store.read_only {
        return Err(RuntimeError::unavailable("SQLite writes unavailable"));
    }
    store.transaction(|conn| rebuild_in_transaction(conn, context))?;
    stats(&store.conn)
}

pub fn ensure_populated(store: &mut Store, context: &RuntimeContext) -> Result<()> {
    let nodes: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM graph_nodes", [], |r| r.get(0))?;
    if nodes == 0 {
        let docs: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
        let has_brain = context.roots.brain_root().join("dynamic").is_dir();
        let has_bank = context.roots.state.join("memory-bank/chunks").is_dir();
        if docs > 0 || has_brain || has_bank {
            sync_graph(store, context)?;
        }
    }
    Ok(())
}

pub fn stats(conn: &Connection) -> Result<Value> {
    let nodes: usize = conn.query_row("SELECT COUNT(*) FROM graph_nodes", [], |r| r.get(0))?;
    let edges: usize = conn.query_row("SELECT COUNT(*) FROM graph_edges", [], |r| r.get(0))?;
    let mut by_node_kind = BTreeMap::<String, usize>::new();
    let mut stmt = conn.prepare("SELECT kind, COUNT(*) FROM graph_nodes GROUP BY kind")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?)))? {
        let (kind, count) = row?;
        by_node_kind.insert(kind, count);
    }
    let mut by_edge_kind = BTreeMap::<String, usize>::new();
    let mut stmt = conn.prepare("SELECT kind, COUNT(*) FROM graph_edges GROUP BY kind")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?)))? {
        let (kind, count) = row?;
        by_edge_kind.insert(kind, count);
    }
    Ok(object!({
        "nodes": nodes,
        "edges": edges,
        "node_kinds": by_node_kind,
        "edge_kinds": by_edge_kind,
    }))
}

fn load_all(conn: &Connection) -> Result<(BTreeMap<String, GraphNode>, Vec<GraphEdge>)> {
    let mut nodes = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT id,kind,path,title,layer,privacy,authority,lifecycle,freshness,updated_at,metadata_json FROM graph_nodes ORDER BY id",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
        ))
    })? {
        let (
            id,
            kind,
            path,
            title,
            layer,
            privacy,
            authority,
            lifecycle,
            freshness,
            updated_at,
            metadata_json,
        ) = row?;
        let metadata = serde_json::from_str(&metadata_json).unwrap_or_else(|_| object!({}));
        nodes.insert(
            id.clone(),
            GraphNode {
                id,
                kind,
                path,
                title,
                layer,
                privacy,
                authority,
                lifecycle,
                freshness,
                updated_at,
                metadata,
            },
        );
    }

    let mut edges = Vec::new();
    let mut stmt = conn
        .prepare("SELECT src,dst,kind,weight,metadata_json FROM graph_edges ORDER BY src,dst,kind")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })? {
        let (src, dst, kind, weight, metadata_json) = row?;
        edges.push(GraphEdge {
            src,
            dst,
            kind,
            weight,
            metadata: serde_json::from_str(&metadata_json).unwrap_or_else(|_| object!({})),
        });
    }
    Ok((nodes, edges))
}

pub fn resolve_node_id(nodes: &BTreeMap<String, GraphNode>, query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if nodes.contains_key(q) {
        return Some(q.to_string());
    }
    let clean = normalize_rel(q);
    for candidate in [
        format!("brain:{q}"),
        format!("bank:{q}"),
        format!("file:{clean}"),
        format!("task:{q}"),
        format!("handoff:{q}"),
        format!("session:{q}"),
        format!("doc:{clean}"),
    ] {
        if nodes.contains_key(&candidate) {
            return Some(candidate);
        }
    }
    for (id, node) in nodes {
        if node.path == clean
            || node
                .metadata
                .get("external_id")
                .and_then(Value::as_str)
                .is_some_and(|ext| ext == q)
        {
            return Some(id.clone());
        }
    }
    None
}

fn validate_kinds(kinds: &[String]) -> Result<BTreeSet<String>> {
    let mut set = BTreeSet::new();
    for kind in kinds {
        let k = kind.trim().to_ascii_lowercase();
        if k.is_empty() {
            continue;
        }
        if !EDGE_KINDS.contains(&k.as_str()) {
            return Err(RuntimeError::invalid(format!(
                "Unsupported graph edge kind '{k}'; expected one of: {}",
                EDGE_KINDS.join(", ")
            )));
        }
        set.insert(k);
    }
    Ok(set)
}

pub fn query_graph(conn: &Connection, req: &GraphQuery) -> Result<GraphQueryResult> {
    let allowed_kinds = validate_kinds(&req.kinds)?;
    let (all_nodes, all_edges) = load_all(conn)?;
    let admitted = |node: &GraphNode| -> bool {
        req.include_archived || node.lifecycle != "archived"
    };

    if let Some(root_raw) = &req.root {
        let resolved = resolve_node_id(&all_nodes, root_raw)
            .ok_or_else(|| RuntimeError::not_found(format!("Graph node not found: {root_raw}")))?;
        let mut visited_depth = BTreeMap::<String, usize>::new();
        let mut parents = BTreeMap::<String, Vec<String>>::new();
        let mut queue = VecDeque::new();
        visited_depth.insert(resolved.clone(), 0);
        parents.insert(resolved.clone(), vec![resolved.clone()]);
        queue.push_back((resolved.clone(), 0usize));

        let mut selected_edges = Vec::new();
        let mut seen_edges = BTreeSet::<(String, String, String)>::new();

        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= req.max_depth || visited_depth.len() >= req.limit.max(1) {
                continue;
            }
            for edge in &all_edges {
                if !allowed_kinds.is_empty() && !allowed_kinds.contains(&edge.kind) {
                    continue;
                }
                let next_id = match req.direction {
                    Direction::Out if edge.src == cur => Some(&edge.dst),
                    Direction::In if edge.dst == cur => Some(&edge.src),
                    Direction::Both if edge.src == cur => Some(&edge.dst),
                    Direction::Both if edge.dst == cur => Some(&edge.src),
                    _ => None,
                };
                let Some(next_id) = next_id else {
                    continue;
                };
                let Some(next_node) = all_nodes.get(next_id) else {
                    continue;
                };
                if !admitted(next_node) {
                    continue;
                }
                let ekey = (edge.src.clone(), edge.dst.clone(), edge.kind.clone());
                if seen_edges.insert(ekey) {
                    selected_edges.push(edge.clone());
                }
                if !visited_depth.contains_key(next_id) && visited_depth.len() < req.limit.max(1) {
                    visited_depth.insert(next_id.clone(), depth + 1);
                    let mut path = parents.get(&cur).cloned().unwrap_or_default();
                    path.push(format!("{} --[{}]--> {}", cur, edge.kind, next_id));
                    parents.insert(next_id.clone(), path);
                    queue.push_back((next_id.clone(), depth + 1));
                }
            }
        }

        // Also include intra-subgraph edges matching the filter between visited nodes.
        for edge in &all_edges {
            if !allowed_kinds.is_empty() && !allowed_kinds.contains(&edge.kind) {
                continue;
            }
            if visited_depth.contains_key(&edge.src) && visited_depth.contains_key(&edge.dst) {
                let ekey = (edge.src.clone(), edge.dst.clone(), edge.kind.clone());
                if seen_edges.insert(ekey) {
                    selected_edges.push(edge.clone());
                }
            }
        }

        let nodes = visited_depth
            .keys()
            .filter_map(|id| all_nodes.get(id).cloned())
            .collect::<Vec<_>>();
        let provenance_paths = parents
            .into_iter()
            .filter(|(id, _)| id != &resolved)
            .map(|(_, path)| path)
            .take(32)
            .collect::<Vec<_>>();
        let summary = summarize_subgraph(&nodes, &selected_edges);
        return Ok(GraphQueryResult {
            root: req.root.clone(),
            resolved_root: Some(resolved),
            direction: req.direction.as_str().into(),
            max_depth: req.max_depth,
            include_archived: req.include_archived,
            nodes,
            edges: selected_edges,
            provenance_paths,
            summary,
        });
    }

    let nodes = all_nodes
        .values()
        .filter(|n| admitted(n))
        .take(req.limit.max(1))
        .cloned()
        .collect::<Vec<_>>();
    let node_ids = nodes
        .iter()
        .map(|n| n.id.as_str())
        .collect::<BTreeSet<_>>();
    let edges = all_edges
        .into_iter()
        .filter(|e| allowed_kinds.is_empty() || allowed_kinds.contains(&e.kind))
        .filter(|e| node_ids.contains(e.src.as_str()) && node_ids.contains(e.dst.as_str()))
        .collect::<Vec<_>>();
    let summary = summarize_subgraph(&nodes, &edges);
    Ok(GraphQueryResult {
        root: None,
        resolved_root: None,
        direction: req.direction.as_str().into(),
        max_depth: req.max_depth,
        include_archived: req.include_archived,
        nodes,
        edges,
        provenance_paths: Vec::new(),
        summary,
    })
}

fn summarize_subgraph(nodes: &[GraphNode], edges: &[GraphEdge]) -> Value {
    let mut node_kinds = BTreeMap::<String, usize>::new();
    let mut freshness = BTreeMap::<String, usize>::new();
    for n in nodes {
        *node_kinds.entry(n.kind.clone()).or_default() += 1;
        *freshness.entry(n.freshness.clone()).or_default() += 1;
    }
    let mut edge_kinds = BTreeMap::<String, usize>::new();
    for e in edges {
        *edge_kinds.entry(e.kind.clone()).or_default() += 1;
    }
    object!({
        "node_count": nodes.len(),
        "edge_count": edges.len(),
        "node_kinds": node_kinds,
        "edge_kinds": edge_kinds,
        "freshness": freshness,
    })
}

/// Full lineage + reverse impact report for a single target node.
pub fn node_links(conn: &Connection, target: &str, include_archived: bool) -> Result<Value> {
    let (all_nodes, all_edges) = load_all(conn)?;
    let id = resolve_node_id(&all_nodes, target)
        .ok_or_else(|| RuntimeError::not_found(format!("Graph node not found: {target}")))?;
    let node = all_nodes[&id].clone();
    let direct_out = all_edges
        .iter()
        .filter(|e| e.src == id)
        .filter_map(|e| {
            let other = all_nodes.get(&e.dst)?;
            if !include_archived && other.lifecycle == "archived" {
                return None;
            }
            Some(object!({
                "edge_kind": e.kind,
                "weight": e.weight,
                "target": other,
                "metadata": e.metadata,
            }))
        })
        .collect::<Vec<_>>();
    let direct_in = all_edges
        .iter()
        .filter(|e| e.dst == id)
        .filter_map(|e| {
            let other = all_nodes.get(&e.src)?;
            if !include_archived && other.lifecycle == "archived" {
                return None;
            }
            Some(object!({
                "edge_kind": e.kind,
                "weight": e.weight,
                "source": other,
                "metadata": e.metadata,
            }))
        })
        .collect::<Vec<_>>();

    let lineage = query_graph(
        conn,
        &GraphQuery {
            root: Some(id.clone()),
            max_depth: 6,
            kinds: vec![],
            direction: Direction::Out,
            include_archived,
            limit: 200,
        },
    )?;
    let impacted = query_graph(
        conn,
        &GraphQuery {
            root: Some(id.clone()),
            max_depth: 4,
            kinds: vec![],
            direction: Direction::In,
            include_archived,
            limit: 200,
        },
    )?;

    let stale_sources = lineage
        .nodes
        .iter()
        .filter(|n| n.kind == "file" && n.freshness == "stale")
        .map(|n| n.path.clone())
        .collect::<Vec<_>>();
    let missing_sources = lineage
        .nodes
        .iter()
        .filter(|n| n.kind == "file" && n.freshness == "missing")
        .map(|n| n.path.clone())
        .collect::<Vec<_>>();
    let conflicts = direct_out
        .iter()
        .chain(direct_in.iter())
        .filter(|entry| entry["edge_kind"] == "conflicts")
        .cloned()
        .collect::<Vec<_>>();

    Ok(object!({
        "resolved_id": id,
        "node": node,
        "outgoing": direct_out,
        "incoming": direct_in,
        "lineage_nodes": lineage.nodes.into_iter().filter(|n| n.id != id).collect::<Vec<_>>(),
        "impacted_nodes": impacted.nodes.into_iter().filter(|n| n.id != id).collect::<Vec<_>>(),
        "stale_sources": stale_sources,
        "missing_sources": missing_sources,
        "conflicts": conflicts,
    }))
}

fn dot_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Deterministic Graphviz DOT serialization for a query result.
pub fn export_dot(result: &GraphQueryResult) -> String {
    let mut lines = vec![
        "digraph context_graph {".to_string(),
        "  rankdir=LR;".to_string(),
        "  node [fontname=\"Helvetica\", fontsize=10];".to_string(),
        "  edge [fontname=\"Helvetica\", fontsize=9];".to_string(),
    ];
    for node in &result.nodes {
        let shape = match node.kind.as_str() {
            "file" => "note",
            "memory-chunk" => "component",
            "task" | "working-task" => "box",
            "handoff" | "session-merge" => "diamond",
            "decision" => "hexagon",
            _ => "ellipse",
        };
        let label = if node.title.is_empty() || node.title == node.id {
            node.id.clone()
        } else {
            let short_title: String = node.title.chars().take(48).collect();
            format!("{}\\n{}", node.id, short_title)
        };
        let style = if node.lifecycle == "archived" || node.lifecycle == "superseded" {
            "dashed"
        } else if node.freshness == "missing" || node.freshness == "stale" {
            "bold"
        } else {
            "solid"
        };
        lines.push(format!(
            "  \"{}\" [label=\"{}\", shape={}, style={}];",
            dot_escape(&node.id),
            dot_escape(&label),
            shape,
            style
        ));
    }
    for edge in &result.edges {
        let style = match edge.kind.as_str() {
            "conflicts" => "style=dashed, color=red",
            "supersedes" => "style=bold, color=purple",
            "promoted_from" | "consolidated_from" => "style=bold, color=blue",
            "handoff_of" | "session_merge" => "color=teal",
            "touches" => "style=dotted",
            _ => "color=black",
        };
        lines.push(format!(
            "  \"{}\" -> \"{}\" [label=\"{}\", {}];",
            dot_escape(&edge.src),
            dot_escape(&edge.dst),
            dot_escape(&edge.kind),
            style
        ));
    }
    lines.push("}".to_string());
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BankService, CreateRecord, OpenOptions, Roots, StartTask, UpdateRecord};

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn graph_captures_all_multi_relation_edges_and_traversals() {
        let root = TempDir(std::env::temp_dir().join(format!(
            "cg-graph-multi-{}",
            domain::uuid4()
        )));
        std::fs::create_dir_all(root.0.join("src")).unwrap();
        std::fs::create_dir_all(root.0.join("project-brain/handoffs")).unwrap();
        std::fs::create_dir_all(root.0.join(".context-handoff/merges")).unwrap();
        std::fs::write(root.0.join("src/auth.rs"), "pub fn verify() {}\n").unwrap();
        std::fs::write(root.0.join("src/router.rs"), "pub fn route() {}\n").unwrap();

        let roots = Roots::installed(&root.0).unwrap();
        let author = RuntimeContext::new(roots.clone(), "alice", "governed").unwrap();
        let brain = BrainRepository::new(author.clone()).unwrap();
        let task = brain
            .create(CreateRecord {
                record_type: "task".into(),
                external_id: "TASK-GRAPH-1".into(),
                title: "Audit authentication flow".into(),
                goal: Some("Verify auth and router integration".into()),
                files: vec!["src/router.rs".into()],
                sources: vec!["src/auth.rs".into()],
                authority: "verified".into(),
                attestation: Some("person".into()),
                ..CreateRecord::default()
            })
            .unwrap();
        let task_uuid = task["id"].as_str().unwrap().to_string();

        let event = brain
            .create(CreateRecord {
                record_type: "event".into(),
                external_id: "EVT-GRAPH-1".into(),
                title: "Completed auth verification".into(),
                sources: vec!["src/auth.rs".into()],
                authority: "verified".into(),
                attestation: Some("person".into()),
                ..CreateRecord::default()
            })
            .unwrap();
        let event_uuid = event["id"].as_str().unwrap().to_string();

        let finding = brain
            .create(CreateRecord {
                record_type: "finding".into(),
                external_id: "CONSOLIDATED-GRAPH-1".into(),
                title: "Consolidated auth rule".into(),
                goal: Some(format!("Verified via [event {event_uuid}]")),
                sources: vec!["src/auth.rs".into()],
                authority: "verified".into(),
                attestation: Some("person".into()),
                ..CreateRecord::default()
            })
            .unwrap();
        let finding_uuid = finding["id"].as_str().unwrap().to_string();
        brain
            .update(
                &finding_uuid,
                UpdateRecord {
                    expected_revision: finding["revision"].as_u64(),
                    progress: Some(format!("Consolidated rule from [event {event_uuid}].")),
                    transition_to: Some("resolved".into()),
                    reason: "Verified".into(),
                    attestation: Some("person".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        // Create conflicting finding and promotion proposal -> bank chunk, plus superseding record.
        let dec = brain
            .create(CreateRecord {
                record_type: "decision".into(),
                external_id: "DEC-CONFLICT-1".into(),
                title: "Conflicting token lifetime decision".into(),
                conflicts: vec![finding_uuid.clone()],
                sources: vec!["src/auth.rs".into()],
                authority: "verified".into(),
                attestation: Some("person".into()),
                ..CreateRecord::default()
            })
            .unwrap();
        let dec_uuid = dec["id"].as_str().unwrap().to_string();
        let mut dec_img = brain.find(&dec_uuid, None, false).unwrap();
        dec_img.record["supersedes"] = serde_json::json!([event_uuid.clone()]);
        brain.write_record(&dec_img.path, &dec_img.record).unwrap();

        let bank = BankService::new(author.clone());
        let prop = bank
            .propose(
                std::slice::from_ref(&finding_uuid),
                "Consolidated auth rule",
                "Auth verification runs strictly before routing.",
                false,
            )
            .unwrap();
        let reviewer = BankService::new(RuntimeContext::new(roots.clone(), "bob", "governed").unwrap());
        reviewer
            .review(prop["id"].as_str().unwrap(), Some("bob"), true)
            .unwrap();
        let applied = reviewer.apply(prop["id"].as_str().unwrap()).unwrap();
        let mem_id = applied["destination_memory_id"].as_str().unwrap().to_string();

        // Handoff + session merge artifacts.
        std::fs::write(
            root.0.join(".context-handoff/handoff-1.json"),
            serde_json::to_string_pretty(&object!({
                "id": "handoff-1",
                "task_id": "TASK-GRAPH-1",
                "title": "Handoff for auth audit",
                "files": ["src/auth.rs"],
                "created_at": "2026-11-18T12:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.0.join(".context-handoff/merges/merge-1.json"),
            serde_json::to_string_pretty(&object!({
                "id": "merge-1",
                "title": "Merged two auth investigations",
                "merged_from": ["session-a", "handoff-1"],
                "created_at": "2026-11-18T12:10:00Z"
            }))
            .unwrap(),
        )
        .unwrap();

        let db_path = crate::storage::default_database(&roots).unwrap();
        let mut store = Store::open(&db_path, OpenOptions::default()).unwrap();
        store
            .start(StartTask {
                task_id: "TASK-GRAPH-1".into(),
                goal: "Audit authentication flow".into(),
                files: vec!["src/router.rs".into()],
                sources: vec!["src/auth.rs".into()],
            })
            .unwrap();
        crate::index::refresh_index(&mut store, &author, false).unwrap();

        let overview = query_graph(&store.conn, &GraphQuery::default()).unwrap();
        let kinds: BTreeSet<_> = overview.edges.iter().map(|e| e.kind.as_str()).collect();
        for expected in EDGE_KINDS {
            assert!(
                kinds.contains(*expected),
                "Missing edge kind '{expected}'; found {kinds:?}"
            );
        }

        // 2-hop traversal from chunk reaches finding -> event + auth.rs.
        let hop2 = query_graph(
            &store.conn,
            &GraphQuery {
                root: Some(mem_id.clone()),
                max_depth: 2,
                kinds: vec![],
                direction: Direction::Out,
                include_archived: true,
                limit: 100,
            },
        )
        .unwrap();
        let node_ids: BTreeSet<_> = hop2.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(node_ids.contains(&*format!("brain:{finding_uuid}")));
        assert!(node_ids.contains("file:src/auth.rs"));
        assert!(node_ids.contains(&*format!("brain:{event_uuid}")));

        // Impact query on src/auth.rs returns citing chunk/records, and DOT renders cleanly.
        let impact = node_links(&store.conn, "src/auth.rs", true).unwrap();
        assert!(!impact["impacted_nodes"].as_array().unwrap().is_empty());
        let dot = export_dot(&hop2);
        assert!(dot.starts_with("digraph context_graph {"));
        assert!(dot.contains("promoted_from"));
        let _ = task_uuid;
    }
}
