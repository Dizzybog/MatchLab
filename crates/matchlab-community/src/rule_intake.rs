//! Intake untrusted v3 GitHub batch comments containing proposed regex.
//!
//! This runs ONLY in GitHub Actions, never in the MatchLab Docker image.
//! No issue text is ever executed. Rule approval remains a human-reviewed PR.
use crate::json::{dumps, Dump, Json};
use crate::learning::allowed_title;
use crate::util::{aware_timestamp, is_hex64, sha256_hex, valid_sha, Ancestor, Result};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

pub const MARKER: &str = "<!-- matchlab-community-batch-v3 -->";
pub const CODES: &[&str] = &[
    "season", "episode", "series", "part", "year", "movie", "sample", "bluray", "webrip", "webdl", "hdtv", "hdr",
    "codec", "audio", "x264", "x265", "h264", "h265", "hevc", "hdr10", "dovi", "ep", "eps", "new", "the", "eng",
    "sub", "dub",
];
pub const MAX_BODY: usize = 16384;
pub const MAX_RULES: usize = 12;
pub const MAX_SAMPLES: usize = 12;
pub const MAX_CATCHUP_PROPOSALS: usize = 4096;
pub const FIELD_EPOCH: u32 = 5;
pub const TV_EPOCH: u32 = 2;
pub const CORRECTIVE_EPOCH: u32 = 2;
pub const DISCOVERY_EPOCH: u32 = 2;

pub const TECH_FIELDS: &[&str] = &[
    "resolution", "codec", "quality", "audio", "bit_depth", "hdr", "channels", "languages", "subbed", "dubbed",
    "extension",
];

pub fn tv_grammar(family: &str) -> Option<&'static str> {
    Some(match family {
        "sxe" => r"(?i)(?:^|[._ -])s(?P<s>[0-9]{1,3})[._ -]*e(?P<e>[0-9]{1,5})(?:$|[._ -])",
        "x" => r"(?i)(?:^|[._ -])(?P<s>[0-9]{1,3})x(?P<e>[0-9]{1,5})(?:$|[._ -])",
        "words" => {
            r"(?i)(?:^|[._ -])season[._ -]+(?P<s>[0-9]{1,3})[._ -]+episode[._ -]+(?P<e>[0-9]{1,5})(?:$|[._ -])"
        }
        _ => return None,
    })
}

pub fn title_boundary_pattern(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "episode" => {
            r"(?i)(?:^|[ ._-])(?:s[0-9]{1,3}(?:[ ._-]*e[0-9]{1,5})?|[0-9]{1,3}x[0-9]{1,5}|e(?:p)?[0-9]{1,5})(?:$|[ ._-])"
        }
        "year" => r"(?i)(?:^|[ ._-])(?:19|20)[0-9]{2}(?:$|[ ._-])",
        "quality" => {
            r"(?i)(?:^|[ ._-])(?:1080p|2160p|720p|480p|4k|webdl|webrip|bluray|brrip|hdtv|remux|x264|x265|h264|h265|hevc|av1)(?:$|[ ._-])"
        }
        _ => return None,
    })
}

pub const TV_FAMILIES: &[&str] = &["sxe", "x", "words"];

/// Must match the restricted native Rust TV regex installed by tv_learning.rs.
pub fn tv_native_pattern(family: &str, field: &str) -> Option<&'static str> {
    Some(match (family, field) {
        ("sxe", "seasons") => r"(?i)(?:^|[._ -])s([0-9]{1,3})[._ -]*e[0-9]{1,5}(?:$|[._ -])",
        ("sxe", "episodes") => r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,5})(?:$|[._ -])",
        ("x", "seasons") => r"(?i)(?:^|[._ -])([0-9]{1,3})x[0-9]{1,5}(?:$|[._ -])",
        ("x", "episodes") => r"(?i)(?:^|[._ -])[0-9]{1,3}x([0-9]{1,5})(?:$|[._ -])",
        ("words", "seasons") => {
            r"(?i)(?:^|[._ -])season[._ -]+([0-9]{1,3})[._ -]+episode[._ -]+[0-9]{1,5}(?:$|[._ -])"
        }
        ("words", "episodes") => {
            r"(?i)(?:^|[._ -])season[._ -]+[0-9]{1,3}[._ -]+episode[._ -]+([0-9]{1,5})(?:$|[._ -])"
        }
        _ => return None,
    })
}

