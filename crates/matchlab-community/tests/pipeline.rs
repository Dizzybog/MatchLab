//! Native source planning, safe source diffs, decisions, backlog and PR gates.
mod common;

use chrono::{TimeZone, Utc};
use common::*;
use matchlab_community::apply::apply;
use matchlab_community::backlog;
use matchlab_community::decisions;
use matchlab_community::evidence;
use matchlab_community::json::{dumps, Dump, Json};
use matchlab_community::regex_plan::{build, resolve};
use matchlab_community::review::screen;
use matchlab_community::review_diff;
use matchlab_community::rule_intake::{self as rules, anchored_general, technical_pattern_str};
use serde_json::json;
use std::collections::BTreeSet;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn field_rule(field: &str, signal: &str, canonical: serde_json::Value) -> Json {
    let token = dumps(&j(canonical.clone()), Dump::COMPACT_UTF8);
    j(json!({
        "kind": "learned_field_regex", "field": field, "signal": signal, "canonical": canonical,
        "regex": technical_pattern_str(field, signal), "source_build": format!("{SHA}:{}", rules::FIELD_EPOCH),
        "evidence_count": 24, "distinct_titles": 8, "canonical_confirmations": 8,
        "verification": "local_canonical_and_explicit_release_marker",
        "fingerprint": rules::digest(&format!("community-field:{field}:{signal}:{token}")),
    }))
}

fn tv_rule(family: &str) -> Json {
    let rule = j(json!({
        "kind": "learned_tv_regex", "family": family, "fields": ["seasons", "episodes"],
        "regex": rules::tv_grammar(family), "source_build": format!("{SHA}:{}", rules::TV_EPOCH),
        "evidence_count": 24, "distinct_titles": 8, "canonical_confirmations": 8,
        "verification": "explicit_tv_marker_and_canonical_confirmations",
        "fingerprint": rules::digest(&format!("community-tv:{family}")),
    }));
    with(&rule, "native_patterns", rules::tv_native_patterns(family).unwrap())
}

fn general_rule(index: usize, mode: &str, regex: &str) -> Json {
    let field = handlers()[index].field("name").as_str().unwrap().to_owned();
    j(json!({
        "kind": "learned_general_regex_review", "field": field, "handler_index": index, "mode": mode,
        "regex": regex, "review_only": true, "source_build": format!("{SHA}:1"),
        "verification": "native_handler_metamorphic_replay_requires_upstream_review",
        "evidence_count": 24, "distinct_titles": 8, "canonical_confirmations": 8,
        "fingerprint": rules::digest(&format!("general:{field}:{index}:{mode}:{regex}")),
    }))
}

fn eligible(rule: &Json, changes: &[Json]) -> Json {
    let fields: BTreeSet<String> = changes.iter().map(|change| change.field("field").as_str().unwrap().to_owned()).collect();
    let audit = j(json!({
        "fingerprint": rule.field("fingerprint").as_str(), "status": "ready_for_source_diff",
        "promotion_eligible": true, "regressions": 0, "verified_improvements": changes.len().max(2),
        "distinct_verified_titles": 2, "verified_fixture_indexes": [0, 1], "verified_fields": fields,
    }));
    with(&audit, "source_changes", Json::Arr(changes.to_vec()))
}

fn reviewed(rule: &Json) -> Json {
    let entry = j(json!({"fingerprint": rule.field("fingerprint").as_str(), "status": "review_required",
                         "automatic_activation": false}));
    with(&entry, "proposal", rule.clone())
}

fn plan(rule: &Json) -> (Vec<Json>, &'static str) {
    resolve(rule, &handlers()).unwrap()
}

fn arr(items: &[Json]) -> Json {
    Json::Arr(items.to_vec())
}

// ---- native plans -------------------------------------------------------

#[test]
fn field_families_select_immutable_transform_template() {
    let handlers = handlers();
    for (field, marker, expected) in [
        ("audio", "opus", json!(["OPUS"])),
        ("codec", "h264", json!("avc")),
        ("quality", "bluray", json!("BluRay")),
        ("resolution", "1440p", json!("1440p")),
        ("extension", "mp4", json!("mp4")),
        ("languages", "english", json!(["en"])),
    ] {
        let rule = field_rule(field, marker, expected);
        let (changes, reason) = plan(&rule);
        assert_eq!(reason, "replay_ready", "{field}");
        assert_eq!(changes.len(), 1, "{field}");
        let op = &changes[0];
        assert!(op.field("operation").str_eq("add"));
        let index = op.field("handler_index").as_int().unwrap() as usize;
        assert!(handlers[index].field("name").str_eq(field));
        assert_eq!(op.field("transform"), handlers[index].field("transform"));
        if field == "extension" {
            assert!(op.field("pattern").str_eq(r"(?i)\.mp4$"));
        }
        let plans = build(&arr(&[rule.clone()]), &handlers, &catalog()).unwrap();
        assert_eq!(plans.as_obj().unwrap().keys().collect::<Vec<_>>(), vec![rule.field("fingerprint").as_str().unwrap()]);
    }
}

