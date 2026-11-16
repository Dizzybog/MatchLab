//! Untrusted community batch intake; current-parser replay; shadow-only ML.
//!
//! Following active-learning principles, a suspected anomaly is NOT a label.
//! Older builds may contribute hypotheses if their SHA is an ancestor of main.
//! Evidence is independently re-evaluated with the current parser before review.
use crate::json::{Json, Object};
use crate::obj;
use crate::util::{aware_timestamp, is_hex64, json_valid_sha, sha256_hex, valid_sha, Ancestor, Result};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

pub const MARKER: &str = "<!-- matchlab-community-batch-v2 -->";
pub const MARKER_V3: &str = "<!-- matchlab-community-batch-v3 -->";
pub const PROTOCOL: i128 = 2;
pub const EPOCH: i128 = 1;
pub const MAX_COMMENTS: usize = 5000;
pub const MAX_CANDIDATES: usize = 200;
pub const MAX_LEDGER: usize = 100_000;
pub const MAX_BATCH: usize = 16;
pub const MAX_BODY: usize = 16384;

pub const CODES: &[&str] = &[
    "matching_error",
    "invalid_selected_coordinate",
    "unusual_season_number",
    "unmatched_explicit_episode",
    "explicit_episode_mismatch",
    "parser_marker_disagreement",
    "unresolved_absolute_anime_episode",
    "incomplete_anime_episode_range",
    "reversed_selected_range",
    "field_missing_title",
    "field_missing_resolution",
    "field_missing_codec",
    "field_missing_quality",
    "field_missing_audio",
    "field_missing_bit_depth",
    "field_missing_hdr",
    "field_missing_channels",
    "field_missing_languages",
    "field_missing_subbed",
    "field_missing_dubbed",
    "field_missing_seasons",
    "field_missing_episodes",
    "field_missing_extension",
    "field_missing_year",
    "field_missing_anime_episode",
];

pub fn is_code(value: &str) -> bool {
    CODES.contains(&value)
}

/// Sorted Bernoulli feature vocabulary.
pub static FEATURES: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut features: BTreeSet<String> = [
        "status:matched",
        "status:unmatched",
        "status:error",
        "has_episode",
        "has_season",
        "selected_episode",
        "selected_season",
    ]
    .iter()
    .map(|feature| feature.to_string())
    .collect();
    features.extend(CODES.iter().map(|code| format!("alert:{code}")));
    features.into_iter().collect()
});

pub fn allowed_title(title: &Json) -> bool {
    let Some(title) = title.as_str() else { return false };
    if !(1..=240).contains(&title.len()) {
        return false;
    }
    if title.chars().any(|ch| "/\\@\r\n\0".contains(ch) || (ch as u32) < 32) {
        return false;
    }
    let text = title.to_lowercase();
    ![
        "http:", "https:", "www.", "magnet:", "token=", "apikey=", "password=", "secret=", "bearer ", "private=",
        "api_key=",
    ]
    .iter()
    .any(|keyword| text.contains(keyword))
}

