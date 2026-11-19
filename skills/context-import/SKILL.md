---
name: context-import
description: Use when inspecting, validating, or importing a context-graph-bundle-v1 directory or .zip archive into a target repository with collision detection and source-drift reporting.
---

# Context Bundle Import (`context-graph-bundle-v1`)

Validate checksums, privacy rules, and secret patterns on an incoming bundle, inspect collisions and target source-path drift, and apply imports atomically.

## Workflow

1. **Always dry-run first**: inspect SHA-256 integrity, collisions (`uuid_exists`, `external_id_exists`, `memory_id_exists`), and `source_drift` against the target codebase before modifying anything.
2. Choose a collision resolution `--strategy`:
   - `skip` (default): retain target items and skip colliding incoming items.
   - `remap-ids`: mint fresh UUIDv4 and `MEM-YYYYMMDD-xxxxxxxx` identifiers while consistently rewriting internal batch links (`conflicts`, `supersedes`, `superseded_by`).
   - `archive-superseded`: archive superseded active target records/chunks and install incoming versions with valid bidirectional `supersedes` / `superseded_by` links.
3. All imported Brain records are re-attributed to the importing actor (`owner` / `authorized_owners`), imported Bank chunks with absent target sources are flagged `needs-review` + `imported-from:<repo>`, and SQLite FTS5 + Context Graph are rebuilt automatically.

## Usage

### Via MCP
Pass `"project": "<target_repo_root>"` explicitly:
- Dry-run: `context_import_bundle` with `{"project": "<target_repo>", "source": "/path/to/bundle.cgb.zip", "strategy": "skip", "dry_run": true}`
- Apply: `context_import_bundle` with `{"project": "<target_repo>", "source": "/path/to/bundle.cgb.zip", "strategy": "remap-ids", "dry_run": false}`

### Via CLI
```bash
context-graph-engine import --root . --source ./transfer-bundle.cgb.zip --dry-run --json
context-graph-engine import --root . --source ./transfer-bundle.cgb.zip --strategy remap-ids --json
```
