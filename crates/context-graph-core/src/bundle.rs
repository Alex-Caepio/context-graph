//! Portable `context-graph-bundle-v1` export and collision-safe import
//! (unpacked directories and `.zip` / `.cgb.zip` archives).
use crate::{
    BankService, BrainRepository, OpenOptions, Result, RuntimeContext, RuntimeError, Store,
    bank::{
        codec,
        publication::{IMAGE_LIMIT, PublicationFiles},
    },
    domain::{self, json},
    fs,
    graph::{self, Direction, GraphEdge, GraphNode, GraphQuery},
    index,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as object};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

pub const BUNDLE_SCHEMA_VERSION: &str = "context-graph-bundle-v1";
const MAX_BUNDLE_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_BUNDLE_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_BUNDLE_ENTRIES: usize = 10_000;

const BUNDLE_README: &str = "# Context Graph Bundle (`context-graph-bundle-v1`)\n\nPortable, privacy-filtered, SHA-256 attested snapshot of a repository's\nContext Graph, Project Brain records, Memory Bank chunks, handoffs, and episodes.\n\n- `BUNDLE_MANIFEST.json` lists every included artifact, its `sha256` digest,\n  source repository metadata, privacy filters, and any excluded records.\n- `graph.json` carries the serialized unified multi-relation context graph.\n- Imported records are re-attributed to the importing actor and cite origin.\n- Always verify cited source paths against the target repository.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleFormat {
    Dir,
    Zip,
}

