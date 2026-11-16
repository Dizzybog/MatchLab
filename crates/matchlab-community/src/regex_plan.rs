//! Resolve validated community suggestions into bounded native Rust replay plans.
//!
//! A rule never supplies executable code or chooses a conversion/handler option.
//! Every planned addition copies an exact handler in the immutable catalogue.
//! Ambiguous mappings remain review-only, never guessed.
use crate::json::{Json, Object};
use crate::obj;
use crate::rule_intake::{
    anchored_general, canonical, static_transform, technical_pattern, tv_native_pattern, valid_extended_rule,
    valid_rule, Catalog, SXE_RANGE_PATTERN,
};
use crate::util::{is_hex64, py_escape, Result};
use std::collections::BTreeSet;

pub const SUPPORTED: &[&str] = &[
    "learned_field_regex",
    "learned_tv_regex",
    "learned_source_regex_patch",
    "learned_corrective_regex",
    "learned_general_regex_review",
    "learned_field_regex_review",
    "historical_regex_review",
    "verified_regex_proposal",
];
pub const TV_FIELDS: [&str; 2] = ["seasons", "episodes"];

fn native_entry<'a>(handlers: &'a [Json], index: &Json, field: Option<&str>) -> Option<&'a Json> {
    let index = usize::try_from(index.as_int()?).ok()?;
    let row = handlers.get(index)?;
    if !row.field("kind").str_eq("regex") || row.field("pattern").as_str().is_none() {
        return None;
    }
    if let Some(field) = field
        && !row.field("name").str_eq(field) {
            return None;
        }
    Some(row)
}

fn native_at<'a>(handlers: &'a [Json], index: usize, field: &str) -> Option<&'a Json> {
    native_entry(handlers, &Json::from(index), Some(field))
}

pub fn operation(handlers: &[Json], index: &Json, new_pattern: &str, mode: &str) -> Result<Json> {
    let row = native_entry(handlers, index, None);
    let Some(row) = row.filter(|_| (4..=512).contains(&new_pattern.chars().count())) else {
        return Err("Invalid native regex source or replacement".into());
    };
    if !matches!(mode, "add" | "patch" | "correct") {
        return Err("Invalid native regex operation".into());
    }
    Ok(obj! {
        "operation" => mode,
        "handler_index" => index.clone(),
        "field" => row.field("name").clone(),
        "original_pattern" => row.field("pattern").clone(),
        "pattern" => new_pattern,
        "transform" => row.field("transform").clone(),
    })
}

pub fn field_template(handlers: &[Json], field: &str, canonical_value: &Json) -> Option<usize> {
    let accepted: Vec<String> = match canonical_value {
        Json::Str(value) => vec![format!("value('{value}')"), format!("uniq_concat(value('{value}'))")],
        Json::Arr(items) if items.len() == 1 && items[0].as_str().is_some() => {
            vec![format!("uniq_concat(value('{}'))", items[0].as_str().unwrap_or_default())]
        }
        Json::Bool(true) => vec!["boolean".into()],
        _ => return None,
    };
    let wanted = |row: &Json| -> bool {
        if field == "extension" && canonical_value.as_str().is_some() {
            row.field("transform").str_eq("lowercase")
        } else {
            row.field("transform").as_str().is_some_and(|transform| accepted.iter().any(|item| item == transform))
        }
    };
    // Deliberately select the first trusted handler with the EXACT desired
    // output transform. Different options are still replayed and checked.
    (0..handlers.len()).find(|index| native_at(handlers, *index, field).is_some_and(wanted))
}

pub fn tv_template(handlers: &[Json], field: &str, is_range: bool) -> Option<usize> {
    let transform = if is_range { "range_func" } else { "array(integer)" };
    (0..handlers.len()).find(|index| {
        native_at(handlers, *index, field).is_some_and(|row| {
            let options = row.field("options");
            row.field("transform").str_eq(transform)
                && options.field("remove").is_false()
                && options.field("skipIfAlreadyFound").is_true()
        })
    })
}

fn ready(changes: Vec<Json>) -> Result<(Vec<Json>, &'static str)> {
    Ok((changes, "replay_ready"))
}

