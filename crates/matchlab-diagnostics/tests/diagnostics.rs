//! Offline checks for the HTTP auditor and the title-test collector.
use matchlab_community::json::{Json, Object};
use matchlab_diagnostics::audit_api::{self as audit, Settings, Transport};
use matchlab_diagnostics::title_tests::{self as collector, Reader};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

fn j(value: serde_json::Value) -> Json {
    Json::from(&value)
}

fn reply(body: serde_json::Value, status: i128, headers: serde_json::Value) -> Json {
    let headers: Object = j(headers).as_obj().cloned().unwrap_or_default();
    audit::response_data(Some(status), headers, body.to_string().as_bytes(), Duration::from_micros(2000), None)
}

fn ok(body: serde_json::Value) -> Json {
    reply(body, 200, json!({}))
}

struct Fake {
    responses: std::vec::IntoIter<Json>,
    calls: Vec<String>,
}

impl Fake {
    fn new(responses: Vec<Json>) -> Fake {
        Fake { responses: responses.into_iter(), calls: Vec::new() }
    }
}

impl Transport for Fake {
    fn name(&self) -> &'static str {
        "offline_fake"
    }

    fn request(&mut self, url: &str, _: &Json, _: f64, _: usize) -> Json {
        self.calls.push(url.to_owned());
        self.responses.next().expect("unexpected request")
    }
}

fn case(id: &str, expected: serde_json::Value, extra: serde_json::Value) -> Json {
    let mut row = j(json!({"id": id, "request": {"title": "Example.S01E01"}, "expected": expected}));
    if let (Json::Obj(map), Some(extra)) = (&mut row, j(extra).as_obj()) {
        map.extend(extra.clone());
    }
    row
}

fn run(cases: &[Json], transport: &mut Fake, delay: f64, waits: &mut Vec<f64>) -> Json {
    let settings = Settings { delay, ..Settings::default() };
    let mut sleep = |seconds: f64| waits.push(seconds);
    audit::run_audit(cases, "https://example.invalid", &settings, transport, &mut sleep, &mut |_, _, _| {})
}

fn result(report: &Json, index: usize) -> &Json {
    &report.field("results").as_arr().unwrap()[index]
}

#[test]
fn partial_objects_exact_arrays_boolean_and_missing_null() {
    let diff = |expected: serde_json::Value, actual: serde_json::Value| {
        let mut out = Vec::new();
        audit::compare_subset(&j(expected), Some(&j(actual)), "$", &mut out);
        out
    };
    assert!(diff(json!({"a": {"b": [1, true]}}), json!({"a": {"b": [1, true], "other": 3}})).is_empty());
    assert!(!diff(json!({"x": null}), json!({})).is_empty());
    assert!(!diff(json!({"x": true}), json!({"x": 1})).is_empty());
    assert!(!diff(json!({"a": [1]}), json!({"a": [1, 2]})).is_empty());
}

#[test]
fn source_assertions_preserve_missing_vs_null_and_coercions() {
    let assertions = j(json!([
        {"path": ["missing"], "op": "absent"},
        {"path": ["missing"], "op": "ne", "value": null},
        {"path": ["items"], "op": "eq", "coercion": "len", "value": 2},
        {"path": ["items", 0], "op": "eq", "value": 4},
    ]));
    let assertions = assertions.as_arr().unwrap();
    assert!(audit::compare_assertions(assertions, &j(json!({"items": [4, 5]}))).is_empty());
    assert_eq!(audit::compare_assertions(assertions, &j(json!({"missing": null, "items": [4, 5]}))).len(), 2);
    let object = j(json!([{"path": ["object"], "op": "eq", "value": {"one": 1}}]));
    assert!(!audit::compare_assertions(object.as_arr().unwrap(), &j(json!({"object": {"one": 1, "two": 2}}))).is_empty());
}

#[test]
fn parser_adapter_and_unsupported_translation_never_calls_api() {
    let inputs = j(json!({"cases": [
        {"title": "Example", "translate_languages": false,
         "expected": [{"path": ["title"], "op": "eq", "value": "Example"}],
         "sources": [{"file": "test.rs", "line": 12}]},
        {"title": "English", "translate_languages": true, "expected": []},
    ]}));
    let cases = audit::validate_cases(&inputs).unwrap();
    assert!(cases[0].field("id").as_str().unwrap().starts_with("parser-"));
    let mut transport = Fake::new(vec![ok(json!({"title": "Example"}))]);
    let report = run(&cases, &mut transport, 0.0, &mut Vec::new());
    assert_eq!(transport.calls, ["https://example.invalid/parse"]);
    assert_eq!(report.field("summary").field("assertions").field("passed"), &Json::Int(1));
    assert!(result(&report, 1).field("outcome").str_eq("unsupported_configuration"));
    assert_eq!(report.field("summary").field("identifier_metrics"), &j(json!({})));
}

