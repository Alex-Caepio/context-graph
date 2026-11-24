//! Symbol extraction and declaration-name matching for Omnigraph `SymbolRef`
//! and `MentionsSymbol` edges.
//!
//! Prefers read-only workspace `ir_symbols` tables (`graph_ir.sqlite3` /
//! `ir.sqlite`) when available and falls back to standalone multi-language
//! regex declaration extraction without external AST parsers.

use crate::fs;
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub const MAX_SYMBOL_FILES: usize = 8;
pub const MAX_ATOM_SYMBOLS: usize = 16;
const MAX_SOURCE_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredSymbol {
    pub symbol_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub parent: Option<String>,
    pub line: usize,
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Case-sensitive identifier match matching PR #112 `graph_live::names_declaration` rules:
/// identifiers with a capital letter, underscore, or digit match as whole words anywhere;
/// plain lowercase identifiers (`check`, `render`) match only in code context
/// (backticks, `name(`, `::name`, `->name`, `.name`).
pub fn names_declaration(text: &str, name: &str) -> bool {
    if name.chars().count() < 3 {
        return false;
    }
    let distinctive = name.contains('_')
        || name.chars().any(|c| c.is_ascii_digit())
        || (name.chars().any(|c| c.is_ascii_uppercase())
            && name.chars().any(|c| c.is_ascii_lowercase()));
    let mut from = 0;
    while let Some(at) = text[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let head = &text[..start];
        let tail = &text[end..];
        let before = head.chars().next_back();
        let after = tail.chars().next();
        if !before.is_some_and(is_ident_char)
            && !after.is_some_and(is_ident_char)
            && (distinctive
                || (before == Some('`') && after == Some('`'))
                || after == Some('(')
                || head.ends_with("::")
                || head.ends_with("->")
                || before == Some('.'))
        {
            return true;
        }
        from = end;
    }
    false
}

fn format_fqn(rel_path: &str, owner: Option<&str>, name: &str) -> String {
    match owner.filter(|o| !o.is_empty() && *o != rel_path) {
        Some(owner) => format!("{rel_path}::{owner}::{name}"),
        None => format!("{rel_path}::{name}"),
    }
}

fn read_workspace_text(root: &Path, rel_path: &str) -> Option<(String, String)> {
    let clean = rel_path
        .split('#')
        .next()
        .unwrap_or(rel_path)
        .trim()
        .trim_start_matches("./")
        .replace('\\', "/");
    if clean.is_empty()
        || clean.starts_with('/')
        || clean.split('/').any(|seg| seg == "..")
        || root.as_os_str().is_empty()
    {
        return None;
    }
    let real_root = fs::canonicalize(root).ok()?;
    let target = root.join(&clean);
    let real = fs::canonicalize(&target).ok()?;
    if !real.starts_with(&real_root) {
        return None;
    }
    let meta = std::fs::symlink_metadata(&real).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() as usize > MAX_SOURCE_BYTES {
        return None;
    }
    let bytes = fs::read_regular(&real).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some((clean, text))
}

fn find_ir_db(state_dir: Option<&Path>, root: &Path) -> Option<PathBuf> {
    let candidates = [
        "graph_ir.sqlite3",
        "memory-bank/local/graph_ir.sqlite3",
        "ir.sqlite",
        "memory-bank/local/ir.sqlite",
    ];
    for base in state_dir.into_iter().chain(std::iter::once(root)) {
        for rel in candidates {
            let candidate = base.join(rel);
            let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
                continue;
            };
            if meta.is_file() && !meta.file_type().is_symlink() {
                return Some(candidate);
            }
        }
    }
    None
}

fn query_ir_symbols(ir_db: &Path, rel_path: &str) -> Option<Vec<DeclaredSymbol>> {
    let conn = Connection::open_with_flags(
        ir_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT symbol_id, kind, name, parent_name, start_line \
             FROM ir_symbols \
             WHERE file_path = ?1 \
             ORDER BY start_line ASC, id ASC",
        )
        .ok()?;
    let rows = stmt
        .query_map([rel_path], |row| {
            Ok(DeclaredSymbol {
                symbol_id: row.get::<_, Option<String>>(0)?,
                kind: row.get::<_, String>(1)?,
                name: row.get::<_, String>(2)?,
                parent: row.get::<_, Option<String>>(3)?,
                line: row.get::<_, i64>(4).unwrap_or(1).max(1) as usize,
            })
        })
        .ok()?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        out.push(row);
    }
    (!out.is_empty()).then_some(out)
}

fn type_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:export\s+(?:default\s+)?)?(?:public\s+|private\s+|protected\s+|internal\s+|pub(?:\([^)]*\))?\s+|final\s+|abstract\s+|readonly\s+|sealed\s+|open\s+|data\s+)*(class|interface|trait|enum|struct|record)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("type declaration regex")
    })
}

fn impl_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*impl(?:<[^>]*>)?\s+(?:[A-Za-z0-9_:<>\s]+?\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("impl declaration regex")
    })
}

