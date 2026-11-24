---
description: Sync canonical knowledge to Omnigraph, inspect file context, diff branches, or resolve merge conflicts.
argument-hint: "<sync-canonical|file-context|atoms|branch-diff|merge-keep> [args]"
---

# /cg-omnigraph

Read `skills/omnigraph-sync/SKILL.md` and execute the requested Omnigraph operation for `$ARGUMENTS` against the target repository (`"project": "<repo_root>"` over MCP with `omnigraph_sync_canonical`, `omnigraph_file_context`, `omnigraph_atoms`, `omnigraph_branch_diff`, `omnigraph_merge_keep`, or CLI `context-graph-engine omnigraph ... --root .`). Always inspect `contradiction_warnings` and preview `omnigraph_branch_diff` before resolving conflicts with `keep_source`, `keep_target`, or verbatim per-atom `keep`.