#[test]
fn native_tv_coordinates_stay_an_atomic_two_rule_change() {
    for family in rules::TV_FAMILIES {
        let (ops, reason) = plan(&tv_rule(family));
        assert_eq!(reason, "replay_ready");
        let fields: Vec<&str> = ops.iter().map(|op| op.field("field").as_str().unwrap()).collect();
        assert_eq!(fields, ["seasons", "episodes"]);
        assert!(ops.iter().all(|op| op.field("operation").str_eq("add")));
        let patterns: Vec<&str> = ops.iter().map(|op| op.field("pattern").as_str().unwrap()).collect();
        assert_eq!(
            patterns,
            [rules::tv_native_pattern(family, "seasons").unwrap(), rules::tv_native_pattern(family, "episodes").unwrap()]
        );
    }
}

#[test]
fn general_add_and_restricted_replacement() {
    let regex = r"(?i)(?:^|[ ._-])(Directors[ ._-]+Cut)(?:$|[ ._-])";
    let original = general_rule(80, "add", regex);
    assert!(anchored_general(regex));
    let (additions, why) = plan(&original);
    assert_eq!(why, "replay_ready");
    assert!(additions[0].field("operation").str_eq("add"));
    let (patches, why) = plan(&general_rule(80, "correct", regex));
    assert_eq!(why, "replay_ready");
    assert!(patches[0].field("operation").str_eq("patch"));
    assert!(patches[0].field("pattern").as_str().unwrap().starts_with("(?:"));
    assert!(plan(&with(&original, "handler_index", Json::Int(73))).0.is_empty());
}

#[test]
fn corrective_uses_existing_tv_source_and_never_guesses() {
    let handlers = handlers();
    let source = handlers
        .iter()
        .position(|row| {
            row.field("kind").str_eq("regex")
                && row.field("name").str_eq("episodes")
                && row.field("transform").str_eq("array(integer)")
                && row.field("options").field("remove").is_false()
                && row.field("options").field("skipIfAlreadyFound").is_true()
        })
        .unwrap();
    let rule = j(json!({"kind": "learned_corrective_regex", "field": "episodes", "handler_index": source,
                        "family": "sxe", "regex": rules::tv_native_pattern("sxe", "episodes")}));
    let (ops, reason) = plan(&rule);
    assert_eq!(reason, "replay_ready");
    assert!(ops[0].field("operation").str_eq("correct"));
    assert!(plan(&with(&rule, "handler_index", Json::Int(229))).0.is_empty());
}

#[test]
fn ambiguous_and_historical_proposals_are_review_only() {
    for rule in [
        json!({"kind": "learned_field_regex_review", "field": "network", "signal": "futuretag", "regex": "abc"}),
        json!({"kind": "historical_regex_review", "field": "audio"}),
        json!({"kind": "learned_general_regex_review", "field": "edition", "handler_index": 80,
               "regex": "(?s)(.*)+", "mode": "add"}),
    ] {
        assert!(plan(&j(rule)).0.is_empty());
    }
}

#[test]
fn invalid_untrusted_proposal_cannot_enter_plan() {
    let rule = field_rule("audio", "opus", json!(["OPUS"]));
    let altered = with(&rule, "regex", Json::from("(?s)(.*)+"));
    let result = build(&arr(&[altered]), &handlers(), &catalog()).unwrap();
    let entry = result.field(rule.field("fingerprint").as_str().unwrap());
    assert_eq!(entry.field("source_changes"), &Json::Arr(vec![]));
    assert!(entry.field("reason").str_eq("stale_or_invalid_queued_proposal"));
}

#[test]
fn plan_batch_limits_and_duplicates_fail_closed() {
    let rule = field_rule("audio", "opus", json!(["OPUS"]));
    assert!(build(&arr(&[rule.clone(), rule.clone()]), &handlers(), &catalog()).is_err());
    assert!(build(&arr(&vec![rule; 25]), &handlers(), &catalog()).is_err());
    assert!(build(&arr(&[j(json!({"fingerprint": "bad"}))]), &handlers(), &catalog()).is_err());
}

// ---- native review and source edits -------------------------------------

fn fixtures(field: &str) -> Json {
    j(json!([
        {"title": "Distinct.Show.S01E01.Opus.WEB-DL", "expected": {field: ["OPUS"]}},
        {"title": "Second.Movie.2025.Opus.mkv", "expected": {field: ["OPUS"]}},
    ]))
}

fn run_apply(after: &Json, audits: &Json, fixtures: &Json) -> Result<(String, Json, Option<Json>), String> {
    apply(&arr(&[]), after, audits, fixtures, &source(), &arr(&[]), &catalog())
}

