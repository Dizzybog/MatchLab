//! GitHub Actions-only reviewer for ML-generated field and source regex changes.
//!
//! Untrusted comments become review-only data, never executable regex or
//! GitHub Actions code. Runtime rules require independent source verification,
//! regression tests and human approval before the production parser is changed.
use crate::json::Json;
use crate::obj;
use crate::rule_intake::{valid_any_rule, Catalog};
use crate::util::Result;
use std::collections::HashMap;

pub const ALLOWED: &[&str] = &[
    "learned_field_regex",
    "learned_tv_regex",
    "learned_source_regex_patch",
    "learned_corrective_regex",
    "learned_general_regex_review",
    "learned_field_regex_review",
    "learned_title_boundary_regex_review",
    "historical_regex_review",
    "verified_regex_proposal",
];
pub const MAX_QUEUE: usize = 4096;

fn number(value: Option<&Json>) -> f64 {
    value.and_then(Json::as_number).unwrap_or(0.0)
}

fn fingerprint_key(value: &Json) -> String {
    crate::json::dumps(value, crate::json::Dump::DEFAULT)
}

/// Promote at most one new candidate into the review queue.
pub fn screen(
    incoming: &Json,
    old: &Json,
    audits: Option<&Json>,
    ancestor: crate::util::Ancestor,
    catalog: &Catalog,
) -> Result<(Json, bool)> {
    let (Some(incoming), Some(queue)) = (incoming.as_arr(), old.as_arr()) else {
        return Err("invalid review candidates".into());
    };
    if queue.len() > MAX_QUEUE {
        return Err("invalid review candidates".into());
    }
    let fingerprints: Vec<String> =
        queue.iter().filter(|item| item.as_obj().is_some()).map(|item| fingerprint_key(item.field("fingerprint"))).collect();
    let mut audit_by_fp: HashMap<String, &Json> = HashMap::new();
    for row in audits.and_then(Json::as_arr).into_iter().flatten() {
        if row.as_obj().is_some() {
            audit_by_fp.insert(fingerprint_key(row.field("fingerprint")), row);
        }
    }
    let empty = Json::Obj(Default::default());
    for candidate in incoming.iter().take(24) {
        if !candidate.field("kind").as_str().is_some_and(|kind| ALLOWED.contains(&kind)) {
            continue;
        }
        let Some((sha, _)) = candidate.field("source_build").as_str().and_then(|source| source.split_once(':')) else {
            continue;
        };
        if !valid_any_rule(candidate, sha, ancestor, catalog) {
            continue;
        }
        let key = fingerprint_key(candidate.field("fingerprint"));
        if fingerprints.contains(&key) {
            continue;
        }
        let audit = audit_by_fp.get(&key).copied().unwrap_or(&empty);
        // In production the trusted Rust auditor is mandatory. Prefer an
        // actually verified source diff over earlier review-only proposals.
        if audits.is_some()
            && !(audit.field("status").str_eq("ready_for_source_diff")
                && audit.field("promotion_eligible").is_true()
                && audit.get("regressions") == Some(&Json::Int(0))
                && number(audit.get("verified_improvements")) >= 2.0
                && number(audit.get("distinct_verified_titles")) >= 2.0
                && (audit.field("source_changes").truthy()
                    || candidate.field("kind").str_eq("learned_source_regex_patch")))
        {
            continue;
        }
        // Proven no-ops and conflicts do not deserve another PR.
        if matches!(audit.field("status").as_str(), Some("no_effect" | "invalid_source" | "regression")) {
            continue;
        }
        // Self-reported evidence is never independent attestation. Propose
        // precisely the source regex change, but DO NOT load the untrusted
        // comment as an active handler or execute its pattern.
        let proposal = obj! {
            "fingerprint" => candidate.field("fingerprint").clone(),
            "status" => "review_required",
            "automatic_activation" => false,
            "source_sha" => sha,
            "proposal" => candidate.clone(),
            "audit_status" => audit.get("status").cloned().unwrap_or(Json::from("not_audited")),
            "audit_reason" => audit.get("reason").cloned().unwrap_or(Json::from("source and field validation required")),
            "review_checks" => Json::from_strs([
                "confirm independent original-parser replay with true labels",
                "check false positives, titles with numbers and episode packs",
                "test parser/matcher precision and throughput on both CPUs",
                "approve exact source diff before enabling in production",
            ]),
        };
        let mut next = queue.clone();
        next.push(proposal);
        return Ok((Json::Arr(next), true));
    }
    Ok((old.clone(), false))
}