#[test]
fn rate_limit_retry_honors_delay_and_other_errors_are_not_retried() {
    let mut waits = Vec::new();
    let mut transport = Fake::new(vec![
        reply(json!({}), 429, json!({"retry-after": "2"})),
        ok(json!({"title": "Example"})),
        reply(json!({}), 503, json!({})),
    ]);
    let cases = [case("one", json!({"title": "Example"}), json!({})), case("two", json!(null), json!({}))];
    let report = run(&cases, &mut transport, 0.15, &mut waits);
    assert!(waits.contains(&2.0));
    assert_eq!(transport.calls.len(), 3);
    assert!(result(&report, 0).field("outcome").str_eq("matched_expectation"));
    assert!(result(&report, 1).field("outcome").str_eq("http_error"));
    assert_eq!(report.field("summary").field("latency_ms"), &j(json!({"p50": 2.0, "p95": 2.0})));
    assert_eq!(audit::retry_after(Some("Thu, 01 Jan 1970 00:01:00 GMT"), 10.0), Some(50.0));
    assert_eq!(audit::retry_after(Some("-1"), 0.0), None);
}

#[test]
fn large_retry_after_stops_remaining_requests() {
    let mut transport = Fake::new(vec![reply(json!({}), 429, json!({"retry-after": "120"}))]);
    let report = run(&[case("one", json!(null), json!({})), case("two", json!(null), json!({}))], &mut transport, 0.15, &mut Vec::new());
    assert_eq!(transport.calls.len(), 1);
    assert!(result(&report, 0).field("outcome").str_eq("rate_limited"));
    assert!(result(&report, 1).field("outcome").str_eq("not_run_rate_limit"));
}

#[test]
fn metrics_exclude_partial_reference_and_unknown_expectations() {
    let positive = json!({"sid_start": "tt1234567:1:1", "sid_end": "tt1234567:1:1"});
    let negative = json!({"sid_start": null, "sid_end": null});
    let wrong = json!({"sid_start": "tt7654321:1:1", "sid_end": "tt7654321:1:1"});
    let labeled = json!({"classification": "labeled"});
    let cases = [
        case("true", positive.clone(), labeled.clone()),
        case("wrong", positive.clone(), labeled.clone()),
        case("abstain", negative.clone(), labeled.clone()),
        case("false-positive", negative.clone(), labeled.clone()),
        case("partial", json!({"sid_start": "tt1234567:1:1"}), labeled.clone()),
        case("reference", positive.clone(), json!({"classification": "differential_reference"})),
        case("unknown", json!(null), json!({})),
        case("unavailable", positive.clone(), labeled),
    ];
    let mut responses: Vec<Json> =
        [&positive, &wrong, &negative, &wrong, &positive, &wrong, &wrong].iter().map(|body| ok((*body).clone())).collect();
    responses.push(reply(json!({}), 503, json!({})));
    let mut transport = Fake::new(responses);
    let report = run(&cases, &mut transport, 0.0, &mut Vec::new());
    let metrics = report.field("summary").field("identifier_metrics").field("sid");
    for (key, value) in [("labeled_cases", 5), ("true_matches", 1), ("wrong_ids", 2), ("correct_abstentions", 1), ("unavailable", 1)] {
        assert_eq!(metrics.field(key), &Json::Int(value), "{key}");
    }
    assert!((metrics.field("precision").as_number().unwrap() - 1.0 / 3.0).abs() < 1e-12);
    assert!((metrics.field("match_recall").as_number().unwrap() - 1.0 / 3.0).abs() < 1e-12);
}

#[test]
fn cloudflare_block_is_transport_outcome() {
    let mut transport = Fake::new(vec![reply(json!({"error": "error code: 1010"}), 403, json!({"server": "cloudflare"}))]);
    let report = run(&[case("one", json!({"title": "Example"}), json!({}))], &mut transport, 0.0, &mut Vec::new());
    assert!(result(&report, 0).field("outcome").str_eq("transport_blocked"));
    assert!(result(&report, 0).field("comparison").is_null());
}