#[test]
fn source_append_contains_exact_native_template_and_regressions() {
    let rule = field_rule("audio", "opus", json!(["OPUS"]));
    let (changes, _) = plan(&rule);
    let fixtures = fixtures("audio");
    let audit = eligible(&rule, &changes);
    let (reviewed_rules, changed) = screen(&arr(&[rule]), &arr(&[]), Some(&arr(&[audit.clone()])), &yes, &catalog()).unwrap();
    assert!(changed);
    let (amended, promoted, summary) = run_apply(&reviewed_rules, &arr(&[audit]), &fixtures).unwrap();
    assert_eq!(promoted.as_arr().unwrap().len(), 2);
    assert_eq!(summary.unwrap().field("fields"), &j(json!(["audio"])));
    let original = handlers();
    let parsed = Json::parse(&amended).unwrap().field("handlers").as_arr().unwrap().clone();
    assert_eq!(parsed[..original.len()], original[..]);
    assert_eq!(parsed.len(), original.len() + 1);
    assert_eq!(parsed.last().unwrap().field("pattern"), changes[0].field("pattern"));
    assert_eq!(parsed.last().unwrap().field("transform"), changes[0].field("transform"));
    // Byte-local: the original text is a strict prefix up to the appended handler.
    let original_text = source();
    let head = original_text.strip_suffix("\n  ]\n}\n").unwrap();
    assert!(amended.starts_with(&format!("{head},\n    {{\n      \"")));
}

#[test]
fn tv_source_appends_two_native_handlers_without_reformatting() {
    let rule = tv_rule("sxe");
    let (changes, _) = plan(&rule);
    let fixtures = j(json!([
        {"title": "First.Show.S04E02.WEB", "expected": {"seasons": [4], "episodes": [2]}},
        {"title": "Different.Show.S08E10.WEB", "expected": {"seasons": [8], "episodes": [10]}},
    ]));
    let audit = eligible(&rule, &changes);
    let (reviewed_rules, accepted) = screen(&arr(&[rule]), &arr(&[]), Some(&arr(&[audit.clone()])), &yes, &catalog()).unwrap();
    assert!(accepted);
    let (amended, _, summary) = run_apply(&reviewed_rules, &arr(&[audit]), &fixtures).unwrap();
    let rows = Json::parse(&amended).unwrap().field("handlers").as_arr().unwrap().clone();
    assert_eq!(rows.len(), handlers().len() + 2);
    let names: Vec<&str> = rows[rows.len() - 2..].iter().map(|row| row.field("name").as_str().unwrap()).collect();
    assert_eq!(names, ["seasons", "episodes"]);
    assert!(rows[rows.len() - 2..].iter().all(|row| row.field("learned_tv_guard").is_true()));
    assert_eq!(summary.unwrap().field("operations"), &j(json!(["add", "add"])));
}

#[test]
fn corrective_source_is_inserted_ahead_of_its_field() {
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
    let rule = j(json!({"kind": "learned_corrective_regex", "field": "episodes", "handler_index": index,
                        "family": "sxe", "regex": rules::tv_native_pattern("sxe", "episodes")}));
    let (changes, _) = plan(&rule);
    let merged = matchlab_community::apply::insert_native_handlers(&source(), &changes).unwrap();
    let rows = Json::parse(&merged).unwrap().field("handlers").as_arr().unwrap().clone();
    let first = handlers.iter().position(|row| row.field("name").str_eq("episodes")).unwrap();
    assert_eq!(rows.len(), handlers.len() + 1);
    assert!(rows[first].field("learned_tv_guard").is_true());
    assert_eq!(rows[first + 1..], handlers[first..]);
}

#[test]
fn forged_audit_plan_never_writes_a_source_diff() {
    let rule = field_rule("audio", "opus", json!(["OPUS"]));
    let (changes, _) = plan(&rule);
    let fixtures = fixtures("audio");
    let audit = eligible(&rule, &changes);
    let forged_change = with(&changes[0], "pattern", Json::from("(?s)(.*)+"));
    let bad = with(&audit, "source_changes", arr(&[forged_change]));
    assert!(run_apply(&arr(&[reviewed(&rule)]), &arr(&[bad]), &fixtures).is_err());
    let unproved = with(&audit, "verified_fields", arr(&[]));
    assert!(run_apply(&arr(&[reviewed(&rule)]), &arr(&[unproved]), &fixtures).is_err());
    let unresolved = with(&audit, "promotion_eligible", Json::Bool(false));
    assert!(run_apply(&arr(&[reviewed(&rule)]), &arr(&[unresolved]), &fixtures).unwrap().2.is_none());
}

// ---- legacy single source-regex patch -----------------------------------

