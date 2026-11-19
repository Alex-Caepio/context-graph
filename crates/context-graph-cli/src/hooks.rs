//! Framework-agnostic native implementations of the memory & continuity hooks.
//! Memory hook failures degrade silently/cleanly (exit 0), never blocking user turns.
use crate::process;
use context_graph_core::{
    BankService, OpenOptions, Result, Roots, RuntimeConfig, RuntimeContext, RuntimeError, Store,
    fs, git::GitFrame, graph, storage,
};
use serde_json::{Value, json as object};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const ROLES: &[&str] = &[
    "working-memory-read",
    "working-memory-write",
    "local-context",
    "context-continuity",
];

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
    dir.join(".codex-plugin/plugin.json").is_file()
        || dir.join(".claude-plugin/plugin.json").is_file()
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

fn working_memory_read(roots: &Roots, task: Option<&str>, host: &str, payload: &Value, raw: &[u8]) -> Result<HookOutput> {
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
        if let Some(session) = payload["session_id"].as_str().filter(|id| {
            regex::Regex::new(r"^[A-Za-z0-9._:-]{1,128}$")
                .is_ok_and(|re| re.is_match(id))
        }) {
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
    if let Some(handle) = native_handle
        && let Ok(Ok(out)) = handle.join()
        && out.code == 0
        && String::from_utf8_lossy(&out.stdout).trim()
            == "Previous native memory result was not saved completely."
    {
        prompt_notice = "Previous native memory result was not saved completely.\n".into();
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
            rendered.push_str("Memory refresh exceeded hook budget. Read canonical sources directly.\n");
        }
        _ => {}
    }
    Ok(HookOutput::text(rendered))
}

fn working_memory_write(roots: &Roots, task: Option<&str>, host: &str, payload: &Value, raw: &[u8]) -> Result<HookOutput> {
    let Some(task_id) = task else {
        return Ok(HookOutput::default());
    };
    let budget = budget_from_env();
    let recovery = if host == "cursor" {
        payload["loop_count"].as_i64().is_some_and(|c| c > 0)
    } else {
        payload["stop_hook_active"] == true
    };
    if !recovery {
        let flush = std::env::var("CONTEXT_FLUSH_AFTER").unwrap_or_else(|_| "5".into());
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
        let _ = invoke_self_memory(roots, "turn", &args, &[], budget);
    }
    if std::env::var("CONTEXT_CAPSULE_DELIVERED").as_deref() == Ok("1") {
        return Ok(HookOutput::default());
    }
    let out = invoke_self_memory(
        roots,
        "native-stop",
        &["--host".into(), host.into(), "--task-id".into(), task_id.into()],
        raw,
        budget,
    );
    match out {
        Ok(res) if res.code == 0 => Ok(HookOutput {
            code: 0,
            stdout: res.stdout,
            stderr: Vec::new(),
        }),
        _ => Ok(HookOutput::default()),
    }
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

fn context_continuity(roots: &Roots, host: &str, raw: &[u8]) -> Result<HookOutput> {
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
    HookOutput::envelope(value)
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
        "context-continuity" => context_continuity(&roots, host, raw),
        _ => Ok(passed(host, role)),
    };
    // Memory hooks never refuse/block the host turn.
    Ok(res.unwrap_or_else(|_| passed(host, role)))
}
