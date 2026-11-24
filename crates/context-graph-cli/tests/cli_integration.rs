use context_graph_core::domain::uuid4;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn engine() -> &'static str {
    env!("CARGO_BIN_EXE_context-graph-engine")
}

fn run_cli(cwd: &Path, args: &[&str], stdin_data: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(engine());
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn().expect("spawn context-graph-engine");
    if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
        stdin.write_all(data.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

#[test]
fn cli_help_version_and_graph_export_import_roundtrip() {
    let tmp = TempDir(std::env::temp_dir().join(format!("cg-cli-rt-{}", uuid4())));
    let repo_a = tmp.0.join("repo-a");
    let repo_b = tmp.0.join("repo-b");
    let bundle_dir = tmp.0.join("bundle-dir");
    let bundle_zip = tmp.0.join("bundle.cgb.zip");
    fs::create_dir_all(repo_a.join("src")).unwrap();
    fs::create_dir_all(repo_b.join("src")).unwrap();
    fs::write(repo_a.join("src/auth.rs"), "pub fn check() -> bool { true }\n").unwrap();

    // --help and --version
    let help = run_cli(&repo_a, &["--help"], None);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("context-graph-engine"));
    let ver = run_cli(&repo_a, &["--version"], None);
    assert!(ver.status.success());
    assert!(String::from_utf8_lossy(&ver.stdout).contains("0.1.0"));

    // Memory start, brain create/share, index, validate
    let start = run_cli(
        &repo_a,
        &[
            "memory",
            "--root",
            repo_a.to_str().unwrap(),
            "--owner",
            "alice",
            "start",
            "--task-id",
            "TASK-CLI-1",
            "--goal",
            "Verify CLI workflow",
            "--source",
            "src/auth.rs",
            "--json",
        ],
        None,
    );
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let start_json: Value = serde_json::from_slice(&start.stdout).unwrap();
    let rev = start_json["revision"].as_u64().unwrap();

    let share = run_cli(
        &repo_a,
        &[
            "memory",
            "--root",
            repo_a.to_str().unwrap(),
            "--owner",
            "alice",
            "brain-share-task",
            "--record-id",
            "TASK-CLI-1",
            "--revision",
            &rev.to_string(),
            "--collaborator",
            "bob",
            "--json",
        ],
        None,
    );
    assert!(
        share.status.success(),
        "{}",
        String::from_utf8_lossy(&share.stderr)
    );
    let share_json: Value = serde_json::from_slice(&share.stdout).unwrap();
    assert!(
        share_json["authorized_owners"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "bob")
    );

    let idx = run_cli(
        &repo_a,
        &["memory", "--root", repo_a.to_str().unwrap(), "index", "--json"],
        None,
    );
    assert!(idx.status.success(), "{}", String::from_utf8_lossy(&idx.stderr));

    // Graph query, links, and DOT export
    let gq = run_cli(
        &repo_a,
        &[
            "graph",
            "query",
            "--root",
            repo_a.to_str().unwrap(),
            "--node",
            "TASK-CLI-1",
            "--depth",
            "2",
            "--json",
        ],
        None,
    );
    assert!(gq.status.success(), "{}", String::from_utf8_lossy(&gq.stderr));
    let gq_json: Value = serde_json::from_slice(&gq.stdout).unwrap();
    assert!(!gq_json["nodes"].as_array().unwrap().is_empty());

    let gl = run_cli(
        &repo_a,
        &[
            "graph",
            "links",
            "--root",
            repo_a.to_str().unwrap(),
            "--target",
            "src/auth.rs",
            "--json",
        ],
        None,
    );
    assert!(gl.status.success(), "{}", String::from_utf8_lossy(&gl.stderr));

    let dot = run_cli(
        &repo_a,
        &[
            "graph",
            "export-dot",
            "--root",
            repo_a.to_str().unwrap(),
            "--node",
            "TASK-CLI-1",
        ],
        None,
    );
    assert!(dot.status.success());
    assert!(String::from_utf8_lossy(&dot.stdout).contains("digraph context_graph"));

    // Export directory and ZIP bundles
    let exp_dir = run_cli(
        &repo_a,
        &[
            "export",
            "--root",
            repo_a.to_str().unwrap(),
            "--destination",
            bundle_dir.to_str().unwrap(),
            "--format",
            "dir",
            "--json",
        ],
        None,
    );
    assert!(
        exp_dir.status.success(),
        "{}",
        String::from_utf8_lossy(&exp_dir.stderr)
    );
    assert!(bundle_dir.join("BUNDLE_MANIFEST.json").is_file());
    assert!(bundle_dir.join("graph.json").is_file());

    let exp_zip = run_cli(
        &repo_a,
        &[
            "export",
            "--root",
            repo_a.to_str().unwrap(),
            "--destination",
            bundle_zip.to_str().unwrap(),
            "--json",
        ],
        None,
    );
    assert!(exp_zip.status.success());
    assert!(bundle_zip.is_file());

    // Dry-run import into repo_b writes nothing
    let dry = run_cli(
        &repo_b,
        &[
            "import",
            "--root",
            repo_b.to_str().unwrap(),
            "--source",
            bundle_zip.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
        None,
    );
    assert!(dry.status.success(), "{}", String::from_utf8_lossy(&dry.stderr));
    let dry_json: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["dry_run"], true);
    assert_eq!(dry_json["counts"]["would_import_brain"], 1);
    assert!(!repo_b.join("project-brain/dynamic").exists());

    // Apply import into repo_b and query graph
    let imp = run_cli(
        &repo_b,
        &[
            "import",
            "--root",
            repo_b.to_str().unwrap(),
            "--owner",
            "dave",
            "--source",
            bundle_zip.to_str().unwrap(),
            "--strategy",
            "skip",
            "--json",
        ],
        None,
    );
    assert!(imp.status.success(), "{}", String::from_utf8_lossy(&imp.stderr));
    let imp_json: Value = serde_json::from_slice(&imp.stdout).unwrap();
    assert_eq!(imp_json["counts"]["imported_brain"], 1);

    let gq_b = run_cli(
        &repo_b,
        &[
            "graph",
            "query",
            "--root",
            repo_b.to_str().unwrap(),
            "--node",
            "TASK-CLI-1",
            "--json",
        ],
        None,
    );
    assert!(gq_b.status.success());
}

#[test]
fn hooks_run_cleanly_and_never_block() {
    let tmp = TempDir(std::env::temp_dir().join(format!("cg-hook-rt-{}", uuid4())));
    fs::create_dir_all(tmp.0.join("repo")).unwrap();
    let repo = tmp.0.join("repo");
    let payload = json!({
        "cwd": repo,
        "prompt": "check auth",
        "session_id": "sess-1"
    })
    .to_string();

    for (host, role) in [
        ("claude", "local-context"),
        ("claude", "working-memory-read"),
        ("claude", "working-memory-write"),
        ("claude", "context-continuity"),
        ("codex", "local-context"),
        ("codex", "working-memory-read"),
        ("codex", "working-memory-write"),
        ("codex", "context-continuity"),
    ] {
        let out = run_cli(
            &repo,
            &["hook", host, role, "--root", repo.to_str().unwrap()],
            Some(&payload),
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "hook {host} {role} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn mcp_binary_refuses_omitted_project_when_cwd_is_plugin_dir_under_ancestor_git() {
    let tmp = TempDir(std::env::temp_dir().join(format!("cg-mcp-guard-{}", uuid4())));
    // Simulate ~/.git or marketplace .git ancestor above plugin dir (#23/#24).
    fs::create_dir_all(tmp.0.join(".git")).unwrap();
    let plugin_dir = tmp.0.join("plugins/context-graph");
    fs::create_dir_all(plugin_dir.join(".codex-plugin")).unwrap();
    fs::write(
        plugin_dir.join(".codex-plugin/plugin.json"),
        r#"{"name":"context-graph"}"#,
    )
    .unwrap();
    let target_repo = tmp.0.join("target-repo");
    fs::create_dir_all(target_repo.join("src")).unwrap();
    fs::write(target_repo.join("src/app.rs"), "fn main() {}\n").unwrap();

    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        // Omitted or "." project while cwd is plugin_dir MUST refuse (not escape to ancestor .git!).
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "memory_status",
            "arguments": {}
        }}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "memory_status",
            "arguments": {"project": "."}
        }}),
        // Passing explicit project succeeds against target_repo cleanly.
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
            "name": "memory_status",
            "arguments": {"project": target_repo}
        }}),
    ];
    let wire = requests
        .iter()
        .map(|v| serde_json::to_string(v).unwrap() + "\n")
        .collect::<String>();

    let out = run_cli(&plugin_dir, &["mcp", "--host", "codex"], Some(&wire));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let responses: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 5);
    assert_eq!(responses[1]["result"]["tools"].as_array().unwrap().len(), 17);
    assert_eq!(responses[2]["result"]["isError"], true, "{:#}", responses[2]);
    assert_eq!(responses[3]["result"]["isError"], true, "{:#}", responses[3]);
    assert_eq!(
        responses[4]["result"]["isError"], false,
        "{:#}",
        responses[4]
    );

    // Neither plugin_dir nor ancestor .git parent should have SQLite or Brain artifacts created.
    assert!(!plugin_dir.join("memory-bank").exists());
    assert!(!tmp.0.join("memory-bank").exists());
    assert!(!tmp.0.join("project-brain").exists());
}

