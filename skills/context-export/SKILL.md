---
name: context-export
description: Use when exporting a privacy-filtered, SHA-256 attested context-graph-bundle-v1 directory or .zip archive containing the context graph, Project Brain records, Memory Bank chunks, handoffs, and episodes.
---

# Context Bundle Export (`context-graph-bundle-v1`)

Create a portable, secret-scanned, SHA-256 verified transfer bundle for sharing repository memory across machines or repositories.

## Guarantees

1. Enforces `allowed_privacy` (defaults to `["public", "team"]`; `restricted`/`private` records are excluded and listed in `BUNDLE_MANIFEST.json`).
2. Scans every exported artifact for secrets (`domain::secret_label_in`) and aborts if any pattern matches.
3. Emits `BUNDLE_MANIFEST.json` with SHA-256 digests and byte lengths for `graph.json`, Brain records, Memory Bank chunks, portable handoffs, and SQLite episodes.
4. Supports optional subgraph scoping via `subgraph_root` + `depth`.

## Usage

### Via MCP
Pass `"project": "<repository_root>"` explicitly:
- `context_export_bundle` with `{"project": "<repo>", "destination": "/path/to/bundle.cgb.zip", "format": "zip", "include_archive": false, "include_superseded": false, "force": false}`
- Optional subgraph export: add `"subgraph_root": "TASK-001", "depth": 3`.

### Via CLI
```bash
context-graph-engine export --root . --destination ./transfer-bundle.cgb.zip --format zip
context-graph-engine export --root . --destination ./transfer-dir --format dir --subgraph-root TASK-001 --depth 2 --force
```
