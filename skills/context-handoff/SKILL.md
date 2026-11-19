---
name: context-handoff
description: Use when saving or loading curated portable session handoffs, tracking file fingerprint drift across branches/commits, or recording merged session lineage.
---

# Portable Context Handoff

Save curated, secret-free session continuity artifacts (`goal`, `summary`, `next_steps`, `decisions`, `constraints`, `files`) with SHA-256 source fingerprints and load them in a subsequent session with live drift detection.

## Safety

- Never include secrets, API keys, PII, or raw transcripts in handoff curated fields.
- Cited `files` are fingerprinted with CRLF-neutral SHA-256 digests so `context_handoff_load` detects changed files, branches, or commits since save time.
- Passing `merge_from` links multiple sessions/handoffs via `session_merge` edges in the Context Graph.

## Usage

### Via MCP
Pass `"project": "<repository_root>"` explicitly:
- Save: `context_handoff_save` with `{"project": "<repo>", "task_id": "TASK-001", "goal": "...", "summary": "...", "next_steps": ["..."], "files": ["src/main.rs"], "merge_from": []}`
- Load latest: `context_handoff_load` with `{"project": "<repo>", "task_id": "TASK-001", "latest": true}`

### Via CLI
```bash
context-graph-engine memory context-save --root . --output tasks/TASK-001/handoff.md --task-id TASK-001 --input-json -
context-graph-engine memory context-load --root . --input tasks/TASK-001/handoff.md --json
```