#[test]
fn curl_transport_uses_argument_list_and_bounds_response() {
    let curl = audit::Curl { binary: "curl".into() };
    let paths = |name: &str| PathBuf::from(format!("/tmp/x/{name}"));
    let arguments = curl.arguments("https://example.invalid/parse", &paths("request.json"), &paths("headers"), &paths("body"), 20.0, 5);
    assert!(!arguments.iter().any(|arg| arg == "--insecure" || arg == "--user-agent"));
    assert_eq!(arguments.last().unwrap(), "https://example.invalid/parse");
    assert!(arguments.windows(2).any(|pair| pair == ["--max-filesize", "5"]));
    let headers = "HTTP/1.1 200 Connection established\r\n\r\nHTTP/2 200\r\nContent-Type: application/json\r\n\r\n";
    let result = audit::Curl::finish(Some(63), br#"{"http_code":200}"#, b"too large", Some(headers.into()), b"0123456789".to_vec(), 5, Duration::from_millis(3));
    assert!(result.field("error").field("kind").str_eq("response_too_large"));
    assert!(result.field("raw_body").str_eq("01234"));
    assert!(result.field("headers").field("content-type").str_eq("application/json"));
    assert_eq!(result.field("status"), &Json::Int(200));
}

#[test]
fn base_urls_never_carry_credentials_queries_or_fragments() {
    assert!(audit::valid_base_url("https://matchlab.example"));
    assert!(audit::valid_base_url("http://127.0.0.1:8765/"));
    for bad in ["ftp://example", "https://user:pw@example", "https://example?x=1", "https://example#x", "https://"] {
        assert!(!audit::valid_base_url(bad), "{bad}");
    }
}

// ---- title-test collector ----------------------------------------------

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        let path = std::env::temp_dir().join(format!("matchlab-diagnostics-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(tag: &str, source: &str) -> Json {
    let temp = Temp::new(tag);
    let path = temp.0.join("fixture.rs");
    std::fs::write(&path, source).unwrap();
    let mut reader = Reader::new(&temp.0);
    reader.file(&path).unwrap();
    reader.payload()
}

fn titles(payload: &Json) -> Vec<String> {
    payload.field("cases").as_arr().unwrap().iter().map(|row| row.field("title").as_str().unwrap().to_owned()).collect()
}

#[test]
fn literal_table_and_nested_field_labels_keep_source_values() {
    let result = read(
        "table",
        r#"
        #[test] fn named_parts() {
            for (raw, episode) in [("Show - 12", 12), ("Another - 09", 9)] {
                let parsed = parse_title(raw, false).unwrap();
                let hint = &parsed["anime_episode_candidate"];
                assert_eq!(hint["episode"], json!(episode));
                assert_eq!(hint["verified"], false);
                assert!(parsed.get("anime_title_verified").is_none());
            }
        }
    "#,
    );
    assert_eq!(titles(&result), ["Show - 12", "Another - 09"]);
    assert_eq!(
        result.field("cases").as_arr().unwrap()[1].field("expected"),
        &j(json!([
            {"path": ["anime_episode_candidate", "episode"], "op": "eq", "value": 9},
            {"path": ["anime_episode_candidate", "verified"], "op": "eq", "value": false},
            {"path": ["anime_title_verified"], "op": "absent"},
        ]))
    );
}

#[test]
fn unknown_conditional_branches_do_not_become_definite_labels() {
    let result = read(
        "branches",
        r#"
        #[test] fn ambiguous() {
            let parsed = parse_title("Show 27", false).unwrap();
            if let Some(hint) = parsed.get("anime_episode_candidate") {
                assert_eq!(hint["episode"], 27);
            } else {
                assert_eq!(parsed["episodes"], json!([27]));
            }
            match unknown {
                _ => assert_eq!(parsed["episodes"], json!([99])),
            }
            assert!(parsed.get("anime_title_verified").is_none());
        }
    "#,
    );
    assert_eq!(
        result.field("cases").as_arr().unwrap()[0].field("expected"),
        &j(json!([{"path": ["anime_title_verified"], "op": "absent"}]))
    );
}

#[test]
fn unicode_dynamic_inputs_expand_and_unknown_format_stays_unknown() {
    let result = read(
        "unicode",
        r#"
        #[test] fn whitespace() {
            for whitespace in ["\u{00a0}", "\u{000b}"] {
                let raw = format!("Example{whitespace}Show");
                let parsed = parse_title(&format!("{raw}.S01E02"), false).unwrap();
                assert_eq!(parsed["title"], "Example Show");
            }
            let unsupported = parse_title(&format!("{unknown}.S01E02"), false).unwrap();
            assert_eq!(unsupported["title"], "Impossible label");
        }
    "#,
    );
    assert_eq!(titles(&result), ["Example\u{00a0}Show.S01E02", "Example\u{000b}Show.S01E02"]);
    assert_eq!(result.field("summary").field("scored_assertions"), &Json::Int(2));
}

