---
description: Query the unified multi-relation Context Graph (nodes, edges, N-hop provenance, impact, or DOT export).
argument-hint: "[node-or-path] [--depth N] [--direction out|in|both] [--dot]"
---

# /context-graph

Read `skills/context-graph/SKILL.md` and run a Context Graph query for `$ARGUMENTS` against the current repository (pass `"project": "<repo_root>"` when invoking `context_graph_query` / `context_graph_links` over MCP, or use `context-graph-engine graph ... --root .`). Report matching nodes, typed edges (`cites`, `touches`, `conflicts`, `supersedes`, `promoted_from`, `consolidated_from`, `handoff_of`, `session_merge`), freshness status, and provenance paths.
