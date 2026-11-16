//! Community report and rule intake: historical builds, spoofing, dedup and
//! every regex sharing family.
mod common;

use chrono::{DateTime, Utc};
use common::*;
use matchlab_community::json::{dumps, Dump, Json};
use matchlab_community::learning::{self as cl, fingerprint};
use matchlab_community::review::screen;
use matchlab_community::rule_intake::{self as rules, technical_pattern_str, Catalog};
use serde_json::json;
use std::collections::BTreeSet;

const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const LATEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const UNKNOWN: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn report(title: &str, codes: &[&str], sha: &str, protocol: i64) -> Json {
    j(json!({
        "protocol": protocol, "learning_epoch": 1, "build_sha": sha,
        "source": "matchlab-community", "fingerprint": fingerprint(title, codes),
        "title": title, "status": "matched", "alerts": codes,
        "parsed": {"title": "Bleach", "seasons": [], "episodes": [338]},
        "selected": {"imdb_id": "tt0434665", "season": null, "episode": null},
        "verified": false,
    }))
}

fn make_report(title: &str, codes: &[&str]) -> Json {
    report(title, codes, OLD, 2)
}

fn bleach() -> Json {
    make_report("Bleach_338", &["unresolved_absolute_anime_episode"])
}

fn batch_id(sha: &str, fingerprints: &[&Json]) -> String {
    let mut fps: Vec<String> = fingerprints.iter().map(|row| row.field("fingerprint").as_str().unwrap().to_owned()).collect();
    fps.sort();
    common::sha(&format!("{sha}\n1\n{}", fps.join("\n")))
}

fn comment_v2(rows: &[Json], sha: &str, overrides: serde_json::Value) -> Json {
    let mut payload = j(json!({
        "protocol": 2, "learning_epoch": 1, "build_sha": sha,
        "source": "matchlab-community-batch", "batch_id": batch_id(sha, &rows.iter().collect::<Vec<_>>()),
        "created_unix": now().timestamp(), "verified": false,
    }));
    payload = with(&payload, "reports", Json::Arr(rows.to_vec()));
    for (key, value) in j(overrides).as_obj().unwrap() {
        payload = with(&payload, key, value.clone());
    }
    let body = format!("{}\n{}", cl::MARKER, dumps(&payload, Dump::DEFAULT));
    j(json!({"body": body, "created_at": now().to_rfc3339(), "id": 1}))
}

fn comment_v3(reports: &[Json], rules_list: &[Json], sha: &str) -> Json {
    let all: Vec<&Json> = reports.iter().chain(rules_list).collect();
    let payload = j(json!({
        "protocol": 3, "learning_epoch": 1, "build_sha": sha,
        "source": "matchlab-community-batch", "created_unix": now().timestamp(),
        "verified": false, "batch_id": batch_id(sha, &all),
    }));
    let payload = with(&with(&payload, "reports", Json::Arr(reports.to_vec())), "rules", Json::Arr(rules_list.to_vec()));
    let body = format!("{}\n{}", rules::MARKER, dumps(&payload, Dump::DEFAULT));
    j(json!({"body": body, "created_at": now().to_rfc3339(), "id": 1}))
}

fn intake_with(payload: &[Json], seen: &BTreeSet<String>, replay: &dyn Fn(&str) -> Option<Json>) -> (Vec<Json>, BTreeSet<String>) {
    cl::intake(&Json::Arr(payload.to_vec()), LATEST, now(), seen, &yes, replay).unwrap()
}

// ---- community_learning --------------------------------------------------

#[test]
fn old_build_allowed_if_known_ancestor() {
    let report = comment_v2(&[bleach()], OLD, json!({}));
    assert!(cl::validate_comment(&report, LATEST, &|old, _| old == OLD).is_some());
    assert!(cl::validate_comment(&report, LATEST, &no).is_none());
    let epoch = comment_v2(&[bleach()], OLD, json!({"learning_epoch": 2}));
    assert!(cl::validate_comment(&epoch, LATEST, &yes).is_none());
}

#[test]
fn reject_forged_labels_and_sensitive_titles() {
    assert!(cl::validate_comment(&comment_v2(&[bleach()], OLD, json!({"verified": true})), LATEST, &yes).is_none());
    for title in ["user@server.com", "https://malicious.example", "C:\\Users\\name\\file.mkv", "/mnt/private/path", "token=mysecret", "A\nB"] {
        let row = make_report(title, &["unresolved_absolute_anime_episode"]);
        assert!(cl::validate_comment(&comment_v2(&[row], OLD, json!({})), LATEST, &yes).is_none(), "{title}");
    }
    let extra = with(&bleach(), "unexpected_secret", Json::from("password"));
    assert!(cl::validate_comment(&comment_v2(&[extra], OLD, json!({})), LATEST, &yes).is_none());
}

