---
description: Export a privacy-filtered, SHA-256 attested context-graph-bundle-v1 directory or .zip archive.
argument-hint: "<destination-path> [--format zip|dir] [--subgraph-root NODE]"
---

# /context-export

Read `skills/context-export/SKILL.md` and export a `context-graph-bundle-v1` bundle for the current repository to `$ARGUMENTS` (passing `"project": "<repo_root>"` in `context_export_bundle` over MCP or `context-graph-engine export --root .`). Report artifact counts, excluded private records, bundle path, and `schema_version`.