fn fixture_candidate() -> (String, Json, Json, Json) {
    let original = source();
    let handlers = handlers();
    let index = handlers
        .iter()
        .position(|row| row.field("name").str_eq("codec") && row.field("pattern").str_eq(r"\b[hx][\. \-]?264\b"))
        .unwrap();
    let native = &handlers[index];
    let pattern = native.field("pattern").as_str().unwrap();
    let replacement = format!("(?:{pattern})|(?:{})", technical_pattern_str("codec", "h264").unwrap());
    let rule = j(json!({
        "kind": "learned_source_regex_patch", "fingerprint": "a".repeat(64), "field": "codec",
        "handler_index": index, "signal": "h264", "original_regex": pattern, "proposed_regex": replacement,
        "transform": native.field("transform").as_str(),
    }));
    let entry = reviewed(&rule);
    let audit = j(json!({"fingerprint": "a".repeat(64), "status": "ready_for_source_diff", "promotion_eligible": true,
                         "regressions": 0, "verified_improvements": 2, "verified_fixture_indexes": [0, 1]}));
    let fixtures = j(json!([
        {"title": "Fixture.Show.S01E01.h264.WEB-DL", "expected": {"codec": "avc"}},
        {"title": "Fixture.Show.S01E02.h264.WEB-DL", "expected": {"codec": "avc"}},
    ]));
    (original, entry, audit, fixtures)
}

fn proposal_of(entry: &Json) -> Json {
    entry.field("proposal").clone()
}

#[test]
fn single_handler_source_diff_keeps_all_other_json_identical() {
    let (source_text, entry, audit, fixtures) = fixture_candidate();
    let (amended, tests, summary) = run_apply(&arr(&[entry.clone()]), &arr(&[audit]), &fixtures).unwrap();
    let summary = summary.unwrap();
    let before = Json::parse(&source_text).unwrap().field("handlers").as_arr().unwrap().clone();
    let after = Json::parse(&amended).unwrap().field("handlers").as_arr().unwrap().clone();
    assert_eq!(before.len(), after.len());
    let changed: Vec<usize> = (0..before.len()).filter(|i| before[*i] != after[*i]).collect();
    assert_eq!(changed, [proposal_of(&entry).field("handler_index").as_int().unwrap() as usize]);
    assert_eq!(tests.as_arr().unwrap().len(), 2);
    assert_eq!(summary.field("verified_improvements"), &Json::Int(2));
    // Exactly ONE line of the generated catalogue is modified.
    let old_lines: Vec<&str> = source_text.lines().collect();
    let new_lines: Vec<&str> = amended.lines().collect();
    assert_eq!(old_lines.len(), new_lines.len());
    assert_eq!(old_lines.iter().zip(&new_lines).filter(|(a, b)| a != b).count(), 1);
}

#[test]
fn without_two_independent_labels_never_writes_source() {
    let (source_text, entry, audit, fixtures) = fixture_candidate();
    for (key, value) in [
        ("verified_improvements", json!(1)),
        ("regressions", json!(1)),
        ("promotion_eligible", json!(false)),
        ("status", json!("needs_independent_evidence")),
    ] {
        let other = with(&audit, key, j(value));
        let (output, tests, summary) = run_apply(&arr(&[entry.clone()]), &arr(&[other]), &fixtures).unwrap();
        assert_eq!(output, source_text);
        assert_eq!(tests, arr(&[]));
        assert!(summary.is_none());
    }
    let forged = with(&audit, "verified_fixture_indexes", j(json!([0, 0])));
    assert!(run_apply(&arr(&[entry]), &arr(&[forged]), &fixtures).is_err());
}

#[test]
fn a_rule_without_source_identity_is_never_merged() {
    let (_, entry, audit, fixtures) = fixture_candidate();
    let rule = with(&proposal_of(&entry), "kind", Json::from("historical_regex_review"));
    let entry = with(&entry, "proposal", rule);
    assert!(run_apply(&arr(&[entry]), &arr(&[audit]), &fixtures).unwrap().2.is_none());
}

#[test]
fn invalid_source_original_fails_closed() {
    let (_, entry, audit, fixtures) = fixture_candidate();
    let rule = with(&proposal_of(&entry), "original_regex", Json::from(".*"));
    assert!(run_apply(&arr(&[with(&entry, "proposal", rule)]), &arr(&[audit]), &fixtures).is_err());
}

#[test]
fn no_new_review_entry_does_not_regenerate_code() {
    let (source_text, entry, audit, fixtures) = fixture_candidate();
    let result = apply(&arr(&[entry.clone()]), &arr(&[entry]), &arr(&[audit]), &fixtures, &source_text, &arr(&[]), &catalog());
    assert_eq!(result.unwrap(), (source_text, arr(&[]), None));
}

