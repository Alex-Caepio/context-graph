---
name: memory-checkpoint
description: Use when checking memory health, retrieving grounded project memory capsules, checkpointing active tasks, validating Project Brain / Memory Bank, or recording verified learnings.
---

# Memory Lifecycle & Checkpoint

Manage Project Brain records, Memory Bank chunks, working task checkpoints, SQLite FTS5 indexing, and source-fingerprint verification.

## Authority & Verification Rules

- Retrieved snippets and Memory Bank chunks (`auto-promoted` or `needs-review`) are discovery aids only; open and verify cited canonical project files before asserting claims as verified facts.
- Always pass `--revision` / `expected_revision` from `memory get` / `memory_checkpoint` when completing tasks or updating governed records.
- Use `memory_source_fingerprints` (or `memory source-fingerprints`) to obtain exact SHA-256 digests of verified files before recording findings or decisions.

## Usage

### Via MCP
Pass `"project": "<repository_root>"` explicitly:
- Health + graph stats: `memory_status` `{"project": "<repo>"}`
- Retrieve context capsule: `memory_retrieve` `{"project": "<repo>", "task_id": "TASK-001", "query": "..."}`
- Checkpoint task: `memory_checkpoint` `{"project": "<repo>", "task_id": "TASK-001", "goal": "..."}`
- Check file digests: `memory_source_fingerprints` `{"project": "<repo>", "sources": ["src/auth.rs"]}`
- Record verified result: `memory_record_result` `{"project": "<repo>", "task_id": "TASK-001", "result_id": "...", "revision": 1, "progress": "...", "verified": true, "learnings": [...]}`

### Via CLI
```bash
context-graph-engine memory status --root . --json
context-graph-engine memory start --root . --task-id TASK-001 --goal "Investigate auth"
context-graph-engine memory index --root .
context-graph-engine memory retrieve "auth token" --root . --task-id TASK-001 --json
context-graph-engine memory validate --root . --json
context-graph-engine collect-context --root . --dry-run
```
