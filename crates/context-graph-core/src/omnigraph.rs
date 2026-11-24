//! Standalone Omnigraph Context Sync, Branch DAG, and Graph-to-Memory Bridge.

pub mod bridge;
pub mod engine;
pub mod schema;
pub mod symbols;
pub mod transfer;

use std::path::Path;

pub use engine::{MergeKeepOptions, OmnigraphEngine, format_atom_line_with_authority};
pub use schema::CANONICAL_KNOWLEDGE_BRANCH;

pub const VALID_ATOM_KINDS: &[&str] = &[
    "finding", "decision", "warning", "contract", "insight", "artifact", "question",
];

pub const VALID_RELATIONS: &[&str] = &[
    "supports", "contradicts", "supersedes", "depends_on", "refines",
];

pub(crate) fn hexid() -> String {
    crate::domain::uuid4().replace('-', "")
}

pub fn session_branch_name(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(12)
        .collect();
    let suffix = if cleaned.is_empty() {
        "session"
    } else {
        cleaned.as_str()
    };
    format!("session/{suffix}")
}

pub(crate) fn now_utc_iso_ms() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn pascal_to_snake(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    for (idx, ch) in raw.chars().enumerate() {
        if ch.is_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn normalize_relation_name(raw: &str) -> Option<String> {
    let normalized = match raw.trim().to_ascii_lowercase().as_str() {
        "relates_to" | "related" => "supports".to_string(),
        other => other.to_string(),
    };
    VALID_RELATIONS
        .contains(&normalized.as_str())
        .then_some(normalized)
}

pub(crate) fn detect_language(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "php" | "phtml" | "inc" => Some("php"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "cs" => Some("csharp"),
        "rb" => Some("ruby"),
        "swift" => Some("swift"),
        "scala" | "sc" => Some("scala"),
        "sql" => Some("sql"),
        "md" => Some("markdown"),
        "css" => Some("css"),
        "html" => Some("html"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        _ => None,
    }
}