/// `TV_NATIVE_PATTERNS[family]` as a JSON object.
pub fn tv_native_patterns(family: &str) -> Option<Json> {
    Some(crate::obj! {
        "seasons" => tv_native_pattern(family, "seasons")?,
        "episodes" => tv_native_pattern(family, "episodes")?,
    })
}

pub const SXE_RANGE_PATTERN: &str =
    r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,4}[._ -]*-[._ -]*(?:e(?:p)?)?[._ -]*[0-9]{1,4})(?:$|[._ -])";
pub const GENERAL_PREFIX: &str = r"(?i)(?:^|[ ._-])(";
pub const GENERAL_SUFFIX: &str = r")(?:$|[ ._-])";

/// The immutable native handler catalogue the intake validates against.
#[derive(Clone)]
pub struct Catalog(pub Option<Vec<Json>>);

impl Catalog {
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os("MATCHLAB_HANDLERS_CATALOG") {
            return PathBuf::from(path);
        }
        let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../matchlab-parser/src/generated/handlers.json");
        if built.exists() { built } else { PathBuf::from("crates/matchlab-parser/src/generated/handlers.json") }
    }

    pub fn load(path: &Path) -> Catalog {
        let handlers = crate::util::read_json(path)
            .ok()
            .and_then(|value| value.get("handlers").and_then(Json::as_arr).cloned());
        Catalog(handlers)
    }

    pub fn load_default() -> Catalog {
        Catalog::load(&Catalog::default_path())
    }

    pub fn handlers(&self) -> Option<&[Json]> {
        self.0.as_deref()
    }

    fn handler(&self, index: &Json) -> Option<&Json> {
        let index = usize::try_from(index.as_int()?).ok()?;
        self.handlers()?.get(index)
    }
}

pub fn digest(text: &str) -> String {
    sha256_hex(text)
}

fn full(pattern: &str, value: &str) -> bool {
    Regex::new(&format!(r"\A(?:{pattern})\z")).is_ok_and(|regex| regex.is_match(value))
}

pub fn canonical(marker: &Json) -> Option<String> {
    let marker = marker.as_str()?;
    if !(3..=9).contains(&marker.len()) || !marker.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    if CODES.contains(&marker) {
        return None;
    }
    Some(format!(r"(?i)(?:^|[ ._-]){marker}[ ._-]*([1-9][0-9]{{0,4}})(?:$|[ ._-])"))
}

/// Python `str(value)` for the identifiers checked by a regex.
fn py_str(value: &Json) -> Option<String> {
    match value {
        Json::Str(text) => Some(text.clone()),
        Json::Int(number) => Some(number.to_string()),
        _ => None,
    }
}

fn positive_number(value: &Json) -> bool {
    value.as_int().is_some_and(|number| 0 < number && number <= 99999)
}