/// Matches the server producer: `char::is_alphanumeric` words, single spaces.
pub fn normalized(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn fingerprint(title: &str, alerts: &[&str]) -> String {
    let unique: BTreeSet<&str> = alerts.iter().copied().collect();
    sha256_hex(format!("{}\n{}", normalized(title), unique.into_iter().collect::<Vec<_>>().join(",")))
}

const REPORT_KEYS: &[&str] = &[
    "protocol",
    "learning_epoch",
    "build_sha",
    "source",
    "fingerprint",
    "title",
    "status",
    "alerts",
    "parsed",
    "selected",
    "verified",
];

fn bounded_number(value: &Json) -> bool {
    value.as_int().is_some_and(|number| (0..=99999).contains(&number))
}

static IMDB_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\Att[0-9]{7,8}\z").unwrap());

pub fn imdb_id(value: &str) -> bool {
    IMDB_ID.is_match(value)
}

pub fn validate_report(row: &Json, sha: &str, is_ancestor: Ancestor) -> bool {
    if !row.has_exact_keys(REPORT_KEYS) {
        return false;
    }
    let protocol = row.field("protocol");
    let build = row.field("build_sha").as_str().unwrap_or_default();
    if !(protocol == &Json::Int(PROTOCOL) || protocol == &Json::Int(3))
        || row.field("learning_epoch") != &Json::Int(EPOCH)
        || !valid_sha(build)
        || !(build == sha || is_ancestor(build, sha))
        || !row.field("source").str_eq("matchlab-community")
        || !row.field("verified").is_false()
        || !allowed_title(row.field("title"))
        || !matches!(row.field("status").as_str(), Some("matched" | "unmatched" | "error"))
    {
        return false;
    }
    let Some(codes) = row.field("alerts").as_arr() else { return false };
    if !(1..=8).contains(&codes.len()) || codes.iter().any(|code| !code.as_str().is_some_and(is_code)) {
        return false;
    }
    let parsed = row.field("parsed");
    let selected = row.field("selected");
    if !parsed.has_exact_keys(&["title", "seasons", "episodes"])
        || !selected.has_exact_keys(&["imdb_id", "season", "episode"])
    {
        return false;
    }
    if !parsed.field("title").is_null() && !allowed_title(parsed.field("title")) {
        return false;
    }
    for key in ["seasons", "episodes"] {
        let Some(numbers) = parsed.field(key).as_arr() else { return false };
        if numbers.len() > 12 || !numbers.iter().all(bounded_number) {
            return false;
        }
    }
    let imdb = selected.field("imdb_id");
    if !imdb.is_null() && !imdb.as_str().is_some_and(imdb_id) {
        return false;
    }
    for key in ["season", "episode"] {
        let number = selected.field(key);
        if !number.is_null() && !bounded_number(number) {
            return false;
        }
    }
    let alerts: Vec<&str> = codes.iter().filter_map(Json::as_str).collect();
    let Some(title) = row.field("title").as_str() else { return false };
    row.field("fingerprint").as_str() == Some(fingerprint(title, &alerts).as_str())
}

fn body_and_marker(comment: &Json) -> Option<(&str, &'static str)> {
    let body = comment.get("body")?.as_str()?;
    if body.len() > MAX_BODY {
        return None;
    }
    let marker = [MARKER, MARKER_V3].into_iter().find(|marker| {
        body.strip_prefix(marker).is_some_and(|rest| rest.starts_with('\n'))
    })?;
    Some((&body[marker.len() + 1..], marker))
}

/// Validate a v2/v3 batch comment and return its valid reports.
pub fn validate_comment(comment: &Json, current_sha: &str, is_ancestor: Ancestor) -> Option<Vec<Json>> {
    let (payload, marker) = body_and_marker(comment)?;
    let protocol: i128 = if marker == MARKER_V3 { 3 } else { 2 };
    let batch = Json::parse(payload).ok()?;
    let mut fields = vec!["protocol", "learning_epoch", "build_sha", "source", "batch_id", "created_unix", "verified", "reports"];
    if protocol == 3 {
        fields.push("rules");
    }
    if !batch.has_exact_keys(&fields) {
        return None;
    }
    let sha = batch.field("build_sha").as_str().unwrap_or_default();
    let empty = Vec::new();
    let rules = match batch.get("rules") {
        None => Some(&empty),
        Some(value) => value.as_arr(),
    };
    let reports = batch.field("reports").as_arr();
    if batch.field("protocol") != &Json::Int(protocol)
        || batch.field("learning_epoch") != &Json::Int(EPOCH)
        || !batch.field("source").str_eq("matchlab-community-batch")
        || !batch.field("verified").is_false()
        || !valid_sha(sha)
        || !is_ancestor(sha, current_sha)
        || batch.field("created_unix").as_int().is_none()
    {
        return None;
    }
    let (Some(raw_reports), Some(rules)) = (reports, rules) else { return None };
    if !(1..=MAX_BATCH).contains(&(raw_reports.len() + rules.len())) {
        return None;
    }
    // Authenticate the ORIGINAL full list before filtering. Dropping a
    // malformed child from the fingerprint computation would accept a
    // modified batch ID and can hide tampering.
    let mut fingerprints = Vec::new();
    for entry in raw_reports.iter().chain(rules) {
        let fingerprint = entry.as_obj()?.get("fingerprint")?.as_str()?;
        if !is_hex64(fingerprint) {
            return None;
        }
        fingerprints.push(fingerprint);
    }
    if fingerprints.iter().collect::<HashSet<_>>().len() != fingerprints.len() {
        return None;
    }
    fingerprints.sort_unstable();
    let expected = sha256_hex(format!("{sha}\n{EPOCH}\n{}", fingerprints.join("\n")));
    if !batch.field("batch_id").str_eq(&expected) {
        return None;
    }
    // A malformed individual report must not suppress other valid reports;
    // it is NEVER treated as an ML training label.
    let reports: Vec<Json> = raw_reports
        .iter()
        .filter(|row| validate_report(row, sha, is_ancestor) && row.field("protocol") == &Json::Int(protocol))
        .cloned()
        .collect();
    if !raw_reports.is_empty() && reports.is_empty() && rules.is_empty() {
        return None;
    }
    Some(reports)
}

/// Replay a title with the current in-process Rust parser.
pub fn replay_current(title: &str) -> Option<Json> {
    if title.len() > 240 {
        return None;
    }
    matchlab_parser::parse_title(title, false).ok().map(|parsed| Json::from(&serde_json::Value::Object(parsed)))
}

static SXE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[^A-Za-z0-9])S([0-9]{1,3})E([0-9]{1,4})(?:$|[^A-Za-z0-9])").unwrap());
static CAP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[^A-Za-z0-9])cap[. :_-]*([0-9]{3,4})(?:$|[^A-Za-z0-9])").unwrap());

