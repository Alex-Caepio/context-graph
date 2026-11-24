---
name: context-graph
description: Use when querying multi-hop relationships, provenance, freshness, conflicts, supersession lineage, impact analysis, or embedded Omnigraph file/symbol context across Project Brain, Memory Bank, working tasks, handoffs, session merges, and repository files.
---

# Unified Context Graph & Embedded Omnigraph

Traverse both the SQLite-backed multi-relation knowledge graph (`graph_nodes` / `graph_edges`) and the embedded offline Omnigraph DAG (`omnigraph.sqlite3` with `ContextAtom`, `FileRef`, `SymbolRef`, `Session`, and `RelatesTo` edges).

## Edge Kinds

### Unified Graph (`context_graph_query` / `context_graph_links`)
- `cites`: record, chunk, handoff, or doc -> cited source file (`current`, `stale`, `missing`)
- `touches`: task or handoff -> modified/touched codebase file
- `conflicts`: conflicting Brain records or Memory chunks
- `supersedes`: newer record/chunk -> superseded predecessor
- `promoted_from`: Memory Bank chunk -> source Brain records
- `consolidated_from`: consolidated finding -> source completion events
- `handoff_of`: handoff artifact -> target task
- `session_merge`: merged session/handoff -> contributing sessions/handoffs

### Embedded Omnigraph (`omnigraph_*` / `context_memory_links_for_file`)
- `TouchesFile`: `ContextAtom -> FileRef`
- `MentionsSymbol`: `ContextAtom -> SymbolRef` (from read-only `ir_symbols` or multi-language declaration scan)
- `RelatesTo`: `supports`, `contradicts`, `supersedes`, `depends_on`, `refines`
- `PublishedBy` / `ConsumedBy`: session provenance across `main`, `knowledge/canonical`, `session/*`, `shared/*`

## Usage

### Via MCP
Always pass `"project": "<repository_root>"` explicitly so tools operate on the target repository (required when the MCP host sets server `cwd` to the plugin directory).

- Multi-hop traversal:
  - `context_graph_query`: `{"project": "<repo>", "root_id": "<uuid|MEM-id|TASK-id|file_path>", "depth": 2, "direction": "both"}` (add `"format": "dot"` or `"kinds": [...]`)
- Lineage & reverse impact:
  - `context_graph_links`: `{"project": "<repo>", "target": "<file_path|id>"}`
- File-level Omnigraph atoms, `SymbolRef` mentions & contradiction warnings:
  - `omnigraph_file_context`: `{"project": "<repo>", "path": "src/Service.php"}`
- File-level Brain/Bank citations, rename/missing hygiene & contradictions:
  - `context_memory_links_for_file`: `{"project": "<repo>", "path": "src/New.php", "previous_path": "src/Old.php"}`

### Via CLI
```bash
context-graph-engine graph query --root . --node src/auth.rs --depth 3 --direction in --json
context-graph-engine graph links --root . --target MEM-20261118-abcd1234 --json
context-graph-engine graph export-dot --root . --node TASK-001 --depth 2
context-graph-engine omnigraph file-context --root . --path src/auth.rs
context-graph-engine memory-links --root . --path src/New.php --previous-path src/Old.php
```
