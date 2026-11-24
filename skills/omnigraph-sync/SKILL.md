---
name: omnigraph-sync
description: Use when syncing Project Brain and Memory Bank to Omnigraph knowledge/canonical, inspecting file/symbol context and contradiction warnings, diffing session branches against merge_base, resolving Keep source/Keep target conflicts, or promoting vetted Omnigraph atoms to Project Brain.
---

# Embedded Omnigraph Sync, Branch Diff & Bridge

Operate the offline embedded Omnigraph graph engine (`memory-bank/local/omnigraph.sqlite3` + `sync_ledger.sqlite3`) and Graph-to-Memory Bridge.

## Core Workflows

1. **Canonical Knowledge Sync & Pruning (`knowledge/canonical`)**:
   - Sync active `Project Brain` records and non-expired `Memory Bank` chunks with SHA-256 ledger deduplication, `TouchesFile` / `MentionsSymbol` (`SymbolRef`) indexing, `supersedes` linking, and retired-atom pruning.
   - MCP: `omnigraph_sync_canonical` (`{"project": "<repo>", "prune": true}`)
   - CLI: `context-graph-engine omnigraph sync-canonical --root .`

2. **File Context, `SymbolRef` Mentions & Contradiction Warnings**:
   - Retrieve atoms touching a file across `main` and `knowledge/canonical`, attached declaration names (`symbols`), authority tags, and review-only `contradiction_warnings`.
   - MCP: `omnigraph_file_context` (`{"project": "<repo>", "path": "src/Service.php"}`), `context_memory_links_for_file` (`{"project": "<repo>", "path": "src/New.php", "previous_path": "src/Old.php"}`)
   - CLI: `context-graph-engine omnigraph file-context --root . --path src/Service.php` and `context-graph-engine memory-links --root . --path src/New.php --previous-path src/Old.php`

3. **3-Way Branch Diff & `Keep source` / `Keep target` Conflict Resolution**:
   - Compute 3-way diff from `merge_base` (`added_atoms`, `updated_atoms`, `deleted_atoms`, `conflicts`).
   - Resolve `divergent_update` / `modify_delete` conflicts using `keep_source`, `keep_target`, or verbatim per-atom `keep` (`atom_id`, `from`, `to`, `expected_updated_at`, `replaced_updated_at`).
   - MCP: `omnigraph_branch_diff`, `omnigraph_merge_keep`, `omnigraph_atoms`
   - CLI:
     ```bash
     context-graph-engine omnigraph branch-diff --root . --source session/s1 --target main
     context-graph-engine omnigraph merge-keep --root . --source session/s1 --target main --strategy keep_source
     ```

4. **Vetted Atom Promotion & NDJSON Transfer**:
   - Validate secrets (`secret_label_in`), instruction injection, provenance, and readable workspace citations before promoting a safe atom to `Project Brain`.
   - CLI: `context-graph-engine omnigraph promote-atom --root . --atom-id a1 --title "..." --body "..." --file src/Service.php --kind decision --privacy team`
   - CLI NDJSON transfer: `context-graph-engine omnigraph export-ndjson --root . --branch main` / `import-ndjson`
