# context-graph

Framework-agnostic standalone plugin providing a **Unified Multi-Relation Context Graph**, **Embedded Offline Omnigraph Engine (`knowledge/canonical` Sync, 3-Way Branch Diff & Keep Merge)**, **Graph-to-Memory Bridge**, **Project Brain**, **Memory Bank**, **Portable Session Handoffs**, **SQLite FTS5 Retrieval & QueryPlanCache**, **IR-Epoch Working Snapshots**, **Settled Preflight Hooks**, and **Portable `context-graph-bundle-v1` Export & Import** for **Claude Code**, **OpenAI Codex CLI**, and **Cursor**.

Source repository: <https://github.com/Alex-Caepio/context-graph>

## Installation

### Claude Code

```bash
claude plugin marketplace add Alex-Caepio/ai-infra-plugins
claude plugin install context-graph@ai-infra-plugins
```

### OpenAI Codex CLI

```bash
codex plugin marketplace add https://github.com/Alex-Caepio/ai-infra-plugins.git
codex plugin add context-graph@ai-infra-plugins
```

### Build From Source

```bash
git clone https://github.com/Alex-Caepio/context-graph.git
cd context-graph
cargo build --release --locked --bin context-graph-engine
install -m 755 target/release/context-graph-engine bin/context-graph-engine
```

## Features

- **Unified Context Graph (`graph query`, `graph links`, `graph export-dot`)**:
  - Indexes `cites`, `touches`, `conflicts`, `supersedes`, `promoted_from`, `consolidated_from`, `handoff_of`, and `session_merge` edges in SQLite (`graph_nodes`, `graph_edges`).
  - Supports N-hop BFS traversal (`out`, `in`, `both`), lineage/impact queries, freshness diagnostics (`current`, `stale`, `missing`), and Graphviz DOT output.
- **Embedded Offline Omnigraph & Branch DAG (`omnigraph`)**:
  - Stores `ContextAtom`, `FileRef`, `SymbolRef`, `Session`, `TouchesFile`, `MentionsSymbol`, and `RelatesTo` (`contradicts`, `supersedes`, `supports`, `depends_on`, `refines`) in `memory-bank/local/omnigraph.sqlite3` with zero external daemon required.
  - Syncs active `Project Brain` records and `Memory Bank` chunks to `knowledge/canonical` using `sync_ledger.sqlite3` digest deduplication and retired-atom pruning.
  - Supports 3-way branch diffs against LCA `merge_base` (`branch-diff`), conflict detection (`divergent_update`, `modify_delete`), `Keep source` / `Keep target` / verbatim `keep` conflict resolution (`merge-keep`), NDJSON export/import (`export-ndjson`, `import-ndjson`), and Omnigraph `.pg`/`.gq`/Cedar schema emission (`schema`).
- **Graph-to-Memory Bridge (`memory-links`, `omnigraph promote-atom`)**:
  - Resolves cited `Project Brain` records, active `Memory Bank` chunks, workspace hygiene warnings (`cited-file-missing`, `cited-file-renamed` via `--previous-path`), and Omnigraph `contradiction_warnings`.
  - Promotes safe, source-grounded atoms to verified `Project Brain` records while blocking secrets, prompt-injection patterns, bad provenance, and unreadable files.
- **PR #112 Retrieval, Validation & Hook Optimizations**:
  - Persistent `QueryPlanCache` (`query_plan_cache`) and `document_sections` pre-parsed digests in SQLite, `RACY_NS` source/promotion caches, `JsonlCursor` chat continuity, `screen_rendered_text` excerpt sanitization, `CONTEXT_CAPSULE_PACK_DEDUP`, `ir_symbols` `#Symbol` validation (`bank/validation.rs`), IR-epoch invalidation (`snapshot.rs`), and `HOOK_PREFLIGHT_RACY_NS` settled-mtime hook preflight caches (`hooks.rs`).
