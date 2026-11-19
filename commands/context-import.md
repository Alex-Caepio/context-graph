---
description: Dry-run validate or import a context-graph-bundle-v1 bundle with collision and drift handling.
argument-hint: "<bundle-path> [--strategy skip|remap-ids|archive-superseded] [--apply]"
---

# /context-import

Read `skills/context-import/SKILL.md`. First run a `--dry-run` (`dry_run: true`) on `$ARGUMENTS` against the current repository (`"project": "<repo_root>"` over MCP or `context-graph-engine import --root . --dry-run`) to report SHA-256 validation, collisions, and target `source_drift`. Apply changes only when requested or after confirming strategy (`skip`, `remap-ids`, or `archive-superseded`).