/// Return (native_operations, reason); empty operations never stage a PR.
pub fn resolve(rule: &Json, handlers: &[Json]) -> Result<(Vec<Json>, &'static str)> {
    let none = |reason| Ok((Vec::new(), reason));
    let Some(kind) = rule.field("kind").as_str().filter(|_| rule.as_obj().is_some()) else {
        return none("not_a_valid_proposal");
    };
    if !SUPPORTED.contains(&kind) {
        return none("unknown_proposal_family");
    }
    let text = |key: &str| rule.field(key).as_str();
    match kind {
        "historical_regex_review" => none("historical_source_requires_current_grammar_and_new_proof"),
        "learned_source_regex_patch" => {
            let field = text("field").unwrap_or_default();
            if !matches!(field, "codec" | "resolution" | "bit_depth") {
                return none("unsafe_source_patch_field");
            }
            let index = rule.field("handler_index");
            let row = native_entry(handlers, index, Some(field));
            let extra = technical_pattern(rule.field("field"), rule.field("signal"));
            let (Some(row), Some(extra)) = (row, extra) else { return none("stale_or_invalid_source_handler") };
            if row.field("pattern") != rule.field("original_regex") || row.field("transform") != rule.field("transform") {
                return none("stale_or_invalid_source_handler");
            }
            let proposed = format!("(?:{})|(?:{extra})", row.field("pattern").as_str().unwrap_or_default());
            if !rule.field("proposed_regex").str_eq(&proposed) || proposed.chars().count() > 512 {
                return none("invalid_native_source_replacement");
            }
            ready(vec![operation(handlers, index, &proposed, "patch")?])
        }
        "learned_field_regex" => {
            let Some(mut pattern) = technical_pattern(rule.field("field"), rule.field("signal")) else {
                return none("unsupported_field_regex_grammar");
            };
            if !rule.field("regex").str_eq(&pattern) {
                return none("unsupported_field_regex_grammar");
            }
            let field = text("field").unwrap_or_default();
            let Some(index) = field_template(handlers, field, rule.field("canonical")) else {
                return none("no_unambiguous_matching_native_transform");
            };
            // Extension regexes must operate on actual filename suffixes, not
            // on arbitrary title words that happen to equal 'mp4' or 'mkv'.
            if field == "extension" {
                let signal = text("signal").unwrap_or_default();
                if !(2..=8).contains(&signal.len()) || !signal.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
                    return none("invalid_filename_extension_marker");
                }
                pattern = format!(r"(?i)\.{}$", py_escape(signal));
            }
            ready(vec![operation(handlers, &Json::from(index), &pattern, "add")?])
        }
        "learned_tv_regex" => {
            let family = text("family").unwrap_or_default();
            let Some(patterns) = crate::rule_intake::tv_native_patterns(family) else {
                return none("invalid_tv_family");
            };
            if rule.field("native_patterns") != &patterns {
                return none("invalid_tv_family");
            }
            let mut changes = Vec::new();
            for field in TV_FIELDS {
                let Some(index) = tv_template(handlers, field, false) else {
                    return none("missing_safe_native_tv_handler");
                };
                let pattern = tv_native_pattern(family, field).unwrap_or_default();
                changes.push(operation(handlers, &Json::from(index), pattern, "add")?);
            }
            ready(changes)
        }
        "learned_corrective_regex" => {
            let index = rule.field("handler_index");
            let field = text("field").unwrap_or_default();
            let family = text("family").unwrap_or_default();
            let source = native_entry(handlers, index, Some(field));
            let Some(source) = source.filter(|_| TV_FIELDS.contains(&field)) else {
                return none("stale_corrective_handler_index");
            };
            let options = source.field("options");
            if !options.field("remove").is_false() || !options.field("skipIfAlreadyFound").is_true() {
                return none("unsafe_corrective_handler_options");
            }
            let (pattern, valid) = if family == "sxe_range" {
                (Some(SXE_RANGE_PATTERN), field == "episodes" && source.field("transform").str_eq("range_func"))
            } else {
                (tv_native_pattern(family, field), source.field("transform").str_eq("array(integer)"))
            };
            match pattern {
                Some(pattern) if valid && rule.field("regex").str_eq(pattern) => {
                    ready(vec![operation(handlers, index, pattern, "correct")?])
                }
                _ => none("invalid_corrective_coordinate_grammar"),
            }
        }
        "learned_general_regex_review" => {
            let index = rule.field("handler_index");
            let Some(row) = native_entry(handlers, index, text("field")) else {
                return none("stale_general_handler_index");
            };
            let pattern = text("regex").unwrap_or_default();
            if !rule.field("regex").as_str().is_some_and(anchored_general) {
                return none("invalid_separator_grammar");
            }
            if row.field("pattern").str_eq(pattern) {
                return none("already_supported");
            }
            match text("mode") {
                Some("add") => ready(vec![operation(handlers, index, pattern, "add")?]),
                Some("correct") => {
                    let transform = row.field("transform").as_str().unwrap_or_default();
                    if !static_transform(transform) {
                        return none("corrective_rule_has_dynamic_transform");
                    }
                    let merged = format!("(?:{})|(?:{pattern})", row.field("pattern").as_str().unwrap_or_default());
                    if merged.chars().count() > 512 {
                        return none("replacement_exceeds_safe_limit");
                    }
                    ready(vec![operation(handlers, index, &merged, "patch")?])
                }
                _ => none("unknown_general_mode"),
            }
        }
        "learned_field_regex_review" => {
            let field = text("field");
            let signal = text("signal").unwrap_or_default();
            if !(3..=16).contains(&signal.len()) || !signal.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
                return none("invalid_field_discovery_marker");
            }
            let indices: Vec<usize> = (0..handlers.len())
                .filter(|index| {
                    native_entry(handlers, &Json::from(*index), field).is_some()
                        && !matches!(handlers[*index].field("transform").as_str(), Some("none" | ""))
                })
                .collect();
            let transforms: BTreeSet<String> =
                indices.iter().map(|index| crate::json::dumps(handlers[*index].field("transform"), crate::json::Dump::DEFAULT)).collect();
            if transforms.len() != 1 {
                return none("ambiguous_native_transform_requires_explicit_source_identity");
            }
            let expected = format!(r"(?i)(?:^|[ ._-]){signal}(?:$|[ ._-])");
            if !rule.field("regex").str_eq(&expected) {
                return none("invalid_field_discovery_grammar");
            }
            ready(vec![operation(handlers, &Json::from(indices[0]), &expected, "add")?])
        }
        "verified_regex_proposal" => {
            let Some(pattern) = canonical(rule.field("marker")) else {
                return none("invalid_verified_episode_marker");
            };
            if !rule.field("regex").str_eq(&pattern) {
                return none("invalid_verified_episode_marker");
            }
            let Some(index) = tv_template(handlers, "episodes", false) else {
                return none("missing_native_episode_handler");
            };
            ready(vec![operation(handlers, &Json::from(index), &pattern, "add")?])
        }
        _ => none("not_replayable"),
    }
}