fn fn_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^([ \t]*)(?:export\s+(?:default\s+)?)?(?:pub(?:\([^)]*\))?\s+|public\s+|private\s+|protected\s+|internal\s+|static\s+|final\s+|abstract\s+|override\s+|suspend\s+|unsafe\s+|extern\s+|async\s+)*(fn|function|def|func)\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(\[<]",
        )
        .expect("function declaration regex")
    })
}

/// Lightweight cross-language declaration scan pairing types/impls with their indented/braced members.
pub fn extract_file_declarations(_rel_path: &str, source: &str) -> Vec<DeclaredSymbol> {
    let type_re = type_decl_re();
    let impl_re = impl_decl_re();
    let fn_re = fn_decl_re();

    let mut out = Vec::new();
    let mut brace_depth: i32 = 0;
    let mut current_parent: Option<(String, i32)> = None;

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        let line_opens = trimmed.bytes().filter(|b| *b == b'{').count() as i32;
        let line_closes = trimmed.bytes().filter(|b| *b == b'}').count() as i32;

        if let Some(cap) = type_re.captures(line) {
            let kind = cap[1].to_string();
            let name = cap[2].to_string();
            out.push(DeclaredSymbol {
                symbol_id: None,
                kind,
                name: name.clone(),
                parent: None,
                line: line_no,
            });
            current_parent = Some((name, brace_depth));
        } else if let Some(cap) = impl_re.captures(line) {
            let name = cap[1].to_string();
            current_parent = Some((name, brace_depth));
        } else if let Some(cap) = fn_re.captures(line) {
            let indent = cap[1].len();
            let name = cap[3].to_string();
            let parent = if indent > 0 {
                current_parent.as_ref().map(|(p, _)| p.clone())
            } else if current_parent
                .as_ref()
                .is_some_and(|(_, depth)| brace_depth > *depth)
            {
                current_parent.as_ref().map(|(p, _)| p.clone())
            } else {
                None
            };
            let kind = if parent.is_some() { "method" } else { "function" };
            out.push(DeclaredSymbol {
                symbol_id: None,
                kind: kind.to_string(),
                name,
                parent,
                line: line_no,
            });
        }

        brace_depth = (brace_depth + line_opens - line_closes).max(0);
        if let Some((_, open_depth)) = &current_parent
            && brace_depth <= *open_depth
            && line_closes > 0
        {
            current_parent = None;
        }
    }

    out
}

/// Returns up to `MAX_ATOM_SYMBOLS` declaration rows mentioned by `text` in the cited `files`.
pub fn extract_symbol_mentions(
    state_dir: Option<&Path>,
    project_root: Option<&Path>,
    files: &[String],
    text: &str,
) -> Vec<Value> {
    let Some(root) = project_root else {
        return Vec::new();
    };
    if text.trim().is_empty() || files.is_empty() {
        return Vec::new();
    }
    let ir_db = find_ir_db(state_dir, root);
    let mut symbols = Vec::new();

    for file in files.iter().take(MAX_SYMBOL_FILES) {
        let Some((rel, source)) = read_workspace_text(root, file) else {
            continue;
        };
        let decls = ir_db
            .as_deref()
            .and_then(|db| query_ir_symbols(db, &rel))
            .unwrap_or_else(|| extract_file_declarations(&rel, &source));

        let mut seen_names: BTreeSet<(Option<String>, String)> = BTreeSet::new();
        for decl in decls {
            if symbols.len() >= MAX_ATOM_SYMBOLS {
                break;
            }
            if decl.kind == "file"
                || !names_declaration(text, &decl.name)
                || !seen_names.insert((decl.parent.clone(), decl.name.clone()))
            {
                continue;
            }
            let fqn = format_fqn(&rel, decl.parent.as_deref(), &decl.name);
            let mut row = json!({
                "fqn": fqn,
                "file_path": rel,
                "kind": decl.kind,
                "name": decl.name,
                "line": decl.line,
            });
            if let Some(sid) = decl.symbol_id.filter(|s| !s.is_empty()) {
                row["symbol_id"] = json!(sid);
            }
            symbols.push(row);
        }
        if symbols.len() >= MAX_ATOM_SYMBOLS {
            break;
        }
    }
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_classes_methods_and_matches_names_declaration_safely() {
        let root = std::env::temp_dir().join(format!("cg-sym-{}", crate::domain::uuid4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/OrderService.php"),
            "<?php\nclass OrderService {\n    public function issueInvoice(): void {}\n    public function check(): bool { return true; }\n}\n",
        )
        .unwrap();

        let files = vec!["src/OrderService.php".to_string()];
        // `issueInvoice` is CamelCase so matches bare; `check` is lowercase so requires code syntax (`check()`), ignored as plain English "check".
        let found = extract_symbol_mentions(
            None,
            Some(&root),
            &files,
            "Use OrderService::issueInvoice to verify, and check logs later.",
        );
        let fqns = found
            .iter()
            .filter_map(|v| v["fqn"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            fqns,
            vec![
                "src/OrderService.php::OrderService",
                "src/OrderService.php::OrderService::issueInvoice",
            ]
        );

        let with_code_ref = extract_symbol_mentions(
            None,
            Some(&root),
            &files,
            "Call `check` or $svc->check() before retry.",
        );
        assert!(
            with_code_ref
                .iter()
                .any(|v| v["fqn"] == "src/OrderService.php::OrderService::check")
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