#[test]
fn current_parser_replay_drops_already_fixed_marker_disagreement() {
    let row = make_report("Show.S01E03", &["parser_marker_disagreement"]);
    let payload = [comment_v2(&[row], OLD, json!({}))];
    let fixed = |_: &str| Some(j(json!({"title": "Show", "seasons": [1], "episodes": [3]})));
    assert_eq!(intake_with(&payload, &BTreeSet::new(), &fixed), (vec![], BTreeSet::new()));
    let unresolved = |_: &str| Some(j(json!({"title": "Show", "seasons": [2], "episodes": [3]})));
    let (result, seen) = intake_with(&payload, &BTreeSet::new(), &unresolved);
    assert_eq!(result.len(), 1);
    assert!(result[0].field("status").str_eq("still_present_in_current_parser"));
    assert_eq!(seen.len(), 1);
}

#[test]
fn absolute_anime_requires_metadata_recheck_even_if_current_parse_changed() {
    let payload = [comment_v2(&[bleach()], OLD, json!({}))];
    let parsed = |_: &str| Some(j(json!({"title": "Bleach", "episodes": [338], "seasons": []})));
    let (result, seen) = intake_with(&payload, &BTreeSet::new(), &parsed);
    assert!(result[0].field("status").str_eq("needs_current_metadata_review"));
    assert_eq!(seen.len(), 1);
    let (again, again_seen) = intake_with(&payload, &seen, &|_| Some(j(json!({}))));
    assert!(again.is_empty());
    assert_eq!(again_seen, seen);
}

#[test]
fn bleach_s16_anidb_range_failure_not_discarded_as_parsing_fix() {
    let mut row = make_report("Bleach S16", &["incomplete_anime_episode_range"]);
    row = with(&row, "parsed", j(json!({"title": "Bleach", "seasons": [16], "episodes": []})));
    row = with(&row, "selected", j(json!({"imdb_id": "tt0434665", "season": 16, "episode": 1})));
    let payload = [comment_v2(&[row.clone()], OLD, json!({}))];
    let parsed = |_: &str| Some(j(json!({"title": "Bleach", "seasons": [16], "episodes": []})));
    let (result, seen) = intake_with(&payload, &BTreeSet::new(), &parsed);
    assert_eq!(result.len(), 1);
    assert!(result[0].field("status").str_eq("needs_current_metadata_review"));
    assert_eq!(seen.len(), 1);
    // Normalized fingerprints prevent another report for the same issue.
    let other = make_report("Bleach.S16", &["incomplete_anime_episode_range"]);
    assert_eq!(row.field("fingerprint"), other.field("fingerprint"));
    let (after, after_seen) =
        intake_with(&[comment_v2(&[other], OLD, json!({}))], &seen, &|_| Some(j(json!({"title": "Bleach", "seasons": [16]}))));
    assert!(after.is_empty());
    assert_eq!(after_seen, seen);
}

#[test]
fn reject_duplicate_rows_and_overlong_payload() {
    assert!(cl::validate_comment(&comment_v2(&[bleach(), bleach()], OLD, json!({})), LATEST, &yes).is_none());
    let corrupted = comment_v2(&[bleach()], OLD, json!({}));
    let body = format!("{}{}", corrupted.field("body").as_str().unwrap(), "x".repeat(20000));
    assert!(cl::validate_comment(&with(&corrupted, "body", Json::from(body)), LATEST, &yes).is_none());
}

#[test]
fn malformed_unicode_and_nested_json_are_rejected() {
    let body = format!("{}\n{}{}", cl::MARKER, "[".repeat(2000), "]".repeat(2000));
    assert!(cl::validate_comment(&j(json!({"body": body})), LATEST, &yes).is_none());
    // A lone surrogate cannot be decoded at all.
    let lone = format!("{}\n{{\"title\":\"Example.\\ud800.1080p\"}}", cl::MARKER);
    assert!(cl::validate_comment(&j(json!({"body": lone})), LATEST, &yes).is_none());
}

#[test]
fn current_source_replay_clears_fixed_parser_field_hypotheses() {
    let mut row = make_report("Example.1080p.x265", &["field_missing_codec", "field_missing_resolution"]);
    row = with(&row, "parsed", j(json!({"title": "Example", "seasons": [], "episodes": []})));
    let payload = [comment_v2(&[row], OLD, json!({}))];
    let resolved = |_: &str| Some(j(json!({"title": "Example", "resolution": "1080p", "codec": "hevc"})));
    assert_eq!(intake_with(&payload, &BTreeSet::new(), &resolved), (vec![], BTreeSet::new()));
    let missing = |_: &str| Some(j(json!({"title": "Example", "resolution": "1080p"})));
    let (result, seen) = intake_with(&payload, &BTreeSet::new(), &missing);
    assert_eq!(result.len(), 1);
    assert!(result[0].field("status").str_eq("possible_missing_field_in_current_parser"));
    assert_eq!(seen.len(), 1);
}