impl BundleFormat {
    pub fn from_destination(path: &Path, explicit: Option<&str>) -> Result<Self> {
        if let Some(raw) = explicit.filter(|s| !s.trim().is_empty()) {
            return match raw.trim().to_ascii_lowercase().as_str() {
                "dir" | "directory" => Ok(Self::Dir),
                "zip" | "cgb" | "cgb.zip" => Ok(Self::Zip),
                other => Err(RuntimeError::invalid(format!(
                    "Unsupported bundle format: {other}"
                ))),
            };
        }
        let lower = path.to_string_lossy().to_ascii_lowercase();
        if lower.ends_with(".zip") {
            Ok(Self::Zip)
        } else {
            Ok(Self::Dir)
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::Zip => "zip",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportStrategy {
    Skip,
    RemapIds,
    ArchiveSuperseded,
}

impl ImportStrategy {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("skip").trim().to_ascii_lowercase().as_str() {
            "" | "skip" => Ok(Self::Skip),
            "remap" | "remap-ids" | "remap_ids" => Ok(Self::RemapIds),
            "archive-superseded" | "archive_superseded" | "supersede" => {
                Ok(Self::ArchiveSuperseded)
            }
            other => Err(RuntimeError::invalid(format!(
                "Invalid import strategy '{other}'; expected one of: skip, remap-ids, archive-superseded"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::RemapIds => "remap-ids",
            Self::ArchiveSuperseded => "archive-superseded",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub destination: PathBuf,
    pub format: Option<BundleFormat>,
    pub include_archive: bool,
    pub include_superseded: bool,
    pub include_handoffs: bool,
    pub include_episodes: bool,
    pub root: Option<String>,
    pub depth: usize,
    pub force: bool,
}

impl Default for ExportRequest {
    fn default() -> Self {
        Self {
            destination: PathBuf::new(),
            format: None,
            include_archive: false,
            include_superseded: false,
            include_handoffs: true,
            include_episodes: true,
            root: None,
            depth: 2,
            force: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ImportRequest {
    pub source: PathBuf,
    pub strategy: ImportStrategy,
    pub dry_run: bool,
    pub include_archive: bool,
    pub include_handoffs: bool,
    pub include_episodes: bool,
}

impl Default for ImportRequest {
    fn default() -> Self {
        Self {
            source: PathBuf::new(),
            strategy: ImportStrategy::Skip,
            dry_run: false,
            include_archive: true,
            include_handoffs: true,
            include_episodes: true,
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    domain::hex_lower(Sha256::digest(bytes))
}

fn validate_bundle_entry_path(rel: &str) -> Result<String> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.contains('\\')
        || rel.contains('\0')
        || Path::new(rel).is_absolute()
    {
        return Err(RuntimeError::policy(format!(
            "Unsafe bundle entry path: {rel}"
        )));
    }
    let parts = rel.split('/').collect::<Vec<_>>();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(RuntimeError::policy(format!(
            "Unsafe bundle traversal path: {rel}"
        )));
    }
    Ok(parts.join("/"))
}

fn git_stdout(project: &Path, args: &[&str]) -> Option<String> {
    crate::git::run(
        project,
        &crate::git::strings(args),
        None,
        std::time::Duration::from_secs(3),
        16384,
    )
    .ok()
    .filter(|o| o.code == 0)
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

fn repo_name(context: &RuntimeContext) -> String {
    context
        .roots
        .project
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("repository")
        .to_string()
}

fn slug_token(raw: &str) -> String {
    let token = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let collapsed = token
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "bundle".into()
    } else {
        collapsed.chars().take(48).collect()
    }
}

fn handoff_artifact_candidates(state_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for rel_dir in [
        ".context-handoff",
        ".context-handoff/merges",
        "project-brain/handoffs",
        "project-brain/control/handoffs",
    ] {
        let dir = state_root.join(rel_dir);
        if !fs::admit_directory(&dir)? {
            continue;
        }
        let mut entries = std::fs::read_dir(&dir)?
            .filter_map(std::result::Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if fs::linked(&meta) || !meta.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            if !name.ends_with(".json") && !name.ends_with(".md") {
                continue;
            }
            let rel = format!("{rel_dir}/{name}");
            out.push((rel, path));
        }
    }
    Ok(out)
}

fn collect_episodes(service: &BankService) -> Result<Vec<Value>> {
    let db_path = crate::storage::default_database(&service.context.roots)?;
    if !db_path.is_file() {
        return Ok(vec![]);
    }
    let store = Store::open(&db_path, OpenOptions::default())?;
    let mut stmt = store.conn.prepare(
        "SELECT summary,outcome,files,verification,sources,created_at FROM episodes ORDER BY rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (summary, outcome, files, verification, sources, created_at) = row?;
        out.push(object!({
            "summary": summary,
            "outcome": outcome,
            "files": serde_json::from_str::<Value>(&files).unwrap_or(object!([])),
            "verification": serde_json::from_str::<Value>(&verification).unwrap_or(object!([])),
            "sources": serde_json::from_str::<Value>(&sources).unwrap_or(object!([])),
            "created_at": created_at,
        }));
    }
    Ok(out)
}

/// Export a privacy-filtered, SHA-256 attested `context-graph-bundle-v1` archive or directory.
pub fn export(service: &BankService, req: &ExportRequest) -> Result<Value> {
    let format = req
        .format
        .unwrap_or(BundleFormat::from_destination(&req.destination, None)?);
    if req.destination.as_os_str().is_empty() {
        return Err(RuntimeError::invalid("Export destination must not be empty"));
    }
    match std::fs::symlink_metadata(&req.destination) {
        Ok(meta) => {
            if fs::linked(&meta) {
                return Err(RuntimeError::policy(
                    "Export destination must not be a symbolic link",
                ));
            }
            match format {
                BundleFormat::Dir => {
                    if !meta.is_dir() {
                        return Err(RuntimeError::policy(
                            "Directory export destination exists and is not a directory",
                        ));
                    }
                    if !req.force && std::fs::read_dir(&req.destination)?.next().is_some() {
                        return Err(RuntimeError::conflict(
                            "Export destination is not empty; pass force=true to overwrite",
                        ));
                    }
                }
                BundleFormat::Zip => {
                    if meta.is_dir() {
                        return Err(RuntimeError::policy(
                            "ZIP export destination exists and is a directory",
                        ));
                    }
                    if !req.force {
                        return Err(RuntimeError::conflict(
                            "Export destination already exists; pass force=true to overwrite",
                        ));
                    }
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let config = service.config()?;
    let mut allowed_privacy = config.strings("allowed_privacy")?;
    allowed_privacy.sort();

    // Ensure SQLite & Context Graph reflect the current workspace before snapshotting.
    let db_path = crate::storage::default_database(&service.context.roots)?;
    let mut store = Store::open(&db_path, OpenOptions::default())?;
    index::refresh_index(&mut store, &service.context, false)?;

    let subgraph = graph::query_graph(
        &store.conn,
        &GraphQuery {
            root: req.root.clone(),
            max_depth: req.depth,
            kinds: vec![],
            direction: Direction::Both,
            include_archived: req.include_archive,
            limit: 10_000,
        },
    )?;
    let scoped_ids: Option<BTreeSet<String>> = if req.root.is_some() {
        Some(subgraph.nodes.iter().map(|n| n.id.clone()).collect())
    } else {
        None
    };

    let mut files = PublicationFiles::new(&service.context.roots.state)?;
    let mut excluded = Vec::new();
    let mut artifacts = Vec::new();
    let mut payload: Vec<(String, Vec<u8>)> = Vec::new();
    let mut included_brain_ids = BTreeSet::new();
    let mut included_chunk_ids = BTreeSet::new();
    let mut included_node_ids = BTreeSet::new();

    for image in service.brain()?.records(req.include_archive)? {
        let rel = image
            .path
            .strip_prefix(&service.context.roots.state)
            .ok()
            .and_then(|p| p.to_str())
            .map(|s| s.replace('\\', "/"))
            .ok_or_else(|| RuntimeError::policy("Export source path escapes state"))?;
        let record = &image.record;
        let rec_id = record["id"].as_str().unwrap_or("");
        let node_id = format!("brain:{rec_id}");
        if let Some(scope) = &scoped_ids
            && !scope.contains(&node_id)
        {
            continue;
        }
        let priv_val = record["privacy"].as_str().unwrap_or("");
        if !allowed_privacy.iter().any(|p| p == priv_val) {
            excluded.push(object!({
                "path": rel,
                "id": record["id"],
                "reason": format!("privacy '{priv_val}' is not in allowed_privacy"),
            }));
            continue;
        }
        let raw = files
            .read(&rel, 4 * IMAGE_LIMIT)?
            .ok_or_else(|| RuntimeError::unavailable("Export source disappeared"))?;
        let text = std::str::from_utf8(&raw)
            .map_err(|_| RuntimeError::invalid(format!("Export source {rel} is not UTF-8")))?;
        if let Some(label) = domain::secret_label_in(&service.context.roots, text)? {
            return Err(RuntimeError::policy(format!(
                "Export aborted: {rel} matches a {label} secret pattern; sanitize the source first"
            )));
        }
        included_brain_ids.insert(rec_id.to_string());
        included_node_ids.insert(node_id);
        artifacts.push(object!({
            "kind": "brain-record",
            "path": rel,
            "id": record["id"],
            "external_id": record["external_id"],
            "type": record["type"],
            "status": record["status"],
            "archived": image.archived,
            "privacy": record["privacy"],
            "authority": record["authority"],
            "owner": record["owner"],
            "revision": record["revision"],
            "sources": record["sources"],
            "sha256": sha256_hex(&raw),
            "bytes": raw.len(),
        }));
        payload.push((rel, raw));
    }

    for rel in codec::paths(service)? {
        let chunk = match codec::read(&mut files, &rel) {
            Ok(c) => c,
            Err(err) => {
                excluded.push(object!({
                    "path": rel,
                    "id": null,
                    "reason": format!("unreadable chunk: {err}"),
                }));
                continue;
            }
        };
        let m = &chunk.metadata;
        let mem_id = m["id"].as_str().unwrap_or("");
        let node_id = format!("bank:{mem_id}");
        if let Some(scope) = &scoped_ids
            && !scope.contains(&node_id)
        {
            continue;
        }
        let status = m["status"].as_str().unwrap_or("");
        if !req.include_superseded && !["active", "needs-review"].contains(&status) {
            excluded.push(object!({
                "path": rel,
                "id": m["id"],
                "reason": format!("status '{status}' is not exported without include-superseded"),
            }));
            continue;
        }
        let text = std::str::from_utf8(&chunk.raw)
            .map_err(|_| RuntimeError::invalid(format!("Export source {rel} is not UTF-8")))?;
        if let Some(label) = domain::secret_label_in(&service.context.roots, text)? {
            return Err(RuntimeError::policy(format!(
                "Export aborted: {rel} matches a {label} secret pattern; sanitize the source first"
            )));
        }
        included_chunk_ids.insert(mem_id.to_string());
        included_node_ids.insert(node_id);
        artifacts.push(object!({
            "kind": "memory-chunk",
            "path": rel,
            "id": m["id"],
            "title": m["title"],
            "type": m["type"],
            "status": m["status"],
            "tags": m.get("tags").cloned().unwrap_or(object!([])),
            "last_verified": m["last_verified"],
            "sources": m.get("sources").cloned().unwrap_or(object!([])),
            "auto_promoted": m["tags"].as_array().is_some_and(|tags| tags.iter().any(|t| t == "auto-promoted")),
            "sha256": sha256_hex(&chunk.raw),
            "bytes": chunk.raw.len(),
        }));
        payload.push((rel, chunk.raw));
    }

    let mut handoff_count = 0usize;
    if req.include_handoffs {
        for (rel, path) in handoff_artifact_candidates(&service.context.roots.state)? {
            let raw = fs::read_regular(&path)?;
            let text = std::str::from_utf8(&raw)
                .map_err(|_| RuntimeError::invalid(format!("Export handoff {rel} is not UTF-8")))?;
            if let Some(label) = domain::secret_label_in(&service.context.roots, text)? {
                return Err(RuntimeError::policy(format!(
                    "Export aborted: {rel} matches a {label} secret pattern; sanitize the source first"
                )));
            }
            if let Some(scope) = &scoped_ids {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let matched = scope.contains(&format!("handoff:{stem}"))
                    || scope.contains(&format!("session:{stem}"))
                    || scope.iter().any(|id| {
                        subgraph
                            .nodes
                            .iter()
                            .any(|n| &n.id == id && n.path == rel)
                    });
                if !matched {
                    continue;
                }
            }
            handoff_count += 1;
            included_node_ids.insert(rel.clone());
            artifacts.push(object!({
                "kind": "handoff",
                "path": rel,
                "sha256": sha256_hex(&raw),
                "bytes": raw.len(),
            }));
            payload.push((rel, raw));
        }
    }

    let mut episode_count = 0usize;
    if req.include_episodes {
        let episodes = collect_episodes(service)?;
        if !episodes.is_empty() {
            let raw_text = json::serialize(&object!(episodes), false, false, true, true)? + "\n";
            if let Some(label) = domain::secret_label_in(&service.context.roots, &raw_text)? {
                return Err(RuntimeError::policy(format!(
                    "Export aborted: episodes.json matches a {label} secret pattern"
                )));
            }
            episode_count = episodes.len();
            let raw = raw_text.into_bytes();
            artifacts.push(object!({
                "kind": "episodes",
                "path": "episodes.json",
                "count": episode_count,
                "sha256": sha256_hex(&raw),
                "bytes": raw.len(),
            }));
            payload.push(("episodes.json".into(), raw));
        }
    }

    // Export privacy-filtered unified graph subgraph matching included records/chunks + referenced leaves.
    let mut exported_edges: Vec<GraphEdge> = Vec::new();
    let mut kept_node_ids = BTreeSet::new();
    let excluded_brain_or_chunk_nodes: BTreeSet<String> = excluded
        .iter()
        .filter_map(|row| {
            let id = row["id"].as_str()?;
            let path = row["path"].as_str().unwrap_or("");
            if path.starts_with("memory-bank/chunks/") {
                Some(format!("bank:{id}"))
            } else {
                Some(format!("brain:{id}"))
            }
        })
        .collect();
    for edge in &subgraph.edges {
        if excluded_brain_or_chunk_nodes.contains(&edge.src)
            || excluded_brain_or_chunk_nodes.contains(&edge.dst)
        {
            continue;
        }
        if included_node_ids.contains(&edge.src) || included_node_ids.contains(&edge.dst) || req.root.is_some() {
            kept_node_ids.insert(edge.src.clone());
            kept_node_ids.insert(edge.dst.clone());
            exported_edges.push(edge.clone());
        }
    }
    let exported_nodes: Vec<GraphNode> = subgraph
        .nodes
        .into_iter()
        .filter(|n| {
            !excluded_brain_or_chunk_nodes.contains(&n.id)
                && (kept_node_ids.contains(&n.id)
                    || included_node_ids.contains(&n.id)
                    || (!["brain", "record", "task", "finding", "bug", "incident", "decision", "event", "memory-chunk"]
                        .contains(&n.kind.as_str())
                        && allowed_privacy.iter().any(|p| p == &n.privacy)))
        })
        .collect();

    let graph_doc = object!({
        "schema_version": BUNDLE_SCHEMA_VERSION,
        "root": req.root,
        "nodes": exported_nodes,
        "edges": exported_edges,
    });
    let graph_bytes = (json::serialize(&graph_doc, false, false, true, true)? + "\n").into_bytes();
    artifacts.push(object!({
        "kind": "context-graph",
        "path": "graph.json",
        "sha256": sha256_hex(&graph_bytes),
        "bytes": graph_bytes.len(),
    }));
    payload.push(("graph.json".into(), graph_bytes));

    let commit = git_stdout(&service.context.roots.project, &["rev-parse", "HEAD"]);
    let branch = git_stdout(
        &service.context.roots.project,
        &["branch", "--show-current"],
    );
    let source_repo = repo_name(&service.context);
    let policy_fp = domain::secret_policy_fingerprint().unwrap_or_default();

    let bundle_manifest = object!({
        "schema_version": BUNDLE_SCHEMA_VERSION,
        "bundle_id": domain::uuid4(),
        "exported_at": domain::clock::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "source": {
            "repo_name": source_repo,
            "commit": commit,
            "branch": branch,
            "framework": config.value["framework"],
            "mode": config.value["mode"],
            "owner": service.context.actor,
            "automatic_promotion": config.value["automatic_promotion"],
        },
        "filters": {
            "allowed_privacy": allowed_privacy,
            "include_archive": req.include_archive,
            "include_superseded": req.include_superseded,
            "include_handoffs": req.include_handoffs,
            "include_episodes": req.include_episodes,
            "root": req.root,
            "depth": req.depth,
        },
        "privacy_policy": {
            "secret_policy_fingerprint": policy_fp,
            "secret_scan_attested": true,
        },
        "counts": {
            "brain_records": included_brain_ids.len(),
            "memory_chunks": included_chunk_ids.len(),
            "handoffs": handoff_count,
            "episodes": episode_count,
            "graph_nodes": exported_nodes.len(),
            "graph_edges": exported_edges.len(),
            "artifacts": artifacts.len(),
            "excluded": excluded.len(),
        },
        "artifacts": artifacts,
        "excluded": excluded,
    });
    let manifest_bytes =
        (json::serialize(&bundle_manifest, false, false, true, true)? + "\n").into_bytes();
    let readme_bytes = BUNDLE_README.as_bytes().to_vec();

    payload.push(("BUNDLE_MANIFEST.json".into(), manifest_bytes.clone()));
    payload.push(("MANIFEST.json".into(), manifest_bytes));
    payload.push(("README.md".into(), readme_bytes));

    match format {
        BundleFormat::Dir => {
            fs::ensure_directory(&req.destination, false)?;
            for (rel, bytes) in &payload {
                let rel = validate_bundle_entry_path(rel)?;
                let target = req.destination.join(&rel);
                fs::admit_descendant(&req.destination, &target)?;
                fs::atomic_write(&target, bytes)?;
            }
        }
        BundleFormat::Zip => {
            let parent = req
                .destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            fs::ensure_directory(parent, false)?;
            let temp = parent.join(format!(
                ".context-graph-bundle-{}.tmp",
                domain::uuid4()
            ));
            let write_result = (|| -> Result<()> {
                let file = File::create(&temp)?;
                let mut writer = ZipWriter::new(file);
                let opts = FileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .unix_permissions(0o644);
                for (rel, bytes) in &payload {
                    let rel = validate_bundle_entry_path(rel)?;
                    writer
                        .start_file(&rel, opts)
                        .map_err(|e| RuntimeError::unavailable(format!("ZIP write error: {e}")))?;
                    writer.write_all(bytes)?;
                }
                writer
                    .finish()
                    .map_err(|e| RuntimeError::unavailable(format!("ZIP finish error: {e}")))?;
                Ok(())
            })();
            if let Err(err) = write_result {
                let _ = std::fs::remove_file(&temp);
                return Err(err);
            }
            std::fs::rename(&temp, &req.destination)?;
        }
    }

    let written = fs::canonicalize(&req.destination).unwrap_or_else(|_| req.destination.clone());
    Ok(object!({
        "schema_version": BUNDLE_SCHEMA_VERSION,
        "format": format.as_str(),
        "destination": written.to_string_lossy(),
        "bundle_id": bundle_manifest["bundle_id"],
        "counts": bundle_manifest["counts"],
    }))
}

fn read_bundle_entries(source: &Path) -> Result<(BundleFormat, BTreeMap<String, Vec<u8>>)> {
    let meta = std::fs::symlink_metadata(source)?;
    if fs::linked(&meta) {
        return Err(RuntimeError::policy(
            "Bundle import source must not be a symbolic link",
        ));
    }
    let mut entries = BTreeMap::new();
    let mut total = 0usize;
    if meta.is_dir() {
        let mut stack = vec![source.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for item in std::fs::read_dir(&dir)? {
                let item = item?;
                let path = item.path();
                let imeta = std::fs::symlink_metadata(&path)?;
                if fs::linked(&imeta) {
                    return Err(RuntimeError::policy(
                        "Bundle import refuses symbolic links",
                    ));
                }
                if imeta.is_dir() {
                    stack.push(path);
                } else if imeta.is_file() {
                    if imeta.len() as usize > MAX_BUNDLE_ENTRY_BYTES {
                        return Err(RuntimeError::policy("Bundle entry exceeds size limit"));
                    }
                    let rel = path
                        .strip_prefix(source)
                        .ok()
                        .and_then(|p| p.to_str())
                        .ok_or_else(|| RuntimeError::invalid("Bundle path is not UTF-8"))?;
                    let clean = validate_bundle_entry_path(&rel.replace('\\', "/"))?;
                    fs::admit_descendant(source, &path)?;
                    let bytes = fs::read_regular(&path)?;
                    total = total.saturating_add(bytes.len());
                    if total > MAX_BUNDLE_TOTAL_BYTES || entries.len() >= MAX_BUNDLE_ENTRIES {
                        return Err(RuntimeError::policy("Bundle exceeds maximum size/entry limit"));
                    }
                    entries.insert(clean, bytes);
                }
            }
        }
        return Ok((BundleFormat::Dir, entries));
    }
    if !meta.is_file() {
        return Err(RuntimeError::policy(
            "Bundle import source must be a directory or ZIP archive",
        ));
    }
    let raw_zip = fs::read_regular(source)?;
    let mut archive = ZipArchive::new(std::io::Cursor::new(raw_zip))
        .map_err(|e| RuntimeError::invalid(format!("Invalid ZIP bundle: {e}")))?;
    if archive.len() > MAX_BUNDLE_ENTRIES {
        return Err(RuntimeError::policy("Bundle archive exceeds entry limit"));
    }
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| RuntimeError::invalid(format!("ZIP entry error: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        if let Some(mode) = entry.unix_mode()
            && (mode & 0o170000) == 0o120000
        {
            return Err(RuntimeError::policy("Bundle ZIP refuses symbolic links"));
        }
        if entry.size() as usize > MAX_BUNDLE_ENTRY_BYTES {
            return Err(RuntimeError::policy("Bundle ZIP entry exceeds size limit"));
        }
        let clean = validate_bundle_entry_path(entry.name())?;
        let mut buf = Vec::with_capacity(entry.size().min(MAX_BUNDLE_ENTRY_BYTES as u64) as usize);
        entry
            .by_ref()
            .take((MAX_BUNDLE_ENTRY_BYTES + 1) as u64)
            .read_to_end(&mut buf)?;
        if buf.len() > MAX_BUNDLE_ENTRY_BYTES {
            return Err(RuntimeError::policy("Bundle ZIP entry exceeds decompressed limit"));
        }
        total = total.saturating_add(buf.len());
        if total > MAX_BUNDLE_TOTAL_BYTES {
            return Err(RuntimeError::policy("Bundle archive exceeds total size limit"));
        }
        entries.insert(clean, buf);
    }
    Ok((BundleFormat::Zip, entries))
}

fn remap_uuid_value(value: &mut Value, map: &BTreeMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(new_id) = map.get(s.as_str()) {
                *s = new_id.clone();
            }
        }
        Value::Array(items) => {
            for item in items {
                remap_uuid_value(item, map);
            }
        }
        Value::Object(obj) => {
            for (_, v) in obj.iter_mut() {
                remap_uuid_value(v, map);
            }
        }
        _ => {}
    }
}

fn inspect_target_sources(project: &Path, sources: &[String]) -> Vec<Value> {
    let mut drift = Vec::new();
    for src in sources {
        let rel = src.split('#').next().unwrap_or(src).trim();
        if rel.is_empty() || rel.starts_with("http://") || rel.starts_with("https://") {
            continue;
        }
        let clean = rel.replace('\\', "/");
        let clean = clean.strip_prefix("./").unwrap_or(&clean);
        let full = project.join(clean);
        let status = if fs::admit_descendant(project, &full).is_err() {
            "outside_target"
        } else if std::fs::symlink_metadata(&full)
            .map(|m| m.is_file() && !fs::linked(&m))
            .unwrap_or(false)
        {
            "present"
        } else {
            "missing_in_target"
        };
        if status != "present" {
            drift.push(object!({
                "path": clean,
                "target_status": status,
            }));
        }
    }
    drift
}

fn next_unique_mem_id(used: &mut BTreeSet<String>) -> String {
    let day = domain::clock::today().format("%Y%m%d").to_string();
    loop {
        let hex8 = &domain::uuid4().replace('-', "")[..8];
        let id = format!("MEM-{day}-{hex8}");
        if used.insert(id.clone()) {
            return id;
        }
    }
}

fn archive_transition_steps(kind: &str, from: &str) -> Vec<&'static str> {
    if domain::is_terminal(kind, from) {
        return vec![];
    }
    match (kind, from) {
        ("task", _) => vec!["cancelled"],
        ("finding", _) => vec!["superseded"],
        ("bug", _) => vec!["cancelled"],
        ("incident", "open") => vec!["cancelled"],
        ("incident", "contained" | "recovering") => vec!["resolved", "closed"],
        ("incident", "resolved") => vec!["closed"],
        ("decision", "proposed") => vec!["accepted", "superseded"],
        ("decision", "accepted") => vec!["superseded"],
        ("event", "recorded") => vec!["superseded"],
        _ => vec![],
    }
}

/// Validate and optionally apply a `context-graph-bundle-v1` import into the target repo.
pub fn import(service: &BankService, req: &ImportRequest) -> Result<Value> {
    let (format, entries) = read_bundle_entries(&req.source)?;
    let manifest_raw = entries.get("BUNDLE_MANIFEST.json").ok_or_else(|| {
        RuntimeError::invalid("Bundle is missing BUNDLE_MANIFEST.json")
    })?;
    let manifest: Value = serde_json::from_slice(manifest_raw).map_err(|_| {
        RuntimeError::invalid("BUNDLE_MANIFEST.json is not valid JSON")
    })?;
    if manifest["schema_version"].as_str() != Some(BUNDLE_SCHEMA_VERSION) {
        return Err(RuntimeError::invalid(format!(
            "Unsupported bundle schema_version (expected {BUNDLE_SCHEMA_VERSION})"
        )));
    }
    let artifacts = manifest["artifacts"]
        .as_array()
        .ok_or_else(|| RuntimeError::invalid("BUNDLE_MANIFEST.json artifacts must be an array"))?;

    // Verify every artifact digest, byte length, and secret/privacy gate BEFORE planning mutations.
    for item in artifacts {
        let path = item["path"]
            .as_str()
            .ok_or_else(|| RuntimeError::invalid("Artifact missing path"))?;
        let clean = validate_bundle_entry_path(path)?;
        let bytes = entries.get(&clean).ok_or_else(|| {
            RuntimeError::invalid(format!("Bundle artifact missing: {clean}"))
        })?;
        if let Some(expected_bytes) = item["bytes"].as_u64()
            && bytes.len() as u64 != expected_bytes
        {
            return Err(RuntimeError::policy(format!(
                "Bundle artifact byte-size mismatch for {clean}"
            )));
        }
        let expected_sha = item["sha256"].as_str().unwrap_or("");
        let actual_sha = sha256_hex(bytes);
        if expected_sha.len() != 64 || actual_sha != expected_sha {
            return Err(RuntimeError::policy(format!(
                "Bundle artifact SHA-256 mismatch for {clean}"
            )));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| RuntimeError::invalid(format!("Bundle artifact {clean} is not UTF-8")))?;
        if let Some(label) = domain::secret_label_in(&service.context.roots, text)? {
            return Err(RuntimeError::policy(format!(
                "Import refused: {clean} matches a {label} secret pattern"
            )));
        }
    }

    let source_repo = manifest["source"]["repo_name"]
        .as_str()
        .unwrap_or("unknown-repo")
        .to_string();
    let import_tag = format!("imported-from:{}", slug_token(&source_repo));
    let config = service.config()?;
    let allowed_privacy = config.strings("allowed_privacy")?;
    let actor = service.context.actor.clone();
    let now = domain::utc_now();

    let brain = BrainRepository::new(service.context.clone())?;
    let existing_brain = brain.inventory(true)?.records;
    let mut existing_brain_by_uuid = BTreeMap::<String, crate::RecordImage>::new();
    let mut existing_brain_by_ext = BTreeMap::<String, crate::RecordImage>::new();
    for img in existing_brain {
        if let Some(id) = img.record["id"].as_str() {
            existing_brain_by_uuid.insert(id.to_string(), img.clone());
        }
        if let Some(ext) = img.record["external_id"].as_str()
            && !ext.trim().is_empty()
        {
            existing_brain_by_ext.insert(ext.to_string(), img);
        }
    }

    let mut bank_files = PublicationFiles::new(&service.context.roots.state)?;
    let existing_chunk_paths = codec::paths(service)?;
    let mut existing_chunks_by_id = BTreeMap::<String, (String, codec::Chunk)>::new();
    for cpath in &existing_chunk_paths {
        if let Ok(c) = codec::read(&mut bank_files, cpath)
            && let Some(id) = c.metadata["id"].as_str()
        {
            existing_chunks_by_id.insert(id.to_string(), (cpath.clone(), c));
        }
    }
    let mut used_mem_ids: BTreeSet<String> = existing_chunks_by_id.keys().cloned().collect();

    #[derive(Clone)]
    struct PlannedBrain {
        orig_id: String,
        final_id: String,
        archived: bool,
        record: Value,
        supersede_existing_uuid: Option<String>,
        remapped: bool,
    }
    #[derive(Clone)]
    struct PlannedChunk {
        orig_id: String,
        final_id: String,
        dest_rel: String,
        metadata: Value,
        body: String,
        supersede_existing_id: Option<String>,
        remapped: bool,
    }

    let mut planned_brains = Vec::<PlannedBrain>::new();
    let mut planned_chunks = Vec::<PlannedChunk>::new();
    let mut planned_handoffs = Vec::<(String, Vec<u8>)>::new();
    let mut planned_episodes = Vec::<Value>::new();
    let mut uuid_remap = BTreeMap::<String, String>::new();
    let mut mem_remap = BTreeMap::<String, String>::new();
    let mut collisions = Vec::<Value>::new();
    let mut skipped = Vec::<Value>::new();
    let mut source_drift = Vec::<Value>::new();

    for item in artifacts {
        let kind = item["kind"].as_str().unwrap_or("");
        let path = item["path"].as_str().unwrap_or("");
        let raw = &entries[path];
        match kind {
            "brain-record" => {
                let text = std::str::from_utf8(raw)
                    .map_err(|_| RuntimeError::invalid("Brain record is not valid UTF-8"))?;
                let (mut record, _body) = domain::parse_markdown_record(text)?;
                let archived = item["archived"].as_bool().unwrap_or_else(|| path.contains("/archive/"));
                if archived && !req.include_archive {
                    skipped.push(object!({"kind":kind,"id":record["id"],"reason":"archived record omitted without include-archive"}));
                    continue;
                }
                let privacy = record["privacy"].as_str().unwrap_or("team");
                if !allowed_privacy.iter().any(|p| p == privacy) {
                    return Err(RuntimeError::policy(format!(
                        "Import refused: record {path} privacy '{privacy}' violates allowed_privacy"
                    )));
                }
                let orig_id = record["id"].as_str().unwrap_or("").to_string();
                let ext_id = record["external_id"].as_str().unwrap_or("").to_string();
                let sources = record["sources"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                    .unwrap_or_default();
                let drift = inspect_target_sources(&service.context.roots.project, &sources);
                if !drift.is_empty() {
                    source_drift.push(object!({"kind":kind,"id":orig_id,"path":path,"missing_or_outside":drift}));
                }

                let uuid_hit = existing_brain_by_uuid.get(&orig_id);
                let ext_hit = (!ext_id.is_empty())
                    .then(|| existing_brain_by_ext.get(&ext_id))
                    .flatten();
                let collision_reason = if uuid_hit.is_some() {
                    Some("uuid_exists")
                } else if ext_hit.is_some() {
                    Some("external_id_exists")
                } else {
                    None
                };

                let mut final_id = orig_id.clone();
                let mut supersede_existing_uuid = None;
                let mut remapped = false;
                if let Some(reason) = collision_reason {
                    collisions.push(object!({
                        "kind": kind,
                        "id": orig_id,
                        "external_id": ext_id,
                        "reason": reason,
                        "strategy": req.strategy.as_str(),
                    }));
                    match req.strategy {
                        ImportStrategy::Skip => {
                            skipped.push(object!({"kind":kind,"id":orig_id,"reason":reason}));
                            continue;
                        }
                        ImportStrategy::RemapIds => {
                            final_id = domain::uuid4();
                            remapped = true;
                            uuid_remap.insert(orig_id.clone(), final_id.clone());
                            record["id"] = Value::String(final_id.clone());
                            if ext_hit.is_some() && !ext_id.is_empty() {
                                record["external_id"] =
                                    Value::String(format!("{ext_id}-{}", &final_id[..8]));
                            }
                        }
                        ImportStrategy::ArchiveSuperseded => {
                            if let Some(hit) = uuid_hit.or(ext_hit) {
                                supersede_existing_uuid =
                                    hit.record["id"].as_str().map(String::from);
                            }
                            if uuid_hit.is_some() {
                                // Prevent active + archive sharing identical UUID.
                                final_id = domain::uuid4();
                                remapped = true;
                                uuid_remap.insert(orig_id.clone(), final_id.clone());
                                record["id"] = Value::String(final_id.clone());
                            }
                        }
                    }
                }

                // Re-attribute ownership & record import origin provenance cleanly within dynamic-record schema.
                record["owner"] = Value::String(actor.clone());
                record["authorized_owners"] = object!([actor.clone()]);
                record["updated_at"] = Value::String(now.clone());
                let prov_note = format!("[Imported via {import_tag} (origin_id={orig_id})]");
                let prev_prog = record["progress"].as_str().unwrap_or("").trim();
                record["progress"] = Value::String(if prev_prog.is_empty() {
                    prov_note
                } else if prev_prog.contains(&prov_note) {
                    prev_prog.to_string()
                } else {
                    format!("{prev_prog}\n{prov_note}")
                });
                if let Some(old_uuid) = &supersede_existing_uuid
                    && domain::is_uuid4(old_uuid)
                {
                    // Handled after batch remaps below so existing target UUID is preserved.
                }
                domain::validate_record(&record, false)?;
                domain::validate_schema_file(
                    &service.context.roots,
                    "dynamic-record.schema.json",
                    &record,
                )?;
                planned_brains.push(PlannedBrain {
                    orig_id,
                    final_id,
                    archived,
                    record,
                    supersede_existing_uuid,
                    remapped,
                });
            }
            "memory-chunk" => {
                let (mut meta, body) = codec::parse_raw(raw)?;
                let orig_id = meta["id"].as_str().unwrap_or("").to_string();
                let title = meta["title"].as_str().unwrap_or("imported").to_string();
                let sources = meta["sources"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                    .unwrap_or_default();
                let drift = inspect_target_sources(&service.context.roots.project, &sources);
                let has_missing_sources = !drift.is_empty();
                if has_missing_sources {
                    source_drift.push(object!({"kind":kind,"id":orig_id,"path":path,"missing_or_outside":drift}));
                }

                let existing_hit = existing_chunks_by_id.get(&orig_id);
                let mut final_id = orig_id.clone();
                let mut supersede_existing_id = None;
                let mut remapped = false;
                if existing_hit.is_some() {
                    collisions.push(object!({
                        "kind": kind,
                        "id": orig_id,
                        "reason": "memory_id_exists",
                        "strategy": req.strategy.as_str(),
                    }));
                    match req.strategy {
                        ImportStrategy::Skip => {
                            skipped.push(object!({"kind":kind,"id":orig_id,"reason":"memory_id_exists"}));
                            continue;
                        }
                        ImportStrategy::RemapIds => {
                            final_id = next_unique_mem_id(&mut used_mem_ids);
                            remapped = true;
                            mem_remap.insert(orig_id.clone(), final_id.clone());
                            meta["id"] = Value::String(final_id.clone());
                        }
                        ImportStrategy::ArchiveSuperseded => {
                            if existing_hit.is_some_and(|(_, c)| {
                                ["active", "needs-review"]
                                    .contains(&c.metadata["status"].as_str().unwrap_or(""))
                            }) {
                                supersede_existing_id = Some(orig_id.clone());
                            }
                            final_id = next_unique_mem_id(&mut used_mem_ids);
                            remapped = true;
                            mem_remap.insert(orig_id.clone(), final_id.clone());
                            meta["id"] = Value::String(final_id.clone());
                        }
                    }
                }
                let mut tags = meta["tags"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                    .unwrap_or_default();
                if !tags.iter().any(|t| t == &import_tag) {
                    tags.push(import_tag.clone());
                }
                if has_missing_sources {
                    if meta["status"] == "active" {
                        meta["status"] = Value::String("needs-review".into());
                    }
                    if !tags.iter().any(|t| t == "needs-source-rebind") {
                        tags.push("needs-source-rebind".into());
                    }
                }
                meta["tags"] = object!(tags);
                let slug = codec::chunk_slug(&title)?;
                let dest_rel = format!("memory-bank/chunks/{final_id}-{slug}.md");
                planned_chunks.push(PlannedChunk {
                    orig_id,
                    final_id,
                    dest_rel,
                    metadata: meta,
                    body,
                    supersede_existing_id,
                    remapped,
                });
            }
            "handoff" if req.include_handoffs => {
                let clean = validate_bundle_entry_path(path)?;
                planned_handoffs.push((clean, raw.clone()));
            }
            "episodes" if req.include_episodes => {
                if let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(raw) {
                    planned_episodes.extend(items);
                }
            }
            _ => {}
        }
    }

    // Apply internal cross-reference remaps across Brain records & Bank chunks inside the imported batch.
    if !uuid_remap.is_empty() || !mem_remap.is_empty() {
        for pb in &mut planned_brains {
            for field in ["conflicts", "supersedes", "superseded_by"] {
                if let Some(val) = pb.record.get_mut(field) {
                    remap_uuid_value(val, &uuid_remap);
                }
            }
        }
        for pc in &mut planned_chunks {
            for field in ["supersedes", "superseded_by"] {
                if let Some(val) = pc.metadata.get_mut(field) {
                    remap_uuid_value(val, &mem_remap);
                }
            }
        }
    }
    for pb in &mut planned_brains {
        if let Some(old_uuid) = &pb.supersede_existing_uuid
            && domain::is_uuid4(old_uuid)
            && old_uuid != &pb.final_id
        {
            let mut sup = pb.record["supersedes"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                .unwrap_or_default();
            if !sup.contains(old_uuid) {
                sup.push(old_uuid.clone());
            }
            pb.record["supersedes"] = object!(sup);
        }
    }
    for pc in &mut planned_chunks {
        if let Some(old_id) = &pc.supersede_existing_id
            && old_id != &pc.final_id
        {
            let mut sup = pc.metadata["supersedes"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                .unwrap_or_default();
            if !sup.contains(old_id) {
                sup.push(old_id.clone());
            }
            pc.metadata["supersedes"] = object!(sup);
        }
    }

    let preview_items: Vec<Value> = planned_brains
        .iter()
        .map(|pb| {
            object!({
                "kind": "brain-record",
                "original_id": pb.orig_id,
                "final_id": pb.final_id,
                "remapped": pb.remapped,
                "archived": pb.archived,
                "supersedes_existing": pb.supersede_existing_uuid,
                "owner": actor,
            })
        })
        .chain(planned_chunks.iter().map(|pc| {
            object!({
                "kind": "memory-chunk",
                "original_id": pc.orig_id,
                "final_id": pc.final_id,
                "remapped": pc.remapped,
                "path": pc.dest_rel,
                "supersedes_existing": pc.supersede_existing_id,
            })
        }))
        .collect();

    if req.dry_run {
        return Ok(object!({
            "schema_version": BUNDLE_SCHEMA_VERSION,
            "dry_run": true,
            "format": format.as_str(),
            "source_repo": source_repo,
            "source_commit": manifest["source"]["commit"],
            "strategy": req.strategy.as_str(),
            "actor": actor,
            "counts": {
                "would_import_brain": planned_brains.len(),
                "would_import_chunks": planned_chunks.len(),
                "would_import_handoffs": planned_handoffs.len(),
                "would_import_episodes": planned_episodes.len(),
                "skipped": skipped.len(),
                "collisions": collisions.len(),
                "source_drift": source_drift.len(),
            },
            "planned": preview_items,
            "skipped": skipped,
            "collisions": collisions,
            "source_drift": source_drift,
        }));
    }

    // Atomic transactional commit across filesystem + SQLite.
    let mut brain_lock = Some(brain.lock()?);
    let mut snapshot_targets = vec![
        service.context.roots.brain_root().join("indexes/active.json"),
        service.context.roots.brain_root().join("indexes/archive.json"),
        service.context.roots.state.join("memory-bank/INDEX.md"),
        service.context.roots.state.join("memory-bank/README.md"),
    ];
    for pb in &planned_brains {
        let dest = if pb.archived {
            brain.archive_path(&pb.record)?
        } else {
            brain.dynamic_path(&pb.record)?
        };
        snapshot_targets.push(dest);
        if pb.record["type"] == "task" {
            snapshot_targets.push(brain.handoff_path(&pb.final_id)?);
        }
        if let Some(old_uuid) = &pb.supersede_existing_uuid
            && let Some(old_img) = existing_brain_by_uuid.get(old_uuid)
        {
            snapshot_targets.push(old_img.path.clone());
            snapshot_targets.push(brain.archive_path(&old_img.record)?);
        }
    }
    for pc in &planned_chunks {
        snapshot_targets.push(service.context.roots.state.join(&pc.dest_rel));
        if let Some(old_id) = &pc.supersede_existing_id
            && let Some((old_path, _)) = existing_chunks_by_id.get(old_id)
        {
            snapshot_targets.push(service.context.roots.state.join(old_path));
        }
    }
    for (rel, _) in &planned_handoffs {
        snapshot_targets.push(service.context.roots.state.join(rel));
    }
    let snapshot = fs::snapshot(snapshot_targets)?;

    let write_res = (|| -> Result<()> {
        for pb in &planned_brains {
            if let Some(old_uuid) = &pb.supersede_existing_uuid
                && let Some(old_img) = existing_brain_by_uuid.get(old_uuid)
            {
                let mut archived_rec = old_img.record.clone();
                let rec_kind = archived_rec["type"].as_str().unwrap_or("").to_string();
                let mut cur_status = archived_rec["status"].as_str().unwrap_or("").to_string();
                let steps = archive_transition_steps(&rec_kind, &cur_status);
                for next_status in steps {
                    if let Some(arr) = archived_rec["transitions"].as_array_mut() {
                        arr.push(object!({
                            "from": cur_status.clone(),
                            "to": next_status,
                            "at": now.clone(),
                            "actor": actor.clone(),
                            "reason": format!("Superseded by imported record {}", pb.final_id),
                        }));
                    }
                    archived_rec["status"] = Value::String(next_status.into());
                    cur_status = next_status.into();
                }
                if domain::is_uuid4(&pb.final_id) && archived_rec.get("superseded_by").is_some() {
                    archived_rec["superseded_by"] = Value::String(pb.final_id.clone());
                }
                archived_rec["updated_at"] = Value::String(now.clone());
                archived_rec["revision"] =
                    (archived_rec["revision"].as_u64().unwrap_or(1) + 1).into();
                let arch_dest = brain.archive_path(&archived_rec)?;
                if old_img.path != arch_dest && old_img.path.exists() {
                    std::fs::remove_file(&old_img.path)?;
                }
                brain.write_record(&arch_dest, &archived_rec)?;
            }
            let dest = if pb.archived {
                brain.archive_path(&pb.record)?
            } else {
                brain.dynamic_path(&pb.record)?
            };
            brain.write_record(&dest, &pb.record)?;
        }
        brain.rebuild_indexes()?;

        for pc in &planned_chunks {
            if let Some(old_id) = &pc.supersede_existing_id
                && let Some((old_rel, old_chunk)) = existing_chunks_by_id.get(old_id)
            {
                let mut old_meta = old_chunk.metadata.clone();
                old_meta["status"] = Value::String("superseded".into());
                old_meta["superseded_by"] = Value::String(pc.final_id.clone());
                let raw_old = codec::render_chunk(&old_meta, &old_chunk.body)?;
                let old_abs = service.context.roots.state.join(old_rel);
                fs::atomic_write(&old_abs, &raw_old)?;
            }
            let rendered = codec::render_chunk(&pc.metadata, &pc.body)?;
            let dest_abs = service.context.roots.state.join(&pc.dest_rel);
            fs::admit_descendant(&service.context.roots.state, &dest_abs)?;
            fs::atomic_write(&dest_abs, &rendered)?;
        }
        if !planned_chunks.is_empty() {
            let readme_path = service.context.roots.state.join("memory-bank/README.md");
            if std::fs::symlink_metadata(&readme_path).is_err() {
                fs::admit_descendant(&service.context.roots.state, &readme_path)?;
                fs::atomic_write(
                    &readme_path,
                    b"# Memory Bank\n\nDurable repository knowledge chunks indexed in `INDEX.md`. Verify cited sources before relying on claims, especially chunks tagged `imported-from:*` or `needs-review`.\n",
                )?;
            }
            brain_lock.take();
            codec::reindex(service)?;
        }

        for (rel, bytes) in &planned_handoffs {
            let dest = service.context.roots.state.join(rel);
            fs::admit_descendant(&service.context.roots.state, &dest)?;
            fs::atomic_write(&dest, bytes)?;
        }
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::restore(&snapshot);
        return Err(err);
    }

    let db_path = crate::storage::default_database(&service.context.roots)?;
    let mut store = Store::open(&db_path, OpenOptions::default())?;
    let mut imported_episodes = 0usize;
    if !planned_episodes.is_empty() {
        store.transaction(|conn| {
            for ep in &planned_episodes {
                let summary = ep["summary"].as_str().unwrap_or("").trim();
                let outcome = ep["outcome"].as_str().unwrap_or("").trim();
                let created_at = ep["created_at"].as_str().unwrap_or("");
                if summary.is_empty() || outcome.is_empty() {
                    continue;
                }
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM episodes WHERE summary=?1 AND outcome=?2 AND created_at=?3",
                    rusqlite::params![summary, outcome, created_at],
                    |r| r.get(0),
                )?;
                if exists == 0 {
                    let files = serde_json::to_string(&ep["files"]).unwrap_or_else(|_| "[]".into());
                    let verif = serde_json::to_string(&ep["verification"]).unwrap_or_else(|_| "[]".into());
                    let srcs = serde_json::to_string(&ep["sources"]).unwrap_or_else(|_| "[]".into());
                    let ts = if created_at.is_empty() { now.as_str() } else { created_at };
                    conn.execute(
                        "INSERT INTO episodes(summary,outcome,files,verification,sources,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                        rusqlite::params![summary, outcome, files, verif, srcs, ts],
                    )?;
                    imported_episodes += 1;
                }
            }
            Ok(())
        })?;
    }

    let idx_report = index::refresh_index(&mut store, &service.context, false)?;
    let gstats = graph::stats(&store.conn)?;

    Ok(object!({
        "schema_version": BUNDLE_SCHEMA_VERSION,
        "dry_run": false,
        "format": format.as_str(),
        "source_repo": source_repo,
        "source_commit": manifest["source"]["commit"],
        "strategy": req.strategy.as_str(),
        "actor": actor,
        "counts": {
            "imported_brain": planned_brains.len(),
            "imported_chunks": planned_chunks.len(),
            "imported_handoffs": planned_handoffs.len(),
            "imported_episodes": imported_episodes,
            "skipped": skipped.len(),
            "collisions": collisions.len(),
            "source_drift": source_drift.len(),
        },
        "imported": preview_items,
        "skipped": skipped,
        "collisions": collisions,
        "source_drift": source_drift,
        "index_documents": idx_report.documents,
        "graph": gstats,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateRecord, EpisodeInput, Roots, StartTask, UpdateRecord};

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cross_repo_export_import_dry_run_and_all_strategies() {
        let src_dir = TempDir(std::env::temp_dir().join(format!(
            "cg-bundle-src-{}",
            domain::uuid4()
        )));
        let dst_dir = TempDir(std::env::temp_dir().join(format!(
            "cg-bundle-dst-{}",
            domain::uuid4()
        )));
        let bundle_zip = std::env::temp_dir().join(format!(
            "cg-bundle-{}.cgb.zip",
            domain::uuid4()
        ));

        std::fs::create_dir_all(src_dir.0.join("src")).unwrap();
        std::fs::write(src_dir.0.join("src/policy.rs"), "pub fn rule() {}\n").unwrap();

        let src_roots = Roots::installed(&src_dir.0).unwrap();
        let src_ctx = RuntimeContext::new(src_roots.clone(), "alice", "governed").unwrap();
        let brain = BrainRepository::new(src_ctx.clone()).unwrap();
        let rec = brain
            .create(CreateRecord {
                record_type: "finding".into(),
                external_id: "FIND-BUNDLE-1".into(),
                title: "Session tokens expire in 15m".into(),
                sources: vec!["src/policy.rs".into()],
                authority: "verified".into(),
                attestation: Some("person".into()),
                ..CreateRecord::default()
            })
            .unwrap();
        let rec_uuid = rec["id"].as_str().unwrap().to_string();
        brain
            .update(
                &rec_uuid,
                UpdateRecord {
                    expected_revision: rec["revision"].as_u64(),
                    progress: Some("Verified 15-minute expiry rule.".into()),
                    transition_to: Some("resolved".into()),
                    reason: "Checked code".into(),
                    attestation: Some("person".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        // Also create a restricted secret record that MUST NOT be exported.
        let _secret_rec = brain
            .create(CreateRecord {
                record_type: "decision".into(),
                external_id: "PRIV-1".into(),
                title: "Private decision excluded".into(),
                privacy: "restricted".into(),
                sources: vec!["src/policy.rs".into()],
                ..CreateRecord::default()
            })
            .unwrap();

        let src_bank = BankService::new(src_ctx.clone());
        let prop = src_bank
            .propose(
                std::slice::from_ref(&rec_uuid),
                "Token expiration window",
                "Issue short-lived 15m access tokens.",
                false,
            )
            .unwrap();
        let rev = BankService::new(RuntimeContext::new(src_roots.clone(), "bob", "governed").unwrap());
        rev.review(prop["id"].as_str().unwrap(), Some("bob"), true)
            .unwrap();
        rev.apply(prop["id"].as_str().unwrap()).unwrap();

        let db_path = crate::storage::default_database(&src_roots).unwrap();
        let mut store = Store::open(&db_path, OpenOptions::default()).unwrap();
        store
            .start(StartTask {
                task_id: "TASK-EP".into(),
                goal: "Check expiry".into(),
                ..Default::default()
            })
            .unwrap();
        store
            .record_episode(EpisodeInput {
                summary: "Audited token TTL".into(),
                outcome: "Confirmed 15m limit".into(),
                files: vec![],
                verification: vec!["cargo test".into()],
                sources: vec!["src/policy.rs".into()],
            })
            .unwrap();

        let exported = export(
            &src_bank,
            &ExportRequest {
                destination: bundle_zip.clone(),
                format: Some(BundleFormat::Zip),
                include_archive: true,
                include_superseded: false,
                include_handoffs: true,
                include_episodes: true,
                root: None,
                depth: 3,
                force: true,
            },
        )
        .unwrap();
        assert_eq!(exported["schema_version"], BUNDLE_SCHEMA_VERSION);
        assert_eq!(exported["counts"]["brain_records"], 1);
        assert_eq!(exported["counts"]["memory_chunks"], 1);
        assert_eq!(exported["counts"]["excluded"], 1);
        assert_eq!(exported["counts"]["episodes"], 1);

        // Target repo starts empty (notice src/policy.rs does NOT exist -> cross-repo drift!).
        std::fs::create_dir_all(&dst_dir.0).unwrap();
        let dst_roots = Roots::installed(&dst_dir.0).unwrap();
        let dst_ctx = RuntimeContext::new(dst_roots.clone(), "charlie", "governed").unwrap();
        let dst_bank = BankService::new(dst_ctx.clone());

        // Dry-run writes nothing, flags source_drift cleanly.
        let dry = import(
            &dst_bank,
            &ImportRequest {
                source: bundle_zip.clone(),
                strategy: ImportStrategy::Skip,
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["counts"]["would_import_brain"], 1);
        assert_eq!(dry["counts"]["would_import_chunks"], 1);
        assert_eq!(dry["counts"]["source_drift"], 2);
        assert!(!dst_dir.0.join("project-brain/dynamic").exists());

        // Real import into target repo -> re-attributed to charlie, tagged imported-from:...
        let applied = import(
            &dst_bank,
            &ImportRequest {
                source: bundle_zip.clone(),
                strategy: ImportStrategy::Skip,
                dry_run: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(applied["counts"]["imported_brain"], 1);
        assert_eq!(applied["counts"]["imported_chunks"], 1);
        assert_eq!(applied["counts"]["imported_episodes"], 1);
        let imported_brain = BrainRepository::new(dst_ctx.clone())
            .unwrap()
            .inventory(true)
            .unwrap();
        assert_eq!(imported_brain.records.len(), 1);
        assert_eq!(imported_brain.records[0].record["owner"], "charlie");

        // Second import with Skip -> 0 imported, 2 skipped.
        let skipped_res = import(
            &dst_bank,
            &ImportRequest {
                source: bundle_zip.clone(),
                strategy: ImportStrategy::Skip,
                dry_run: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(skipped_res["counts"]["skipped"], 2);
        assert_eq!(skipped_res["counts"]["imported_brain"], 0);

        // Third import with RemapIds -> remaps IDs and adds new copies cleanly.
        let remapped_res = import(
            &dst_bank,
            &ImportRequest {
                source: bundle_zip.clone(),
                strategy: ImportStrategy::RemapIds,
                dry_run: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(remapped_res["counts"]["imported_brain"], 1);
        assert_eq!(remapped_res["counts"]["imported_chunks"], 1);
        assert_eq!(
            BrainRepository::new(dst_ctx.clone())
                .unwrap()
                .inventory(true)
                .unwrap()
                .records
                .len(),
            2
        );

        // Fourth import with ArchiveSuperseded -> supersedes existing records cleanly.
        let superseded_res = import(
            &dst_bank,
            &ImportRequest {
                source: bundle_zip.clone(),
                strategy: ImportStrategy::ArchiveSuperseded,
                dry_run: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(superseded_res["counts"]["imported_brain"], 1);
        let validation = dst_bank.validate().unwrap();
        assert!(validation.is_empty(), "{validation:#?}");

        let _ = std::fs::remove_file(&bundle_zip);
    }
}