#[test]
fn unsupported_expected_expression_is_not_serialized_as_truth() {
    let result = read(
        "unsupported",
        r#"
        #[test] fn unknown() {
            let parsed = parse_title("Show S01E02", false).unwrap();
            assert_eq!(parsed["episodes"], json!([unknown()]));
        }
    "#,
    );
    assert_eq!(result.field("cases").as_arr().unwrap()[0].field("expected"), &j(json!([])));
}

#[test]
fn release_generator_changes_fail_closed() {
    let temp = Temp::new("variants");
    let path = temp.0.join("release_300.rs");
    std::fs::write(&path, "fn variants() { changed_source(); }").unwrap();
    assert!(collector::verify_variant_source(&path).unwrap_err().contains("changed"));
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    collector::verify_variant_source(&repository.join("crates/matchlab-server/tests/release_300.rs")).unwrap();
}

#[test]
fn tokenizer_skips_comments_and_keeps_literals() {
    let texts: Vec<String> = collector::tokens("let a = r#\"x\"y\"#; // note\n/* block */ b..=c 'z' 'a").into_iter().map(|t| t.text).collect();
    assert_eq!(texts, ["let", "a", "=", "r#\"x\"y\"#", ";", "b", "..=", "c", "'z'", "'", "a"]);
    assert_eq!(collector::rust_string(r#""tab\tnul\0\u{1F600}""#).unwrap(), "tab\tnul\0😀");
    assert_eq!(collector::rust_string("r##\"raw \"quote\"##").unwrap(), "raw \"quote");
}

fn trace_row(title: &str, observed: serde_json::Value, overrides: serde_json::Value) -> String {
    let mut row = json!({"kind": "parse_title", "title": title, "translate_languages": false, "thread": "source_test",
                         "caller_file": "fixture.rs", "caller_line": 4, "parser_mode": "live", "revision": 0,
                         "observed": observed, "error": null});
    for (key, value) in overrides.as_object().unwrap() {
        row[key] = value.clone();
    }
    row.to_string() + "\n"
}

#[test]
fn replay_and_unstable_outputs_are_not_public_parity_baselines() {
    let trace: String = [
        trace_row("Stable", json!({"title": "Stable"}), json!({})),
        trace_row("Curated", json!({"title": "Curated"}), json!({})),
        trace_row("Unstable", json!({"title": "one"}), json!({})),
        trace_row("Unstable", json!({"title": "two"}), json!({})),
        trace_row("Replay", json!({"title": "learned"}), json!({"parser_mode": "replay"})),
        trace_row("Translated", json!({"languages": ["English"]}), json!({"translate_languages": true})),
        trace_row("", json!({"title": ""}), json!({})),
    ]
    .concat();
    let curated: BTreeSet<String> = ["Curated".to_owned()].into();
    let outputs = collector::summarize_trace(&trace, &curated);
    let get = |name: &str| outputs.iter().find(|(key, _)| *key == name).unwrap().1.clone();
    let parity_titles = |name: &str| -> Vec<String> {
        get(name).field("cases").as_arr().unwrap().iter().map(|row| row.field("request").field("title").as_str().unwrap().to_owned()).collect()
    };
    assert_eq!(parity_titles("title-parity-corpus.json"), ["Curated", "Stable"]);
    assert_eq!(parity_titles("title-parity-new-only.json"), ["Stable"]);
    assert_eq!(get("trace-summary.json").field("unstable_public_baseline_inputs"), &Json::Int(1));
    let corpus = get("title-parity-corpus.json");
    let first = &corpus.field("cases").as_arr().unwrap()[0];
    assert!(first.field("expected_provenance").as_str().unwrap().contains("never independent"));
}

#[test]
fn trace_patch_applies_to_the_current_parser() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let patch = collector::trace_patch(&repository).unwrap();
    assert!(patch.starts_with("--- a/crates/matchlab-parser/src/lib.rs\n+++ b/crates/matchlab-parser/src/lib.rs\n@@ -"));
    assert!(patch.contains("+fn diagnostic_title_trace("));
    assert_eq!(patch.matches("+    diagnostic_title_trace(\"parse_title\"").count(), 1);
}