#[test]
fn no_shadow_ml_without_independent_labels() {
    assert!(cl::train(&[], LATEST).is_none());
    assert!(cl::fit(&[]).is_none());
}

#[test]
fn shadow_model_trains_on_separable_verified_labels() {
    let mut rows = Vec::new();
    for index in 0..200 {
        let label = index % 2 == 0;
        let status = if label { "unmatched" } else { "matched" };
        rows.push(j(json!({
            "title": format!("Example.Title.{index}"), "label": label, "status": status,
            "alerts": [if label { "matching_error" } else { "field_missing_codec" }],
            "parsed": {"episodes": [1]}, "selected": {},
        })));
    }
    let model = cl::train(&rows, LATEST).expect("separable labels produce a shadow model");
    assert!(model.field("mode").str_eq("shadow_only"));
    assert_eq!(model.field("holdout_precision"), &Json::Float(1.0));
}

#[test]
fn real_parser_replay_is_in_process() {
    let parsed = cl::replay_current("Show.S01E03.1080p").expect("parser result");
    assert_eq!(parsed.field("seasons"), &j(json!([1])));
    assert!(cl::replay_current(&"x".repeat(241)).is_none());
}

// ---- historical batches ------------------------------------------------

fn ancestor(source: &str, target: &str) -> bool {
    source == target || (source == OLD && target == LATEST)
}

fn field_rule(signal: &str, src: &str) -> Json {
    let canonical = "AV1";
    j(json!({
        "kind": "learned_field_regex", "field": "codec", "signal": signal,
        "regex": technical_pattern_str("codec", signal), "canonical": canonical,
        "source_build": format!("{src}:{}", rules::FIELD_EPOCH),
        "evidence_count": 30, "distinct_titles": 12, "canonical_confirmations": 10,
        "verification": "local_canonical_and_explicit_release_marker",
        "fingerprint": rules::digest(&format!("community-field:codec:{signal}:\"{canonical}\"")),
    }))
}

fn tv_report(title: &str, src: &str) -> Json {
    let alert = ["field_missing_seasons"];
    j(json!({
        "protocol": 3, "learning_epoch": 1, "build_sha": src, "source": "matchlab-community",
        "fingerprint": fingerprint(title, &alert), "title": title, "status": "unmatched", "alerts": alert,
        "parsed": {"title": "Example Show", "seasons": [], "episodes": [18]},
        "selected": {"imdb_id": null, "season": null, "episode": null}, "verified": false,
    }))
}

#[test]
fn old_verified_ancestor_report_is_replayed_not_discarded() {
    let entry = comment_v3(&[tv_report("Example.Show.s02e18.WEB-DL", OLD)], &[], LATEST);
    let reports = cl::validate_comment(&entry, LATEST, &ancestor).unwrap();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].field("build_sha").str_eq(OLD));
    let parsed = |_: &str| Some(j(json!({"title": "Example Show", "seasons": [], "episodes": [18]})));
    let (result, _) = cl::intake(&Json::Arr(vec![entry.clone()]), LATEST, now(), &BTreeSet::new(), &ancestor, &parsed).unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].field("verified").is_false());
    assert!(result[0].field("status").str_eq("possible_missing_field_in_current_parser"));
    assert!(cl::validate_comment(&entry, LATEST, &no).is_none());
}

#[test]
fn rule_uses_historical_source_but_never_automatic_activation() {
    let entry = comment_v3(&[], &[field_rule("av1", OLD)], LATEST);
    let accepted = rules::parse_batch(&entry, LATEST, &ancestor, &catalog()).unwrap();
    assert_eq!(accepted.len(), 1);
    assert!(accepted[0].field("source_build").str_eq(&format!("{OLD}:5")));
    assert_eq!(rules::parse_batch(&entry, LATEST, &|source, target| source == target, &catalog()), Some(vec![]));
    let unknown = with(&field_rule("av1", OLD), "source_build", Json::from(format!("{UNKNOWN}:5")));
    assert!(!rules::valid_extended_rule(&unknown, LATEST, &ancestor, &catalog()));
}