fn integrated_proposal() -> (String, Json, Json, Json) {
    let (source_text, entry, audit, fixtures) = fixture_candidate();
    let rule = proposal_of(&entry);
    let index = rule.field("handler_index").as_int().unwrap();
    let original = rule.field("original_regex").as_str().unwrap();
    let proposed = rule.field("proposed_regex").as_str().unwrap();
    let mut proposal = rule.clone();
    for (key, value) in [
        ("evidence_count", json!(24)),
        ("distinct_titles", json!(8)),
        ("canonical_confirmations", json!(8)),
        ("source_build", json!(format!("{SHA}:{}", rules::FIELD_EPOCH))),
        ("verification", json!("source_catalog_and_canonical_release_markers")),
        ("fingerprint", json!(rules::digest(&format!("community-patch:{index}:{original}:{proposed}")))),
    ] {
        proposal = with(&proposal, key, j(value));
    }
    let audit = with(&with(&audit, "fingerprint", proposal.field("fingerprint").clone()), "distinct_verified_titles", Json::Int(2));
    (source_text, proposal, audit, fixtures)
}

#[test]
fn validated_rule_is_reviewed_and_stages_only_verified_source_patch() {
    let (source_text, proposal, audit, fixtures) = integrated_proposal();
    assert!(rules::valid_extended_rule(&proposal, SHA, &yes, &catalog()));
    let audits = arr(&[audit.clone()]);
    let (reviewed_rules, changed) = screen(&arr(&[proposal.clone()]), &arr(&[]), Some(&audits), &yes, &catalog()).unwrap();
    assert!(changed);
    assert_eq!(reviewed_rules.as_arr().unwrap().len(), 1);
    assert!(reviewed_rules.as_arr().unwrap()[0].field("automatic_activation").is_false());
    let (amended, tests, summary) = run_apply(&reviewed_rules, &audits, &fixtures).unwrap();
    assert!(summary.is_some());
    assert_eq!(tests, fixtures);
    let before = Json::parse(&source_text).unwrap().field("handlers").as_arr().unwrap().clone();
    let after = Json::parse(&amended).unwrap().field("handlers").as_arr().unwrap().clone();
    let index = proposal.field("handler_index").as_int().unwrap() as usize;
    assert_eq!((0..before.len()).filter(|i| before[*i] != after[*i]).collect::<Vec<_>>(), [index]);
    assert_eq!(after[index].field("pattern"), proposal.field("proposed_regex"));
    // Already-reviewed suggestions and proven no-ops do not open PRs.
    assert_eq!(
        screen(&arr(&[proposal.clone()]), &reviewed_rules, Some(&audits), &yes, &catalog()).unwrap(),
        (reviewed_rules.clone(), false)
    );
    let noop = arr(&[with(&audit, "status", Json::from("no_effect"))]);
    assert_eq!(screen(&arr(&[proposal]), &arr(&[]), Some(&noop), &yes, &catalog()).unwrap(), (arr(&[]), false));
}

#[test]
fn verified_candidate_is_not_starved_by_unverified_rules() {
    let (_, rule, audit, _) = integrated_proposal();
    let unverified = j(json!({"kind": "learned_field_regex", "field": "quality", "fingerprint": "f".repeat(64),
                              "signal": "bluray", "regex": "review-only", "source_build": format!("{SHA}:5")}));
    let audits = arr(&[j(json!({"fingerprint": "f".repeat(64), "status": "review_only"})), audit]);
    let (result, selected) = screen(&arr(&[unverified, rule.clone()]), &arr(&[]), Some(&audits), &yes, &catalog()).unwrap();
    assert!(selected);
    assert_eq!(result.as_arr().unwrap()[0].field("fingerprint"), rule.field("fingerprint"));
}

#[test]
fn workflow_promotes_only_after_trusted_rust_audit_and_tests() {
    let workflow = std::fs::read_to_string(repo_root().join(".github/workflows/community-learning.yml")).unwrap();
    let position = |needle: &str| workflow.find(needle).unwrap_or_else(|| panic!("{needle} missing"));
    let order = [
        position("--example audit_community_regex"),
        position("matchlab-community review "),
        position("matchlab-community apply "),
        position("RUSTFLAGS=\"-D warnings\" cargo test --workspace"),
        position("gh pr create --repo"),
    ];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    assert!(workflow.contains("if: env.COMMUNITY_RULE_CHANGED == 'true'"));
    assert!(!workflow.contains("gh pr merge"));
    assert!(!workflow.contains("python3"), "the workflow must not depend on Python");
    let review = std::fs::read_to_string(repo_root().join("crates/matchlab-community/src/review.rs")).unwrap();
    assert!(review.contains("automatic_activation"));
}

// ---- backlog ------------------------------------------------------------

fn queued(marker: &str) -> Json {
    j(json!({"kind": "historical_regex_review", "fingerprint": common::sha(marker), "regex": marker}))
}

fn regexes(value: &Json) -> Vec<String> {
    value.field("pending").as_arr().unwrap().iter().map(|row| row.field("regex").as_str().unwrap().to_owned()).collect()
}

