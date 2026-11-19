# context-graph

Framework-agnostic standalone plugin providing a **Unified Multi-Relation Context Graph**, **Project Brain**, **Memory Bank**, **Portable Session Handoffs**, **SQLite FTS5 Retrieval Engine**, **Memory/Continuity Hooks**, and **Portable `context-graph-bundle-v1` Export & Import** for **Claude Code** and **OpenAI Codex CLI**.

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
- **Portable `context-graph-bundle-v1` Export & Import (`export`, `import`)**:
  - Exports directory or `.zip` / `.cgb.zip` bundles with `BUNDLE_MANIFEST.json` (SHA-256 digests per artifact), `graph.json`, privacy-filtered Project Brain records (`allowed_privacy`), Memory Bank chunks, portable handoffs, and SQLite episodes.
  - Enforces secret pattern scanning (`domain::secret_label_in`) on both export and import.
  - Supports `--dry-run` inspection, source-path drift detection, owner re-attribution, origin tagging (`imported-from:<repo>`), atomic rollback-safe writes, and `--strategy skip|remap-ids|archive-superseded`.
- **Native Stdio JSON-RPC MCP Server (`context-graph-engine mcp`)**:
  - Exposes 11 tools accepting optional `"project"` directory targeting:
    - `memory_status`, `memory_retrieve`, `memory_checkpoint`, `memory_record_result`, `memory_source_fingerprints`
    - `context_graph_query`, `context_graph_links`, `context_export_bundle`, `context_import_bundle`, `context_handoff_save`, `context_handoff_load`
  - **Note for Codex CLI**: `.codex-mcp.json` runs `./bin/context-graph-engine` with `"cwd": "."`. Always pass `"project": "<repository_root>"` in MCP tool arguments; calling mutating/index tools without `"project"` while `cwd` is the plugin directory fails safely rather than mutating the plugin cache or an ancestor directory.
- **Working Memory & Continuity Hooks**:
  - `local-context` (`SessionStart`), `working-memory-read` (`UserPromptSubmit`), `working-memory-write` (`Stop`), and `context-continuity` across Claude Code and Codex.
- **Pluggable Privacy & Path Seeds**:
  - Built-in secret detection can be extended per-repo via `.context-graph/privacy-patterns.json` (or `project-brain/config/privacy-patterns.json`).
  - Generic multi-language path-seed resolution supports relative paths, Rust `::` paths, JS/TS/Python imports, custom `.context-graph/path-seeds.json`, and optional `composer.json` PSR-4 autoload maps.

## Quick CLI Usage

```bash
# Memory & index lifecycle
context-graph-engine memory start --root . --task-id TASK-001 --goal "Audit authentication"
context-graph-engine memory index --root .
context-graph-engine memory retrieve "auth token" --root . --task-id TASK-001 --json
context-graph-engine memory validate --root . --json

# Unified Context Graph
context-graph-engine graph query --root . --node TASK-001 --depth 2 --json
context-graph-engine graph links --root . --target src/auth.rs --json
context-graph-engine graph export-dot --root . --node TASK-001 --depth 2

# Portable Export / Import
context-graph-engine export --root . --destination ./bundle.cgb.zip --format zip
context-graph-engine import --root /path/to/target --source ./bundle.cgb.zip --dry-run --json
context-graph-engine import --root /path/to/target --source ./bundle.cgb.zip --strategy remap-ids --json
```

## Components

- `skills/`: `context-graph`, `context-export`, `context-import`, `context-handoff`, `memory-checkpoint`
- `commands/`: `/context-graph`, `/context-export`, `/context-import`, `/context-save`, `/context-load`
- `hooks/`: `hooks.json` (Claude Code) & `codex-hooks.json` (Codex CLI)
- `bin/context-graph-engine`: Compiled standalone Rust binary
- `schemas/`: Embedded & shipped JSON schemas (`dynamic-record`, `handoff`, `message`, `promotion`, `retrieval-manifest`, `token-usage-event`)

## License

MIT
