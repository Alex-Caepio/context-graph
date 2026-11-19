//! CLI parser and dispatcher for `context-graph-engine`.
use crate::{collect, hooks};
use context_graph_core::{
    BankService, McpServer, MemoryService, OpenOptions, Result, Roots, RuntimeConfig,
    RuntimeContext, RuntimeError, Store,
    bundle::{self, BundleFormat, ExportRequest, ImportRequest, ImportStrategy},
    graph::{self, Direction, GraphQuery},
    index, storage,
};
use serde_json::{Map, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

const VERSION: &str = "0.1.0";
const MAX_STDIN_BYTES: usize = 6 * 1024 * 1024;

pub fn help_text() -> &'static str {
    "\
context-graph-engine 0.1.0
Standalone unified Context Graph, Project Brain, Memory Bank, handoff, and bundle runtime.

USAGE:
  context-graph-engine <COMMAND> [OPTIONS]

COMMANDS:
  graph <query|links|export-dot|sync>   Traverse N-hop context graph, lineage, impact, or DOT
  export                                Export portable privacy-filtered context-graph-bundle-v1
  import                                Validate/dry-run/import a context-graph-bundle-v1 bundle
  memory [GLOBAL_OPTS] <OP> [ARGS]      Project Brain, Memory Bank, working memory, handoffs
  hook [claude|codex|cursor] <ROLE>     Run native working-memory / continuity hook from stdin
  collect-context [OPTIONS]             Wrap code2prompt safely for context collection
  mcp [OPTIONS]                         Run stdio JSON-RPC 2.0 MCP server (11 tools)
  --help, -h                            Show help
  --version, -V                         Print version

GRAPH SUBCOMMANDS:
  graph query [--root DIR] [--node ID] [--depth N] [--direction out|in|both]
              [--kinds K1,K2] [--include-archived] [--limit N] [--dot] [--json]
  graph links --target ID [--root DIR] [--include-archived] [--json]
  graph export-dot [--root DIR] [--node ID] [--depth N] [--direction out|in|both]
                   [--kinds K1,K2] [--include-archived]
  graph sync [--root DIR] [--json]

EXPORT / IMPORT OPTIONS:
  export --destination PATH [--root DIR] [--format dir|zip] [--subgraph-root ID]
         [--depth N] [--include-archive] [--include-superseded] [--no-handoffs]
         [--no-episodes] [--force] [--json]
  import --source PATH [--root DIR] [--strategy skip|remap-ids|archive-superseded]
         [--dry-run] [--no-archive] [--no-handoffs] [--no-episodes] [--json]
"
}

fn is_plugin_dir(dir: &Path) -> bool {
    dir.join(".codex-plugin/plugin.json").is_file()
        || dir.join(".claude-plugin/plugin.json").is_file()
}

fn read_stdin_capped() -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    let stdin = std::io::stdin().lock();
    stdin
        .take(MAX_STDIN_BYTES as u64 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > MAX_STDIN_BYTES {
        return Err(RuntimeError::invalid("Standard input exceeds size limit"));
    }
    Ok(raw)
}

fn next_val<'a>(args: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str> {
    if let Some((_, val)) = args[*i].split_once('=') {
        return Ok(val);
    }
    *i += 1;
    args.get(*i)
        .map(String::as_str)
        .ok_or_else(|| RuntimeError::invalid(format!("Option {flag} requires a value")))
}

fn split_flag_arg(arg: &str) -> (&str, Option<&str>) {
    if let Some((k, v)) = arg.split_once('=') {
        (k, Some(v))
    } else {
        (arg, None)
    }
}

fn actor_from_env(explicit: Option<&str>) -> String {
    explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            std::env::var("PROJECT_BRAIN_OWNER")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "local".into())
}