#[test]
fn one_bad_rule_does_not_erase_another_valid_rule() {
    let good = field_rule("av1", OLD);
    let bad = with(&field_rule("h264", OLD), "regex", Json::from(r"(?s)(.*)+"));
    let entry = comment_v3(&[], &[bad, good.clone()], LATEST);
    let accepted = rules::parse_batch(&entry, LATEST, &ancestor, &catalog()).unwrap();
    assert_eq!(accepted, vec![good]);
    let body = entry.field("body").as_str().unwrap();
    let mut payload = Json::parse(&body[rules::MARKER.len() + 1..]).unwrap();
    payload = with(&payload, "batch_id", Json::from("0".repeat(64)));
    let tampered = with(&entry, "body", Json::from(format!("{}\n{}", rules::MARKER, dumps(&payload, Dump::DEFAULT))));
    assert!(rules::parse_batch(&tampered, LATEST, &ancestor, &catalog()).is_none());
}

#[test]
fn one_invalid_observation_does_not_erase_valid_one() {
    let good = tv_report("Example.Show.s02e18.WEB-DL", OLD);
    let bad = with(&tv_report("Different.Show.s03e01", OLD), "unexpected_private_property", Json::from("not-allowed"));
    let selected = cl::validate_comment(&comment_v3(&[bad, good.clone()], &[], LATEST), LATEST, &ancestor).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].field("fingerprint"), good.field("fingerprint"));
}

#[test]
fn cross_branch_or_bad_epoch_never_accepted() {
    let entry = comment_v3(&[tv_report("Example.Show.s02e18.WEB-DL", UNKNOWN)], &[], LATEST);
    assert!(cl::validate_comment(&entry, LATEST, &ancestor).is_none());
    let invalid = with(&field_rule("av1", OLD), "source_build", Json::from(format!("{OLD}:99")));
    assert!(!rules::valid_extended_rule(&invalid, LATEST, &ancestor, &catalog()));
}

// ---- anime marker rule intake ------------------------------------------

const SHA: &str = OLD;

fn proposal(marker: &str) -> Json {
    let samples: Vec<serde_json::Value> = (0..8)
        .map(|n| {
            json!({
                "title": format!("SampleShow{}.EPIS.{}.1080p.WEB-DL", n % 4, 301 + n),
                "imdb_id": format!("tt{}", 1234567 + n % 4), "absolute_episode": 301 + n,
                "season": 2, "episode": n + 1, "anidb_id": (1000 + n % 4).to_string(), "anidb_episode": 301 + n,
            })
        })
        .collect();
    j(json!({
        "kind": "verified_regex_proposal", "fingerprint": common::sha(&format!("community-regex:{marker}")),
        "marker": marker, "regex": rules::canonical(&Json::from(marker)),
        "source_build": format!("{SHA}:1"), "source_metadata": "verified-revision",
        "evidence_count": 20, "distinct_imdb": 4, "distinct_episodes": 8,
        "verification": "local_exact_imdb_title_and_anidb_tvdb_crosswalk", "examples": samples,
    }))
}

fn rule_comment(rule: &Json, sha: &str) -> Json {
    let digest = common::sha(&format!("{sha}\n1\n{}", rule.field("fingerprint").as_str().unwrap()));
    let payload = j(json!({
        "protocol": 3, "learning_epoch": 1, "build_sha": sha, "source": "matchlab-community-batch",
        "batch_id": digest, "created_unix": now().timestamp(), "verified": false, "reports": [],
    }));
    let payload = with(&payload, "rules", Json::Arr(vec![rule.clone()]));
    let body = format!("{}\n{}", rules::MARKER, dumps(&payload, Dump::DEFAULT));
    j(json!({"body": body, "created_at": now().to_rfc3339(), "id": 1}))
}

fn set_example(rule: &Json, index: usize, key: &str, value: Json) -> Json {
    let mut examples = rule.field("examples").as_arr().unwrap().clone();
    examples[index] = with(&examples[index], key, value);
    with(rule, "examples", Json::Arr(examples))
}

#[test]
fn valid_independent_evidence_can_enter_pr_review() {
    assert!(rules::valid_rule(&proposal("epis"), SHA, &cl_git));
    let batch = rule_comment(&proposal("epis"), SHA);
    assert_eq!(rules::parse_batch(&batch, SHA, &yes, &catalog()).unwrap().len(), 1);
    let comments = Json::Arr(vec![batch.clone()]);
    let out = rules::collect(&comments, SHA, &BTreeSet::new(), now(), &yes, &catalog()).unwrap();
    assert_eq!(out.len(), 1);
    // Untrusted reports cannot poison the permanent ledger.
    let again = rules::collect(&comments, SHA, &BTreeSet::new(), now(), &yes, &catalog()).unwrap();
    assert_eq!(again.len(), 1);
    let accepted: BTreeSet<String> = [out[0].field("fingerprint").as_str().unwrap().to_owned()].into();
    assert!(rules::collect(&comments, SHA, &accepted, now(), &yes, &catalog()).unwrap().is_empty());
    // Rule-only batches remain valid for the legacy anomaly reviewer.
    assert_eq!(cl::validate_comment(&batch, SHA, &yes), Some(vec![]));
}