#[test]
fn omnigraph_cli_sync_branch_diff_keep_bridge_and_promote_roundtrip() {
    let tmp = TempDir(std::env::temp_dir().join(format!("cg-omni-cli-{}", uuid4())));
    let repo = tmp.0.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("project-brain/schemas")).unwrap();
    fs::create_dir_all(repo.join("memory-bank/chunks")).unwrap();
    fs::write(
        repo.join("src/OrderService.php"),
        "<?php\nclass OrderService {\n    public function issueInvoice(): void {}\n}\n",
    )
    .unwrap();
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../context-graph-core/schemas/dynamic-record.schema.json"),
        repo.join("project-brain/schemas/dynamic-record.schema.json"),
    )
    .unwrap();

    let r = repo.to_str().unwrap();

    // Publish atom on main with SymbolRef linkage
    let pub_main = run_cli(
        &repo,
        &[
            "omnigraph",
            "publish",
            "--root",
            r,
            "--atom-id",
            "atom-inv",
            "--kind",
            "decision",
            "--title",
            "Invoice idempotency",
            "--body",
            "OrderService::issueInvoice requires an idempotency key.",
            "--file",
            "src/OrderService.php",
        ],
        None,
    );
    assert!(
        pub_main.status.success(),
        "{}",
        String::from_utf8_lossy(&pub_main.stderr)
    );
    let pub_json: Value = serde_json::from_slice(&pub_main.stdout).unwrap();
    assert!(
        pub_json["atom"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("issueInvoice")),
        "{:#}",
        pub_json
    );

    // Branch, edit on both sides, branch-diff -> conflict, merge-keep -> resolved
    let br = run_cli(&repo, &["omnigraph", "branch-create", "--root", r, "--name", "session/s1"], None);
    assert!(br.status.success());

    let e1 = run_cli(
        &repo,
        &[
            "omnigraph",
            "publish",
            "--root",
            r,
            "--branch",
            "session/s1",
            "--atom-id",
            "atom-inv",
            "--kind",
            "decision",
            "--title",
            "Invoice idempotency via header",
            "--body",
            "OrderService::issueInvoice requires X-Idempotency-Key.",
            "--file",
            "src/OrderService.php",
        ],
        None,
    );
    assert!(e1.status.success());

    let e2 = run_cli(
        &repo,
        &[
            "omnigraph",
            "publish",
            "--root",
            r,
            "--branch",
            "main",
            "--atom-id",
            "atom-inv",
            "--kind",
            "decision",
            "--title",
            "Invoice idempotency via DB token",
            "--body",
            "OrderService::issueInvoice checks ledger table.",
            "--file",
            "src/OrderService.php",
        ],
        None,
    );
    assert!(e2.status.success());

    let diff = run_cli(
        &repo,
        &["omnigraph", "branch-diff", "--root", r, "--source", "session/s1", "--target", "main"],
        None,
    );
    assert!(diff.status.success());
    let diff_json: Value = serde_json::from_slice(&diff.stdout).unwrap();
    assert_eq!(diff_json["outcome"], "conflict");

    let mk = run_cli(
        &repo,
        &[
            "omnigraph",
            "merge-keep",
            "--root",
            r,
            "--source",
            "session/s1",
            "--target",
            "main",
            "--strategy",
            "keep_source",
        ],
        None,
    );
    assert!(mk.status.success());
    let mk_json: Value = serde_json::from_slice(&mk.stdout).unwrap();
    assert_eq!(mk_json["outcome"], "merged");

    // Promote safe atom to Project Brain + sync-canonical + memory-links
    let prom = run_cli(
        &repo,
        &[
            "omnigraph",
            "promote-atom",
            "--root",
            r,
            "--atom-id",
            "atom-inv",
            "--title",
            "Invoice idempotency via header",
            "--body",
            "OrderService::issueInvoice requires X-Idempotency-Key.",
            "--file",
            "src/OrderService.php",
            "--kind",
            "decision",
            "--privacy",
            "team",
        ],
        None,
    );
    assert!(prom.status.success(), "{}", String::from_utf8_lossy(&prom.stderr));

    let sync = run_cli(&repo, &["omnigraph", "sync-canonical", "--root", r], None);
    assert!(sync.status.success(), "{}", String::from_utf8_lossy(&sync.stderr));
    let sync_json: Value = serde_json::from_slice(&sync.stdout).unwrap();
    assert!(sync_json["synced"].as_u64().unwrap() >= 1);

    let links = run_cli(
        &repo,
        &["memory-links", "--root", r, "--path", "src/OrderService.php"],
        None,
    );
    assert!(links.status.success());
    let links_json: Value = serde_json::from_slice(&links.stdout).unwrap();
    assert_eq!(links_json["items"][0]["brain_records"].as_array().unwrap().len(), 1);
}