fn build_context(
    root: Option<&Path>,
    project: Option<&Path>,
    owner: Option<&str>,
    mode_override: Option<&str>,
    allow_plugin_cwd: bool,
) -> Result<RuntimeContext> {
    let cwd = std::env::current_dir()?;
    let raw_state = root.unwrap_or(&cwd);
    let raw_state_abs = if raw_state.is_absolute() {
        raw_state.to_path_buf()
    } else {
        cwd.join(raw_state)
    };
    let roots = if root.is_none() && project.is_none() && allow_plugin_cwd && is_plugin_dir(&raw_state_abs)
    {
        Roots::installed(&raw_state_abs)?
    } else if let Some(proj) = project {
        Roots::discover(&raw_state_abs, proj)?
    } else {
        Roots::installed(&raw_state_abs)?
    };
    let cfg = RuntimeConfig::load(&roots)?;
    let mode = cfg.mode(mode_override)?;
    let actor = actor_from_env(owner);
    RuntimeContext::new(roots, actor, mode)
}

fn print_json(value: &Value) -> Result<i32> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(0)
}

fn run_mcp(args: &[String]) -> Result<i32> {
    let mut host = std::env::var("CONTEXT_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if std::env::var_os("CODEX_THREAD_ID").is_some()
                || std::env::var_os("CODEX_HOME").is_some()
            {
                "codex".into()
            } else {
                "claude".into()
            }
        });
    let mut root: Option<PathBuf> = None;
    let mut project: Option<PathBuf> = None;
    let mut owner: Option<String> = None;
    let mut mode: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, _) = split_flag_arg(&args[i]);
        match flag {
            "--host" => host = next_val(args, &mut i, flag)?.into(),
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--project" => project = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--owner" | "--actor" => owner = Some(next_val(args, &mut i, flag)?.into()),
            "--mode" => mode = Some(next_val(args, &mut i, flag)?.into()),
            "-h" | "--help" => {
                println!(
                    "context-graph-engine mcp [--host claude|codex] [--root DIR] [--project DIR] [--owner ACTOR]"
                );
                return Ok(0);
            }
            other => return Err(RuntimeError::invalid(format!("Unknown mcp option: {other}"))),
        }
        i += 1;
    }

    let ctx = build_context(
        root.as_deref(),
        project.as_deref(),
        owner.as_deref(),
        mode.as_deref(),
        true,
    )?;
    let server = McpServer::new(ctx, host)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server.serve(stdin.lock(), stdout.lock())?;
    Ok(0)
}