pub fn explicit_coordinates(title: &str) -> Option<(i128, i128)> {
    if let Some(found) = SXE.captures(title) {
        return Some((found[1].parse().ok()?, found[2].parse().ok()?));
    }
    let found = CAP.captures(title)?;
    let number: i128 = found[1].parse().ok()?;
    Some((number / 100, number % 100))
}

/// Compatibility check only: never infer a missing value from a label.
fn current_field_present(current: &Json, field: &str) -> bool {
    let value = current.field(field);
    !value.is_null()
        && !value.str_eq("")
        && value.as_arr().is_none_or(|items| !items.is_empty())
        && value.as_obj().is_none_or(|map| !map.is_empty())
        && !value.is_false()
        && value != &Json::Bool(false)
}

/// Only classify *provably* corrected parser-marker disagreements.
/// IMDb identity/ASID complaints need current metadata; do not guess.
pub fn current_status(row: &Json, current: Option<&Json>) -> &'static str {
    let Some(current) = current else { return "replay_unavailable" };
    let alerts: Vec<&str> = row.field("alerts").as_arr().into_iter().flatten().filter_map(Json::as_str).collect();
    let fields: Vec<&str> = alerts.iter().filter_map(|alert| alert.strip_prefix("field_missing_")).collect();
    if !fields.is_empty() {
        // Analyzer alerts are hypotheses. A field actually present in the
        // new parser is enough to retire this field-specific hypothesis;
        // identity/ASID cases always require metadata-based validation.
        let parser_fields: Vec<&str> = fields.iter().copied().filter(|field| *field != "anime_episode").collect();
        if !parser_fields.is_empty() && parser_fields.iter().all(|field| current_field_present(current, field)) {
            if parser_fields.len() == alerts.len() {
                return "resolved_in_current_parser";
            }
            return "other_alerts_need_metadata_review";
        }
        if parser_fields.iter().any(|field| !current_field_present(current, field)) {
            return "possible_missing_field_in_current_parser";
        }
    }
    if alerts.contains(&"parser_marker_disagreement") {
        let title = row.field("title").as_str().unwrap_or_default();
        if let Some((season, episode)) = explicit_coordinates(title) {
            let contains = |key: &str, number: i128| {
                current.field(key).as_arr().is_some_and(|items| items.iter().any(|item| item == &Json::Int(number)))
            };
            if contains("seasons", season) && contains("episodes", episode) {
                if alerts == ["parser_marker_disagreement"] {
                    return "resolved_in_current_parser";
                }
                return "other_alerts_need_metadata_review";
            }
            return "still_present_in_current_parser";
        }
    }
    "needs_current_metadata_review"
}

