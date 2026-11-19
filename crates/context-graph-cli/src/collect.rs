//! Optional `code2prompt` context collector wrapper scoped safely to the workspace.
use crate::process;
use context_graph_core::{
    Result, RuntimeError,
    domain::json,
    fs,
};
use serde_json::{Value, json as object};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::PathBuf,
    time::Duration,
};

const ALWAYS_EXCLUDE: &[&str] = &[
    "**/.git/**",
    "**/.env",
    "**/.env.*",
    "**/*.pem",
    "**/*.key",
    "**/*.sqlite",
    "**/*.db",
    "**/secrets/**",
    "**/.secrets/**",
    "**/credentials/**",
    "**/node_modules/**",
    "**/vendor/**",
    "**/target/**",
    "**/memory-bank/local/**",
    "**/project-brain/local/**",
    "**/.c2p/**",
];

const DEFAULT_INCLUDES: &[&str] = &[
    "*.md",
    "docs/**",
    "specs/**",
    "src/**",
    "crates/**",
    "app/**",
    "lib/**",
];

#[derive(Clone, Debug, Default)]
pub struct CollectRequest {
    pub root: PathBuf,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub format: String,
    pub encoding: String,
    pub output: Option<PathBuf>,
    pub dry_run: bool,
    pub stdout_bundle: bool,
    pub top: usize,
    pub binary: Option<OsString>,
}

pub fn execute(req: &CollectRequest) -> Result<Value> {
    let root = fs::canonicalize(&req.root)?;
    if !root.is_dir() {
        return Err(RuntimeError::invalid("Collection root must be a directory"));
    }

    let encoding = if req.encoding.is_empty() {
        "cl100k"
    } else {
        req.encoding.as_str()
    };
    if !["cl100k", "p50k", "p50k_edit", "r50k"].contains(&encoding) {
        return Err(RuntimeError::invalid("Unsupported collection encoding"));
    }
    let format = if req.format.is_empty() {
        "markdown"
    } else {
        req.format.as_str()
    };
    if !["markdown", "json", "xml"].contains(&format) {
        return Err(RuntimeError::invalid("Unsupported collection format"));
    }

    let includes: Vec<String> = if req.includes.is_empty() {
        DEFAULT_INCLUDES.iter().map(|s| (*s).to_string()).collect()
    } else {
        req.includes.clone()
    };
    let mut excludes: Vec<String> = ALWAYS_EXCLUDE.iter().map(|s| (*s).to_string()).collect();
    excludes.extend(req.excludes.clone());

    let binary = req
        .binary
        .clone()
        .or_else(|| std::env::var_os("CODE2PROMPT_BIN"))
        .unwrap_or_else(|| OsString::from("code2prompt"));

    let empty_env = BTreeMap::new();
    let probe = process::run_with_env(
        &binary,
        &[OsString::from("--version")],
        &root,
        None,
        Duration::from_secs(10),
        64 * 1024,
        &empty_env,
    )
    .map_err(|_| {
        RuntimeError::unavailable(
            "code2prompt binary is not installed or unavailable in PATH (install code2prompt or set CODE2PROMPT_BIN)",
        )
    })?;
    if probe.code != 0 {
        return Err(RuntimeError::unavailable("code2prompt --version probe failed"));
    }
    let version = String::from_utf8_lossy(&probe.stdout).trim().to_string();

    let run_fmt = |fmt: &str| -> Result<Vec<u8>> {
        let mut args: Vec<OsString> = vec![
            root.as_os_str().to_os_string(),
            OsString::from("--output-format"),
            OsString::from(fmt),
            OsString::from("--encoding"),
            OsString::from(encoding),
            OsString::from("--no-clipboard"),
        ];
        if !includes.is_empty() {
            args.push(OsString::from("--include"));
            args.push(OsString::from(includes.join(",")));
        }
        if !excludes.is_empty() {
            args.push(OsString::from("--exclude"));
            args.push(OsString::from(excludes.join(",")));
        }
        let out = process::run_with_env(
            &binary,
            &args,
            &root,
            None,
            Duration::from_secs(30),
            64 * 1024 * 1024,
            &empty_env,
        )?;
        if out.code != 0 {
            return Err(RuntimeError::unavailable(format!(
                "code2prompt failed (exit {}): {}",
                out.code,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    };

    let json_raw = run_fmt("json")?;
    let payload: Value = serde_json::from_slice(&json_raw)
        .map_err(|_| RuntimeError::invalid("code2prompt returned invalid JSON"))?;
    let files = payload["files"]
        .as_array()
        .filter(|arr| !arr.is_empty())
        .ok_or_else(|| RuntimeError::not_found("Collection patterns matched no files"))?;
    let token_count = payload["token_count"].as_u64().unwrap_or(0);

    let mut largest = Vec::new();
    for entry in files {
        let Some(name) = entry.as_str() else {
            continue;
        };
        let clean = name.strip_prefix("./").unwrap_or(name);
        let p = root.join(clean);
        if fs::admit_descendant(&root, &p).is_err() {
            return Err(RuntimeError::policy("Collected file escapes workspace"));
        }
        if let Ok(meta) = std::fs::symlink_metadata(&p)
            && meta.is_file()
        {
            largest.push((meta.len(), clean.to_string()));
        }
    }
    largest.sort_by(|a, b| b.cmp(a));
    largest.truncate(req.top);

    let bundle_bytes = match format {
        "json" => json_raw,
        "xml" => payload["prompt"]
            .as_str()
            .ok_or_else(|| RuntimeError::invalid("code2prompt XML output missing"))?
            .as_bytes()
            .to_vec(),
        _ => run_fmt("markdown")?,
    };
    let bundle_text = String::from_utf8(bundle_bytes)
        .map_err(|_| RuntimeError::invalid("Collected bundle is not UTF-8"))?;

    let report = object!({
        "root": root,
        "include": includes,
        "exclude": excludes,
        "format": format,
        "encoding": encoding,
        "files": files.len(),
        "bytes": bundle_text.len(),
        "tokens": token_count,
        "code2prompt": version,
        "largest": largest,
    });

    if req.dry_run {
        return Ok(object!({"dry_run": true, "result": report}));
    }
    if req.stdout_bundle {
        return Ok(object!({
            "_cli": {
                "code": 0,
                "stdout": bundle_text,
                "stderr": format!("collected {} files ({} tokens, {})\n", files.len(), token_count, encoding),
            },
            "result": report,
        }));
    }

    let ext = if format == "markdown" { "md" } else { format };
    let out_path = match &req.output {
        Some(p) => fs::absolute(p)?,
        None => root.join(".c2p").join(format!("context-collect.{ext}")),
    };
    if let Some(parent) = out_path.parent() {
        fs::ensure_directory(parent, false)?;
    }
    fs::atomic_write(&out_path, bundle_text.as_bytes())?;
    let manifest_path = PathBuf::from(format!("{}.manifest.json", out_path.display()));
    fs::atomic_write(
        &manifest_path,
        format!("{}\n", json::serialize(&report, false, false, true, true)?).as_bytes(),
    )?;

    Ok(object!({
        "output": out_path,
        "manifest": manifest_path,
        "result": report,
    }))
}
