//! Build bounded, untrusted torrent-title *test inputs* for the Rust reviewer.
//!
//! A shared Issue batch has reports and rules. Only associate explicit matching
//! release markers from the SAME authenticated-format batch; never mistake a
//! self-reported IMDb mapping or parsed value for an independent ground-truth
//! label. This file is runner-local and is not attached to public PRs/artifacts.
use crate::json::{Json, Object};
use crate::learning::allowed_title;
use crate::rule_intake::{GENERAL_PREFIX, GENERAL_SUFFIX};
use crate::util::{aware_timestamp, py_escape};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;

pub const MAX_PER_RULE: usize = 12;
pub const WINDOW_DAYS: i64 = 60;

/// Never compiles untrusted regex syntax: only finite trusted grammars.
pub fn marker_regex(rule: &Json) -> Option<Regex> {
    let pattern = match rule.field("kind").as_str()? {
        "learned_field_regex" | "learned_source_regex_patch" | "learned_field_regex_review" => {
            let signal = rule.field("signal").as_str()?;
            let valid = (2..=24).contains(&signal.len())
                && signal.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
            if !valid {
                return None;
            }
            let parts: Vec<String> = signal.split(['_', '.']).filter(|part| !part.is_empty()).map(py_escape).collect();
            if parts.is_empty() {
                return None;
            }
            format!(r"(?i)(?:^|[ ._-])(?:{})(?:$|[ ._-])", parts.join("[ ._-]*"))
        }
        "learned_general_regex_review" => {
            // This is a strict finite grammar. Do not compile untrusted regex
            // syntax even if the upstream validator is accidentally relaxed.
            let value = rule.field("regex").as_str()?;
            let body = value.strip_prefix(GENERAL_PREFIX)?.strip_suffix(GENERAL_SUFFIX)?;
            let parts: Vec<&str> = body.split("[ ._-]+").collect();
            let valid = (2..=6).contains(&parts.len())
                && parts.iter().all(|part| (1..=24).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric()));
            if !valid {
                return None;
            }
            format!(
                r"(?i)(?:^|[ ._-])(?:{})(?:$|[ ._-])",
                parts.iter().map(|part| py_escape(part)).collect::<Vec<_>>().join("[ ._-]+")
            )
        }
        "learned_tv_regex" | "learned_corrective_regex" => match rule.field("family").as_str()? {
            "sxe" => r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e[0-9]{1,5}".into(),
            "x" => r"(?i)(?:^|[._ -])[0-9]{1,3}x[0-9]{1,5}".into(),
            "words" => r"(?i)season[._ -]+[0-9]{1,3}[._ -]+episode[._ -]+[0-9]{1,5}".into(),
            "sxe_range" => r"(?i)s[0-9]{1,3}[._ -]*e[0-9]{1,5}[._ -]*-".into(),
            _ => return None,
        },
        _ => return None,
    };
    Regex::new(&pattern).ok()
}

pub type Validator<'a> = &'a dyn Fn(&Json, &str) -> Option<Vec<Json>>;

pub fn gather(
    comments: &Json,
    candidates: &Json,
    sha: &str,
    now: DateTime<Utc>,
    validate_rules: Validator,
    validate_reports: Validator,
) -> Json {
    let mut wanted = Object::new();
    for row in candidates.as_arr().into_iter().flatten() {
        if let Some(fingerprint) = row.field("fingerprint").as_str().filter(|_| row.as_obj().is_some()) {
            wanted.insert(fingerprint.to_owned(), row.clone());
        }
    }
    let mut output: indexmap::IndexMap<String, Vec<String>> =
        wanted.keys().map(|fingerprint| (fingerprint.clone(), Vec::new())).collect();
    if wanted.is_empty() {
        return Json::Obj(Object::new());
    }
    let cutoff = now - Duration::days(WINDOW_DAYS);
    for group in comments.as_arr().into_iter().flatten() {
        let comments: Vec<&Json> = match group {
            Json::Arr(items) => items.iter().collect(),
            other => vec![other],
        };
        for comment in comments {
            if comment.as_obj().is_none() {
                continue;
            }
            let Some(created) = aware_timestamp(comment.field("created_at")) else { continue };
            if !(cutoff <= created && created <= now + Duration::minutes(5)) {
                continue;
            }
            let Some(rules) = validate_rules(comment, sha).filter(|rules| !rules.is_empty()) else { continue };
            let matching: Vec<&Json> =
                rules.iter().filter(|rule| rule.field("fingerprint").as_str().is_some_and(|fp| wanted.contains_key(fp))).collect();
            if matching.is_empty() {
                continue;
            }
            let Some(reports) = validate_reports(comment, sha).filter(|reports| !reports.is_empty()) else { continue };
            for rule in matching {
                let fingerprint = rule.field("fingerprint").as_str().unwrap_or_default();
                let Some(matcher) = marker_regex(rule) else { continue };
                let titles = output.entry(fingerprint.to_owned()).or_default();
                for report in &reports {
                    let title = report.field("title");
                    let Some(text) = title.as_str() else { continue };
                    if allowed_title(title)
                        && text.len() <= 200
                        && matcher.is_match(text)
                        && !titles.iter().any(|seen| seen == text)
                        && titles.len() < MAX_PER_RULE
                    {
                        titles.push(text.to_owned());
                    }
                }
            }
        }
    }
    Json::Obj(output.into_iter().map(|(fingerprint, titles)| (fingerprint, Json::from_strs(titles))).collect())
}