/// Flatten GitHub comment pages (`[[comment, ...], ...]`).
pub fn flatten_comments(payload: &Json) -> Result<Vec<&Json>> {
    let Some(pages) = payload.as_arr() else { return Ok(Vec::new()) };
    let rows: Vec<&Json> = pages
        .iter()
        .flat_map(|page| match page {
            Json::Arr(items) => items.iter().collect::<Vec<_>>(),
            other => vec![other],
        })
        .filter(|item| item.as_obj().is_some())
        .collect();
    if rows.len() > MAX_COMMENTS {
        return Err("Community comment intake exceeds capacity; refusing truncation".into());
    }
    Ok(rows)
}

/// Replay of a title by the current parser (in tests: a stub).
pub type Replay<'a> = &'a dyn Fn(&str) -> Option<Json>;

pub fn intake(
    comments: &Json,
    current_sha: &str,
    now: DateTime<Utc>,
    seen: &BTreeSet<String>,
    is_ancestor: Ancestor,
    replay: Replay,
) -> Result<(Vec<Json>, BTreeSet<String>)> {
    let mut output = Vec::new();
    let mut added = BTreeSet::new();
    let cutoff = now - Duration::days(60);
    'comments: for comment in flatten_comments(comments)? {
        let Some(when) = aware_timestamp(comment.field("created_at")) else { continue };
        if when < cutoff || when > now + Duration::minutes(5) {
            continue;
        }
        let Some(batch) = validate_comment(comment, current_sha, is_ancestor) else { continue };
        for row in batch {
            let key = row.field("fingerprint").as_str().unwrap_or_default().to_owned();
            if seen.contains(&key) || added.contains(&key) {
                continue;
            }
            let title = row.field("title").as_str().unwrap_or_default();
            let current = replay(title).filter(|value| value.as_obj().is_some());
            let status = current_status(&row, current.as_ref());
            if status == "resolved_in_current_parser" {
                // A known parser problem was already fixed. Do not learn it.
                continue;
            }
            if seen.len() + added.len() >= MAX_LEDGER {
                return Err("Community dedup ledger reached its hard limit".into());
            }
            added.insert(key.clone());
            let current_parse = match &current {
                Some(parsed) => obj! {
                    "title" => parsed.field("title").clone(),
                    "seasons" => parsed.field("seasons").clone(),
                    "episodes" => parsed.field("episodes").clone(),
                },
                None => Json::Null,
            };
            output.push(obj! {
                "fingerprint" => key,
                "status" => status,
                "report" => row,
                "current_parse" => current_parse,
                "verified" => false,
            });
            if output.len() >= MAX_CANDIDATES {
                break 'comments;
            }
        }
    }
    let mut all = seen.clone();
    all.extend(added);
    Ok((output, all))
}

pub fn features(item: &Json) -> BTreeSet<String> {
    let parsed = item.field("parsed");
    let selected = item.field("selected");
    let mut present = BTreeSet::new();
    present.insert(format!("status:{}", item.field("status").as_str().unwrap_or_default()));
    if parsed.field("episodes").truthy() {
        present.insert("has_episode".into());
    }
    if parsed.field("seasons").truthy() {
        present.insert("has_season".into());
    }
    if !selected.field("episode").is_null() {
        present.insert("selected_episode".into());
    }
    if !selected.field("season").is_null() {
        present.insert("selected_season".into());
    }
    for alert in item.field("alerts").as_arr().into_iter().flatten().filter_map(Json::as_str) {
        if is_code(alert) {
            present.insert(format!("alert:{alert}"));
        }
    }
    present.retain(|feature| FEATURES.contains(feature));
    present
}