fn run_graph(args: &[String]) -> Result<i32> {
    let sub = args.first().map(String::as_str).unwrap_or("query");
    let start_idx = if matches!(sub, "query" | "links" | "export-dot" | "sync" | "-h" | "--help") {
        1
    } else {
        0
    };
    if matches!(sub, "-h" | "--help") {
        println!("{}", help_text());
        return Ok(0);
    }

    let mut root: Option<PathBuf> = None;
    let mut project: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut node: Option<String> = None;
    let mut target: Option<String> = None;
    let mut depth: usize = 2;
    let mut direction = Direction::Both;
    let mut kinds: Vec<String> = Vec::new();
    let mut include_archived = false;
    let mut limit: usize = 200;
    let mut as_dot = sub == "export-dot";
    let mut as_json = false;

    let mut i = start_idx;
    while i < args.len() {
        let raw = &args[i];
        let (flag, _) = split_flag_arg(raw);
        match flag {
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--project" => project = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--db" => db = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--node" | "--root-id" => node = Some(next_val(args, &mut i, flag)?.into()),
            "--target" => target = Some(next_val(args, &mut i, flag)?.into()),
            "--depth" => {
                depth = next_val(args, &mut i, flag)?
                    .parse::<usize>()
                    .map_err(|_| RuntimeError::invalid("--depth must be a non-negative integer"))?;
            }
            "--direction" => direction = Direction::parse(next_val(args, &mut i, flag)?)?,
            "--kinds" | "--kind" => {
                for k in next_val(args, &mut i, flag)?.split(',') {
                    let t = k.trim();
                    if !t.is_empty() {
                        kinds.push(t.into());
                    }
                }
            }
            "--limit" => {
                limit = next_val(args, &mut i, flag)?
                    .parse::<usize>()
                    .map_err(|_| RuntimeError::invalid("--limit must be a positive integer"))?
                    .clamp(1, 5000);
            }
            "--include-archived" => include_archived = true,
            "--dot" => as_dot = true,
            "--json" => as_json = true,
            other if !other.starts_with('-') => {
                if sub == "links" && target.is_none() {
                    target = Some(other.into());
                } else if node.is_none() {
                    node = Some(other.into());
                }
            }
            other => return Err(RuntimeError::invalid(format!("Unknown graph option: {other}"))),
        }
        i += 1;
    }

    let ctx = build_context(root.as_deref(), project.as_deref(), None, None, false)?;
    let db_path = db.unwrap_or(storage::default_database(&ctx.roots)?);
    let mut store = Store::open(&db_path, OpenOptions::default())?;
    index::refresh_index(&mut store, &ctx, true)?;

    if sub == "sync" {
        let stats = graph::sync_graph(&mut store, &ctx)?;
        return print_json(&stats);
    }

    if sub == "links" {
        let tgt = target
            .or_else(|| node.clone())
            .ok_or_else(|| RuntimeError::invalid("graph links requires --target <id-or-path>"))?;
        let res = graph::node_links(&store.conn, &tgt, include_archived)?;
        if as_json {
            return print_json(&res);
        }
        return print_json(&res);
    }

    let res = graph::query_graph(
        &store.conn,
        &GraphQuery {
            root: node,
            max_depth: depth,
            kinds,
            direction,
            include_archived,
            limit,
        },
    )?;

    if as_dot {
        let dot = graph::export_dot(&res);
        print!("{dot}");
        return Ok(0);
    }

    if as_json {
        return print_json(&serde_json::to_value(&res)?);
    }

    println!(
        "Context Graph ({} nodes, {} edges, direction={}, depth={}):",
        res.nodes.len(),
        res.edges.len(),
        res.direction,
        res.max_depth
    );
    if let Some(root_id) = &res.resolved_root {
        println!("  root: {root_id}");
    }
    for n in res.nodes.iter().take(25) {
        println!("  [{}] {} ({})", n.kind, n.id, n.freshness);
    }
    for e in res.edges.iter().take(40) {
        println!("  {} --[{}]--> {}", e.src, e.kind, e.dst);
    }
    Ok(0)
}

fn run_export(args: &[String]) -> Result<i32> {
    let mut root: Option<PathBuf> = None;
    let mut destination: Option<PathBuf> = None;
    let mut format: Option<String> = None;
    let mut include_archive = false;
    let mut include_superseded = false;
    let mut include_handoffs = true;
    let mut include_episodes = true;
    let mut subgraph_root: Option<String> = None;
    let mut depth: usize = 2;
    let mut force = false;
    let mut as_json = false;
    let mut owner: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, _) = split_flag_arg(&args[i]);
        match flag {
            "-h" | "--help" => {
                println!("{}", help_text());
                return Ok(0);
            }
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--destination" | "--dest" | "-o" | "--output" => {
                destination = Some(PathBuf::from(next_val(args, &mut i, flag)?));
            }
            "--format" => format = Some(next_val(args, &mut i, flag)?.into()),
            "--include-archive" => include_archive = true,
            "--include-superseded" => include_superseded = true,
            "--no-handoffs" => include_handoffs = false,
            "--no-episodes" => include_episodes = false,
            "--subgraph-root" | "--node" => {
                subgraph_root = Some(next_val(args, &mut i, flag)?.into());
            }
            "--depth" => {
                depth = next_val(args, &mut i, flag)?
                    .parse::<usize>()
                    .map_err(|_| RuntimeError::invalid("--depth must be non-negative"))?;
            }
            "--owner" => owner = Some(next_val(args, &mut i, flag)?.into()),
            "--force" => force = true,
            "--json" => as_json = true,
            other if !other.starts_with('-') && destination.is_none() => {
                destination = Some(PathBuf::from(other));
            }
            other => return Err(RuntimeError::invalid(format!("Unknown export option: {other}"))),
        }
        i += 1;
    }

    let dest = destination
        .ok_or_else(|| RuntimeError::invalid("export requires --destination <path>"))?;
    let ctx = build_context(root.as_deref(), None, owner.as_deref(), None, false)?;
    let dest_abs = if dest.is_absolute() {
        dest
    } else {
        ctx.roots.project.join(dest)
    };
    let fmt = format
        .as_deref()
        .map(|s| BundleFormat::from_destination(&dest_abs, Some(s)))
        .transpose()?;
    let bank = BankService::new(ctx);
    let result = bundle::export(
        &bank,
        &ExportRequest {
            destination: dest_abs,
            format: fmt,
            include_archive,
            include_superseded,
            include_handoffs,
            include_episodes,
            root: subgraph_root,
            depth,
            force,
        },
    )?;

    if as_json {
        return print_json(&result);
    }
    println!(
        "Exported {} bundle to {} (brain={}, chunks={}, handoffs={}, episodes={}, graph={}/{}, excluded={})",
        result["format"].as_str().unwrap_or("dir"),
        result["destination"].as_str().unwrap_or(""),
        result["counts"]["brain_records"],
        result["counts"]["memory_chunks"],
        result["counts"]["handoffs"],
        result["counts"]["episodes"],
        result["counts"]["graph_nodes"],
        result["counts"]["graph_edges"],
        result["counts"]["excluded"],
    );
    Ok(0)
}

