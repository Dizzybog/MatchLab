//! Safely merge a verified regex alternative into its ORIGINAL native handler.
//!
//! Used only by trusted GitHub Actions after:
//!   (a) schema/source-ancestry validation of the community proposal;
//!   (b) the Rust auditor's full-parser replay;
//!   (c) >=2 *maintainer-owned* positive label fixtures and 0 regressions.
//!
//! This only stages a human-review PR. It never merges a GitHub PR itself.
//! Historical/unlabeled/ambiguous/community-claimed examples cannot authorize edits.
use crate::json::{dumps, Dump, Json};
use crate::obj;
use crate::regex_plan::{candidate_valid, resolve};
use crate::rule_intake::{technical_pattern, Catalog};
use crate::util::{is_hex64, Result};
use std::collections::{BTreeSet, HashSet};

pub const MAX_PROMOTED: usize = 1000;

/// Byte spans of the top-level handler objects (`    {` ... `    }`).
fn entry_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n');
        match (start, body) {
            (None, Some("    {")) => start = Some(offset),
            (Some(begin), Some("    }" | "    },")) => {
                spans.push((begin, offset + 5));
                start = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    spans
}

/// Byte span of one JSON string token starting at `from`.
fn string_token(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(from) != Some(&b'"') {
        return None;
    }
    let mut index = from + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            b'\n' => return None,
            _ => index += 1,
        }
    }
    None
}

/// Spans of every `      "pattern": "..."` value in one handler block.
fn pattern_spans(block: &str) -> Vec<(usize, usize)> {
    const PREFIX: &str = "      \"pattern\": ";
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in block.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.starts_with(PREFIX) {
            let start = offset + PREFIX.len();
            if let Some(end) = string_token(block, start)
                && matches!(&block[end..offset + body.len()], "" | ",") {
                    spans.push((start, end));
                }
        }
        offset += line.len();
    }
    spans
}

fn handlers_of(value: &mut Json) -> Option<&mut Vec<Json>> {
    match value {
        Json::Obj(map) => match map.get_mut("handlers") {
            Some(Json::Arr(items)) => Some(items),
            _ => None,
        },
        _ => None,
    }
}

fn parse(text: &str) -> Result<Json> {
    Json::parse(text).map_err(|error| format!("invalid generated handler catalogue: {error}"))
}

/// Modify only one escaped JSON pattern value; never reformat the catalogue.
pub fn update_handler_bytes(text: &str, index: usize, old: &str, new: &str) -> Result<String> {
    let original = parse(text)?;
    let handlers = original.field("handlers").as_arr().cloned().unwrap_or_default();
    if index >= handlers.len() || !handlers[index].field("pattern").str_eq(old) {
        return Err("upstream handler diverged".into());
    }
    let spans = entry_spans(text);
    if spans.len() != handlers.len() {
        return Err("unexpected generated catalogue formatting".into());
    }
    let (begin, end) = spans[index];
    let block = &text[begin..end];
    let matches = pattern_spans(block);
    let isolated = matches.len() == 1 && Json::parse(&block[matches[0].0..matches[0].1]).ok().is_some_and(|value| value.str_eq(old));
    if !isolated {
        return Err("cannot isolate original source pattern".into());
    }
    let (start, stop) = matches[0];
    let updated = format!("{}{}{}", &block[..start], dumps(&Json::from(new), Dump::DEFAULT.utf8()), &block[stop..]);
    let merged = format!("{}{}{}", &text[..begin], updated, &text[end..]);
    let mut expected = original.clone();
    if let Some(Json::Obj(row)) = handlers_of(&mut expected).and_then(|rows| rows.get_mut(index)) {
        row.insert("pattern".into(), Json::from(new));
    }
    if parse(&merged)? != expected {
        return Err("source modification was not limited to one regex".into());
    }
    Ok(merged)
}

fn render(handler: &Json) -> String {
    format!("    {}", dumps(handler, Dump::PRETTY_UTF8).replace('\n', "\n    "))
}