pub fn read_verified(text: Option<&str>, sha: &str) -> Result<Vec<Json>> {
    let Some(text) = text else { return Ok(Vec::new()) };
    let mut rows = Vec::new();
    for line in crate::util::split_lines(text) {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let item = Json::parse(&line).map_err(|error| format!("invalid verified label JSON: {error}"))?;
        // Verified labels are scoped to a fixed validated executable SHA.
        let protocol = item.field("protocol");
        if !(protocol == &Json::Int(PROTOCOL) || protocol == &Json::Int(3))
            || item.field("learning_epoch") != &Json::Int(EPOCH)
            || !item.field("validated_on_sha").str_eq(sha)
            || !item.field("verified").is_true()
            || !matches!(item.field("label"), Json::Bool(_))
            || !allowed_title(item.field("title"))
            || !matches!(item.field("status").as_str(), Some("matched" | "unmatched" | "error"))
            || !item.field("evidence").as_str().is_some_and(|evidence| evidence.chars().count() >= 8)
        {
            continue;
        }
        let Some(alerts) = item.field("alerts").as_arr() else { continue };
        if alerts.iter().any(|alert| !alert.as_str().is_some_and(is_code)) {
            continue;
        }
        rows.push(item);
    }
    Ok(rows)
}

pub struct Model {
    pub class_count: [usize; 2],
    pub weights: [BTreeMap<String, f64>; 2],
}

pub fn fit(items: &[&Json]) -> Option<Model> {
    let mut count = [0usize; 2];
    let mut hit: [BTreeMap<String, usize>; 2] =
        std::array::from_fn(|_| FEATURES.iter().map(|feature| (feature.clone(), 0)).collect());
    for item in items {
        let index = usize::from(item.field("label").is_true());
        count[index] += 1;
        for feature in features(item) {
            *hit[index].entry(feature).or_default() += 1;
        }
    }
    if count[0].min(count[1]) < 10 {
        return None;
    }
    let weights = std::array::from_fn(|class| {
        hit[class]
            .iter()
            .map(|(feature, hits)| (feature.clone(), (*hits as f64 + 1.0) / (count[class] as f64 + 2.0)))
            .collect()
    });
    Some(Model { class_count: count, weights })
}

pub fn predict(model: &Model, item: &Json) -> bool {
    let present = features(item);
    let total = (model.class_count[0] + model.class_count[1]) as f64;
    let scores: Vec<f64> = (0..2)
        .map(|class| {
            let mut score = (model.class_count[class] as f64 / total).ln();
            for (feature, probability) in &model.weights[class] {
                score += if present.contains(feature) { probability.ln() } else { (1.0 - probability).ln() };
            }
            score
        })
        .collect();
    scores[1] > scores[0]
}

/// Python `round(value, digits)`: exact decimal value, ties to even.
pub fn py_round(value: f64, digits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let exact = format!("{:.1100}", value.abs());
    let (whole, fraction) = exact.split_once('.').unwrap_or((&exact, ""));
    let (kept, rest) = fraction.split_at(digits.min(fraction.len()));
    let mut number: Vec<u8> = whole.bytes().chain(kept.bytes()).map(|byte| byte - b'0').collect();
    let first = rest.bytes().next().unwrap_or(b'0');
    let tail_nonzero = rest.bytes().skip(1).any(|byte| byte != b'0');
    let last_odd = number.last().is_some_and(|digit| digit % 2 == 1);
    if first > b'5' || (first == b'5' && (tail_nonzero || last_odd)) {
        let mut index = number.len();
        loop {
            if index == 0 {
                number.insert(0, 1);
                break;
            }
            index -= 1;
            if number[index] == 9 {
                number[index] = 0;
            } else {
                number[index] += 1;
                break;
            }
        }
    }
    let text: String = number.iter().map(|digit| char::from(b'0' + digit)).collect();
    let (whole, fraction) = text.split_at(text.len() - kept.len());
    let rounded: f64 = format!("{whole}.{fraction}0").parse().unwrap_or(value.abs());
    if value.is_sign_negative() { -rounded } else { rounded }
}