- **Portable `context-graph-bundle-v1` Export & Import (`export`, `import`)**:
  - Exports directory or `.zip` / `.cgb.zip` bundles with `BUNDLE_MANIFEST.json` (SHA-256 digests), privacy-filtered `Project Brain` records, `Memory Bank` chunks, portable handoffs, and SQLite episodes.
  - Enforces secret pattern scanning (`domain::secret_label_in`) on export and import, `--dry-run`, source drift checks, `imported-from:<repo>` origin tagging, and `--strategy skip|remap-ids|archive-superseded`.
- **Native Stdio JSON-RPC MCP Server (`context-graph-engine mcp`)**:
  - Exposes **17 tools** accepting optional `"project"` directory targeting:
    - Core memory: `memory_status`, `memory_retrieve`, `memory_checkpoint`, `memory_record_result`, `memory_source_fingerprints`
    - Unified Context Graph & Handoffs: `context_graph_query`, `context_graph_links`, `context_export_bundle`, `context_import_bundle`, `context_handoff_save`, `context_handoff_load`
    - Omnigraph & Bridge: `omnigraph_sync_canonical`, `omnigraph_file_context`, `omnigraph_atoms`, `omnigraph_branch_diff`, `omnigraph_merge_keep`, `context_memory_links_for_file`
  - **Note for Codex CLI / Plugin Hosts**: Always pass `"project": "<repository_root>"` in MCP tool arguments; calling tools without `"project"` while `cwd` is the plugin directory refuses safely instead of writing into the plugin directory or ancestor git repositories.
- **Working Memory & Continuity Hooks**:
  - `local-context` (`SessionStart`), `working-memory-read` (`UserPromptSubmit`), `working-memory-write` (`Stop`), and `context-continuity` with IR-epoch-aware settled preflight caching (`HARNESS_DISABLE_HOOK_PREFLIGHT=1` to bypass).

## Quick CLI Usage

```bash
# Memory, promotions & index lifecycle
context-graph-engine memory start --root . --task-id TASK-001 --goal "Audit authentication"
context-graph-engine memory index --root .
context-graph-engine memory retrieve "auth token" --root . --task-id TASK-001 --json
context-graph-engine memory validate --root . --json
context-graph-engine memory promote-review --root . --proposal-id PROP-1 --decision approve --json

# Unified Context Graph
context-graph-engine graph query --root . --node TASK-001 --depth 2 --json
context-graph-engine graph links --root . --target src/auth.rs --json
context-graph-engine graph export-dot --root . --node TASK-001 --depth 2

# Embedded Omnigraph & Graph-to-Memory Bridge
context-graph-engine omnigraph sync-canonical --root .
context-graph-engine omnigraph file-context --root . --path src/auth.rs
context-graph-engine omnigraph branch-diff --root . --source session/s1 --target main
context-graph-engine omnigraph merge-keep --root . --source session/s1 --target main --strategy keep_source
context-graph-engine memory-links --root . --path src/auth.rs --previous-path src/old_auth.rs

# Portable Export / Import
context-graph-engine export --root . --destination ./bundle.cgb.zip --format zip
context-graph-engine import --root /path/to/target --source ./bundle.cgb.zip --dry-run --json
context-graph-engine import --root /path/to/target --source ./bundle.cgb.zip --strategy remap-ids --json
```

## Components

- `skills/`: `context-graph`, `context-export`, `context-import`, `context-handoff`, `memory-checkpoint`, `omnigraph-sync`
- `commands/`: `/cg-graph`, `/cg-export`, `/cg-import`, `/cg-save`, `/cg-load`, `/cg-omnigraph`, `/cg-links`, `/cg-checkpoint`
- `hooks/`: `hooks.json` (Claude Code) & `codex-hooks.json` (Codex CLI)
- `bin/context-graph-engine`: Compiled standalone stripped Rust binary
- `schemas/`: Shipped JSON schemas (`dynamic-record`, `handoff`, `message`, `promotion`, `retrieval-manifest`, `token-usage-event`)

## License

MIT