fn run_import(args: &[String]) -> Result<i32> {
    let mut root: Option<PathBuf> = None;
    let mut source: Option<PathBuf> = None;
    let mut strategy: Option<String> = None;
    let mut dry_run = false;
    let mut include_archive = true;
    let mut include_handoffs = true;
    let mut include_episodes = true;
    let mut as_json = false;
    let mut owner: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, _) = split_flag_arg(&args[i]);
        match flag {
            "-h" | "--help" => {
                println!("{}", help_text());
                return Ok(0);
            }
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--source" | "--input" | "-i" => {
                source = Some(PathBuf::from(next_val(args, &mut i, flag)?));
            }
            "--strategy" => strategy = Some(next_val(args, &mut i, flag)?.into()),
            "--dry-run" => dry_run = true,
            "--no-archive" => include_archive = false,
            "--no-handoffs" => include_handoffs = false,
            "--no-episodes" => include_episodes = false,
            "--owner" => owner = Some(next_val(args, &mut i, flag)?.into()),
            "--json" => as_json = true,
            other if !other.starts_with('-') && source.is_none() => {
                source = Some(PathBuf::from(other));
            }
            other => return Err(RuntimeError::invalid(format!("Unknown import option: {other}"))),
        }
        i += 1;
    }

    let src = source.ok_or_else(|| RuntimeError::invalid("import requires --source <path>"))?;
    let ctx = build_context(root.as_deref(), None, owner.as_deref(), None, false)?;
    let src_abs = if src.is_absolute() {
        src
    } else {
        ctx.roots.project.join(src)
    };
    let strat = ImportStrategy::parse(strategy.as_deref())?;
    let bank = BankService::new(ctx);
    let result = bundle::import(
        &bank,
        &ImportRequest {
            source: src_abs,
            strategy: strat,
            dry_run,
            include_archive,
            include_handoffs,
            include_episodes,
        },
    )?;

    if as_json {
        return print_json(&result);
    }
    if dry_run {
        println!(
            "Dry-run import ({} from {}; strategy={}): would_import(brain={}, chunks={}, handoffs={}, episodes={}), skipped={}, collisions={}, source_drift={}",
            result["format"].as_str().unwrap_or(""),
            result["source_repo"].as_str().unwrap_or(""),
            result["strategy"].as_str().unwrap_or(""),
            result["counts"]["would_import_brain"],
            result["counts"]["would_import_chunks"],
            result["counts"]["would_import_handoffs"],
            result["counts"]["would_import_episodes"],
            result["counts"]["skipped"],
            result["counts"]["collisions"],
            result["counts"]["source_drift"],
        );
    } else {
        println!(
            "Imported {} bundle from {} (strategy={}): brain={}, chunks={}, handoffs={}, episodes={}, skipped={}, source_drift={}",
            result["format"].as_str().unwrap_or(""),
            result["source_repo"].as_str().unwrap_or(""),
            result["strategy"].as_str().unwrap_or(""),
            result["counts"]["imported_brain"],
            result["counts"]["imported_chunks"],
            result["counts"]["imported_handoffs"],
            result["counts"]["imported_episodes"],
            result["counts"]["skipped"],
            result["counts"]["source_drift"],
        );
    }
    Ok(0)
}