/// Insert byte-local trusted native handler copies, like Rust replay.
///
/// Additions are appended with their original transform/options/flags.
/// Corrective TV handlers are inserted ahead of that field as the Rust
/// replay engine does, never with a made-up transform.
pub fn insert_native_handlers(source: &str, plans: &[Json]) -> Result<String> {
    const ENDING: &str = "\n  ]\n}\n";
    let original = parse(source)?;
    let handlers = original.field("handlers").as_arr().cloned().unwrap_or_default();
    let mut expected = original.clone();
    let mut source = source.to_owned();
    for change in plans {
        let action = change.field("operation").as_str().unwrap_or_default();
        let index = change.field("handler_index");
        if action == "patch" {
            let index = index.as_int().and_then(|value| usize::try_from(value).ok()).ok_or("invalid native template")?;
            let old = change.field("original_pattern").as_str().unwrap_or_default();
            let new = change.field("pattern").as_str().unwrap_or_default();
            source = update_handler_bytes(&source, index, old, new)?;
            if let Some(Json::Obj(row)) = handlers_of(&mut expected).and_then(|rows| rows.get_mut(index)) {
                row.insert("pattern".into(), Json::from(new));
            }
            continue;
        }
        if !matches!(action, "add" | "correct") {
            return Err("unsupported community native source edit".into());
        }
        let Some(template) = index.as_int().and_then(|value| usize::try_from(value).ok()).and_then(|i| handlers.get(i)) else {
            return Err("invalid native template".into());
        };
        if !template.field("kind").str_eq("regex")
            || template.field("name") != change.field("field")
            || template.field("pattern") != change.field("original_pattern")
            || template.field("transform") != change.field("transform")
        {
            return Err("source handler changed since audit".into());
        }
        let mut copied = template.clone();
        let name = template.field("name").as_str().unwrap_or_default().to_owned();
        if let Json::Obj(map) = &mut copied {
            map.insert("pattern".into(), change.field("pattern").clone());
            if matches!(name.as_str(), "seasons" | "episodes") {
                map.insert("learned_tv_guard".into(), Json::Bool(true));
                if action == "correct" {
                    // Range rules do not use the learned single-coordinate guard.
                    let guard = !map.get("transform").is_some_and(|transform| transform.str_eq("range_func"));
                    map.insert("learned_tv_guard".into(), Json::Bool(guard));
                }
            }
        }
        let rows = handlers_of(&mut expected).ok_or("invalid native template")?;
        if action == "add" {
            let Some(head) = source.strip_suffix(ENDING) else {
                return Err("unexpected canonical handler JSON ending".into());
            };
            source = format!("{head},\n{}{ENDING}", render(&copied));
            rows.push(copied);
        } else {
            let spans = entry_spans(&source);
            if spans.len() != rows.len() {
                return Err("nonstandard native handler source format".into());
            }
            let Some(first) = rows.iter().position(|row| row.field("name").str_eq(&name)) else {
                return Err("no original native handler for correction".into());
            };
            let at = spans[first].0;
            source = format!("{}{},\n{}", &source[..at], render(&copied), &source[at..]);
            rows.insert(first, copied);
        }
        if parse(&source)? != expected {
            return Err("native source diff exceeded planned changes".into());
        }
    }
    Ok(source)
}

fn key(value: &Json) -> String {
    dumps(value, Dump::DEFAULT)
}

fn verified_fixtures(audit: &Json, fixtures: &[Json], missing: &str) -> Result<Vec<Json>> {
    let indexes = audit.field("verified_fixture_indexes").as_arr();
    let valid = indexes.is_some_and(|items| {
        items.len() >= 2
            && items.iter().all(|item| item.as_int().is_some_and(|i| 0 <= i && (i as usize) < fixtures.len()))
            && items.iter().map(key).collect::<HashSet<_>>().len() >= 2
    });
    if !valid {
        return Err(missing.into());
    }
    let unique: BTreeSet<usize> = indexes.into_iter().flatten().filter_map(Json::as_int).map(|i| i as usize).collect();
    Ok(unique.into_iter().map(|index| fixtures[index].clone()).collect())
}

