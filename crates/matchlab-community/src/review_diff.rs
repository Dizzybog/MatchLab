//! Avoid meaningless community-learning PRs from JSON whitespace rewrites.
//!
//! The rule JSON files are trusted review data, not executable regex. Compare
//! decoded JSON values, restore exact bytes for unchanged files, and gate PR
//! creation only when a candidate list genuinely changes.
use crate::json::Json;
use crate::util::Result;
use std::path::Path;

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn semantic_change(before: &Path, after: &Path) -> Result<bool> {
    let original = read(before)?;
    let proposed = read(after)?;
    let old = Json::parse_bytes(&original).map_err(|error| format!("invalid review JSON: {error}"))?;
    let new = Json::parse_bytes(&proposed).map_err(|error| format!("invalid review JSON: {error}"))?;
    if old == new {
        // A reviewer may serialize [] instead of [\n]. Do not create a PR
        // for JSON formatting or accidentally mark an empty proposal done.
        std::fs::write(after, &original).map_err(|error| format!("{}: {error}", after.display()))?;
        return Ok(false);
    }
    if old.as_arr().is_none() || new.as_arr().is_none() {
        return Err("community rule files must contain JSON arrays".into());
    }
    Ok(true)
}

pub fn check(rules_before: &Path, rules_after: &Path, extra_before: &Path, extra_after: &Path) -> Result<bool> {
    let rules = semantic_change(rules_before, rules_after)?;
    let extra = semantic_change(extra_before, extra_after)?;
    Ok(rules || extra)
}

/// Only a tested native handler diff plus new trusted regressions opens a PR.
pub fn source_proven(before_handler: &Path, after_handler: &Path, before_fixtures: &Path, after_fixtures: &Path) -> Result<bool> {
    if read(before_handler)? == read(after_handler)? {
        return Ok(false);
    }
    let parse = |path: &Path| -> Result<Json> {
        Json::parse_bytes(&read(path)?).map_err(|error| format!("invalid promoted regressions: {error}"))
    };
    let (old, new) = (parse(before_fixtures)?, parse(after_fixtures)?);
    let (Some(old), Some(new)) = (old.as_arr(), new.as_arr()) else {
        return Err("invalid promoted fixture schema".into());
    };
    if new.len() <= old.len() || new[..old.len()] != old[..] {
        return Err("source changed without verified regression additions".into());
    }
    Ok(true)
}
