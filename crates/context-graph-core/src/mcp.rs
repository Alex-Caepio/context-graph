//! Native stdio JSON-RPC memory & context-graph MCP server.
use crate::{
    BankService, ErrorKind, MemoryService, OpenOptions, RefreshRequest, Result, Roots,
    RuntimeConfig, RuntimeContext, RuntimeError, Store,
    bundle::{self, BundleFormat, ExportRequest, ImportRequest, ImportStrategy},
    domain, fs,
    graph::{self, Direction, GraphQuery},
    handoff::{HandoffService, SaveHandoff},
    index,
    results::{self, RecordResult, ResultService, Revision},
    retrieval::RetrievalRequest,
    serving::TaskService,
    storage,
};
use serde_json::{Value, json as object, value::RawValue};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
};

pub const VERSION: &str = "2025-06-18";
pub const MAX_MESSAGE: usize = 128 * 1024;
pub const MAX_DEPTH: usize = 64;

pub struct McpServer {
    pub context: RuntimeContext,
    pub host: String,
}

impl McpServer {
    pub fn new(context: RuntimeContext, host: impl Into<String>) -> Result<Self> {
        let value = Self {
            context,
            host: host.into(),
        };
        value.authorize_context(&value.context, false)?;
        Ok(value)
    }

    fn authorize_context(&self, ctx: &RuntimeContext, require_governed: bool) -> Result<RuntimeConfig> {
        let config = RuntimeConfig::load(&ctx.roots)?;
        if require_governed
            && (ctx.mode != "governed"
                || config.value["mode"] != "governed"
                || std::env::var("PROJECT_BRAIN_MODE").is_ok_and(|mode| mode != "governed"))
        {
            return Err(RuntimeError::policy("Operation requires governed mode"));
        }
        if !config
            .strings("owners")?
            .iter()
            .any(|owner| owner == "*" || owner == &ctx.actor)
        {
            return Err(RuntimeError::new(
                ErrorKind::Unauthorized,
                "The configured actor is outside the allowed owner scope",
            ));
        }
        for name in ["project-brain", "memory-bank", ".context-handoff"] {
            let root = ctx.roots.state.join(name);
            admit_tree(&root)?;
        }
        Ok(config)
    }

    fn is_plugin_dir(dir: &Path) -> bool {
        dir.join(".codex-plugin/plugin.json").is_file()
            || dir.join(".claude-plugin/plugin.json").is_file()
    }