fn run_hook(args: &[String]) -> Result<i32> {
    let mut positional: Vec<String> = Vec::new();
    let mut root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, _) = split_flag_arg(&args[i]);
        match flag {
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            other if !other.starts_with('-') => positional.push(other.to_string()),
            other => return Err(RuntimeError::invalid(format!("Unknown hook option: {other}"))),
        }
        i += 1;
    }

    let (host, role) = match positional.as_slice() {
        [host, role] => (host.clone(), role.clone()),
        [role] => {
            let h = if std::env::var_os("CODEX_THREAD_ID").is_some()
                || std::env::var_os("CODEX_HOME").is_some()
            {
                "codex"
            } else {
                "claude"
            };
            (h.into(), role.clone())
        }
        _ => {
            return Err(RuntimeError::invalid(
                "hook usage: context-graph-engine hook [claude|codex|cursor] <role> [--root DIR]",
            ));
        }
    };

    let raw = read_stdin_capped()?;
    let root_path = root.unwrap_or_else(|| PathBuf::from("."));
    let out = hooks::execute(&root_path, &host, &role, &raw)?;
    Ok(out.write()?)
}

fn run_collect(args: &[String]) -> Result<i32> {
    let mut req = collect::CollectRequest {
        root: PathBuf::from("."),
        ..Default::default()
    };
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        let (flag, _) = split_flag_arg(&args[i]);
        match flag {
            "-h" | "--help" => {
                println!("{}", help_text());
                return Ok(0);
            }
            "--root" => req.root = PathBuf::from(next_val(args, &mut i, flag)?),
            "--include" | "--paths" | "--path" => {
                for pat in next_val(args, &mut i, flag)?.split(',') {
                    let t = pat.trim();
                    if !t.is_empty() {
                        req.includes.push(t.into());
                    }
                }
            }
            "--exclude" => {
                for pat in next_val(args, &mut i, flag)?.split(',') {
                    let t = pat.trim();
                    if !t.is_empty() {
                        req.excludes.push(t.into());
                    }
                }
            }
            "--format" => req.format = next_val(args, &mut i, flag)?.into(),
            "--encoding" => req.encoding = next_val(args, &mut i, flag)?.into(),
            "--output" | "-o" => req.output = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--top" => {
                req.top = next_val(args, &mut i, flag)?
                    .parse::<usize>()
                    .map_err(|_| RuntimeError::invalid("--top must be a non-negative integer"))?;
            }
            "--dry-run" => req.dry_run = true,
            "--stdout" => req.stdout_bundle = true,
            "--json" => as_json = true,
            other => {
                return Err(RuntimeError::invalid(format!(
                    "Unknown collect-context option: {other}"
                )));
            }
        }
        i += 1;
    }

    let val = collect::execute(&req)?;
    if let Some(cli) = val.get("_cli") {
        let code = cli["code"].as_i64().unwrap_or(0) as i32;
        if let Some(s) = cli["stdout"].as_str() {
            print!("{s}");
        }
        if let Some(s) = cli["stderr"].as_str() {
            eprint!("{s}");
        }
        return Ok(code);
    }
    if as_json || req.dry_run {
        return print_json(&val);
    }
    println!(
        "Collected {} files ({} tokens) -> {}",
        val["result"]["files"],
        val["result"]["tokens"],
        val["output"].as_str().unwrap_or("")
    );
    Ok(0)
}

fn push_string_list(map: &mut Map<String, Value>, key: &str, value: String) {
    let arr = map
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut();
    if let Some(arr) = arr {
        arr.push(Value::String(value));
    }
}