#[test]
fn offline_history_is_deduped_against_pending_and_curated() {
    let old = j(json!({"pending": []}));
    let old = with(&old, "pending", arr(&[queued("one"), queued("two")]));
    let done: BTreeSet<String> = [common::sha("one")].into();
    let merged = backlog::merge(&old, &arr(&[queued("two"), queued("three")]), &done, &BTreeSet::new()).unwrap();
    assert_eq!(regexes(&merged), ["two", "three"]);
}

#[test]
fn failed_pr_does_not_ack_and_rotates_unreviewed_items() {
    let queue = with(&j(json!({})), "pending", arr(&[queued("a"), queued("b"), queued("c")]));
    let attempted = [Some(common::sha("a"))];
    assert_eq!(regexes(&backlog::finalize(&queue, &attempted, &[]).unwrap()), ["b", "c", "a"]);
    assert_eq!(regexes(&backlog::finalize(&queue, &attempted, &[common::sha("a")]).unwrap()), ["b", "c"]);
}

#[test]
fn capacity_failure_never_truncates_queue() {
    let rows: Vec<Json> = (0..=backlog::MAX_PENDING).map(|i| queued(&i.to_string())).collect();
    let old = with(&j(json!({})), "pending", Json::Arr(rows));
    assert!(backlog::merge(&old, &arr(&[]), &BTreeSet::new(), &BTreeSet::new()).is_err());
}

#[test]
fn merged_anime_and_extended_fingerprints_are_globally_known() {
    let curated = backlog::curated_fingerprints(&j(json!([{"marker": "epis"}])), &arr(&[queued("z")]));
    assert!(curated.contains(&common::sha("community-regex:epis")));
    assert!(curated.contains(&common::sha("z")));
}

#[test]
fn accepted_fingerprints_must_be_hex() {
    assert!(backlog::parse_accepted(&j(json!(["bad"]))).is_err());
    assert_eq!(backlog::parse_accepted(&j(json!([common::sha("a")]))).unwrap(), [common::sha("a")]);
}

// ---- decision cache -----------------------------------------------------

struct DecisionFixture {
    temp: Temp,
    handlers: std::path::PathBuf,
    fixtures: std::path::PathBuf,
    sign: String,
}