    fn resolve_context(&self, tool_name: &str, input: &Value) -> Result<RuntimeContext> {
        let project_override = input
            .get("project")
            .or_else(|| {
                if tool_name != "context_graph_query" {
                    input.get("root")
                } else {
                    None
                }
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(path) = project_override else {
            if Self::is_plugin_dir(&self.context.roots.project) {
                return Err(RuntimeError::invalid(
                    "Pass explicit 'project' repository path; MCP server cwd is the plugin directory",
                ));
            }
            return Ok(self.context.clone());
        };
        let raw_path = Path::new(path);
        let candidate = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            self.context.roots.project.join(raw_path)
        };
        let root = fs::canonicalize(&candidate)?;
        if !root.is_dir() {
            return Err(RuntimeError::invalid("project path must be an existing directory"));
        }
        if Self::is_plugin_dir(&root) {
            return Err(RuntimeError::invalid(
                "Pass target repository path for 'project', not the plugin directory",
            ));
        }
        let roots = Roots::installed(&root)?;
        let config = RuntimeConfig::load(&roots)?;
        let mode = config.mode(None)?;
        RuntimeContext::new(roots, self.context.actor.clone(), mode)
    }

    pub fn call(&self, name: &str, input: &Value) -> Result<Value> {
        let _source_frame = crate::git::source_frame_scope();
        let object = input
            .as_object()
            .ok_or_else(|| RuntimeError::invalid("Tool arguments must be an object"))?;
        let spec = tools()
            .into_iter()
            .find(|spec| spec["name"] == name)
            .ok_or_else(|| RuntimeError::invalid(format!("Unknown tool: {name}")))?;
        let properties = spec["inputSchema"]["properties"]
            .as_object()
            .ok_or_else(|| RuntimeError::invalid("Invalid tool schema"))?;
        if object.keys().any(|key| !properties.contains_key(key))
            || spec["inputSchema"]["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|key| !object.contains_key(key))
        {
            return Err(RuntimeError::invalid(
                "Missing or unknown tool arguments",
            ));
        }

        let ctx = self.resolve_context(name, input)?;
        let governed_required = matches!(
            name,
            "memory_checkpoint" | "memory_record_result" | "memory_retrieve"
        );
        self.authorize_context(&ctx, governed_required)?;

        let results = ResultService::new(ctx.clone());
        if name == "memory_source_fingerprints" {
            let paths = results::strings(&input["sources"], "sources", 10, 1024)?;
            return Ok(object!({"source_fingerprints": results.checked_fingerprints(&paths)?}));
        }

        let database = storage::default_database(&ctx.roots)?;
        if name == "memory_status" {
            let mut store = Store::open(&database, OpenOptions::default())?;
            index::refresh_index(&mut store, &ctx, true)?;
            let mut status = crate::health::status(&ctx, &store, &database, None)?;
            status["graph"] = graph::stats(&store.conn)?;
            return Ok(status);
        }

        if name == "context_graph_query" {
            let mut store = Store::open(&database, OpenOptions::default())?;
            index::refresh_index(&mut store, &ctx, true)?;
            let root_id = input
                .get("root_id")
                .or_else(|| input.get("node"))
                .or_else(|| input.get("root"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let max_depth = input
                .get("depth")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(2)
                .min(10);
            let kinds = input
                .get("kinds")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let direction = Direction::parse(
                input
                    .get("direction")
                    .and_then(Value::as_str)
                    .unwrap_or("both"),
            )?;
            let include_archived = input
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let limit = input
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(200)
                .clamp(1, 5000);
            let result = graph::query_graph(
                &store.conn,
                &GraphQuery {
                    root: root_id,
                    max_depth,
                    kinds,
                    direction,
                    include_archived,
                    limit,
                },
            )?;
            if input.get("format").and_then(Value::as_str) == Some("dot") {
                let dot = graph::export_dot(&result);
                return Ok(object!({
                    "format": "dot",
                    "dot": dot,
                    "summary": result.summary,
                    "resolved_root": result.resolved_root,
                }));
            }
            return Ok(serde_json::to_value(&result)?);
        }

        if name == "context_graph_links" {
            let mut store = Store::open(&database, OpenOptions::default())?;
            index::refresh_index(&mut store, &ctx, true)?;
            let target = input["target"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RuntimeError::invalid("target must be a non-empty string"))?;
            let include_archived = input
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return graph::node_links(&store.conn, target, include_archived);
        }

        if name == "context_export_bundle" {
            let dest_raw = input["destination"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RuntimeError::invalid("destination must be a non-empty string"))?;
            let dest_path = resolve_io_path(&ctx.roots.project, dest_raw);
            let format = input
                .get("format")
                .and_then(Value::as_str)
                .map(|s| BundleFormat::from_destination(&dest_path, Some(s)))
                .transpose()?;
            let req = ExportRequest {
                destination: dest_path,
                format,
                include_archive: input
                    .get("include_archive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                include_superseded: input
                    .get("include_superseded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                include_handoffs: input
                    .get("include_handoffs")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                include_episodes: input
                    .get("include_episodes")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                root: input
                    .get("subgraph_root")
                    .or_else(|| input.get("node"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                depth: input
                    .get("depth")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(2)
                    .min(10),
                force: input.get("force").and_then(Value::as_bool).unwrap_or(false),
            };
            let bank = BankService::new(ctx);
            return bundle::export(&bank, &req);
        }

        if name == "context_import_bundle" {
            let src_raw = input["source"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RuntimeError::invalid("source must be a non-empty string"))?;
            let src_path = resolve_io_path(&ctx.roots.project, src_raw);
            let strategy = ImportStrategy::parse(input.get("strategy").and_then(Value::as_str))?;
            let req = ImportRequest {
                source: src_path,
                strategy,
                dry_run: input.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
                include_archive: input
                    .get("include_archive")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                include_handoffs: input
                    .get("include_handoffs")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                include_episodes: input
                    .get("include_episodes")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            };
            let bank = BankService::new(ctx);
            return bundle::import(&bank, &req);
        }

        if name == "context_handoff_save" {
            return call_handoff_save(&ctx, &self.host, input);
        }

        if name == "context_handoff_load" {
            return call_handoff_load(&ctx, input);
        }

        let task = results::task_id(
            input["task_id"]
                .as_str()
                .ok_or_else(|| RuntimeError::invalid("task_id must be text"))?,
        )?;
        if name == "memory_retrieve" {
            match results.task(&task, true) {
                Ok(_) => {}
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let query = results::text(&input["query"], "query", 4000)?;
            let paths = if let Some(paths) = input.get("paths") {
                results::strings(paths, "paths", 10, 1024)?
            } else {
                vec![]
            };
            for path in &paths {
                results.source(path, true)?;
            }
            let result = MemoryService::new(ctx.clone(), None).refresh(RefreshRequest {
                capsule: Some(RetrievalRequest {
                    query,
                    task_id: Some(task.clone()),
                    host: self.host.clone(),
                    entry_point: "refresh".into(),
                    gate: Some("off".into()),
                    ephemeral: true,
                    manifest_scope: "local".into(),
                    capsule_profile: Some("full".into()),
                    paths,
                    allow_unprovisioned: true,
                    ..RetrievalRequest::default()
                }),
                host: Some(self.host.clone()),
                task_id: Some(task),
                capsule_profile: Some("full".into()),
                ..RefreshRequest::default()
            })?;
            if !crate::application::refresh_succeeded(&result) {
                return Err(RuntimeError::unavailable(
                    "Memory refresh is incomplete; check runtime health",
                ));
            }
            if !result["capsule"].is_object() {
                return Err(RuntimeError::unavailable(
                    "Memory capsule is unavailable; check runtime health",
                ));
            }
            return Ok(result);
        }
        if name == "memory_checkpoint" {
            let goal = input
                .get("goal")
                .map(|value| results::text(value, "goal", 200))
                .transpose()?;
            let record =
                TaskService::new(ctx.clone(), None).explicit(&task, goal.as_deref())?;
            let uuid = record["id"].as_str().unwrap_or_default().to_owned();
            let service = MemoryService::new(ctx.clone(), None);
            service.execute("rebind", &object!({"task_id": task, "record_id": uuid}), &[])?;
            let flushed = service.execute("turn", &object!({"task_id": task, "flush": true}), &[])?;
            let current = crate::BrainRepository::new(ctx)?.get_task(&uuid)?;
            return Ok(object!({
                "task_id": task,
                "task_uuid": current["id"],
                "revision": current["revision"],
                "checkpoint": flushed,
            }));
        }
        if name != "memory_record_result" {
            return Err(RuntimeError::invalid(format!("Unknown tool: {name}")));
        }
        let revision = input["revision"]
            .as_u64()
            .filter(|revision| *revision > 0)
            .ok_or_else(|| {
                RuntimeError::invalid("Supply the current task revision as a positive integer")
            })?;
        if object
            .get("progress")
            .is_some_and(|value| !value.is_string())
        {
            return Err(RuntimeError::invalid("progress must be text"));
        }
        let mut data = object!({});
        for key in [
            "progress",
            "next_steps",
            "learnings",
            "verified",
            "capture_decision",
        ] {
            if let Some(value) = input.get(key) {
                data[key] = value.clone();
            }
        }
        results.record(RecordResult {
            task_id: task,
            result_id: input["result_id"]
                .as_str()
                .ok_or_else(|| RuntimeError::invalid("result_id must be text"))?
                .into(),
            revision: Revision::Exact(revision),
            data,
            reason: "Saved through Memory MCP; agent-attested, not reviewed by a person".into(),
            attestation: "agent".into(),
            prepared: None,
            replay_only: false,
        })
    }

    pub fn serve<R: BufRead, W: Write>(&self, mut incoming: R, mut outgoing: W) -> Result<()> {
        let (mut initialized, mut ready) = (false, false);
        loop {
            let mut raw = Vec::new();
            let count = (&mut incoming)
                .take(MAX_MESSAGE as u64 + 1)
                .read_until(b'\n', &mut raw)?;
            if count == 0 {
                break;
            }
            let oversized = raw.len() > MAX_MESSAGE;
            let mut identifier = "null".to_string();
            let outcome = (|| -> std::result::Result<Option<Value>, i64> {
                if oversized || too_deep(&raw) {
                    return Err(-32700);
                }
                let members = rpc_members(&raw).map_err(|_| -32700)?;
                let version = members
                    .get("jsonrpc")
                    .and_then(|value| serde_json::from_str::<String>(value.get()).ok());
                let method = members
                    .get("method")
                    .and_then(|value| serde_json::from_str::<String>(value.get()).ok());
                if version.as_deref() != Some("2.0") || method.is_none() {
                    return Err(-32600);
                }
                if let Some(id) = members.get("id") {
                    let raw_id = id.get();
                    if !valid_identifier(raw_id) {
                        return Err(-32600);
                    }
                    identifier = raw_id.into();
                }
                let method = method.unwrap_or_default();
                let params = members
                    .get("params")
                    .map(|value| {
                        crate::strict_json::parse(
                            value.get().as_bytes(),
                            MAX_MESSAGE,
                            MAX_DEPTH,
                            MAX_MESSAGE,
                        )
                    })
                    .transpose()
                    .map_err(|_| -32700)?
                    .unwrap_or_else(|| object!({}));
                if !params.is_object()
                    || params.get("_meta").is_some_and(|value| !value.is_object())
                {
                    return Err(-32602);
                }
                if method == "notifications/initialized" && !members.contains_key("id") {
                    ready = initialized;
                    return Ok(None);
                }
                if !members.contains_key("id") {
                    return Ok(None);
                }
                let value = match method.as_str() {
                    "initialize" => {
                        if initialized || !params["protocolVersion"].is_string() {
                            return Err(-32602);
                        }
                        initialized = true;
                        object!({
                            "protocolVersion": VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "context-graph", "version": "0.1.0"},
                            "instructions": "Use memory_retrieve before complex work and material decisions, context_graph_query / context_graph_links for multi-hop provenance and impact analysis, context_export_bundle / context_import_bundle for portable bundle transfer, and context_handoff_save / context_handoff_load for portable session continuity. Never store transcripts or secrets."
                        })
                    }
                    "ping" => object!({}),
                    _ if !ready => return Err(-32602),
                    "tools/list" => {
                        if params
                            .as_object()
                            .is_some_and(|map| map.keys().any(|key| key != "_meta"))
                        {
                            return Err(-32602);
                        }
                        object!({"tools": tools()})
                    }
                    "tools/call" => {
                        let name = params["name"].as_str().ok_or(-32602)?;
                        let arguments = params
                            .get("arguments")
                            .cloned()
                            .unwrap_or_else(|| object!({}));
                        match self.call(name, &arguments) {
                            Ok(value) => {
                                object!({
                                    "content": [{
                                        "type": "text",
                                        "text": domain::json::python_default(&value, false, false, false).map_err(|_| -32602)?
                                    }],
                                    "structuredContent": value,
                                    "isError": false
                                })
                            }
                            Err(error) => {
                                let code = error
                                    .code
                                    .as_deref()
                                    .and_then(|code| {
                                        code.strip_prefix("memory-result-v1:")
                                            .or_else(|| code.strip_prefix("native-result:"))
                                    })
                                    .filter(|code| {
                                        !code.is_empty()
                                            && code.bytes().all(|byte| {
                                                byte.is_ascii_lowercase()
                                                    || byte.is_ascii_digit()
                                                    || byte == b'_'
                                            })
                                    });
                                let detail = domain::secret_label(&error.message)
                                    .ok()
                                    .flatten()
                                    .map(|_| String::from("redacted"))
                                    .unwrap_or_else(|| error.message.clone());
                                let message = format!(
                                    "Context operation failed{}: {detail}",
                                    code.map(|c| format!(" ({c})")).unwrap_or_default(),
                                );
                                object!({
                                    "content": [{"type": "text", "text": message}],
                                    "isError": true
                                })
                            }
                        }
                    }
                    _ => return Err(-32601),
                };
                Ok(Some(object!({"result": value})))
            })();
            let response = match outcome {
                Ok(None) => continue,
                Ok(Some(value)) => format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{identifier},\"result\":{}}}\n",
                    domain::json::canonical(&value["result"])?
                ),
                Err(code) => {
                    let message = match code {
                        -32600 => "Invalid JSON-RPC request",
                        -32601 => "Method not found",
                        -32602 => "Invalid request parameters or initialization state",
                        _ => "Invalid JSON-RPC message",
                    };
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{identifier},\"error\":{{\"code\":{code},\"message\":\"{message}\"}}}}\n"
                    )
                }
            };
            outgoing.write_all(response.as_bytes())?;
            outgoing.flush()?;
            if oversized {
                break;
            }
        }
        Ok(())
    }
}

fn resolve_io_path(project: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        project.join(p)
    }
}

fn call_handoff_save(ctx: &RuntimeContext, host: &str, input: &Value) -> Result<Value> {
    let detail = input
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("summary")
        .to_string();
    let topic = input
        .get("topic")
        .and_then(Value::as_str)
        .map(String::from);
    let task_id = input
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let default_client = match host {
        "claude" | "codex" | "cursor" => host,
        _ => "other",
    };
    let source_client = input
        .get("source_client")
        .and_then(Value::as_str)
        .unwrap_or(default_client)
        .to_string();

    let mut curated = object!({
        "goal": input["goal"].clone(),
        "summary": input["summary"].clone(),
        "next_steps": input["next_steps"].clone(),
    });
    for key in [
        "constraints",
        "decisions",
        "progress",
        "verification",
        "open_questions",
        "files",
    ] {
        if let Some(v) = input.get(key) {
            curated[key] = v.clone();
        }
    }

    let id = format!(
        "handoff-{}-{}",
        domain::clock::today().format("%Y%m%d"),
        &domain::uuid4().replace('-', "")[..8]
    );
    let output_path = if let Some(out) = input.get("output").and_then(Value::as_str) {
        resolve_io_path(&ctx.roots.project, out)
    } else {
        let task_folder = task_id
            .as_deref()
            .map(|t| {
                t.chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "handoffs".into());
        let dir = ctx.roots.project.join("tasks").join(task_folder);
        fs::ensure_directory(&dir, false)?;
        dir.join(format!("{id}.md"))
    };
    let handoff_dir = ctx.roots.state.join(".context-handoff");
    fs::ensure_directory(&handoff_dir, false)?;

    let saved = HandoffService::new(ctx.clone()).save(SaveHandoff {
        input: curated,
        output: output_path.clone(),
        detail,
        topic,
        task_id: task_id.clone(),
        source_client,
        transcript: None,
    })?;

    // Write portable JSON index entry + optional merge lineage so Context Graph links it cleanly.
    let merge_from: Vec<String> = input
        .get("merge_from")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();

    let handoff_json_path = handoff_dir.join(format!("{id}.json"));
    let handoff_meta = object!({
        "id": id,
        "task_id": task_id,
        "title": input["goal"],
        "summary": input["summary"],
        "files": input.get("files").cloned().unwrap_or(object!([])),
        "sources": input.get("sources").cloned().unwrap_or(object!([])),
        "merged_from": merge_from,
        "markdown_path": output_path.strip_prefix(&ctx.roots.state).ok().and_then(|p| p.to_str()).unwrap_or(""),
        "created_at": domain::utc_now(),
    });
    let text_json = serde_json::to_string_pretty(&handoff_meta)?;
    if let Some(label) = domain::secret_label_in(&ctx.roots, &text_json)? {
        return Err(RuntimeError::policy(format!(
            "{label} detected in handoff metadata"
        )));
    }
    fs::atomic_write(&handoff_json_path, format!("{text_json}\n").as_bytes())?;

    if !merge_from.is_empty() {
        let merges_dir = handoff_dir.join("merges");
        fs::ensure_directory(&merges_dir, false)?;
        let merge_id = format!("merge-{}", &domain::uuid4().replace('-', "")[..8]);
        let mut sources = merge_from;
        sources.push(id.clone());
        let merge_doc = object!({
            "id": merge_id,
            "task_id": task_id,
            "title": format!("Merged continuity into {id}"),
            "merged_from": sources,
            "created_at": domain::utc_now(),
        });
        fs::atomic_write(
            &merges_dir.join(format!("{merge_id}.json")),
            format!("{}\n", serde_json::to_string_pretty(&merge_doc)?).as_bytes(),
        )?;
    }

    let db_path = storage::default_database(&ctx.roots)?;
    let mut store = Store::open(&db_path, OpenOptions::default())?;
    let _ = graph::sync_graph(&mut store, ctx);

    let mut out = saved;
    out["handoff_id"] = Value::String(id);
    Ok(out)
}

fn call_handoff_load(ctx: &RuntimeContext, input: &Value) -> Result<Value> {
    let include_transcript = input
        .get("include_transcript")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(path_str) = input
        .get("input")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let p = resolve_io_path(&ctx.roots.project, path_str);
        return HandoffService::new(ctx.clone()).load(&p, include_transcript);
    }

    let filter_task = input
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let filter_id = input
        .get("handoff_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut dirs = vec![
        ctx.roots.state.join(".context-handoff"),
        ctx.roots.state.join("project-brain/handoffs"),
        ctx.roots.state.join("project-brain/control/handoffs"),
    ];
    let tasks_dir = ctx.roots.project.join("tasks");
    if fs::admit_directory(&tasks_dir)? {
        for entry in std::fs::read_dir(&tasks_dir)?.filter_map(std::result::Result::ok) {
            let p = entry.path();
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir() && !fs::linked(&m)) {
                dirs.push(p);
            }
        }
        dirs.push(tasks_dir);
    }
    let mut candidates = Vec::<PathBuf>::new();
    for dir in dirs {
        if !fs::admit_directory(&dir)? {
            continue;
        }
        let mut entries = std::fs::read_dir(&dir)?
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect::<Vec<_>>();
        entries.sort();
        candidates.extend(entries);
    }
    candidates.reverse();

    let svc = HandoffService::new(ctx.clone());
    let mut loaded = Vec::new();
    for path in candidates {
        if let Some(wanted) = filter_id
            && !path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem == wanted || stem.contains(wanted))
        {
            continue;
        }
        if let Ok(item) = svc.load(&path, include_transcript) {
            if let Some(task) = filter_task {
                let meta_task = item["metadata"]["task_id"].as_str().unwrap_or("");
                let ext_task = item["metadata"]["external_id"].as_str().unwrap_or("");
                if meta_task != task && ext_task != task {
                    continue;
                }
            }
            loaded.push(item);
            if input.get("latest").and_then(Value::as_bool).unwrap_or(true) {
                return Ok(loaded.remove(0));
            }
            if loaded.len() >= 10 {
                break;
            }
        }
    }

    if loaded.is_empty() {
        return Ok(object!({
            "found": false,
            "handoffs": [],
        }));
    }
    Ok(object!({
        "found": true,
        "count": loaded.len(),
        "handoffs": loaded,
    }))
}

fn rpc_members(raw: &[u8]) -> std::result::Result<BTreeMap<String, Box<RawValue>>, ()> {
    use serde::{
        Deserialize, Deserializer,
        de::{Error as _, MapAccess, Visitor},
    };
    std::str::from_utf8(raw).map_err(|_| ())?;
    struct Envelope(BTreeMap<String, Box<RawValue>>);
    impl<'de> Deserialize<'de> for Envelope {
        fn deserialize<D: Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct Fields;
            impl<'de> Visitor<'de> for Fields {
                type Value = Envelope;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a JSON-RPC object")
                }
                fn visit_map<A: MapAccess<'de>>(
                    self,
                    mut input: A,
                ) -> std::result::Result<Envelope, A::Error> {
                    let mut members = BTreeMap::new();
                    while let Some(key) = input.next_key::<String>()? {
                        if members.contains_key(&key) {
                            return Err(A::Error::custom("Duplicate JSON-RPC field."));
                        }
                        let value = input.next_value::<Box<RawValue>>()?;
                        members.insert(key, value);
                    }
                    Ok(Envelope(members))
                }
            }
            deserializer.deserialize_map(Fields)
        }
    }
    let mut parser = serde_json::Deserializer::from_slice(raw);
    let Envelope(members) = Envelope::deserialize(&mut parser).map_err(|_| ())?;
    parser.end().map_err(|_| ())?;
    for (key, value) in &members {
        if !["id", "jsonrpc", "method"].contains(&key.as_str()) {
            crate::strict_json::parse(value.get().as_bytes(), MAX_MESSAGE, MAX_DEPTH, MAX_MESSAGE)
                .map_err(|_| ())?;
        }
    }
    Ok(members)
}