fn run_memory(args: &[String]) -> Result<i32> {
    let mut root: Option<PathBuf> = None;
    let mut project: Option<PathBuf> = None;
    let mut db: Option<PathBuf> = None;
    let mut mode: Option<String> = None;
    let mut owner: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];
        if !raw.starts_with('-') {
            break;
        }
        let (flag, _) = split_flag_arg(raw);
        match flag {
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--project" => project = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--db" => db = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--mode" => mode = Some(next_val(args, &mut i, flag)?.into()),
            "--owner" => owner = Some(next_val(args, &mut i, flag)?.into()),
            "-h" | "--help" => {
                println!("{}", help_text());
                return Ok(0);
            }
            _ => break,
        }
        i += 1;
    }

    let op = args
        .get(i)
        .map(String::as_str)
        .ok_or_else(|| RuntimeError::invalid("memory requires an operation (e.g. status, index, retrieve, start, validate)"))?;
    i += 1;

    let mut req = Map::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut input_bytes = Vec::new();
    let mut read_stdin = matches!(
        op,
        "native-prompt"
            | "native-stop"
            | "native-result"
            | "prepare-result"
            | "record-result"
            | "replay-result"
    );

    while i < args.len() {
        let raw = &args[i];
        if !raw.starts_with('-') {
            positionals.push(raw.clone());
            i += 1;
            continue;
        }
        let (flag, _) = split_flag_arg(raw);
        match flag {
            "--root" => root = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--project" => project = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--db" => db = Some(PathBuf::from(next_val(args, &mut i, flag)?)),
            "--mode" => mode = Some(next_val(args, &mut i, flag)?.into()),
            "--owner" => owner = Some(next_val(args, &mut i, flag)?.into()),
            "--json" => {
                req.insert("json".into(), Value::Bool(true));
            }
            "--force" => {
                req.insert("force".into(), Value::Bool(true));
            }
            "--incremental" => {
                req.insert("incremental".into(), Value::Bool(true));
            }
            "--ephemeral" => {
                req.insert("ephemeral".into(), Value::Bool(true));
            }
            "--sanitize" => {
                req.insert("sanitize".into(), Value::Bool(true));
            }
            "--include-archive" => {
                req.insert("include_archive".into(), Value::Bool(true));
            }
            "--include-superseded" => {
                req.insert("include_superseded".into(), Value::Bool(true));
            }
            "--include-transcript" => {
                req.insert("include_transcript".into(), Value::Bool(true));
            }
            "--flush" => {
                req.insert("flush".into(), Value::Bool(true));
            }
            "--apply" => {
                req.insert("apply".into(), Value::Bool(true));
            }
            "--reject" => {
                req.insert("reject".into(), Value::Bool(true));
            }
            "--reverify" => {
                req.insert("reverify".into(), Value::Bool(true));
            }
            "--replace-next-steps" => {
                req.insert("replace_next_steps".into(), Value::Bool(true));
            }
            "--entered" => {
                req.insert("entered".into(), Value::Bool(true));
            }
            "--skip" => {
                req.insert("skip".into(), Value::Bool(true));
            }
            "--provenance" | "--provenance-response" => {
                req.insert("provenance".into(), Value::Bool(true));
                req.insert("provenance_response".into(), Value::Bool(true));
            }
            "--report-json" => {
                req.insert("report_json".into(), Value::Bool(true));
            }
            "--diagnostic" => {
                req.insert("diagnostic".into(), Value::Bool(true));
            }
            "--file" | "--files" => {
                push_string_list(&mut req, "files", next_val(args, &mut i, flag)?.into());
            }
            "--source" | "--sources" => {
                push_string_list(&mut req, "sources", next_val(args, &mut i, flag)?.into());
            }
            "--verification" | "--verifications" => {
                push_string_list(&mut req, "verification", next_val(args, &mut i, flag)?.into());
            }
            "--conflict" | "--conflicts" => {
                push_string_list(&mut req, "conflicts", next_val(args, &mut i, flag)?.into());
            }
            "--next-step" | "--next-steps" => {
                push_string_list(&mut req, "next_steps", next_val(args, &mut i, flag)?.into());
            }
            "--ref" | "--refs" => {
                push_string_list(&mut req, "refs", next_val(args, &mut i, flag)?.into());
            }
            "--source-id" | "--source-ids" => {
                push_string_list(&mut req, "source_ids", next_val(args, &mut i, flag)?.into());
            }
            "--path" | "--paths" => {
                push_string_list(&mut req, "paths", next_val(args, &mut i, flag)?.into());
            }
            "--revision" => {
                let v = next_val(args, &mut i, flag)?;
                if v == "auto" {
                    req.insert("revision".into(), Value::String("auto".into()));
                } else if let Ok(n) = v.parse::<u64>() {
                    req.insert("revision".into(), Value::from(n));
                } else {
                    return Err(RuntimeError::invalid("--revision must be positive integer or auto"));
                }
            }
            "--flush-after" | "--max-files" | "--since-seq" | "--window" | "--since" | "--max-tokens" => {
                let key = flag.trim_start_matches("--").replace('-', "_");
                let n = next_val(args, &mut i, flag)?
                    .parse::<u64>()
                    .map_err(|_| RuntimeError::invalid(format!("{flag} must be a non-negative integer")))?;
                req.insert(key, Value::from(n));
            }
            "--confidence" => {
                let f = next_val(args, &mut i, flag)?
                    .parse::<f64>()
                    .map_err(|_| RuntimeError::invalid("--confidence must be numeric"))?;
                req.insert("confidence".into(), Value::from(f));
            }
            "--collaborator" | "--with" => {
                req.insert("with".into(), Value::String(next_val(args, &mut i, flag)?.into()));
            }
            "--input-json" => {
                let v = next_val(args, &mut i, flag)?;
                if v == "-" {
                    read_stdin = true;
                } else {
                    let parsed: Value = serde_json::from_str(v)?;
                    req.insert("data".into(), parsed);
                }
            }
            "--input" => {
                let v = next_val(args, &mut i, flag)?;
                if v == "-" {
                    read_stdin = true;
                } else {
                    req.insert("input".into(), Value::String(v.into()));
                }
            }
            other => {
                let key = other.trim_start_matches("--").replace('-', "_");
                if key.is_empty() {
                    return Err(RuntimeError::invalid("Invalid empty option"));
                }
                let val = next_val(args, &mut i, other)?.to_string();
                req.insert(key, Value::String(val));
            }
        }
        i += 1;
    }

    if let Some(first) = positionals.first() {
        match op.replace('_', "-").as_str() {
            "retrieve" | "context" | "find" | "brain-find" => {
                req.entry("query").or_insert_with(|| Value::String(first.clone()));
            }
            "get" | "brain-get" | "brain-handoff" => {
                req.entry("id").or_insert_with(|| Value::String(first.clone()));
            }
            "export" => {
                req.entry("destination").or_insert_with(|| Value::String(first.clone()));
            }
            "links" => {
                req.entry("target").or_insert_with(|| Value::String(first.clone()));
            }
            _ => {
                req.entry("task_id").or_insert_with(|| Value::String(first.clone()));
            }
        }
    }

    if read_stdin {
        input_bytes = read_stdin_capped()?;
    }

    let ctx = build_context(
        root.as_deref(),
        project.as_deref(),
        owner.as_deref(),
        mode.as_deref(),
        false,
    )?;
    let service = MemoryService::new(ctx, db);
    let req_val = Value::Object(req.clone());
    let output = service.execute_cli(op, &req_val, &input_bytes)?;

    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    if let Some(stdout) = output.stdout {
        print!("{stdout}");
    } else if req.get("json") == Some(&Value::Bool(true)) || output.value.is_object() {
        let serialized = serde_json::to_string_pretty(&output.value)?;
        println!("{serialized}");
    }
    Ok(output.code)
}

pub fn dispatch(args: &[String]) -> Result<i32> {
    if args.is_empty() {
        print!("{}", help_text());
        return Ok(0);
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print!("{}", help_text());
            Ok(0)
        }
        "-V" | "--version" | "version" => {
            println!("context-graph-engine {VERSION}");
            Ok(0)
        }
        "mcp" => run_mcp(&args[1..]),
        "graph" => run_graph(&args[1..]),
        "export" => run_export(&args[1..]),
        "import" => run_import(&args[1..]),
        "hook" => run_hook(&args[1..]),
        "collect-context" => run_collect(&args[1..]),
        "memory" => run_memory(&args[1..]),
        other => Err(RuntimeError::invalid(format!(
            "Unknown command '{other}'. Run with --help for usage."
        ))),
    }
}