/// Returns (handlers source, promoted fixtures, summary).
pub fn apply(
    before: &Json,
    after: &Json,
    audits: &Json,
    fixtures: &Json,
    source: &str,
    promoted: &Json,
    catalog: &Catalog,
) -> Result<(String, Json, Option<Json>)> {
    let (Some(before), Some(after), Some(audits), Some(fixtures), Some(promoted_rows)) =
        (before.as_arr(), after.as_arr(), audits.as_arr(), fixtures.as_arr(), promoted.as_arr())
    else {
        return Err("invalid automation JSON array".into());
    };
    let old: HashSet<String> =
        before.iter().filter(|row| row.as_obj().is_some()).map(|row| key(row.field("fingerprint"))).collect();
    let mut by_fp = std::collections::HashMap::new();
    for audit in audits.iter().filter(|row| row.as_obj().is_some()) {
        by_fp.insert(key(audit.field("fingerprint")), audit);
    }
    let parsed = parse(source)?;
    let handlers = parsed.field("handlers").as_arr().cloned().unwrap_or_default();
    let history: HashSet<String> = promoted_rows.iter().map(|row| key(row.field("title"))).collect();
    let empty = Json::Obj(Default::default());
    let fresh = |proven: &[Json]| -> Vec<Json> {
        let mut rows = promoted_rows.clone();
        rows.extend(proven.iter().filter(|row| !history.contains(&key(row.field("title")))).cloned());
        rows
    };
    for entry in after.iter().filter(|row| row.as_obj().is_some() && !old.contains(&key(row.field("fingerprint")))) {
        if !entry.field("status").str_eq("review_required") || !entry.field("automatic_activation").is_false() {
            continue;
        }
        let rule = entry.field("proposal");
        if rule.as_obj().is_none() {
            continue;
        }
        let fp = entry.field("fingerprint");
        let audit = by_fp.get(&key(fp)).copied().unwrap_or(&empty);
        let improvements = audit.field("verified_improvements").as_number().unwrap_or(0.0);
        if !(audit.field("status").str_eq("ready_for_source_diff")
            && audit.field("promotion_eligible").is_true()
            && audit.get("regressions") == Some(&Json::Int(0))
            && improvements >= 2.0)
        {
            continue;
        }
        if fp != rule.field("fingerprint") || !fp.as_str().is_some_and(is_hex64) {
            return Err("review/audit fingerprint mismatch".into());
        }
        if let Some(verified) = audit.get("source_changes").filter(|value| !value.is_null()) {
            if !candidate_valid(rule, catalog) {
                return Err("unauthenticated community candidate for source edits".into());
            }
            if audit.field("distinct_verified_titles").as_number().unwrap_or(0.0) < 2.0 {
                return Err("native regex lacks distinct independently verified titles".into());
            }
            let (plans, reason) = resolve(rule, &handlers)?;
            if plans.is_empty() || reason != "replay_ready" || &Json::Arr(plans.clone()) != verified {
                return Err("untrusted or stale native handler audit plan".into());
            }
            let target: BTreeSet<String> = plans.iter().map(|plan| plan.field("field").as_str().unwrap_or_default().to_owned()).collect();
            let proven_fields: HashSet<&str> =
                audit.field("verified_fields").as_arr().into_iter().flatten().filter_map(Json::as_str).collect();
            if !target.iter().all(|field| proven_fields.contains(field.as_str())) {
                return Err("not all modified native fields have independent proof".into());
            }
            let proven = verified_fixtures(audit, fixtures, "trusted fixture indexes missing")?;
            if promoted_rows.len() + proven.len() > MAX_PROMOTED {
                return Err("verified native regression fixture capacity exceeded".into());
            }
            let labels_target = |row: &Json| {
                row.field("expected").as_obj().is_some_and(|expected| target.iter().any(|field| expected.contains_key(field)))
            };
            if !proven.iter().all(labels_target) {
                return Err("trusted fixtures do not label modified fields".into());
            }
            let merged = insert_native_handlers(source, &plans)?;
            let tests = fresh(&proven);
            if tests.len() == promoted_rows.len() {
                return Err("no new independently verified regression fixtures".into());
            }
            let stored = tests.len() - promoted_rows.len();
            let summary = obj! {
                "fingerprint" => fp.clone(),
                "fields" => Json::from_strs(target.iter().cloned()),
                "operations" => Json::Arr(plans.iter().map(|plan| plan.field("operation").clone()).collect()),
                "source_handler_indexes" => Json::Arr(plans.iter().map(|plan| plan.field("handler_index").clone()).collect()),
                "verified_improvements" => audit.field("verified_improvements").clone(),
                "stored_regressions" => stored,
            };
            return Ok((merged, Json::Arr(tests), Some(summary)));
        }
        if !rule.field("kind").str_eq("learned_source_regex_patch") {
            continue;
        }
        let Some(index) = rule.field("handler_index").as_int().and_then(|i| usize::try_from(i).ok()).filter(|i| *i < handlers.len())
        else {
            return Err("invalid source handler index".into());
        };
        let row = &handlers[index];
        let field = rule.field("field").as_str().unwrap_or_default();
        let extra = technical_pattern(rule.field("field"), rule.field("signal"));
        let original = rule.field("original_regex").as_str().unwrap_or_default();
        let replacement = rule.field("proposed_regex").as_str().unwrap_or_default();
        let trusted = extra.as_ref().is_some_and(|extra| {
            row.field("kind").str_eq("regex")
                && matches!(field, "codec" | "resolution" | "bit_depth")
                && row.field("name").str_eq(field)
                && row.field("pattern").str_eq(original)
                && row.field("transform") == rule.field("transform")
                && replacement == format!("(?:{original})|(?:{extra})")
                && replacement.chars().count() <= 512
        });
        if !trusted || rule.field("original_regex").as_str().is_none() || rule.field("proposed_regex").as_str().is_none() {
            return Err("untrusted or stale source regex replacement".into());
        }
        let proven = verified_fixtures(audit, fixtures, "trusted fixture proof missing")?;
        if promoted_rows.len() + proven.len() > MAX_PROMOTED {
            return Err("approved community regression corpus is full".into());
        }
        // Ensure that the fixture labels actually cover the target field.
        if !proven.iter().all(|row| row.field("expected").as_obj().is_some_and(|expected| expected.contains_key(field))) {
            return Err("review proof does not label the changed field".into());
        }
        let merged = update_handler_bytes(source, index, original, replacement)?;
        let tests = fresh(&proven);
        if tests.len() == promoted_rows.len() {
            return Err("positive training proof already recorded".into());
        }
        let stored = tests.len() - promoted_rows.len();
        let summary = obj! {
            "fingerprint" => fp.clone(),
            "field" => field,
            "handler_index" => index,
            "verified_improvements" => audit.field("verified_improvements").clone(),
            "stored_regressions" => stored,
        };
        return Ok((merged, Json::Arr(tests), Some(summary)));
    }
    Ok((source.to_owned(), promoted.clone(), None))
}
