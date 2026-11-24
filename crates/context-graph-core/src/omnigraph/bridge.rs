//! Graph-to-Memory Bridge ported from PR #101/`#112` (`infra-harness/src/knowledge.rs`):
//!
//! 1. `memory_links_for_files`: resolves Project Brain records & active Memory Bank
//!    chunks citing target workspace files, plus workspace hygiene warnings
//!    (`cited-file-missing`, `cited-file-renamed` via `previous_path`) and
//!    Omnigraph `contradiction_warnings`.
//! 2. `contradiction_warnings`: converts Omnigraph `contradicts` relations into
//!    review-only warnings naming both atoms and backing Brain/Memory IDs.
//! 3. `promote_atom`: promotes a safe, verified Omnigraph `ContextAtom` to a
//!    `Project Brain` record via `MemoryService`, enforcing `allowed_privacy`,
//!    `secret_label_in`, `instruction_like`, `provenance_problems`, and workspace
//!    readability custody.

use super::engine::OmnigraphEngine;
use crate::{
    BankService, BrainRepository, MemoryService, Result, RuntimeConfig, RuntimeContext,
    RuntimeError,
    bank::{codec, promotion, publication::PublicationFiles},
    domain, index,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn clean_relative_path(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.split('/').any(|seg| seg == "..")
    {
        None
    } else {
        Some(trimmed)
    }
}

fn collapse_preview(raw: &str, limit: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(limit.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Converts Omnigraph `contradicts` relations and `atoms` into review-only warnings
/// naming both ends and backing `Project Brain` / `Memory Bank` IDs when linked.
pub fn contradiction_warnings(contradictions: &Value, atoms: &Value) -> Vec<Value> {
    let atoms = atoms.as_array().map(Vec::as_slice).unwrap_or_default();
    let opens = |atom_id: &str| -> Option<(&'static str, String)> {
        let atom = atoms
            .iter()
            .find(|atom| atom["atom_id"].as_str() == Some(atom_id));
        let field = |key: &str| {
            atom.and_then(|atom| atom[key].as_str())
                .map(str::trim)
                .filter(|id| !id.is_empty())
        };
        field("brain_record_id")
            .or_else(|| atom_id.strip_prefix("brain-"))
            .map(|id| ("brain", id.to_owned()))
            .or_else(|| {
                field("memory_id")
                    .or_else(|| {
                        atom_id
                            .strip_prefix("mem-")
                            .filter(|id| id.starts_with("MEM-"))
                    })
                    .map(|id| ("memory", id.to_owned()))
            })
    };
    let mut warnings = Vec::new();
    let id = |relation: &Value, key: &str| {
        relation[key]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    for relation in contradictions.as_array().into_iter().flatten().take(24) {
        let (Some(source), Some(target)) = (id(relation, "source_id"), id(relation, "target_id"))
        else {
            continue;
        };
        let (source, target) = (source.as_str(), target.as_str());
        let source_title = relation["source_title"].as_str().unwrap_or(source);
        let target_title = relation["target_title"].as_str().unwrap_or(target);
        let mut linked = Vec::new();
        let mut open = Vec::new();
        for (end, title) in [(source, source_title), (target, target_title)] {
            if let Some((scope, target_id)) = opens(end)
                && !linked.contains(&target_id)
            {
                open.push(json!({
                    "scope": scope,
                    "target_id": target_id,
                    "title": collapse_preview(title, 240)
                }));
                linked.push(target_id);
            }
        }
        let text = relation["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "\"{source_title}\" contradicts \"{target_title}\"; check both against the code before relying on either."
                )
            });
        warnings.push(json!({
            "kind": "atom-contradiction",
            "source_id": source,
            "source_title": collapse_preview(source_title, 240),
            "target_id": target,
            "target_title": collapse_preview(target_title, 240),
            "note": relation["note"],
            "branch": relation["branch"],
            "linked": linked,
            "open": open,
            "recommended_action": "review",
            "text": collapse_preview(&text, 600),
        }));
    }
    warnings
}

/// Resolves cited Brain records, active Memory Bank chunks, workspace hygiene warnings
/// (`cited-file-missing`, `cited-file-renamed`), and Omnigraph `contradiction_warnings`
/// for one or more `(path, previous_path)` specs.
pub fn memory_links_for_files(
    context: &RuntimeContext,
    project_id: &str,
    specs: &[(String, Option<String>)],
    engine: Option<&OmnigraphEngine>,
) -> Result<Value> {
    let normalized_specs: Vec<(String, Option<String>)> = specs
        .iter()
        .filter_map(|(p, prev)| {
            let clean = clean_relative_path(p)?;
            let prev_clean = prev.as_deref().and_then(clean_relative_path);
            Some((clean, prev_clean))
        })
        .collect();
    if normalized_specs.is_empty() {
        return Ok(json!({ "ok": true, "items": [] }));
    }

    let config = RuntimeConfig::load(&context.roots)?;
    let allowed_privacy = config.strings("allowed_privacy")?;
    let mut wanted = BTreeSet::new();
    for (path, previous) in &normalized_specs {
        wanted.insert(path.clone());
        if let Some(prev) = previous {
            wanted.insert(prev.clone());
        }
    }

    let matching_paths = |values: &Value| -> Vec<String> {
        values
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|raw| clean_relative_path(raw.split('#').next().unwrap_or(raw)))
            .filter(|cited| wanted.contains(cited))
            .collect()
    };
    let pick_cited = |hits: &[String], path: &str, previous: Option<&str>| -> Option<String> {
        hits.iter()
            .find(|cited| cited.as_str() == path || previous == Some(cited.as_str()))
            .cloned()
    };

    let mut per_brain: Vec<Vec<Value>> = vec![Vec::new(); normalized_specs.len()];
    if context.mode == "governed"
        && let Ok(brain) = BrainRepository::new(context.clone())
        && let Ok(records) = brain.records(false)
    {
        for image in records {
            let r = &image.record;
            let privacy = r["privacy"].as_str().unwrap_or("");
            if !allowed_privacy.iter().any(|allowed| allowed == privacy) {
                continue;
            }
            let file_hits = matching_paths(&r["files"]);
            let source_hits = matching_paths(&r["sources"]);
            if file_hits.is_empty() && source_hits.is_empty() {
                continue;
            }
            let title = collapse_preview(r["title"].as_str().unwrap_or(""), 240);
            for (idx, (path, previous)) in normalized_specs.iter().enumerate() {
                let Some(cited) = pick_cited(&file_hits, path, previous.as_deref())
                    .or_else(|| pick_cited(&source_hits, path, previous.as_deref()))
                else {
                    continue;
                };
                per_brain[idx].push(json!({
                    "id": r["id"],
                    "external_id": r["external_id"],
                    "type": r["type"],
                    "title": title,
                    "status": r["status"],
                    "authority": r["authority"],
                    "cited_path": cited,
                }));
            }
        }
    }

    let mut per_chunks: Vec<Vec<Value>> = vec![Vec::new(); normalized_specs.len()];
    if crate::fs::admit_directory(&context.roots.state.join("memory-bank/chunks")).unwrap_or(false)
    {
        let bank_service = BankService::new(context.clone());
        let mut state_files = PublicationFiles::new(&context.roots.state)?;
        for chunk_path in codec::paths(&bank_service)? {
            let Ok(chunk) = codec::read(&mut state_files, &chunk_path) else {
                continue;
            };
            let m = &chunk.metadata;
            if m["status"] != "active" || index::memory_temporal_reason(m).is_some() {
                continue;
            }
            let source_hits = matching_paths(&m["sources"]);
            if source_hits.is_empty() {
                continue;
            }
            let title = collapse_preview(m["title"].as_str().unwrap_or(""), 240);
            let auto_promoted = m["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag == "auto-promoted"));
            for (idx, (path, previous)) in normalized_specs.iter().enumerate() {
                let Some(cited) = pick_cited(&source_hits, path, previous.as_deref()) else {
                    continue;
                };
                per_chunks[idx].push(json!({
                    "id": m["id"],
                    "title": title,
                    "type": m["type"],
                    "review_after": m["review_after"],
                    "auto_promoted": auto_promoted,
                    "cited_path": cited,
                }));
            }
        }
        state_files.check()?;
    }

    let mut items = Vec::with_capacity(normalized_specs.len());
    for ((path, previous), (brain_records, memory_chunks)) in normalized_specs
        .iter()
        .zip(per_brain.into_iter().zip(per_chunks))
    {
        let linked = |name: &str| {
            brain_records
                .iter()
                .chain(&memory_chunks)
                .filter(|row| row["cited_path"] == name)
                .filter_map(|row| row["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let mut hygiene_warnings = Vec::new();
        let gone = !context.roots.project.join(path).exists();
        let here = linked(path);
        if gone && !here.is_empty() {
            hygiene_warnings.push(json!({
                "kind": "cited-file-missing",
                "path": path,
                "linked": here,
                "recommended_action": "review",
                "text": "This file is gone from the workspace; the memory that cites it may be stale.",
            }));
        }
        if let Some(previous) = previous {
            let before = linked(previous);
            if !before.is_empty() {
                hygiene_warnings.push(json!({
                    "kind": "cited-file-renamed",
                    "path": path,
                    "previous_path": previous,
                    "linked": before,
                    "recommended_action": "review",
                    "text": format!("This file was {previous}; the memory that cites the old name may need its citation updated."),
                }));
            }
        }

        let mut c_warnings = Vec::new();
        if let Some(eng) = engine
            && let Ok(fctx) = eng.file_context(project_id, path, None)
        {
            c_warnings = contradiction_warnings(&fctx["contradictions"], &fctx["atoms"]);
        }

        items.push(json!({
            "project_id": project_id,
            "path": path,
            "previous_path": previous,
            "brain_records": brain_records,
            "memory_chunks": memory_chunks,
            "hygiene_warnings": hygiene_warnings,
            "contradiction_warnings": c_warnings,
        }));
    }

    Ok(json!({
        "ok": true,
        "items": items,
    }))
}

/// Promotes a safe, source-grounded Omnigraph `ContextAtom` into a `verified`
/// `Project Brain` record via `MemoryService`, enforcing privacy, secret,
/// prompt-injection, provenance, and readable-file custody checks.
pub fn promote_atom(context: &RuntimeContext, body: &Value) -> Result<Value> {
    if context.mode != "governed" {
        return Err(RuntimeError::policy(
            "Promoting an Omnigraph atom to Project Brain requires governed mode.",
        ));
    }
    let atom = body
        .get("atom")
        .filter(|v| v.is_object())
        .ok_or_else(|| RuntimeError::invalid("Missing Omnigraph `atom` object payload."))?;
    let title = atom["title"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RuntimeError::invalid("Missing atom `title`."))?;
    let body_text = atom["body"]
        .as_str()
        .or_else(|| atom["summary"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(title);
    if title.chars().count() > 2000 || body_text.chars().count() > 16000 {
        return Err(RuntimeError::invalid("Atom title or body exceeds promotion limits."));
    }

    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(explicit) => {
            if !["finding", "decision", "bug", "incident"].contains(&explicit) {
                return Err(RuntimeError::invalid(
                    "Promote kind must be one of: finding, decision, bug, incident.",
                ));
            }
            explicit
        }
        None if body.get("kind").is_some_and(|v| !v.is_null()) => {
            return Err(RuntimeError::invalid(
                "Promote kind must be one of: finding, decision, bug, incident.",
            ));
        }
        None => match atom["kind"].as_str().unwrap_or("finding") {
            "decision" => "decision",
            _ => "finding",
        },
    };

    let privacy = match body.get("privacy").and_then(Value::as_str) {
        Some(explicit) => {
            if !["public", "team"].contains(&explicit) {
                return Err(RuntimeError::invalid(
                    "Promote privacy must be public or team.",
                ));
            }
            explicit
        }
        None if body.get("privacy").is_some_and(|v| !v.is_null()) => {
            return Err(RuntimeError::invalid(
                "Promote privacy must be public or team.",
            ));
        }
        None => "team",
    };

    let config = RuntimeConfig::load(&context.roots)?;
    let allowed_privacy = config.strings("allowed_privacy")?;
    if !allowed_privacy.iter().any(|p| p == privacy) {
        return Err(RuntimeError::policy(format!(
            "Privacy '{privacy}' is disallowed by project configuration."
        )));
    }

    let combined = format!("{title}\n{body_text}");
    if let Some(found) = domain::secret_label_in(&context.roots, &combined)? {
        return Err(RuntimeError::policy(format!(
            "Atom content contains {found} and cannot be promoted to Project Brain."
        )));
    }
    if let Some(found) = domain::sensitive_label(&combined)? {
        return Err(RuntimeError::policy(format!(
            "Atom content contains {found} and cannot be promoted to Project Brain."
        )));
    }
    if promotion::instruction_like(title, body_text)? {
        return Err(RuntimeError::policy(
            "Atom contains instruction-like content and cannot be promoted without sanitization.",
        ));
    }

    let raw_files: Vec<String> = body
        .get("files")
        .or_else(|| atom.get("files"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .take(50)
        .collect();
    let mut raw_sources_for_probe = raw_files.clone();
    for src in atom["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        raw_sources_for_probe.push(src.to_owned());
    }
    let probe = json!({
        "title": title,
        "sources": raw_sources_for_probe,
    });
    let problems = promotion::provenance_problems(&probe, Some(body_text))?;
    if !problems.is_empty() {
        return Err(RuntimeError::invalid(format!(
            "Atom provenance check failed: {}",
            problems.join("; ")
        )));
    }

    let mut verified_files = Vec::new();
    let mut seen_files = BTreeSet::new();
    let mut project_files = PublicationFiles::new(&context.roots.project)?;
    for raw in &raw_files {
        let clean = clean_relative_path(raw.split('#').next().unwrap_or(raw)).ok_or_else(|| {
            RuntimeError::policy(format!("Invalid file path in promoted atom: {raw}"))
        })?;
        if project_files.image_identity(&clean)?.is_none()
            || project_files.read_prefix(&clean, 65536)?.is_none()
        {
            return Err(RuntimeError::conflict(format!(
                "Cited file is unavailable or unreadable in the workspace: {clean}"
            )));
        }
        if seen_files.insert(clean.clone()) {
            verified_files.push(clean);
        }
    }
    project_files.check()?;

    let atom_id = atom["atom_id"]
        .as_str()
        .or_else(|| atom["id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let external_id = if atom_id.is_empty() {
        format!("omnigraph-{}", super::hexid())
    } else {
        let sanitized: String = atom_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .take(180)
            .collect();
        format!("omnigraph-{sanitized}")
    };

    let brain = BrainRepository::new(context.clone())?;
    if let Some(existing_id) = atom["brain_record_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match brain.find(existing_id, None, false) {
            Ok(img) if img.record["authority"] == "verified" => {
                return Ok(json!({
                    "record_id": img.record["id"],
                    "record_type": img.record["type"],
                    "revision": img.record["revision"],
                    "verified_files": verified_files,
                    "privacy": img.record["privacy"],
                    "authority": "verified",
                    "origin_layer": "project-brain",
                }));
            }
            Ok(_) => {
                return Err(RuntimeError::conflict(
                    "Linked Project Brain record is not verified.",
                ));
            }
            Err(_) => {
                return Err(RuntimeError::conflict(format!(
                    "Linked Project Brain record is missing: {existing_id}"
                )));
            }
        }
    }
    if let Ok(img) = brain.find(&external_id, None, false) {
        if img.record["authority"] == "verified" {
            return Ok(json!({
                "record_id": img.record["id"],
                "record_type": img.record["type"],
                "revision": img.record["revision"],
                "verified_files": verified_files,
                "privacy": img.record["privacy"],
                "authority": "verified",
                "origin_layer": "project-brain",
            }));
        }
        return Err(RuntimeError::conflict(
            "An unverified Project Brain record already exists for this atom.",
        ));
    }

    let brain_req = json!({
        "type": kind,
        "external_id": external_id,
        "title": title,
        "goal": body_text,
        "files": verified_files,
        "sources": verified_files,
        "privacy": privacy,
        "authority": "verified",
        "attestation": "person",
        "confidence": atom["confidence"].as_f64().unwrap_or(1.0),
        "json": true,
    });
    let created = MemoryService::new(context.clone(), None).execute("brain-create", &brain_req, &[])?;
    Ok(json!({
        "record_id": created["id"],
        "record_type": created["type"],
        "revision": created["revision"],
        "verified_files": verified_files,
        "privacy": created["privacy"],
        "authority": "verified",
        "origin_layer": "project-brain",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Roots, domain::uuid4};

    #[test]
    fn bridge_detects_missing_renamed_and_contradicting_links_and_promotes_atom() {
        let root = std::env::temp_dir().join(format!("cg-bridge-{}", uuid4()));
        std::fs::create_dir_all(root.join("memory-bank/chunks")).unwrap();
        std::fs::create_dir_all(root.join("project-brain/schemas")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::copy(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("schemas/dynamic-record.schema.json"),
            root.join("project-brain/schemas/dynamic-record.schema.json"),
        )
        .unwrap();
        std::fs::write(root.join("src/InvoiceService.php"), "<?php\nclass InvoiceService {}\n").unwrap();
        std::fs::write(root.join("src/OldInvoice.php"), "<?php\nclass OldInvoice {}\n").unwrap();

        let ctx = RuntimeContext::new(Roots::installed(&root).unwrap(), "local", "governed").unwrap();
        let brain = BrainRepository::new(ctx.clone()).unwrap();
        let rec = brain
            .create(crate::CreateRecord {
                record_type: "decision".into(),
                external_id: "EXT-INV-1".into(),
                title: "Invoice rounding decision".into(),
                goal: Some("Always round half-up".into()),
                files: vec!["src/OldInvoice.php".into()],
                sources: vec!["src/OldInvoice.php".into()],
                privacy: "team".into(),
                authority: "verified".into(),
                attestation: Some("person".into()),
                ..Default::default()
            })
            .unwrap();
        std::fs::remove_file(root.join("src/OldInvoice.php")).unwrap();

        let eng = OmnigraphEngine::open(&ctx).unwrap();
        eng.publish_atom(&json!({
            "atom_id": format!("brain-{}", rec["id"].as_str().unwrap()),
            "project_id": "proj",
            "kind": "decision",
            "title": "Invoice rounding decision",
            "files": ["src/InvoiceService.php"],
            "brain_record_id": rec["id"],
            "verified": true,
            "origin_layer": "project-brain",
        }))
        .unwrap();
        eng.publish_atom(&json!({
            "atom_id": "atom-contra",
            "project_id": "proj",
            "kind": "warning",
            "title": "Legacy banker rounding note",
            "files": ["src/InvoiceService.php"],
            "relates_to": {
                "target_id": format!("brain-{}", rec["id"].as_str().unwrap()),
                "relation": "contradicts",
                "note": "Check VAT rule"
            }
        }))
        .unwrap();

        let links = memory_links_for_files(
            &ctx,
            "proj",
            &[("src/InvoiceService.php".into(), Some("src/OldInvoice.php".into()))],
            Some(&eng),
        )
        .unwrap();
        let item = &links["items"][0];
        assert_eq!(item["brain_records"].as_array().unwrap().len(), 1);
        assert_eq!(item["hygiene_warnings"][0]["kind"], "cited-file-renamed");
        assert_eq!(item["contradiction_warnings"][0]["kind"], "atom-contradiction");

        // Secret atom blocked during promote_atom
        let err = promote_atom(
            &ctx,
            &json!({
                "atom": {
                    "atom_id": "a-bad",
                    "title": "Bad secret",
                    "body": "ghp_1234567890abcdefghijklmnopqrstuvwxyz"
                }
            }),
        )
        .unwrap_err();
        assert!(err.message.contains("cannot be promoted"));

        // Safe atom promotes cleanly into Project Brain
        let promoted = promote_atom(
            &ctx,
            &json!({
                "atom": {
                    "atom_id": "a-good",
                    "kind": "finding",
                    "title": "Invoice totals cache in local ledger",
                    "body": "Invoice totals are verified before emission.",
                    "files": ["src/InvoiceService.php"]
                },
                "kind": "finding",
                "privacy": "team"
            }),
        )
        .unwrap();
        assert_eq!(promoted["authority"], "verified");
        assert_eq!(promoted["origin_layer"], "project-brain");

        let _ = std::fs::remove_dir_all(&root);
    }
}
