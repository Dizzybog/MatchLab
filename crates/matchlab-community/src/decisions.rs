//! Persist evidence-based review decisions without losing unmerged proposals.
//!
//! A definitive/no-proof result is cached only for the EXACT canonical handler
//! catalogue and independently labeled fixture corpus. When either changes, the
//! same original GitHub proposal is eligible for a fresh Rust replay. No Issue
//! claim can become a ground-truth label through this cache.
use crate::json::{Json, Object};
use crate::obj;
use crate::util::{is_hex64, sha256_hex, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const DECISIONS: &[&str] = &[
    "review_only",
    "invalid_source",
    "no_effect",
    "regression",
    "needs_independent_evidence",
    "ready_for_source_diff",
];
pub const MAX_DECISIONS: usize = 20_000;

fn deferred_status(status: &str) -> bool {
    DECISIONS.contains(&status) && status != "ready_for_source_diff"
}

fn rust_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// SHA-256 identity of the handler catalogue, fixtures and parser sources.
pub fn signature(handlers: &Path, fixtures: &Path, parser_src: Option<&Path>) -> Result<String> {
    let read = |path: &Path| std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()));
    let mut data = read(handlers)?;
    data.push(0);
    data.extend(read(fixtures)?);
    data.push(0);
    if let Some(root) = parser_src {
        let mut files = Vec::new();
        if root.is_dir() {
            rust_files(root, &mut files)?;
            // Python sorts Path objects component-wise.
            files.sort_by(|left, right| left.components().cmp(right.components()));
        } else {
            files.push(root.to_path_buf());
        }
        if files.is_empty() || files.len() > 64 {
            return Err("Invalid native Rust parser source tree".into());
        }
        for path in files {
            let name = if root.is_dir() {
                path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned()
            } else {
                path.file_name().unwrap_or_default().to_string_lossy().into_owned()
            };
            data.extend(name.as_bytes());
            data.push(0);
            data.extend(read(&path)?);
            data.push(0);
        }
    }
    Ok(sha256_hex(data))
}

pub fn validate(state: &Json) -> Result<()> {
    if !state.has_exact_keys(&["decisions"]) {
        return Err("Invalid durable community decision state".into());
    }
    let Some(data) = state.field("decisions").as_obj().filter(|data| data.len() <= MAX_DECISIONS) else {
        return Err("Community decisions capacity or schema invalid".into());
    };
    for (fingerprint, info) in data {
        if !is_hex64(fingerprint) {
            return Err("Invalid decision fingerprint".into());
        }
        let valid = info.has_exact_keys(&["status", "signature", "checked_at"])
            && info.field("status").as_str().is_some_and(|status| DECISIONS.contains(&status))
            && info.field("signature").as_str().is_some_and(is_hex64)
            && info.field("checked_at").as_str().is_some_and(|text| text.chars().count() <= 48);
        if !valid {
            return Err("Invalid durable decision record".into());
        }
    }
    Ok(())
}

pub fn due(pending: &Json, state: &Json, current_signature: &str, limit: usize) -> Result<(Vec<Json>, usize)> {
    validate(state)?;
    let Some(pending) = pending.as_arr() else { return Err("Invalid pending candidate queue".into()) };
    let decisions = state.field("decisions");
    let mut selected = Vec::new();
    let mut deferred = 0;
    for proposal in pending {
        let Some(fingerprint) = proposal.field("fingerprint").as_str().filter(|fp| is_hex64(fp)) else {
            return Err("Invalid pending candidate fingerprint".into());
        };
        if let Some(last) = decisions.get(fingerprint)
            && last.field("signature").str_eq(current_signature)
                && last.field("status").as_str().is_some_and(deferred_status)
            {
                deferred += 1;
                continue;
            }
        if selected.len() < limit {
            selected.push(proposal.clone());
        }
    }
    Ok((selected, deferred))
}

pub fn update(state: &Json, audit: &Json, pending: &Json, current_signature: &str, now: &str) -> Result<Json> {
    validate(state)?;
    let (Some(audit), Some(pending)) = (audit.as_arr(), pending.as_arr()) else {
        return Err("Invalid audited results or pending backlog".into());
    };
    let mut allowed = HashSet::new();
    for row in pending {
        let Some(fingerprint) = row.field("fingerprint").as_str().filter(|fp| is_hex64(fp)) else {
            return Err("Invalid pending fingerprint".into());
        };
        allowed.insert(fingerprint.to_owned());
    }
    let mut results: Object = state
        .field("decisions")
        .as_obj()
        .into_iter()
        .flatten()
        .filter(|(fingerprint, _)| allowed.contains(*fingerprint))
        .map(|(fingerprint, record)| (fingerprint.clone(), record.clone()))
        .collect();
    let mut visited = HashSet::new();
    for item in audit {
        if item.as_obj().is_none() {
            return Err("Invalid native auditor result".into());
        }
        let fingerprint = item.field("fingerprint").as_str().filter(|fp| is_hex64(fp));
        let Some(fingerprint) = fingerprint.filter(|fp| visited.insert(fp.to_string())) else {
            return Err("Duplicate or invalid decision result".into());
        };
        let Some(status) = item.field("status").as_str().filter(|status| DECISIONS.contains(status)) else {
            return Err("Unknown audit decision status".into());
        };
        if allowed.contains(fingerprint) {
            results.insert(
                fingerprint.to_owned(),
                obj! {"status" => status, "signature" => current_signature, "checked_at" => now},
            );
        }
    }
    let updated = obj! {"decisions" => Json::Obj(results)};
    validate(&updated)?;
    Ok(updated)
}