pub fn valid_sample(sample: &Json, marker: &Json) -> bool {
    if !sample.has_exact_keys(&["title", "imdb_id", "absolute_episode", "season", "episode", "anidb_id", "anidb_episode"]) {
        return false;
    }
    let title = sample.field("title");
    if !allowed_title(title) {
        return false;
    }
    let title = title.as_str().unwrap_or_default();
    if title.chars().count() > 180 {
        return false;
    }
    if !py_str(sample.field("imdb_id")).is_some_and(|id| crate::learning::imdb_id(&id)) {
        return false;
    }
    if !py_str(sample.field("anidb_id")).is_some_and(|id| (1..=9).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    if !["absolute_episode", "season", "episode", "anidb_episode"].iter().all(|name| positive_number(sample.field(name))) {
        return false;
    }
    let Some(expression) = canonical(marker) else { return false };
    let Ok(regex) = Regex::new(&expression) else { return false };
    regex
        .captures(title)
        .and_then(|found| found[1].parse::<i128>().ok())
        .is_some_and(|number| sample.field("absolute_episode") == &Json::Int(number))
}

/// A prior *real* ancestor build may supply an unverified hypothesis.
///
/// The trusted GitHub runner checks git history (fetch-depth: 0). This is not
/// independent verification: historical proposals remain review-only.
pub fn compatible_source(source_build: &Json, batch_sha: &str, epoch: u32, ancestor: Ancestor) -> bool {
    let Some(source_build) = source_build.as_str() else { return false };
    if !valid_sha(batch_sha) || source_build.matches(':').count() != 1 {
        return false;
    }
    let (source_sha, source_epoch) = source_build.split_once(':').unwrap_or_default();
    if !valid_sha(source_sha) || source_epoch != epoch.to_string() {
        return false;
    }
    source_sha == batch_sha || ancestor(source_sha, batch_sha)
}

const RULE_KEYS: &[&str] = &[
    "kind",
    "fingerprint",
    "marker",
    "regex",
    "source_build",
    "source_metadata",
    "evidence_count",
    "distinct_imdb",
    "distinct_episodes",
    "verification",
    "examples",
];

pub fn valid_rule(row: &Json, build_sha: &str, ancestor: Ancestor) -> bool {
    if !row.has_exact_keys(RULE_KEYS) {
        return false;
    }
    let marker = row.field("marker");
    let Some(expression) = canonical(marker) else { return false };
    if !row.field("kind").str_eq("verified_regex_proposal") || !row.field("regex").str_eq(&expression) {
        return false;
    }
    if !compatible_source(row.field("source_build"), build_sha, 1, ancestor) {
        return false;
    }
    if !row.field("source_metadata").as_str().is_some_and(|text| text.chars().count() <= 200) {
        return false;
    }
    let marker = marker.as_str().unwrap_or_default();
    if !row.field("fingerprint").str_eq(&digest(&format!("community-regex:{marker}"))) {
        return false;
    }
    for (name, minimum) in [("evidence_count", 12), ("distinct_imdb", 4), ("distinct_episodes", 5)] {
        if !row.field(name).as_int().is_some_and(|value| minimum <= value && value <= 2048) {
            return false;
        }
    }
    if !row.field("verification").str_eq("local_exact_imdb_title_and_anidb_tvdb_crosswalk") {
        return false;
    }
    let Some(examples) = row.field("examples").as_arr() else { return false };
    if !(8..=MAX_SAMPLES).contains(&examples.len()) {
        return false;
    }
    if !examples.iter().all(|sample| valid_sample(sample, row.field("marker"))) {
        return false;
    }
    // Proposal must contain useful independent examples, not 8 copies of
    // one torrent under cosmetic variations.
    let key = |value: &Json| dumps(value, Dump::DEFAULT);
    let pairs: HashSet<(String, String)> = examples
        .iter()
        .map(|sample| (key(sample.field("imdb_id")), key(sample.field("absolute_episode"))))
        .collect();
    let imdb: HashSet<String> = examples.iter().map(|sample| key(sample.field("imdb_id"))).collect();
    let episodes: HashSet<String> = examples.iter().map(|sample| key(sample.field("absolute_episode"))).collect();
    pairs.len() >= 8 && imdb.len() >= 3 && episodes.len() >= 5
}

pub fn technical_pattern(field: &Json, signal: &Json) -> Option<String> {
    let (Some(field), Some(signal)) = (field.as_str(), signal.as_str()) else { return None };
    technical_pattern_str(field, signal)
}

pub fn technical_pattern_str(field: &str, signal: &str) -> Option<String> {
    if !TECH_FIELDS.contains(&field) {
        return None;
    }
    if !(2..=24).contains(&signal.chars().count())
        || !signal.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'.')
    {
        return None;
    }
    let parts: Vec<String> =
        signal.split(['_', '.']).filter(|part| !part.is_empty()).map(crate::util::py_escape).collect();
    if !(1..=4).contains(&parts.len()) {
        return None;
    }
    let pattern = format!(r"(?i)(?:^|[ ._-])(?:{})(?:$|[ ._-])", parts.join("[ ._-]*"));
    (pattern.chars().count() <= 140).then_some(pattern)
}