fn round4(value: f64) -> f64 {
    py_round(value, 4)
}

pub fn train(items: &[Json], sha: &str) -> Option<Json> {
    // Stable deterministic split prevents re-running one model repeatedly
    // against a cherry-picked test set. Holdout must include both classes.
    let mut train_rows = Vec::new();
    let mut test_rows = Vec::new();
    for item in items {
        let digest = Sha256::digest(item.field("title").as_str().unwrap_or_default().as_bytes());
        if digest[0] < 52 { test_rows.push(item) } else { train_rows.push(item) }
    }
    if items.len() < 60 || test_rows.len() < 10 {
        return None;
    }
    let model = fit(&train_rows)?;
    let (mut true_pos, mut false_pos, mut true_neg, mut false_neg) = (0usize, 0usize, 0usize, 0usize);
    for item in &test_rows {
        let actual = item.field("label").is_true();
        let guessed = predict(&model, item);
        match (guessed, actual) {
            (true, true) => true_pos += 1,
            (true, false) => false_pos += 1,
            (false, false) => true_neg += 1,
            (false, true) => false_neg += 1,
        }
    }
    if true_pos + false_neg < 3 || false_pos + true_neg < 3 {
        return None;
    }
    let precision = true_pos as f64 / (true_pos + false_pos).max(1) as f64;
    let accuracy = (true_pos + true_neg) as f64 / test_rows.len() as f64;
    if precision < 0.80 || accuracy < 0.70 {
        return None;
    }
    let weights: Vec<Json> = model
        .weights
        .iter()
        .map(|class| Json::Obj(class.iter().map(|(key, value)| (key.clone(), Json::Float(*value))).collect::<Object>()))
        .collect();
    Some(obj! {
        "model_kind" => "bernoulli_naive_bayes",
        "mode" => "shadow_only",
        "protocol" => Json::Int(PROTOCOL),
        "learning_epoch" => Json::Int(EPOCH),
        "validated_on_sha" => sha,
        "training_rows" => train_rows.len(),
        "holdout_rows" => test_rows.len(),
        "holdout_precision" => Json::Float(round4(precision)),
        "holdout_accuracy" => Json::Float(round4(accuracy)),
        "class_count" => Json::Arr(model.class_count.iter().map(|count| Json::from(*count)).collect()),
        "weights" => weights,
    })
}

/// Validated ledger contents.
pub fn read_ledger(text: Option<&str>) -> Result<BTreeSet<String>> {
    let seen: BTreeSet<String> = text.map(crate::util::split_lines).unwrap_or_default().into_iter().collect();
    if seen.len() > MAX_LEDGER || seen.iter().any(|line| !is_hex64(line)) {
        return Err("Invalid community learning ledger; fail closed".into());
    }
    Ok(seen)
}

pub fn json_valid_batch_sha(value: &Json) -> bool {
    json_valid_sha(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_like_python() {
        assert_eq!(round4(0.83335), 0.8334);
        assert_eq!(round4(0.03125), 0.0312);
        assert_eq!(round4(2.0 / 3.0), 0.6667);
        assert_eq!(round4(0.00015), 0.0001);
        assert_eq!(round4(1.0), 1.0);
        assert_eq!(py_round(0.875, 2), 0.88);
    }

    #[test]
    fn explicit_markers() {
        assert_eq!(explicit_coordinates("Show.S01E03"), Some((1, 3)));
        assert_eq!(explicit_coordinates("Show cap.105"), Some((1, 5)));
        assert_eq!(explicit_coordinates("XS01E03"), None);
    }
}