fn cl_git(source: &str, target: &str) -> bool {
    source == target
}

#[test]
fn rejects_injection_and_unsafe_regex() {
    for marker in ["episode", "h265", "xxx.*", "a\nb", "en"] {
        assert!(rules::canonical(&Json::from(marker)).is_none(), "{marker}");
    }
    let unsafe_regex = with(&proposal("epis"), "regex", Json::from("(?s)(.*)+"));
    assert!(!rules::valid_rule(&unsafe_regex, SHA, &cl_git));
    for title in ["/private/Series.EPIS.301", "example@host.EPIS.301"] {
        assert!(!rules::valid_rule(&set_example(&proposal("epis"), 0, "title", Json::from(title)), SHA, &cl_git));
    }
    assert!(!rules::valid_rule(&set_example(&proposal("epis"), 0, "absolute_episode", Json::Int(999)), SHA, &cl_git));
}

#[test]
fn forged_build_and_old_epochs_are_rejected() {
    let batch = rule_comment(&proposal("epis"), SHA);
    assert!(rules::parse_batch(&batch, LATEST, &no, &catalog()).is_none());
    let forged = with(&proposal("epis"), "source_build", Json::from(format!("{LATEST}:1")));
    assert!(!rules::valid_rule(&forged, SHA, &cl_git));
    let body = batch.field("body").as_str().unwrap();
    let content = with(&Json::parse(&body[rules::MARKER.len() + 1..]).unwrap(), "learning_epoch", Json::Int(0));
    let old = with(&batch, "body", Json::from(format!("{}\n{}", rules::MARKER, dumps(&content, Dump::DEFAULT))));
    assert!(rules::parse_batch(&old, SHA, &yes, &catalog()).is_none());
}

#[test]
fn requires_heterogeneous_evidence() {
    let first = proposal("epis").field("examples").as_arr().unwrap()[0].clone();
    let copies = with(&proposal("epis"), "examples", Json::Arr(vec![first; 8]));
    assert!(!rules::valid_rule(&copies, SHA, &cl_git));
    assert!(!rules::valid_rule(&with(&proposal("epis"), "distinct_imdb", Json::Int(1)), SHA, &cl_git));
}

#[test]
fn invalid_marker_with_null_regex_is_rejected() {
    for marker in [json!(null), json!([]), json!({}), json!(7), json!("episode")] {
        let item = with(&with(&proposal("epis"), "marker", j(marker)), "regex", Json::Null);
        assert!(!rules::valid_rule(&item, SHA, &cl_git));
    }
}

#[test]
fn excessively_nested_json_is_rejected() {
    let body = format!("{}\n{}{}", rules::MARKER, "[".repeat(2000), "]".repeat(2000));
    assert!(rules::parse_batch(&j(json!({"body": body})), SHA, &yes, &catalog()).is_none());
}

// ---- every regex sharing family ----------------------------------------

fn common_rule(kind: &str, epoch: u32) -> serde_json::Value {
    json!({"kind": kind, "evidence_count": 24, "distinct_titles": 8, "canonical_confirmations": 8,
           "source_build": format!("{SHA}:{epoch}")})
}

fn merge(base: serde_json::Value, extra: serde_json::Value) -> Json {
    let mut out = j(base);
    for (key, value) in j(extra).as_obj().unwrap() {
        out = with(&out, key, value.clone());
    }
    out
}

fn technical() -> Json {
    let (field, signal, value) = ("resolution", "1440p", "1440p");
    merge(
        common_rule("learned_field_regex", rules::FIELD_EPOCH),
        json!({"verification": "local_canonical_and_explicit_release_marker", "field": field, "signal": signal,
               "regex": technical_pattern_str(field, signal), "canonical": value,
               "fingerprint": common::sha(&format!("community-field:{field}:{signal}:\"{value}\""))}),
    )
}