fn bounded_int(value: &Json, small: i128, big: i128) -> bool {
    value.as_int().is_some_and(|number| small <= number && number <= big)
}

fn int(value: &Json) -> i128 {
    value.as_int().unwrap_or_default()
}

fn with_common(extra: &[&'static str]) -> Vec<&'static str> {
    let mut keys =
        vec!["kind", "fingerprint", "evidence_count", "distinct_titles", "canonical_confirmations", "source_build", "verification"];
    keys.extend_from_slice(extra);
    keys
}

const TEXT_FIELDS: &[&str] = &[
    "kind",
    "fingerprint",
    "source_build",
    "verification",
    "field",
    "signal",
    "original_kind",
    "family",
    "mode",
    "regex",
    "original_regex",
    "proposed_regex",
    "transform",
];

/// Validate every non-anime community regex family. Returns `true` only for
/// a complete, correctly fingerprinted proposal.
pub fn valid_extended_rule(r: &Json, sha: &str, ancestor: Ancestor, catalog: &Catalog) -> bool {
    let Some(map) = r.as_obj() else { return false };
    // JSON arrays/objects are valid input data but cannot be discriminators.
    // Reject them before family validation so one malformed proposal cannot
    // abort intake of all other Issue reports.
    if TEXT_FIELDS.iter().any(|key| map.get(*key).is_some_and(|value| value.as_str().is_none())) {
        return false;
    }
    let kind = r.field("kind").as_str().unwrap_or_default();
    let (min_evidence, min_titles) = match kind {
        "historical_regex_review" => (0, 0),
        "learned_general_regex_review" => (4, 2),
        _ => (12, 4),
    };
    if !bounded_int(r.field("evidence_count"), min_evidence, 2_000_000)
        || !bounded_int(r.field("distinct_titles"), min_titles, 2_000_000)
        || !bounded_int(r.field("canonical_confirmations"), 0, 2_000_000)
    {
        return false;
    }
    let evidence = int(r.field("evidence_count"));
    let titles = int(r.field("distinct_titles"));
    let confirmations = int(r.field("canonical_confirmations"));
    if confirmations > evidence || r.field("fingerprint").as_str().is_none() {
        return false;
    }
    let text = |key: &str| r.field(key).as_str().unwrap_or_default();
    let expected_fp = match kind {
        "learned_title_boundary_regex_review" => {
            // Title matches are NOT an independent authorization to rewrite
            // parsing; only hard-coded review-only boundary grammars.
            if !r.has_exact_keys(&with_common(&["field", "signal", "regex", "review_only"]))
                || !compatible_source(r.field("source_build"), sha, 1, ancestor)
            {
                return false;
            }
            if !r.field("field").str_eq("title")
                || !r.field("review_only").is_true()
                || !r.field("verification").str_eq("matcher_alias_matches_complete_raw_title_review_only")
                || confirmations != 0
                || evidence < 12
                || titles < 4
            {
                return false;
            }
            let signal = text("signal");
            if !(signal.starts_with("underextracted_") || signal.starts_with("overextracted_")) {
                return false;
            }
            let suffix = signal.split_once('_').map(|(_, rest)| rest).unwrap_or_default();
            let Some(pattern) = title_boundary_pattern(suffix) else { return false };
            if !r.field("regex").str_eq(pattern) {
                return false;
            }
            digest(&format!("title-boundary:{signal}:{pattern}"))
        }
        "learned_field_regex_review" => {
            // Unknown-field predictions are intentionally review-only.
            if !r.has_exact_keys(&with_common(&["field", "signal", "regex", "review_only"]))
                || !compatible_source(r.field("source_build"), sha, DISCOVERY_EPOCH, ancestor)
            {
                return false;
            }
            let (Some(field), Some(signal)) = (r.field("field").as_str(), r.field("signal").as_str()) else {
                return false;
            };
            if !r.field("review_only").is_true()
                || !r.field("verification").str_eq("unverified_model_discovery_review_only")
                || confirmations != 0
                || evidence < 12
                || titles < 12
            {
                return false;
            }
            if !(3..=16).contains(&signal.len()) || !signal.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
                return false;
            }
            let Some(handlers) = catalog.handlers() else { return false };
            if !handlers.iter().any(|row| row.field("kind").str_eq("regex") && row.field("name").str_eq(field)) {
                return false;
            }
            let pattern = format!(r"(?i)(?:^|[ ._-]){signal}(?:$|[ ._-])");
            if !r.field("regex").str_eq(&pattern) {
                return false;
            }
            digest(&format!("community-review:{field}:{signal}:{pattern}"))
        }
        "historical_regex_review" => {
            // Old local rules remain useful review evidence, but they cannot
            // inherit activation rights from a retired parser/metadata build.
            if !r.has_exact_keys(&[
                "kind",
                "original_kind",
                "fingerprint",
                "source_build",
                "field",
                "signal",
                "regex",
                "evidence_count",
                "distinct_titles",
                "canonical_confirmations",
                "review_only",
                "verification",
            ]) {
                return false;
            }
            if !matches!(
                r.field("original_kind").as_str(),
                Some(
                    "verified_regex_proposal"
                        | "learned_field_regex"
                        | "learned_tv_regex"
                        | "learned_source_regex_patch"
                        | "learned_corrective_regex"
                        | "learned_general_regex_review"
                        | "learned_field_regex_review"
                        | "learned_title_boundary_regex_review"
                )
            ) {
                return false;
            }
            let Some((old_sha, epoch)) = text("source_build").rsplit_once(':') else { return false };
            if !valid_sha(old_sha)
                || !(1..=2).contains(&epoch.chars().count())
                || !epoch.bytes().all(|byte| byte.is_ascii_digit())
                || !epoch.parse::<u32>().is_ok_and(|number| (1..=20).contains(&number))
            {
                return false;
            }
            if !ancestor(old_sha, sha) {
                return false;
            }
            if !r.field("review_only").is_true()
                || !r.field("verification").str_eq("historical_archive_requires_independent_review")
            {
                return false;
            }
            let field = text("field");
            if !(2..=40).contains(&field.len()) || !field.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                return false;
            }
            let signal = text("signal");
            if signal.len() > 24
                || !signal.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
            {
                return false;
            }
            let regex = text("regex");
            if !(1..=1024).contains(&regex.chars().count()) || regex.chars().any(|ch| !(' '..='~').contains(&ch)) {
                return false;
            }
            if !is_hex64(text("fingerprint")) || confirmations != 0 {
                return false;
            }
            // Opaque identity, review-only forever.
            text("fingerprint").to_owned()
        }
        "learned_field_regex" => {
            if !r.has_exact_keys(&with_common(&["field", "signal", "regex", "canonical"]))
                || !compatible_source(r.field("source_build"), sha, FIELD_EPOCH, ancestor)
            {
                return false;
            }
            let (field, signal) = (text("field"), text("signal"));
            let Some(pattern) = technical_pattern(r.field("field"), r.field("signal")) else { return false };
            if !r.field("regex").str_eq(&pattern)
                || !r.field("verification").str_eq("local_canonical_and_explicit_release_marker")
            {
                return false;
            }
            let value = r.field("canonical");
            let valid = match value {
                Json::Str(text) => (1..=48).contains(&text.chars().count()) && full(r"[a-zA-Z0-9_. +\-]+", text),
                Json::Int(number) => (0..=9999).contains(number),
                Json::Bool(_) => true,
                // Native audio is an array (e.g. ["OPUS"]), just like
                // languages. Reject arbitrary nested objects/long strings.
                Json::Arr(items) => match field {
                    "languages" => {
                        (1..=8).contains(&items.len())
                            && items.iter().all(|item| item.as_str().is_some_and(|text| full("[a-zA-Z]{1,5}", text)))
                    }
                    "audio" => {
                        (1..=4).contains(&items.len())
                            && items.iter().all(|item| {
                                item.as_str().is_some_and(|text| full("[a-zA-Z0-9 .+_-]{1,32}", text))
                            })
                    }
                    _ => false,
                },
                _ => false,
            };
            if !valid {
                return false;
            }
            let canon = dumps(value, Dump::COMPACT_UTF8);
            digest(&format!("community-field:{field}:{signal}:{canon}"))
        }
        "learned_tv_regex" => {
            if !r.has_exact_keys(&with_common(&["family", "fields", "regex", "native_patterns"]))
                || !compatible_source(r.field("source_build"), sha, TV_EPOCH, ancestor)
            {
                return false;
            }
            let family = text("family");
            let (Some(grammar), Some(native)) = (tv_grammar(family), tv_native_patterns(family)) else {
                return false;
            };
            if !r.field("regex").str_eq(grammar)
                || r.field("fields") != &Json::from_strs(["seasons", "episodes"])
                || r.field("native_patterns") != &native
            {
                return false;
            }
            if !r.field("verification").str_eq("explicit_tv_marker_and_canonical_confirmations") || confirmations < 2 {
                return false;
            }
            digest(&format!("community-tv:{family}"))
        }
        "learned_general_regex_review" => {
            // Reconstruct the exact finite separator-variation grammar to
            // ensure the comment cannot inject arbitrary regex code.
            if !r.has_exact_keys(&with_common(&["handler_index", "field", "mode", "regex", "review_only"]))
                || !compatible_source(r.field("source_build"), sha, 1, ancestor)
            {
                return false;
            }
            if !r.field("verification").str_eq("native_handler_metamorphic_replay_requires_upstream_review")
                || !r.field("review_only").is_true()
            {
                return false;
            }
            let mode = text("mode");
            if !matches!(mode, "add" | "correct") {
                return false;
            }
            let field = text("field");
            if !(1..=32).contains(&field.len())
                || !field.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return false;
            }
            if !bounded_int(r.field("handler_index"), 0, 4096) || evidence < 4 || titles < 2 {
                return false;
            }
            let Some(row) = catalog.handler(r.field("handler_index")) else { return false };
            if !row.field("kind").str_eq("regex") || !row.field("name").str_eq(field) {
                return false;
            }
            let Some(transform) = row.field("transform").as_str() else { return false };
            if mode == "correct" && !static_transform(transform) {
                return false;
            }
            let regex = text("regex");
            if !anchored_general(regex) {
                return false;
            }
            let index = int(r.field("handler_index"));
            digest(&format!("general:{field}:{index}:{mode}:{regex}"))
        }
        "learned_corrective_regex" => {
            // Review-only proposals must be reconstructed from the trusted
            // native parser catalogue. No arbitrary user-supplied regex runs.
            if !r.has_exact_keys(&with_common(&[
                "handler_index",
                "field",
                "family",
                "signal",
                "regex",
                "failure_classes",
                "review_only",
            ])) || !compatible_source(r.field("source_build"), sha, CORRECTIVE_EPOCH, ancestor)
            {
                return false;
            }
            let family = text("family");
            if !TV_FAMILIES.contains(&family) && family != "sxe_range" {
                return false;
            }
            let field = text("field");
            if !matches!(field, "seasons" | "episodes") || (family == "sxe_range" && field != "episodes") {
                return false;
            }
            if !r.field("signal").str_eq(&format!("corrective_{family}"))
                || !bounded_int(r.field("handler_index"), 0, 4096)
            {
                return false;
            }
            let Some(failures) = r.field("failure_classes").as_arr() else { return false };
            let names: Vec<&str> = failures.iter().filter_map(Json::as_str).collect();
            if failures.is_empty() || names.len() != failures.len() {
                return false;
            }
            let unique: BTreeSet<&str> = names.iter().copied().collect();
            if names.len() > 5
                || unique.len() != names.len()
                || !unique.iter().all(|name| matches!(*name, "missing" | "over" | "under" | "misassigned" | "incorrect"))
                || !names.is_sorted()
            {
                return false;
            }
            if confirmations < 2 || evidence < 12 {
                return false;
            }
            let Some(source) = catalog.handler(r.field("handler_index")) else { return false };
            if !source.field("name").str_eq(field) || !source.field("kind").str_eq("regex") {
                return false;
            }
            let options = source.field("options");
            if !options.field("remove").is_false() || !options.field("skipIfAlreadyFound").is_true() {
                return false;
            }
            let (correct, is_range) = if family == "sxe_range" {
                if !source.field("transform").str_eq("range_func") {
                    return false;
                }
                (SXE_RANGE_PATTERN, "true")
            } else {
                if !source.field("transform").str_eq("array(integer)") {
                    return false;
                }
                (tv_native_pattern(family, field).unwrap_or_default(), "false")
            };
            if !r.field("regex").str_eq(correct) {
                return false;
            }
            let verification = text("verification");
            if !matches!(verification, "independent_coordinates_and_regression_replay" | "review_only_requires_safe_replay") {
                return false;
            }
            if r.field("review_only").is_true() != (verification == "review_only_requires_safe_replay") {
                return false;
            }
            let index = int(r.field("handler_index"));
            digest(&format!("corrective:{family}:{index}:{is_range}:{correct}"))
        }
        "learned_source_regex_patch" => {
            if !r.has_exact_keys(&with_common(&[
                "handler_index",
                "field",
                "signal",
                "original_regex",
                "proposed_regex",
                "transform",
            ])) || !compatible_source(r.field("source_build"), sha, FIELD_EPOCH, ancestor)
            {
                return false;
            }
            if !bounded_int(r.field("handler_index"), 0, 4096) {
                return false;
            }
            let field = text("field");
            if !matches!(field, "resolution" | "codec" | "bit_depth") {
                return false;
            }
            let Some(extra) = technical_pattern(r.field("field"), r.field("signal")) else { return false };
            let Some(source) = catalog.handler(r.field("handler_index")) else { return false };
            if !source.field("kind").str_eq("regex") || !source.field("name").str_eq(field) {
                return false;
            }
            let Some(old) = source.field("pattern").as_str() else { return false };
            if !r.field("original_regex").str_eq(old) || r.field("transform") != source.field("transform") {
                return false;
            }
            let new = format!("(?:{old})|(?:{extra})");
            if new.chars().count() > 512 || !r.field("proposed_regex").str_eq(&new) {
                return false;
            }
            if !r.field("verification").str_eq("source_catalog_and_canonical_release_markers")
                || evidence < 16
                || titles < 6
                || confirmations < 2
            {
                return false;
            }
            let index = int(r.field("handler_index"));
            digest(&format!("community-patch:{index}:{old}:{new}"))
        }
        _ => return false,
    };
    r.field("fingerprint").str_eq(&expected_fp)
}

