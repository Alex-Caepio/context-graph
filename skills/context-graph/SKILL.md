---
name: context-graph
description: Use when querying multi-hop relationships, provenance, freshness, conflicts, supersession lineage, or impact across Project Brain, Memory Bank, working tasks, handoffs, session merges, and repository files.
---

# Unified Context Graph

Traverse the SQLite-backed multi-relation knowledge graph across all layers of repository memory and codebase files.

## Edge Kinds

- `cites`: record, chunk, handoff, or doc -> cited source file (tracks `current`, `stale`, `missing` freshness)
- `touches`: task or handoff -> modified/touched codebase file
- `conflicts`: conflicting Brain records or Memory chunks
- `supersedes`: newer record/chunk -> superseded predecessor
- `promoted_from`: Memory Bank chunk -> source Brain records
- `consolidated_from`: consolidated finding -> source completion events
- `handoff_of`: handoff artifact -> target task
- `session_merge`: merged session/handoff -> contributing sessions/handoffs

## Usage

### Via MCP
Always pass `"project": "<repository_root>"` explicitly so tools operate on the target repository (required when the MCP host sets server `cwd` to the plugin directory).

- Multi-hop traversal:
  - Tool: `context_graph_query` with `{"project": "<repo>", "root_id": "<uuid|MEM-id|TASK-id|file_path>", "depth": 2, "direction": "both"}`
  - Add `"format": "dot"` to get Graphviz DOT output, or `"kinds": ["cites", "promoted_from"]` to filter edge kinds.
- Lineage & reverse impact:
  - Tool: `context_graph_links` with `{"project": "<repo>", "target": "<file_path|id>"}` to inspect incoming dependents (`impacted_nodes`), transitive dependencies (`lineage_nodes`), `stale_sources`, `missing_sources`, and `conflicts`.

### Via CLI
```bash
context-graph-engine graph query --root . --node src/auth.rs --depth 3 --direction in --json
context-graph-engine graph links --root . --target MEM-20261118-abcd1234 --json
context-graph-engine graph export-dot --root . --node TASK-001 --depth 2
```
