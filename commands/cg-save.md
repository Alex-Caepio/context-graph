---
description: Save a curated, secret-checked portable session handoff for continuing work cleanly.
argument-hint: "[--task-id TASK-ID] [--detail summary|topic|full]"
---

# /cg-save

Read `skills/context-handoff/SKILL.md` and save a portable context handoff for `$ARGUMENTS` in the current repository (`context_handoff_save` with explicit `"project": "<repo_root>"` over MCP, or `context-graph-engine memory context-save --root .`). Ensure `goal`, `summary`, `next_steps`, and cited `files` contain zero secrets or raw transcripts.