/// A replacement transform that cannot capture a dynamic `$1` value.
pub fn static_transform(transform: &str) -> bool {
    transform == "boolean"
        || (transform.starts_with("value('") && !transform.contains("$1"))
        || transform.starts_with("uniq_concat(value('")
}

/// The strict finite separator grammar of a general review regex.
pub fn anchored_general(value: &str) -> bool {
    let Some(body) = value.strip_prefix(GENERAL_PREFIX).and_then(|rest| rest.strip_suffix(GENERAL_SUFFIX)) else {
        return false;
    };
    if !(20..=256).contains(&value.chars().count()) {
        return false;
    }
    let chunks: Vec<&str> = body.split("[ ._-]+").collect();
    (2..=6).contains(&chunks.len())
        && chunks.iter().all(|part| (1..=24).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Validate any rule kind (anime marker or extended family).
pub fn valid_any_rule(candidate: &Json, sha: &str, ancestor: Ancestor, catalog: &Catalog) -> bool {
    if candidate.field("kind").str_eq("verified_regex_proposal") {
        valid_rule(candidate, sha, ancestor)
    } else {
        valid_extended_rule(candidate, sha, ancestor, catalog)
    }
}

pub fn parse_batch(comment: &Json, current_sha: &str, ancestor: Ancestor, catalog: &Catalog) -> Option<Vec<Json>> {
    let body = comment.get("body")?.as_str()?;
    let payload = body.strip_prefix(MARKER)?.strip_prefix('\n')?;
    if body.len() > MAX_BODY {
        return None;
    }
    let batch = Json::parse(payload).ok()?;
    if !batch.has_exact_keys(&[
        "protocol",
        "learning_epoch",
        "build_sha",
        "source",
        "batch_id",
        "created_unix",
        "verified",
        "reports",
        "rules",
    ]) {
        return None;
    }
    let sha = batch.field("build_sha").as_str().unwrap_or_default();
    let (Some(reports), Some(rules)) = (batch.field("reports").as_arr(), batch.field("rules").as_arr()) else {
        return None;
    };
    if batch.field("protocol") != &Json::Int(3)
        || batch.field("learning_epoch") != &Json::Int(1)
        || !valid_sha(sha)
        || !ancestor(sha, current_sha)
        || !batch.field("verified").is_false()
        || !batch.field("source").str_eq("matchlab-community-batch")
        || batch.field("created_unix").as_int().is_none()
        || rules.len() > MAX_RULES
        || reports.len() + rules.len() > 16
    {
        return None;
    }
    // Check integrity of the complete batch before validating individual
    // children. The SHA-256 batch ID is a consistency checksum, NOT an
    // authenticated signature. Every candidate still needs independent review.
    let mut fingerprints = Vec::new();
    for item in rules.iter().chain(reports) {
        let fingerprint = item.as_obj()?.get("fingerprint").and_then(Json::as_str).filter(|fp| is_hex64(fp))?;
        fingerprints.push(fingerprint);
    }
    if fingerprints.iter().collect::<HashSet<_>>().len() != fingerprints.len() {
        return None;
    }
    fingerprints.sort_unstable();
    let expected = sha256_hex(format!("{sha}\n1\n{}", fingerprints.join("\n")));
    if !batch.field("batch_id").str_eq(&expected) {
        return None;
    }
    // Invalid or stale individual proposals stay on the GitHub Issue,
    // but cannot suppress other valid candidates from this batch.
    Some(
        rules
            .iter()
            .filter(|candidate| candidate.as_obj().is_some() && valid_any_rule(candidate, sha, ancestor, catalog))
            .cloned()
            .collect(),
    )
}

pub fn collect(
    all_comments: &Json,
    sha: &str,
    seen: &BTreeSet<String>,
    now: DateTime<Utc>,
    ancestor: Ancestor,
    catalog: &Catalog,
) -> Result<Vec<Json>> {
    let cutoff = now - Duration::days(60);
    let mut output = Vec::new();
    let mut pending = HashSet::new();
    for sub in all_comments.as_arr().into_iter().flatten() {
        let comments: Vec<&Json> = match sub {
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
            let Some(rules) = parse_batch(comment, sha, ancestor, catalog) else { continue };
            for rule in rules {
                let fingerprint = rule.field("fingerprint").as_str().unwrap_or_default().to_owned();
                if seen.contains(&fingerprint) || !pending.insert(fingerprint) {
                    continue;
                }
                output.push(rule);
                if output.len() > MAX_CATCHUP_PROPOSALS {
                    return Err("Community proposal intake exceeded capacity; \
                                refusing to silently truncate GitHub comments"
                        .into());
                }
            }
        }
    }
    // Do NOT poison the permanent ledger with only self-reported evidence.
    // A fingerprint becomes permanent only after trusted screening, regression
    // tests and successful creation of the human-review PR.
    Ok(output)
}
