//! Small workflow steps that were inline Python in community-learning.yml.
use crate::json::{float_repr, Json};
use crate::util::{sha256_hex, Result};
use std::collections::{BTreeSet, HashSet};

/// Python `repr()` of a decoded JSON value.
pub fn py_repr(value: &Json) -> String {
    match value {
        Json::Null => "None".into(),
        Json::Bool(true) => "True".into(),
        Json::Bool(false) => "False".into(),
        Json::Int(number) => number.to_string(),
        Json::Float(number) => float_repr(*number).replace("Infinity", "inf").replace("NaN", "nan"),
        Json::Str(text) => py_str_repr(text),
        Json::Arr(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Json::Obj(map) => format!(
            "{{{}}}",
            map.iter().map(|(key, value)| format!("{}: {}", py_str_repr(key), py_repr(value))).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if (ch as u32) < 0x20 || (0x7f..0xa0).contains(&(ch as u32)) => out.push_str(&format!("\\x{:02x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push(quote);
    out
}

/// Python `str()` of a decoded JSON value.
fn py_str(value: &Json) -> String {
    match value {
        Json::Str(text) => text.clone(),
        other => py_repr(other),
    }
}

fn py_int(value: Option<&Json>) -> i128 {
    match value {
        None => 0,
        Some(Json::Str(text)) => text.trim().parse().unwrap_or(0),
        Some(other) => other.as_number().map(|number| number.trunc() as i128).unwrap_or(0),
    }
}

fn prefix(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// Markdown audit counters appended to the bot PR body. Counters and
/// fingerprints only: never titles.
pub fn pr_report(audits: &Json, promotion: &Json) -> String {
    let mut out = String::from("\n### Native Rust regex audit\n");
    for row in audits.as_arr().into_iter().flatten() {
        let text = |key: &str, default: &str| row.get(key).map(py_str).unwrap_or_else(|| default.to_owned());
        let fingerprint = prefix(&text("fingerprint", ""), 12);
        let kind = text("kind", "unknown");
        let status = text("status", "unknown");
        let reason = prefix(&text("reason", ""), 160);
        let operations: Vec<Json> = row
            .get("source_changes")
            .and_then(Json::as_arr)
            .into_iter()
            .flatten()
            .map(|change| change.get("operation").cloned().unwrap_or(Json::from("?")))
            .collect();
        out.push_str(&format!(
            "- {fingerprint} ({kind}): {status}; real cases={}, changed={}, verified gains={}, synthetic changes={}, \
             regressions={}, operations={}; reason={reason}\n",
            py_int(row.get("community_title_cases")),
            py_int(row.get("community_title_changes")),
            py_int(row.get("verified_improvements")),
            py_int(row.get("synthetic_changes")),
            py_int(row.get("regressions")),
            py_repr(&Json::Arr(operations)),
        ));
    }
    if promotion.truthy() {
        let indexes = promotion.get("source_handler_indexes").cloned().unwrap_or(Json::Arr(Vec::new()));
        out.push_str(&format!(
            "\nSource merge proposal: handler {}; trusted fixtures promoted to Rust regression tests.\n",
            py_repr(&indexes)
        ));
    }
    out
}

/// Fingerprints of rules newly placed into the review PR.
pub fn accepted_fingerprints(rules_before: &Json, rules_after: &Json, extended_before: &Json, extended_after: &Json) -> Result<Vec<String>> {
    let text = |row: &Json, key: &str| -> Result<String> {
        row.get(key).and_then(Json::as_str).map(str::to_owned).ok_or_else(|| format!("community rule without {key}"))
    };
    let rows = |value: &Json| value.as_arr().cloned().unwrap_or_default();
    let old_markers: HashSet<String> = rows(rules_before).iter().map(|row| text(row, "marker")).collect::<Result<_>>()?;
    let mut accepted = Vec::new();
    for row in rows(rules_after) {
        let marker = text(&row, "marker")?;
        if !old_markers.contains(&marker) {
            accepted.push(sha256_hex(format!("community-regex:{marker}")));
        }
    }
    let old: HashSet<String> = rows(extended_before).iter().map(|row| text(row, "fingerprint")).collect::<Result<_>>()?;
    for row in rows(extended_after) {
        let fingerprint = text(&row, "fingerprint")?;
        if !old.contains(&fingerprint) {
            accepted.push(fingerprint);
        }
    }
    Ok(accepted)
}

/// Add accepted fingerprints to the dedup ledger lines.
pub fn ledger_with(seen: Vec<String>, accepted: &[String]) -> BTreeSet<String> {
    let mut all: BTreeSet<String> = seen.into_iter().collect();
    all.extend(accepted.iter().cloned());
    all
}
