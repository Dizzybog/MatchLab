//! Durable GitHub review backlog for independently screened rule candidates.
//!
//! This file is a GitHub Release asset, not executable code. A proposal is removed
//! only after its fingerprint enters a successfully created human-review PR.
//! Failed screening and interrupted daily workflows never silently discard it.
use crate::json::Json;
use crate::obj;
use crate::util::{is_hex64, sha256_hex, Result};
use std::collections::{BTreeSet, HashSet};

pub const MAX_PENDING: usize = 20_000;
pub const MAX_REVIEW: usize = 4096;
pub const MAX_DAILY: usize = 24;

pub fn fp_of(row: &Json) -> Option<&str> {
    row.as_obj()?;
    row.field("fingerprint").as_str().filter(|fp| is_hex64(fp))
}

/// Fingerprints of anime marker rules (`community_rules.json`) and
/// extended proposals already queued in `community_regex_proposals.json`.
pub fn curated_fingerprints(rules: &Json, extended: &Json) -> BTreeSet<String> {
    let mut known: BTreeSet<String> =
        extended.as_arr().into_iter().flatten().filter_map(fp_of).map(str::to_owned).collect();
    for row in rules.as_arr().into_iter().flatten() {
        if let Some(marker) = row.field("marker").as_str().filter(|_| row.as_obj().is_some()) {
            known.insert(sha256_hex(format!("community-regex:{marker}")));
        }
    }
    known
}

pub fn merge(old: &Json, incoming: &Json, seen: &BTreeSet<String>, curated: &BTreeSet<String>) -> Result<Json> {
    if !old.has_exact_keys(&["pending"]) {
        return Err("Invalid previous community backlog".into());
    }
    let (Some(queue), Some(incoming)) = (old.field("pending").as_arr(), incoming.as_arr()) else {
        return Err("Invalid candidate list".into());
    };
    if queue.len() > MAX_PENDING || incoming.len() > MAX_REVIEW {
        return Err("Community backlog exceeds capacity; do not discard data".into());
    }
    let mut out = Vec::new();
    let mut added = HashSet::new();
    for row in queue.iter().chain(incoming) {
        let Some(fingerprint) = fp_of(row) else { return Err("Invalid queued rule fingerprint".into()) };
        if seen.contains(fingerprint) || curated.contains(fingerprint) || !added.insert(fingerprint.to_owned()) {
            continue;
        }
        out.push(row.clone());
    }
    if out.len() > MAX_PENDING {
        return Err("Community backlog full; source comments remain on GitHub".into());
    }
    Ok(obj! {"pending" => out})
}

pub fn finalize(backlog: &Json, attempted: &[Option<String>], accepted: &[String]) -> Result<Json> {
    if !backlog.has_exact_keys(&["pending"]) {
        return Err("Invalid backlog".into());
    }
    let accepted: HashSet<Option<&str>> = accepted.iter().map(|fp| Some(fp.as_str())).collect();
    let attempt: HashSet<Option<&str>> = attempted.iter().map(Option::as_deref).collect();
    let queue = backlog.field("pending").as_arr().cloned().unwrap_or_default();
    // Failed/unresolved suggestions rotate to the back, not deleted.
    let remaining: Vec<&Json> = queue.iter().filter(|row| !accepted.contains(&fp_of(row))).collect();
    let mut out: Vec<Json> = remaining.iter().filter(|row| !attempt.contains(&fp_of(row))).map(|row| (*row).clone()).collect();
    out.extend(remaining.iter().filter(|row| attempt.contains(&fp_of(row))).map(|row| (*row).clone()));
    Ok(obj! {"pending" => out})
}

pub fn parse_accepted(value: &Json) -> Result<Vec<String>> {
    let Some(items) = value.as_arr() else { return Err("invalid accepted fingerprint list".into()) };
    items
        .iter()
        .map(|item| item.as_str().filter(|fp| is_hex64(fp)).map(str::to_owned).ok_or_else(|| "invalid accepted fingerprint list".to_owned()))
        .collect()
}