pub fn candidate_valid(rule: &Json, catalog: &Catalog) -> bool {
    let Some(source) = rule.field("source_build").as_str().filter(|_| rule.as_obj().is_some()) else {
        return false;
    };
    let Some((sha, _)) = source.split_once(':') else { return false };
    // The intake independently verifies ancestry against current main.
    // Here revalidate syntax, schema, and fingerprint against source build.
    if rule.field("kind").str_eq("verified_regex_proposal") {
        valid_rule(rule, sha, &crate::util::git_ancestor)
    } else {
        valid_extended_rule(rule, sha, &|_, _| true, catalog)
    }
}

pub fn build(candidates: &Json, handlers: &[Json], catalog: &Catalog) -> Result<Json> {
    let Some(candidates) = candidates.as_arr().filter(|items| items.len() <= 24) else {
        return Err("Invalid review candidate batch".into());
    };
    let mut plans = Object::new();
    for rule in candidates {
        let Some(fingerprint) = rule.field("fingerprint").as_str().filter(|fp| is_hex64(fp)) else {
            return Err("Invalid review candidate fingerprint".into());
        };
        if plans.contains_key(fingerprint) {
            return Err("Duplicate rule fingerprint in review batch".into());
        }
        let (changes, reason) = if candidate_valid(rule, catalog) {
            resolve(rule, handlers)?
        } else {
            // Old queued proposals can legitimately become stale as main
            // changes. One invalid item must not abort review of valid rules.
            (Vec::new(), "stale_or_invalid_queued_proposal")
        };
        plans.insert(fingerprint.to_owned(), obj! {"source_changes" => changes, "reason" => reason});
    }
    Ok(Json::Obj(plans))
}
