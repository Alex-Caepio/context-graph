---
description: Load the latest or named portable session handoff and report git/file drift.
argument-hint: "[--task-id TASK-ID | --input path/to/handoff.md]"
---

# /context-load

Read `skills/context-handoff/SKILL.md` and load the handoff matching `$ARGUMENTS` for the current repository (`context_handoff_load` with explicit `"project": "<repo_root>"` over MCP, or `context-graph-engine memory context-load --root .`). Present restored goal, decisions, next steps, and any detected branch, commit, or file digest drift.