fn decision_fixture() -> DecisionFixture {
    let temp = Temp::new();
    let handlers = temp.file("handlers.json", r#"{"handlers":[]}"#);
    let fixtures = temp.file("fixtures.json", "[]");
    let sign = decisions::signature(&handlers, &fixtures, None).unwrap();
    DecisionFixture { temp, handlers, fixtures, sign }
}

fn decision_state(fp: &str, status: &str, sign: &str) -> Json {
    j(json!({"decisions": {fp: {"status": status, "signature": sign, "checked_at": "2026-11-11T12:00:00+00:00"}}}))
}

#[test]
fn same_evidence_is_cached_but_changed_source_or_fixtures_rechecks() {
    let fixture = decision_fixture();
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let pending = j(json!([{"fingerprint": a}, {"fingerprint": b}]));
    let state = decision_state(&a, "no_effect", &fixture.sign);
    let (due, deferred) = decisions::due(&pending, &state, &fixture.sign, 24).unwrap();
    assert_eq!(due.iter().map(|row| row.field("fingerprint").as_str().unwrap()).collect::<Vec<_>>(), [b.as_str()]);
    assert_eq!(deferred, 1);
    std::fs::write(&fixture.fixtures, r#"[{"title":"new trusted fixture"}]"#).unwrap();
    let next = decisions::signature(&fixture.handlers, &fixture.fixtures, None).unwrap();
    assert_ne!(fixture.sign, next);
    let (due, deferred) = decisions::due(&pending, &state, &next, 24).unwrap();
    assert_eq!((due.len(), deferred), (2, 0));
}

#[test]
fn parser_source_tree_changes_invalidate_cached_verdicts() {
    let fixture = decision_fixture();
    let native = fixture.temp.0.join("parser");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(native.join("lib.rs"), "stable lib").unwrap();
    let before = decisions::signature(&fixture.handlers, &fixture.fixtures, Some(&native)).unwrap();
    std::fs::write(native.join("prefilter.rs"), "new prefilter").unwrap();
    let after = decisions::signature(&fixture.handlers, &fixture.fixtures, Some(&native)).unwrap();
    assert_ne!(before, after);
    std::fs::write(native.join("prefilter.rs"), "modified prefilter").unwrap();
    assert_ne!(after, decisions::signature(&fixture.handlers, &fixture.fixtures, Some(&native)).unwrap());
}

#[test]
fn signature_matches_the_python_definition() {
    let fixture = decision_fixture();
    let native = fixture.temp.0.join("parser");
    std::fs::create_dir_all(native.join("sub")).unwrap();
    std::fs::write(native.join("lib.rs"), "lib").unwrap();
    std::fs::write(native.join("sub/a.rs"), "a").unwrap();
    std::fs::write(native.join("notes.txt"), "ignored").unwrap();
    let expected = common::sha(&format!(
        "{}\0{}\0{}",
        r#"{"handlers":[]}"#,
        "[]",
        "lib.rs\0lib\0sub/a.rs\0a\0"
    ));
    assert_eq!(decisions::signature(&fixture.handlers, &fixture.fixtures, Some(&native)).unwrap(), expected);
}

#[test]
fn all_nonpromoted_decisions_are_cached_and_review_ready_is_not() {
    let fixture = decision_fixture();
    let a = "a".repeat(64);
    let pending = j(json!([{"fingerprint": a}]));
    for status in decisions::DECISIONS {
        let state = decision_state(&a, status, &fixture.sign);
        let (due, deferred) = decisions::due(&pending, &state, &fixture.sign, 24).unwrap();
        if *status == "ready_for_source_diff" {
            assert_eq!(due.len(), 1);
        } else {
            assert_eq!((due.len(), deferred), (0, 1), "{status}");
        }
    }
}

#[test]
fn update_keeps_pending_only_and_never_trusts_arbitrary_status() {
    let fixture = decision_fixture();
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let pending = j(json!([{"fingerprint": a}]));
    let before = decision_state(&b, "no_effect", &fixture.sign);
    let audit = j(json!([{"fingerprint": a, "status": "needs_independent_evidence"}]));
    let after = decisions::update(&before, &audit, &pending, &fixture.sign, "2026-11-11T13:00:00+00:00").unwrap();
    assert_eq!(after.field("decisions").as_obj().unwrap().keys().collect::<Vec<_>>(), [&a]);
    assert!(after.field("decisions").field(&a).field("signature").str_eq(&fixture.sign));
    let forged = j(json!([{"fingerprint": a, "status": "merge_without_review"}]));
    assert!(decisions::update(&before, &forged, &pending, &fixture.sign, "now").is_err());
    let duplicate = j(json!([{"fingerprint": a, "status": "no_effect"}, {"fingerprint": a, "status": "no_effect"}]));
    assert!(decisions::update(&before, &duplicate, &pending, &fixture.sign, "now").is_err());
}

// ---- evidence -----------------------------------------------------------

#[test]
fn real_title_only_with_same_batch_and_matching_source_marker() {
    let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
    let candidate = j(json!({"fingerprint": "a".repeat(64), "kind": "learned_field_regex", "signal": "opus", "field": "audio"}));
    let unrelated = j(json!({"fingerprint": "b".repeat(64), "kind": "learned_field_regex", "signal": "opus", "field": "audio"}));
    let reports: Vec<Json> = [
        "Some.Show.S01E01.Opus.720p",
        "Another.Movie.2024.x265.mkv",
        "Private.Show.token=secret.Opus",
        "Some.Show.S01E01.Opus.720p",
    ]
    .iter()
    .map(|title| j(json!({"title": title})))
    .collect();
    let rules_of = |comment: &Json, _: &str| {
        Some(vec![if comment.field("id") == &Json::Int(1) { candidate.clone() } else { unrelated.clone() }])
    };
    let reports_of = |_: &Json, _: &str| Some(reports.clone());
    let comments = j(json!([[{"id": 1, "created_at": now.to_rfc3339()}, {"id": 2, "created_at": now.to_rfc3339()}]]));
    let result = evidence::gather(&comments, &arr(&[candidate.clone()]), SHA, now, &rules_of, &reports_of);
    assert_eq!(result.field(&"a".repeat(64)), &j(json!(["Some.Show.S01E01.Opus.720p"])));
}

#[test]
fn regex_injection_and_bad_source_are_never_compiled() {
    assert!(evidence::marker_regex(&j(json!({"kind": "learned_field_regex", "signal": "(.*)+"}))).is_none());
    assert!(evidence::marker_regex(&j(json!({"kind": "learned_general_regex_review", "regex": "(.*)+"}))).is_none());
    assert!(evidence::marker_regex(&j(json!({"kind": "learned_tv_regex", "family": "attacker"}))).is_none());
    let general = j(json!({"kind": "learned_general_regex_review",
                           "regex": r"(?i)(?:^|[ ._-])(Directors[ ._-]+Cut)(?:$|[ ._-])"}));
    assert!(evidence::marker_regex(&general).unwrap().is_match("Movie.Directors.Cut.2020"));
}

#[test]
fn no_rules_never_processes_reports() {
    let now = Utc::now();
    let result = evidence::gather(&arr(&[]), &arr(&[]), SHA, now, &|_, _| Some(vec![]), &|_, _| Some(vec![]));
    assert_eq!(result, j(json!({})));
}

// ---- review diff --------------------------------------------------------

fn diff_files(temp: &Temp, texts: [&str; 4]) -> Vec<std::path::PathBuf> {
    texts.iter().enumerate().map(|(index, text)| temp.file(&format!("{index}.json"), text)).collect()
}

#[test]
fn empty_json_formatting_does_not_create_pr() {
    let temp = Temp::new();
    let paths = diff_files(&temp, ["[\n]\n", "[]\n", "[]\n", "[\n]\n"]);
    assert!(!review_diff::check(&paths[0], &paths[1], &paths[2], &paths[3]).unwrap());
    assert_eq!(std::fs::read(&paths[1]).unwrap(), std::fs::read(&paths[0]).unwrap());
    assert_eq!(std::fs::read(&paths[3]).unwrap(), std::fs::read(&paths[2]).unwrap());
}

#[test]
fn reordered_json_object_keys_are_semantically_equal() {
    let temp = Temp::new();
    let paths = diff_files(&temp, [r#"[{"a":1,"b":2}]"#, r#"[{"b":2,"a":1}]"#, "[]", "[]"]);
    assert!(!review_diff::check(&paths[0], &paths[1], &paths[2], &paths[3]).unwrap());
}

#[test]
fn real_review_data_creates_pr_and_preserves_unmodified_rule_file() {
    let temp = Temp::new();
    let paths = diff_files(&temp, ["[\n]\n", "[]\n", "[]", r#"[{"fingerprint":"ok"}]"#]);
    assert!(review_diff::check(&paths[0], &paths[1], &paths[2], &paths[3]).unwrap());
    assert_eq!(std::fs::read(&paths[1]).unwrap(), std::fs::read(&paths[0]).unwrap());
    assert_eq!(matchlab_community::util::read_json(&paths[3]).unwrap(), j(json!([{"fingerprint": "ok"}])));
}

#[test]
fn bot_requires_real_source_and_promoted_regression() {
    let temp = Temp::new();
    let old_handler = temp.file("handler-old.json", r#"{"regex":"old"}"#);
    let new_handler = temp.file("handler-new.json", r#"{"regex":"old"}"#);
    let old_cases = temp.file("cases-old.json", "[]");
    let new_cases = temp.file("cases-new.json", "[]");
    assert!(!review_diff::source_proven(&old_handler, &new_handler, &old_cases, &new_cases).unwrap());
    std::fs::write(&new_handler, r#"{"regex":"better"}"#).unwrap();
    assert!(review_diff::source_proven(&old_handler, &new_handler, &old_cases, &new_cases).is_err());
    std::fs::write(&new_cases, r#"[{"title":"Example.Show.S01E01"}]"#).unwrap();
    assert!(review_diff::source_proven(&old_handler, &new_handler, &old_cases, &new_cases).unwrap());
    std::fs::write(&old_cases, r#"[{"title":"Old.Show.S01E01"}]"#).unwrap();
    assert!(review_diff::source_proven(&old_handler, &new_handler, &old_cases, &new_cases).is_err());
}

#[test]
fn unparseable_review_json_fails_closed() {
    let temp = Temp::new();
    let paths = diff_files(&temp, ["[]", "not-json", "[]", "[]"]);
    assert!(review_diff::check(&paths[0], &paths[1], &paths[2], &paths[3]).is_err());
}

// ---- inline workflow steps ---------------------------------------------

#[test]
fn pr_report_lists_counters_and_never_titles() {
    use matchlab_community::workflow::pr_report;
    let audits = j(json!([{
        "fingerprint": "0123456789abcdef", "kind": "learned_field_regex", "status": "ready_for_source_diff",
        "verified_improvements": 3, "synthetic_changes": 1, "regressions": 0, "community_title_cases": 4,
        "community_title_changes": 2, "source_changes": [{"operation": "add"}, {}], "reason": "x".repeat(200),
        "titles": ["Private.Title.2024"],
    }]));
    let report = pr_report(&audits, &j(json!({"source_handler_indexes": [3, 5]})));
    assert!(report.starts_with("\n### Native Rust regex audit\n- 0123456789ab (learned_field_regex): ready_for_source_diff; "));
    assert!(report.contains("real cases=4, changed=2, verified gains=3, synthetic changes=1, regressions=0, operations=['add', '?']; "));
    assert!(report.contains(&format!("reason={}\n", "x".repeat(160))));
    assert!(report.ends_with("\nSource merge proposal: handler [3, 5]; trusted fixtures promoted to Rust regression tests.\n"));
    assert!(!report.contains("Private.Title"));
    assert_eq!(pr_report(&arr(&[]), &Json::Null), "\n### Native Rust regex audit\n");
}

#[test]
fn accepted_fingerprints_cover_new_anime_and_extended_rules() {
    use matchlab_community::workflow::accepted_fingerprints;
    let accepted = accepted_fingerprints(
        &j(json!([{"marker": "old"}])),
        &j(json!([{"marker": "old"}, {"marker": "epis"}])),
        &j(json!([{"fingerprint": "f1"}])),
        &j(json!([{"fingerprint": "f1"}, {"fingerprint": "f2"}])),
    )
    .unwrap();
    assert_eq!(accepted, [common::sha("community-regex:epis"), "f2".to_owned()]);
}