fn tv() -> Json {
    merge(
        common_rule("learned_tv_regex", rules::TV_EPOCH),
        json!({"verification": "explicit_tv_marker_and_canonical_confirmations", "family": "sxe",
               "fields": ["seasons", "episodes"], "regex": rules::tv_grammar("sxe"),
               "fingerprint": common::sha("community-tv:sxe")}),
    )
    .pipe(|rule| with(&rule, "native_patterns", rules::tv_native_patterns("sxe").unwrap()))
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl Pipe for Json {}

fn patch() -> Json {
    let handlers = handlers();
    let index = 33;
    let source = &handlers[index];
    let name = source.field("name").as_str().unwrap();
    let pattern = source.field("pattern").as_str().unwrap();
    let extra = technical_pattern_str(name, "1440p").unwrap();
    let value = format!("(?:{pattern})|(?:{extra})");
    merge(
        common_rule("learned_source_regex_patch", rules::FIELD_EPOCH),
        json!({"verification": "source_catalog_and_canonical_release_markers", "handler_index": index,
               "field": name, "signal": "1440p", "original_regex": pattern, "proposed_regex": value,
               "transform": source.field("transform").as_str().unwrap(),
               "fingerprint": common::sha(&format!("community-patch:{index}:{pattern}:{value}"))}),
    )
}

fn review_discovery(field: &str, marker: &str) -> Json {
    let pattern = format!(r"(?i)(?:^|[ ._-]){marker}(?:$|[ ._-])");
    merge(
        common_rule("learned_field_regex_review", rules::DISCOVERY_EPOCH),
        json!({"evidence_count": 24, "distinct_titles": 12, "verification": "unverified_model_discovery_review_only",
               "field": field, "signal": marker, "regex": pattern, "canonical_confirmations": 0,
               "review_only": true, "fingerprint": common::sha(&format!("community-review:{field}:{marker}:{pattern}"))}),
    )
}

fn title_boundary() -> Json {
    let pattern = rules::title_boundary_pattern("episode").unwrap();
    let signal = "underextracted_episode";
    merge(
        common_rule("learned_title_boundary_regex_review", 1),
        json!({"evidence_count": 16, "distinct_titles": 4, "canonical_confirmations": 0, "field": "title",
               "signal": signal, "regex": pattern, "review_only": true,
               "verification": "matcher_alias_matches_complete_raw_title_review_only",
               "fingerprint": common::sha(&format!("title-boundary:{signal}:{pattern}"))}),
    )
}

fn batch(proposals: &[Json]) -> Json {
    let mut fps: Vec<&str> = proposals.iter().map(|row| row.field("fingerprint").as_str().unwrap()).collect();
    fps.sort();
    let payload = j(json!({
        "protocol": 3, "learning_epoch": 1, "build_sha": SHA, "source": "matchlab-community-batch",
        "batch_id": common::sha(&format!("{SHA}\n1\n{}", fps.join("\n"))), "created_unix": 1,
        "verified": false, "reports": [],
    }));
    let payload = with(&payload, "rules", Json::Arr(proposals.to_vec()));
    j(json!({"body": format!("{}\n{}", rules::MARKER, dumps(&payload, Dump::DEFAULT))}))
}

fn valid(rule: &Json) -> bool {
    rules::valid_extended_rule(rule, SHA, &cl_git, &catalog())
}

fn parsed(rule_batch: &Json) -> Option<Vec<Json>> {
    rules::parse_batch(rule_batch, SHA, &yes, &catalog())
}

fn screen_new(candidates: &[Json], old: &Json) -> (Json, bool) {
    screen(&Json::Arr(candidates.to_vec()), old, None, &cl_git, &catalog()).unwrap()
}

#[test]
fn all_three_regex_families_survive_github_intake() {
    for candidate in [technical(), tv(), patch()] {
        assert!(valid(&candidate), "{}", text(&candidate));
        assert_eq!(parsed(&batch(std::slice::from_ref(&candidate))).unwrap().len(), 1);
        let (chosen, changed) = screen_new(std::slice::from_ref(&candidate), &Json::Arr(vec![]));
        assert!(changed);
        let first = &chosen.as_arr().unwrap()[0];
        assert!(first.field("automatic_activation").is_false());
        assert_eq!(first.field("proposal"), &candidate);
        let (_, again) = screen_new(&[candidate], &chosen);
        assert!(!again, "review queue must deduplicate");
    }
}

#[test]
fn rejects_forged_source_and_regex_mutations() {
    for proposal in [technical(), tv(), patch()] {
        for key in ["fingerprint", "source_build"] {
            assert!(!valid(&with(&proposal, key, Json::from("bad"))));
        }
    }
    assert!(!valid(&with(&patch(), "proposed_regex", Json::from("(?s)(.*)+"))));
    assert!(!valid(&with(&patch(), "handler_index", Json::Int(34))));
    assert!(!valid(&with(&tv(), "regex", Json::from("(?s)(.*)+"))));
    let native = with(&rules::tv_native_patterns("sxe").unwrap(), "episodes", Json::from("(?s)(.*)+"));
    assert!(!valid(&with(&tv(), "native_patterns", native)));
    assert!(!valid(&with(&tv(), "source_build", Json::from(format!("{SHA}:1")))));
}

#[test]
fn wrong_json_types_do_not_suppress_valid_sibling_rules() {
    let factories: [fn() -> Json; 5] = [technical, tv, patch, || review_discovery("network", "futuretag"), title_boundary];
    for (index, factory) in factories.iter().enumerate() {
        let candidate = factory();
        let good = if index == 0 { tv() } else { technical() };
        for (key, value) in candidate.as_obj().unwrap() {
            // The batch checksum deliberately requires every fingerprint
            // to be valid. Other malformed fields reject only their rule.
            if key == "fingerprint" || value.as_str().is_none() {
                continue;
            }
            for malformed in [json!(null), json!([]), json!({}), json!(7), json!(true)] {
                let bad = with(&candidate, key, j(malformed));
                assert!(!valid(&bad), "{key}");
                assert_eq!(parsed(&batch(&[bad, good.clone()])), Some(vec![good.clone()]), "{key}");
            }
        }
    }
}

fn corrective_candidate() -> Json {
    let handlers = handlers();
    let index = handlers
        .iter()
        .position(|row| {
            row.field("name").str_eq("episodes")
                && row.field("transform").str_eq("array(integer)")
                && row.field("options").field("remove").is_false()
                && row.field("options").field("skipIfAlreadyFound").is_true()
        })
        .unwrap();
    let pattern = rules::tv_native_pattern("sxe", "episodes").unwrap();
    merge(
        common_rule("learned_corrective_regex", rules::CORRECTIVE_EPOCH),
        json!({"handler_index": index, "field": "episodes", "family": "sxe", "signal": "corrective_sxe",
               "regex": pattern, "failure_classes": ["missing"], "review_only": true,
               "verification": "review_only_requires_safe_replay",
               "fingerprint": common::sha(&format!("corrective:sxe:{index}:false:{pattern}"))}),
    )
}

#[test]
fn corrective_failure_classes_require_strings() {
    let candidate = corrective_candidate();
    assert!(valid(&candidate));
    assert!(!valid(&with(&candidate, "source_build", Json::from(format!("{SHA}:1")))));
    for failures in [json!([[]]), json!([{}]), json!([null]), json!(["missing", 1])] {
        let bad = with(&candidate, "failure_classes", j(failures));
        assert!(!valid(&bad));
        assert_eq!(parsed(&batch(&[bad, technical()])), Some(vec![technical()]));
    }
    assert!(!valid(&with(&candidate, "failure_classes", j(json!(["over", "missing"])))));
}

#[test]
fn corrective_epoch_matches_native_producer() {
    let source = std::fs::read_to_string(repo_root().join("crates/matchlab-server/src/corrective_learning.rs")).unwrap();
    let epoch = regex::Regex::new(r"const\s+EPOCH\s*:\s*u32\s*=\s*(\d+)\s*;").unwrap();
    let found = epoch.captures(&source).expect("native corrective protocol epoch must be declared");
    assert_eq!(rules::CORRECTIVE_EPOCH, found[1].parse::<u32>().unwrap());
}

#[test]
fn any_parser_field_can_submit_discovery_for_review_only() {
    for field in ["title", "year", "group", "network", "languages", "seasons", "episodes", "quality", "audio"] {
        let candidate = review_discovery(field, "futuretag");
        assert!(valid(&candidate), "{field}");
        assert_eq!(parsed(&batch(std::slice::from_ref(&candidate))).unwrap().len(), 1);
        let (queued, added) = screen_new(&[candidate], &Json::Arr(vec![]));
        assert!(added);
        assert!(queued.as_arr().unwrap()[0].field("automatic_activation").is_false());
    }
    let base = review_discovery("network", "futuretag");
    assert!(!valid(&with(&base, "regex", Json::from("(?s)(.*)+"))));
    assert!(!valid(&with(&base, "canonical_confirmations", Json::Int(8))));
    assert!(!valid(&with(&base, "field", Json::from("imdb_id"))));
    assert!(!valid(&with(&base, "source_build", Json::from(format!("{SHA}:1")))));
}

#[test]
fn verified_title_boundary_proposals_are_review_only_and_private() {
    let candidate = title_boundary();
    assert!(valid(&candidate));
    assert_eq!(parsed(&batch(std::slice::from_ref(&candidate))).unwrap().len(), 1);
    let (queued, changed) = screen_new(std::slice::from_ref(&candidate), &Json::Arr(vec![]));
    assert!(changed);
    assert!(queued.as_arr().unwrap()[0].field("automatic_activation").is_false());
    for (key, value) in [
        ("regex", json!("(?s)(.*)+")),
        ("source_build", json!(format!("{SHA}:2"))),
        ("field", json!("imdb_id")),
        ("canonical_confirmations", json!(4)),
        ("verification", json!("automatically_activated")),
        ("signal", json!("underextracted_custom_marker")),
        ("fingerprint", json!("0".repeat(64))),
        ("evidence_count", json!(11)),
        ("distinct_titles", json!(3)),
    ] {
        assert!(!valid(&with(&candidate, key, j(value))), "{key}");
    }
    for private in ["raw_title", "imdb_id", "matched_title"] {
        assert!(candidate.get(private).is_none());
    }
}

#[test]
fn two_rules_are_allowed_one_pr_review_at_a_time() {
    let accepted = parsed(&batch(&[technical(), tv()])).unwrap();
    assert_eq!(accepted.len(), 2);
    let (first, changed) = screen_new(&accepted, &Json::Arr(vec![]));
    assert!(changed);
    assert_eq!(first.as_arr().unwrap().len(), 1);
    let (second, changed) = screen_new(&accepted, &first);
    assert!(changed);
    assert_eq!(second.as_arr().unwrap().len(), 2);
}

#[test]
fn offline_historical_backfill_is_review_only() {
    let old = j(json!({
        "kind": "historical_regex_review", "original_kind": "learned_field_regex",
        "fingerprint": common::sha("historical-codec-x266"), "source_build": format!("{LATEST}:5"),
        "field": "codec", "signal": "x266", "regex": r"(?i)(?:^|[ ._-])x266(?:$|[ ._-])",
        "evidence_count": 8, "distinct_titles": 4, "canonical_confirmations": 0, "review_only": true,
        "verification": "historical_archive_requires_independent_review",
    }));
    let catalog = catalog();
    assert!(rules::valid_extended_rule(&old, SHA, &yes, &catalog));
    assert_eq!(rules::parse_batch(&batch(std::slice::from_ref(&old)), SHA, &yes, &catalog).unwrap().len(), 1);
    let (proposal, changed) = screen(&Json::Arr(vec![old.clone()]), &Json::Arr(vec![]), None, &yes, &catalog).unwrap();
    assert!(changed);
    assert!(proposal.as_arr().unwrap()[0].field("automatic_activation").is_false());
    let (_, again) = screen(&Json::Arr(vec![old.clone()]), &proposal, None, &yes, &catalog).unwrap();
    assert!(!again);
    assert!(!rules::valid_extended_rule(&old, SHA, &no, &catalog));
    assert!(!rules::valid_extended_rule(&with(&old, "verification", Json::from("untrusted")), SHA, &yes, &catalog));
    for epoch in ["9".repeat(5000), "\u{0665}".to_owned()] {
        let forged = with(&old, "source_build", Json::from(format!("{LATEST}:{epoch}")));
        assert!(!rules::valid_extended_rule(&forged, SHA, &yes, &catalog));
    }
}

#[test]
fn no_untrusted_extra_fields_or_title_samples() {
    assert!(!valid(&with(&technical(), "samples", j(json!(["Private.Movie.2024"])))));
}

#[test]
fn missing_catalogue_fails_closed() {
    let missing = Catalog(None);
    assert!(!rules::valid_extended_rule(&patch(), SHA, &cl_git, &missing));
    assert!(rules::valid_extended_rule(&technical(), SHA, &cl_git, &missing));
}

#[test]
fn canonical_values_use_python_json_spelling() {
    let rule = |canonical: serde_json::Value, field: &str, signal: &str| {
        let token = dumps(&j(canonical.clone()), Dump::COMPACT_UTF8);
        merge(
            common_rule("learned_field_regex", rules::FIELD_EPOCH),
            json!({"verification": "local_canonical_and_explicit_release_marker", "field": field, "signal": signal,
                   "regex": technical_pattern_str(field, signal), "canonical": canonical,
                   "fingerprint": common::sha(&format!("community-field:{field}:{signal}:{token}"))}),
        )
    };
    assert!(valid(&rule(json!(["OPUS"]), "audio", "opus")));
    assert!(valid(&rule(json!(["en", "fr"]), "languages", "multi")));
    assert!(valid(&rule(json!(true), "subbed", "subs")));
    assert!(valid(&rule(json!(10), "bit_depth", "10bit")));
    assert!(!valid(&rule(json!(10.5), "bit_depth", "10bit")));
    assert!(!valid(&rule(json!(["OPUS"]), "codec", "opus")));
    assert!(!valid(&rule(json!("bad/value"), "codec", "opus")));
}