fn valid_identifier(value: &str) -> bool {
    if value == "null" {
        return true;
    }
    if value.starts_with('"') {
        return value.ends_with('"');
    }
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn too_deep(raw: &[u8]) -> bool {
    let (mut depth, mut string, mut escaped) = (0usize, false, false);
    for byte in raw {
        if string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                string = false;
            }
        } else if *byte == b'"' {
            string = true;
        } else if *byte == b'[' || *byte == b'{' {
            depth += 1;
            if depth > MAX_DEPTH {
                return true;
            }
        } else if *byte == b']' || *byte == b'}' {
            depth = depth.saturating_sub(1);
        }
    }
    false
}

fn admit_tree(path: &std::path::Path) -> Result<()> {
    if !crate::fs::admit_directory(path)? {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if crate::fs::linked_at(&entry.path(), &metadata)? {
            return Err(RuntimeError::policy("Memory storage contains a link"));
        }
        if metadata.is_dir() {
            admit_tree(&entry.path())?;
        }
    }
    Ok(())
}

pub fn tools() -> Vec<Value> {
    let string = object!({"type": "string"});
    let boolean = object!({"type": "boolean"});
    let string_array = object!({"type": "array", "items": string});
    let strings3 = object!({"type": "array", "items": string, "maxItems": 3});
    let source_image = object!({
        "type": "array",
        "minItems": 1,
        "maxItems": 10,
        "items": {
            "type": "object",
            "required": ["path", "sha256"],
            "additionalProperties": false,
            "properties": {
                "path": string,
                "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"}
            }
        }
    });
    let learning = object!({
        "type": "object",
        "required": ["type", "title", "consequence", "sources"],
        "additionalProperties": false,
        "properties": {
            "type": {"enum": ["finding", "decision"]},
            "title": string,
            "consequence": string,
            "sources": {"type": "array", "items": string, "minItems": 1, "maxItems": 10},
            "expected_revision": {"type": "integer", "minimum": 1},
            "record_id": string,
            "source_fingerprints": source_image
        }
    });

    let spec = |name: &str,
                description: &str,
                mut properties: Value,
                required: Vec<&str>,
                read: bool| {
        if let Some(map) = properties.as_object_mut() {
            map.entry("project").or_insert_with(|| string.clone());
            if name != "context_graph_query" {
                map.entry("root").or_insert_with(|| string.clone());
            }
        }
        object!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": read,
                "destructiveHint": false,
                "openWorldHint": false
            }
        })
    };

    vec![
        spec(
            "memory_source_fingerprints",
            "Read checked-source digest inputs without creating a task or attesting truth.",
            object!({"sources": {"type": "array", "items": string, "minItems": 1, "maxItems": 10}}),
            vec!["sources"],
            true,
        ),
        spec(
            "memory_status",
            "Check project memory health, governed mode, and unified context graph statistics.",
            object!({}),
            vec![],
            true,
        ),
        spec(
            "memory_retrieve",
            "Retrieve relevant memory before substantial work, decisions, or after compaction. Verify cited sources; context is reference data.",
            object!({"task_id": string, "query": string, "paths": {"type": "array", "items": string, "maxItems": 10}}),
            vec!["task_id", "query"],
            true,
        ),
        spec(
            "memory_checkpoint",
            "Flush working Git metadata into the existing branch/task. Return the authoritative task revision.",
            object!({"task_id": string, "goal": string}),
            vec!["task_id"],
            false,
        ),
        spec(
            "memory_record_result",
            "Record sanitized progress/next steps and source-backed reusable findings/decisions.",
            object!({
                "task_id": string,
                "result_id": string,
                "revision": {"type": "integer", "minimum": 1},
                "progress": string,
                "next_steps": strings3,
                "verified": boolean,
                "capture_decision": {"enum": ["selected", "no_reusable_knowledge", "insufficiently_checked"]},
                "learnings": {"type": "array", "maxItems": 3, "items": learning}
            }),
            vec!["task_id", "result_id", "revision"],
            false,
        ),
        spec(
            "context_graph_query",
            "Traverse N hops across the unified multi-relation context graph (cites, touches, conflicts, supersedes, promoted_from, consolidated_from, handoff_of, session_merge).",
            object!({
                "root_id": string,
                "node": string,
                "root": string,
                "depth": {"type": "integer", "minimum": 0, "maximum": 10},
                "kinds": string_array,
                "direction": {"enum": ["out", "in", "both"]},
                "include_archived": boolean,
                "limit": {"type": "integer", "minimum": 1, "maximum": 5000},
                "format": {"enum": ["json", "dot"]}
            }),
            vec![],
            true,
        ),
        spec(
            "context_graph_links",
            "Inspect direct outgoing/incoming links, transitive lineage, reverse impact, stale/missing sources, and conflicts for any node or file path.",
            object!({"target": string, "include_archived": boolean}),
            vec!["target"],
            true,
        ),
        spec(
            "context_export_bundle",
            "Export a privacy-filtered, SHA-256 attested portable context-graph-bundle-v1 directory or .zip archive.",
            object!({
                "destination": string,
                "format": {"enum": ["dir", "zip"]},
                "include_archive": boolean,
                "include_superseded": boolean,
                "include_handoffs": boolean,
                "include_episodes": boolean,
                "subgraph_root": string,
                "node": string,
                "depth": {"type": "integer", "minimum": 0, "maximum": 10},
                "force": boolean
            }),
            vec!["destination"],
            false,
        ),
        spec(
            "context_import_bundle",
            "Validate, check collisions and source drift, and import a portable context-graph-bundle-v1 directory or .zip archive with strategy skip, remap-ids, or archive-superseded.",
            object!({
                "source": string,
                "strategy": {"enum": ["skip", "remap-ids", "archive-superseded"]},
                "dry_run": boolean,
                "include_archive": boolean,
                "include_handoffs": boolean,
                "include_episodes": boolean
            }),
            vec!["source"],
            false,
        ),
        spec(
            "context_handoff_save",
            "Save a curated, secret-checked portable context handoff with optional session merge lineage.",
            object!({
                "goal": string,
                "summary": string,
                "next_steps": string_array,
                "task_id": string,
                "constraints": string_array,
                "decisions": string_array,
                "progress": string_array,
                "verification": string_array,
                "open_questions": string_array,
                "files": string_array,
                "sources": string_array,
                "merge_from": string_array,
                "output": string,
                "detail": {"enum": ["summary", "topic", "full"]},
                "topic": string,
                "source_client": {"enum": ["claude", "codex", "cursor", "other"]}
            }),
            vec!["goal", "summary", "next_steps"],
            false,
        ),
        spec(
            "context_handoff_load",
            "Load and drift-check a saved portable context handoff by file path, handoff_id, or task_id.",
            object!({
                "input": string,
                "handoff_id": string,
                "task_id": string,
                "latest": boolean,
                "include_transcript": boolean
            }),
            vec![],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn mcp_server_lists_11_tools_and_executes_graph_and_handoff_roundtrip() {
        let root = TempDir(std::env::temp_dir().join(format!(
            "cg-mcp-test-{}",
            domain::uuid4()
        )));
        std::fs::create_dir_all(root.0.join("src")).unwrap();
        std::fs::write(root.0.join("src/lib.rs"), "pub fn init() {}\n").unwrap();

        let roots = Roots::installed(&root.0).unwrap();
        let ctx = RuntimeContext::new(roots, "tester", "governed").unwrap();
        let server = McpServer::new(ctx, "claude").unwrap();

        let requests = [
            object!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": VERSION}}),
            object!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            object!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
            object!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "context_handoff_save",
                "arguments": {
                    "goal": "Ship context-graph MCP",
                    "summary": "Verified graph and bundle pipeline",
                    "next_steps": ["Run full integration suite"],
                    "files": ["src/lib.rs"],
                    "task_id": "TASK-MCP-1"
                }
            }}),
            object!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
                "name": "context_graph_query",
                "arguments": {"depth": 2}
            }}),
            object!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
                "name": "context_handoff_load",
                "arguments": {"task_id": "TASK-MCP-1", "latest": true}
            }}),
        ];

        let wire = requests
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect::<String>();
        let mut out = Vec::new();
        server.serve(Cursor::new(wire.as_bytes()), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(
            lines[1]["result"]["tools"].as_array().unwrap().len(),
            11
        );
        assert_eq!(lines[2]["result"]["isError"], false, "{:#}", lines[2]);
        assert_eq!(lines[3]["result"]["isError"], false, "{:#}", lines[3]);
        assert!(
            !lines[3]["result"]["structuredContent"]["edges"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(lines[4]["result"]["isError"], false, "{:#}", lines[4]);
    }
}
