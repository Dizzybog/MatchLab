//! Sequential, bounded HTTP evaluation of a labeled MatchLab JSON corpus.
//!
//! Input is a list, or {"cases": [...]}. Each case has id, request, optional
//! expected (a partial response object), source, classification and notes.
//! Optional endpoint is /parse or /match. Only explicitly labeled/verified/
//! ground_truth/production_expectation cases contribute identifier metrics;
//! reference and differential observations are comparisons, never ground truth.
//! No concurrency, semantic retries or TLS bypasses are used.
use matchlab_community::json::{dumps, Dump, Json, Object};
use matchlab_community::learning::py_round;
use matchlab_community::obj;
use matchlab_community::util::{isoformat, sha256_hex, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const ENDPOINTS: &[&str] = &["/match", "/parse"];
const LABELED: &[&str] = &["labeled", "verified", "ground_truth", "production_expectation"];

/// A response and its timing as recorded in the report.
pub fn response_data(status: Option<i128>, headers: Object, body: &[u8], elapsed: Duration, error: Option<Json>) -> Json {
    let text = String::from_utf8_lossy(body).into_owned();
    let (parsed, json_error) = match strict_json(&text) {
        Ok(value) => (value, Json::Null),
        Err(error) => (Json::Null, Json::from(error)),
    };
    obj! {
        "status" => status.map_or(Json::Null, Json::Int),
        "headers" => Json::Obj(headers),
        "response" => parsed,
        "raw_body" => text,
        "elapsed_ms" => Json::Float(py_round(elapsed.as_secs_f64() * 1000.0, 3)),
        "error" => error.unwrap_or(Json::Null),
        "json_error" => json_error,
    }
}

/// JSON without the non-standard NaN/Infinity constants.
pub fn strict_json(text: &str) -> std::result::Result<Json, String> {
    Json::parse(text)
}

/// Exact JSON equality where `true` never equals `1`.
fn exact_equal(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Obj(a), Json::Obj(b)) => a.len() == b.len() && a.iter().all(|(key, value)| b.get(key).is_some_and(|other| exact_equal(value, other))),
        (Json::Arr(a), Json::Arr(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| exact_equal(x, y)),
        (Json::Bool(_), Json::Bool(_)) => left == right,
        (Json::Bool(_), _) | (_, Json::Bool(_)) => false,
        _ => left == right,
    }
}

/// Objects are partial expectations; arrays and scalar values are exact.
/// `None` is an absent field (distinct from JSON null).
pub fn compare_subset(expected: &Json, actual: Option<&Json>, path: &str, differences: &mut Vec<Json>) {
    match (expected, actual) {
        (Json::Obj(want), Some(Json::Obj(have))) => {
            for (key, value) in want {
                compare_subset(value, have.get(key), &format!("{path}.{key}"), differences);
            }
        }
        (Json::Arr(want), Some(Json::Arr(have))) => {
            if want.len() != have.len() {
                differences.push(obj! {"path" => format!("{path}.length"), "expected" => want.len(), "actual" => have.len()});
            }
            for (index, value) in want.iter().take(have.len()).enumerate() {
                compare_subset(value, Some(&have[index]), &format!("{path}[{index}]"), differences);
            }
        }
        (_, None) => differences.push(obj! {"path" => path, "expected" => expected.clone(), "actual" => Json::Null, "actual_missing" => true}),
        (_, Some(actual)) => {
            let bool_mismatch = matches!(expected, Json::Bool(_)) != matches!(actual, Json::Bool(_));
            if bool_mismatch || expected != actual {
                differences.push(obj! {"path" => path, "expected" => expected.clone(), "actual" => actual.clone()});
            }
        }
    }
}

pub fn compare_assertions(assertions: &[Json], actual: &Json) -> Vec<Json> {
    let mut differences = Vec::new();
    for assertion in assertions {
        let path = assertion.field("path").as_arr().cloned().unwrap_or_default();
        let mut value = Some(actual.clone());
        for key in &path {
            value = match (value, key) {
                (Some(Json::Obj(map)), Json::Str(key)) => map.get(key).cloned(),
                (Some(Json::Arr(items)), Json::Int(index)) => usize::try_from(*index).ok().and_then(|i| items.get(i).cloned()),
                _ => None,
            };
            if value.is_none() {
                break;
            }
        }
        if assertion.field("coercion").str_eq("len") {
            value = value.and_then(|value| match value {
                Json::Arr(items) => Some(Json::from(items.len())),
                Json::Obj(map) => Some(Json::from(map.len())),
                Json::Str(text) => Some(Json::from(text.chars().count())),
                _ => None,
            });
        }
        let operation = assertion.field("op").as_str().unwrap_or_default();
        let expected = assertion.field("value");
        let equal = value.as_ref().is_some_and(|value| exact_equal(expected, value));
        let matches = match operation {
            "absent" => value.is_none(),
            "eq" => equal,
            _ => !equal,
        };
        if !matches {
            differences.push(obj! {
                "path" => Json::Arr(path), "op" => operation,
                "coercion" => assertion.field("coercion").clone(),
                "expected" => expected.clone(), "actual" => value.clone().unwrap_or(Json::Null),
                "actual_missing" => value.is_none(),
            });
        }
    }
    differences
}

/// Seconds from a Retry-After header (delta seconds or HTTP date).
pub fn retry_after(value: Option<&str>, now: f64) -> Option<f64> {
    let value = value.filter(|text| !text.is_empty())?;
    if let Ok(seconds) = value.trim().parse::<f64>()
        && seconds.is_finite() && seconds >= 0.0 {
            return Some(seconds);
        }
    let stamp = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    Some((stamp.timestamp() as f64 + f64::from(stamp.timestamp_subsec_micros()) / 1e6 - now).max(0.0))
}

/// Headers of the final response block of a curl `--dump-header` file.
pub fn read_headers(raw: &str) -> Object {
    let mut headers = Object::new();
    let normalized = raw.replace("\r\n", "\n");
    for block in normalized.split("\n\n") {
        let mut lines = block.lines();
        if !lines.next().is_some_and(|line| line.starts_with("HTTP/")) {
            continue;
        }
        headers = Object::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.to_lowercase(), Json::from(value.trim()));
            }
        }
    }
    headers
}

pub trait Transport {
    fn name(&self) -> &'static str;
    fn request(&mut self, url: &str, payload: &Json, timeout: f64, max_bytes: usize) -> Json;
}

/// Preserves normal curl identity, inherited proxy/CA settings and TLS.
pub struct Curl {
    pub binary: String,
}

impl Curl {
    pub fn new() -> Result<Curl> {
        let found = Command::new("curl").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status();
        if !found.is_ok_and(|status| status.success()) {
            return Err("curl is required; choose --transport native if unavailable".into());
        }
        Ok(Curl { binary: "curl".into() })
    }

    /// The exact argument list (no shell).
    pub fn arguments(&self, url: &str, data: &Path, headers: &Path, body: &Path, timeout: f64, max_bytes: usize) -> Vec<String> {
        [
            "--silent", "--show-error", "--retry", "0", "--max-time", &timeout.to_string(), "--max-filesize",
            &max_bytes.to_string(), "--proto", "=http,https", "--request", "POST", "--header",
            "Content-Type: application/json", "--data-binary", &format!("@{}", data.display()), "--dump-header",
            &headers.display().to_string(), "--output", &body.display().to_string(), "--write-out", "%{json}", url,
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect()
    }

    /// Turn a finished curl run into a response record.
    pub fn finish(exit: Option<i32>, stdout: &[u8], stderr: &[u8], headers: Option<String>, body: Vec<u8>, max_bytes: usize, elapsed: Duration) -> Json {
        let metadata = std::str::from_utf8(stdout).ok().and_then(|text| strict_json(text).ok()).unwrap_or(Json::Null);
        let code = exit.unwrap_or(-1);
        let error = (code != 0 || body.len() > max_bytes).then(|| {
            let kind = if code == 28 {
                "timeout"
            } else if code == 63 || body.len() > max_bytes {
                "response_too_large"
            } else {
                "network_error"
            };
            let message: String = String::from_utf8_lossy(stderr).chars().take(4096).collect();
            obj! {"kind" => kind, "curl_exit_code" => Json::Int(i128::from(code)), "message" => message}
        });
        let status = metadata.field("http_code").as_int().filter(|code| *code != 0);
        let header_map = headers.map(|raw| read_headers(&raw)).unwrap_or_default();
        response_data(status, header_map, &body[..body.len().min(max_bytes)], elapsed, error)
    }
}

impl Transport for Curl {
    fn name(&self) -> &'static str {
        "curl"
    }

    fn request(&mut self, url: &str, payload: &Json, timeout: f64, max_bytes: usize) -> Json {
        let started = Instant::now();
        let failure = |message: String| {
            response_data(None, Object::new(), b"", started.elapsed(), Some(obj! {"kind" => "network_error", "message" => message}))
        };
        let folder = match matchlab_community::state::TempDir::new() {
            Ok(folder) => folder,
            Err(error) => return failure(error),
        };
        let (data, headers, body) = (folder.path().join("request.json"), folder.path().join("headers"), folder.path().join("body"));
        if let Err(error) = std::fs::write(&data, dumps(payload, Dump::DEFAULT)) {
            return failure(error.to_string());
        }
        let child = Command::new(&self.binary)
            .args(self.arguments(url, &data, &headers, &body, timeout, max_bytes))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => return failure(error.to_string()),
        };
        let output = match wait_output(child, Duration::from_secs_f64(timeout + 5.0)) {
            Some(output) => output,
            None => {
                return response_data(None, Object::new(), b"", started.elapsed(),
                    Some(obj! {"kind" => "timeout", "message" => "curl process timed out"}));
            }
        };
        let raw = std::fs::read(&body).map(|mut bytes| {
            bytes.truncate(max_bytes + 1);
            bytes
        });
        let header_text = std::fs::read(&headers).ok().map(|bytes| bytes.iter().map(|byte| char::from(*byte)).collect());
        Curl::finish(output.status.code(), &output.stdout, &output.stderr, header_text, raw.unwrap_or_default(), max_bytes, started.elapsed())
    }
}

fn wait_output(mut child: std::process::Child, limit: Duration) -> Option<std::process::Output> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Native HTTPS client (rustls); honors the standard proxy variables.
pub struct Native {
    client: reqwest::blocking::Client,
}

impl Native {
    pub fn new() -> Result<Native> {
        let mut builder = reqwest::blocking::Client::builder();
        for variable in ["REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "SSL_CERT_FILE"] {
            if let Some(path) = std::env::var_os(variable) {
                let pem = std::fs::read(&path).map_err(|error| format!("{variable}: {error}"))?;
                for certificate in reqwest::Certificate::from_pem_bundle(&pem).map_err(|error| error.to_string())? {
                    builder = builder.add_root_certificate(certificate);
                }
                break;
            }
        }
        Ok(Native { client: builder.build().map_err(|error| error.to_string())? })
    }
}

impl Transport for Native {
    fn name(&self) -> &'static str {
        "native"
    }

    fn request(&mut self, url: &str, payload: &Json, timeout: f64, max_bytes: usize) -> Json {
        let started = Instant::now();
        let sent = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs_f64(timeout))
            .body(dumps(payload, Dump::DEFAULT))
            .send();
        match sent {
            Ok(response) => {
                let status = i128::from(response.status().as_u16());
                let headers: Object = response
                    .headers()
                    .iter()
                    .map(|(key, value)| (key.as_str().to_lowercase(), Json::from(String::from_utf8_lossy(value.as_bytes()).into_owned())))
                    .collect();
                let mut body = Vec::new();
                let mut reader = std::io::Read::take(response, max_bytes as u64 + 1);
                let read = std::io::Read::read_to_end(&mut reader, &mut body);
                let error = match read {
                    Err(error) => Some(obj! {"kind" => "network_error", "message" => error.to_string()}),
                    Ok(_) if body.len() > max_bytes => {
                        Some(obj! {"kind" => "response_too_large", "message" => "Response exceeded byte limit"})
                    }
                    Ok(_) => None,
                };
                response_data(Some(status), headers, &body[..body.len().min(max_bytes)], started.elapsed(), error)
            }
            Err(error) => {
                let kind = if error.is_timeout() { "timeout" } else { "network_error" };
                response_data(None, Object::new(), b"", started.elapsed(), Some(obj! {"kind" => kind, "message" => error.to_string()}))
            }
        }
    }
}

fn with(base: &Json, entries: Vec<(&str, Json)>) -> Json {
    let mut out = base.clone();
    if let Json::Obj(map) = &mut out {
        for (key, value) in entries {
            map.insert(key.to_owned(), value);
        }
    }
    out
}

/// Python `str()` of a scalar for the parser-case key.
fn py_str(value: &Json) -> String {
    match value {
        Json::Bool(true) => "True".into(),
        Json::Bool(false) => "False".into(),
        Json::Null => "None".into(),
        Json::Str(text) => text.clone(),
        other => dumps(other, Dump::DEFAULT),
    }
}

pub fn validate_cases(payload: &Json) -> Result<Vec<Json>> {
    let cases = match payload {
        Json::Obj(_) => payload.field("cases"),
        other => other,
    };
    let Some(cases) = cases.as_arr() else { return Err("Corpus must be an array or an object containing cases".into()) };
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for case in cases {
        let mut case = case.clone();
        if case.get("title").is_some() && case.get("request").is_none() {
            let title = case.field("title").as_str().ok_or("Every case requires request.title")?.to_owned();
            let translate = case.get("translate_languages").cloned().unwrap_or(Json::Bool(false));
            let key = format!("{title}\0{}", py_str(&translate));
            let assertions = case.get("expected").cloned().unwrap_or(Json::Arr(vec![]));
            case = with(&case, vec![
                ("id", Json::from(format!("parser-{}", &sha256_hex(key)[..16]))),
                ("request", obj! {"title" => title.as_str()}),
                ("endpoint", Json::from("/parse")),
                ("classification", Json::from("parser_regression")),
                ("expected", Json::Obj(Object::new())),
                ("assertions", assertions),
            ]);
        }
        let Some(id) = case.field("id").as_str().filter(|_| case.as_obj().is_some()).map(str::to_owned) else {
            return Err("Every case requires a string id".into());
        };
        if !seen.insert(id.clone()) {
            return Err(format!("Duplicate case id: {id}"));
        }
        let request = case.field("request");
        let Some(fields) = request.as_obj().filter(|_| request.field("title").as_str().is_some()) else {
            return Err("Every case requires request.title".into());
        };
        if fields.keys().any(|key| !matches!(key.as_str(), "title" | "year" | "series")) {
            return Err(format!("Unknown request field in {id}"));
        }
        if !case.field("expected").is_null() && case.field("expected").as_obj().is_none() {
            return Err("Expected must be a partial object or null".into());
        }
        let endpoint = case.get("endpoint").cloned().unwrap_or(Json::from("/match"));
        if !endpoint.as_str().is_some_and(|endpoint| ENDPOINTS.contains(&endpoint)) {
            return Err("Unsupported case endpoint".into());
        }
        let assertions = case.get("assertions").cloned().unwrap_or(Json::Arr(vec![]));
        for assertion in assertions.as_arr().ok_or(format!("Unsupported assertion in {id}"))? {
            let valid = assertion.as_obj().is_some()
                && matches!(assertion.field("op").as_str(), Some("eq" | "ne" | "absent"))
                && assertion.field("path").as_arr().is_some_and(|path| path.iter().all(|key| matches!(key, Json::Str(_) | Json::Int(_))))
                && (assertion.field("coercion").is_null() || assertion.field("coercion").str_eq("len"));
            if !valid {
                return Err(format!("Unsupported assertion in {id}"));
            }
        }
        normalized.push(case);
    }
    Ok(normalized)
}

pub fn role(case: &Json) -> &'static str {
    let classification = match case.field("classification") {
        Json::Null => String::new(),
        other => py_str(other).to_lowercase(),
    };
    if classification.contains("reference") || classification.contains("differential") {
        return "reference";
    }
    if !case.field("expected").truthy() && !case.field("assertions").truthy() {
        return "unlabeled";
    }
    if LABELED.contains(&classification.as_str()) { "labeled" } else { "expectation" }
}

pub fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let position = (ordered.len() - 1) as f64 * fraction;
    let (low, high) = (position.floor() as usize, position.ceil() as usize);
    Some(py_round(ordered[low] + (ordered[high] - ordered[low]) * (position - low as f64), 3))
}

fn float_or_null(value: Option<f64>) -> Json {
    value.map_or(Json::Null, Json::Float)
}

pub fn summarize(results: &[Json]) -> Json {
    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    let mut roles: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    for row in results {
        *outcomes.entry(row.field("outcome").as_str().unwrap_or_default().to_owned()).or_default() += 1;
        *roles.entry(row.field("evaluation_role").as_str().unwrap_or_default().to_owned()).or_default() += 1;
    }
    let timings: Vec<f64> = results
        .iter()
        .filter(|row| row.field("attempts").truthy())
        .filter_map(|row| row.field("elapsed_ms").as_number())
        .collect();
    let compared: Vec<&Json> = results.iter().filter(|row| !row.field("comparison").is_null()).collect();
    let sum = |key: &str| -> i128 {
        compared.iter().map(|row| row.field("comparison").field("assertions").field(key).as_int().unwrap_or(0)).sum()
    };
    let mut metrics = Object::new();
    for prefix in ["sid", "asid"] {
        let keys = [format!("{prefix}_start"), format!("{prefix}_end")];
        let labeled: Vec<&Json> = results
            .iter()
            .filter(|row| {
                row.field("endpoint").str_eq("/match")
                    && row.field("evaluation_role").str_eq("labeled")
                    && keys.iter().all(|key| row.field("expected").get(key).is_some())
            })
            .collect();
        if labeled.is_empty() {
            continue;
        }
        let mut counts: indexmap::IndexMap<&str, i128> = indexmap::IndexMap::new();
        for row in &labeled {
            let expected: Vec<&Json> = keys.iter().map(|key| row.field("expected").field(key)).collect();
            let positive = expected.iter().any(|value| !value.is_null());
            *counts.entry(if positive { "expected_positive" } else { "expected_abstention" }).or_default() += 1;
            let actual = row.field("response");
            let available = matches!(row.field("outcome").as_str(), Some("matched_expectation" | "mismatch" | "reference" | "unlabeled"))
                && actual.as_obj().is_some()
                && keys.iter().all(|key| actual.get(key).is_some());
            if !available {
                *counts.entry("unavailable").or_default() += 1;
                continue;
            }
            let values: Vec<&Json> = keys.iter().map(|key| actual.field(key)).collect();
            let key = if values == expected {
                if positive { "true_matches" } else { "correct_abstentions" }
            } else if values.iter().any(|value| !value.is_null()) {
                "wrong_ids"
            } else {
                "false_abstentions"
            };
            *counts.entry(key).or_default() += 1;
        }
        let count = |key: &str| counts.get(key).copied().unwrap_or(0);
        let (true_matches, wrong) = (count("true_matches"), count("wrong_ids"));
        let mut entry = Object::new();
        entry.insert("labeled_cases".into(), Json::from(labeled.len()));
        for (key, value) in &counts {
            entry.insert((*key).to_owned(), Json::Int(*value));
        }
        entry.insert(
            "precision".into(),
            float_or_null((true_matches + wrong > 0).then(|| true_matches as f64 / (true_matches + wrong) as f64)),
        );
        let positives = count("expected_positive");
        entry.insert("match_recall".into(), float_or_null((positives > 0).then(|| true_matches as f64 / positives as f64)));
        metrics.insert(prefix.into(), Json::Obj(entry));
    }
    obj! {
        "cases" => results.len(),
        "outcomes" => Json::Obj(outcomes.into_iter().map(|(key, value)| (key, Json::from(value))).collect()),
        "roles" => Json::Obj(roles.into_iter().map(|(key, value)| (key, Json::from(value))).collect()),
        "compared" => compared.len(),
        "matches_expectation" => compared.iter().filter(|row| row.field("comparison").field("matches").is_true()).count(),
        "latency_ms" => obj! {"p50" => float_or_null(percentile(&timings, 0.5)), "p95" => float_or_null(percentile(&timings, 0.95))},
        "identifier_metrics" => Json::Obj(metrics),
        "metric_scope" => "Only explicitly labeled cases with both expected identifier endpoints. \
            Reference, partial and unknown expectations are excluded. \
            Recall includes unavailable labeled positive cases; precision excludes them.",
        "assertions" => obj! {
            "manual_unscored" => results.iter().map(|row| row.field("assertions_manual").as_arr().map_or(0, Vec::len)).sum::<usize>(),
            "scored" => Json::Int(sum("total")),
            "passed" => Json::Int(sum("passed")),
        },
    }
}

pub struct Settings {
    pub endpoint: String,
    pub delay: f64,
    pub timeout: f64,
    pub max_bytes: usize,
    pub rate_limit_retries: usize,
    pub max_retry_after: f64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { endpoint: "/match".into(), delay: 0.15, timeout: 20.0, max_bytes: 2 * 1024 * 1024, rate_limit_retries: 1, max_retry_after: 60.0 }
    }
}

fn set(row: &mut Json, key: &str, value: Json) {
    if let Json::Obj(map) = row {
        map.insert(key.to_owned(), value);
    }
}

pub fn run_audit(
    cases: &[Json],
    base_url: &str,
    settings: &Settings,
    transport: &mut dyn Transport,
    sleep: &mut dyn FnMut(f64),
    progress: &mut dyn FnMut(usize, usize, &Json),
) -> Json {
    let mut results = Vec::new();
    let mut stop_reason: Option<String> = None;
    let unix_now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    for (index, case) in cases.iter().enumerate() {
        let endpoint = case.get("endpoint").cloned().unwrap_or(Json::from(settings.endpoint.as_str()));
        let mut row = with(case, vec![
            ("endpoint", endpoint.clone()),
            ("evaluation_role", Json::from(role(case))),
            ("expected", case.field("expected").clone()),
            ("attempts", Json::Arr(vec![])),
            ("comparison", Json::Null),
            ("elapsed_ms", Json::Int(0)),
            ("outcome", Json::from("not_run_rate_limit")),
        ]);
        let endpoint = endpoint.as_str().unwrap_or("/match").to_owned();
        if endpoint == "/parse" && case.field("translate_languages").is_true() {
            set(&mut row, "outcome", Json::from("unsupported_configuration"));
            set(&mut row, "error", obj! {"kind" => "unsupported_configuration",
                "message" => "Public /parse uses translate_languages=false"});
            results.push(row);
            continue;
        }
        if let Some(reason) = &stop_reason {
            set(&mut row, "error", obj! {"kind" => "rate_limit_stop", "message" => reason.as_str()});
            results.push(row);
            continue;
        }
        let started = Instant::now();
        let mut attempts = Vec::new();
        for attempt in 0..=settings.rate_limit_retries {
            if index > 0 || attempt > 0 {
                sleep(settings.delay);
            }
            let url = format!("{}{endpoint}", base_url.trim_end_matches('/'));
            let response = transport.request(&url, case.field("request"), settings.timeout, settings.max_bytes);
            for (key, value) in response.as_obj().into_iter().flatten() {
                set(&mut row, key, value.clone());
            }
            attempts.push(obj! {"status" => response.field("status").clone(),
                "elapsed_ms" => response.field("elapsed_ms").clone(), "error" => response.field("error").clone()});
            if response.field("status") != &Json::Int(429) {
                break;
            }
            let header = response.field("headers").field("retry-after").as_str().map(str::to_owned);
            let wait = settings.delay.max(retry_after(header.as_deref(), unix_now()).unwrap_or(1.0));
            if wait > settings.max_retry_after || attempt == settings.rate_limit_retries {
                stop_reason = Some(format!("HTTP 429: required wait {wait:.3}s; audit stopped conservatively"));
                break;
            }
            if let Some(Json::Obj(last)) = attempts.last_mut() {
                last.insert("retry_after_seconds".into(), Json::Float(wait));
            }
            sleep(wait);
        }
        set(&mut row, "attempts", Json::Arr(attempts));
        set(&mut row, "wall_elapsed_ms", Json::Float(py_round(started.elapsed().as_secs_f64() * 1000.0, 3)));
        let status = row.field("status").as_int();
        let server = row.field("headers").field("server").as_str().unwrap_or_default().to_lowercase();
        let outcome = if row.field("error").truthy() {
            row.field("error").field("kind").as_str().unwrap_or("error").to_owned()
        } else if status == Some(403) && (row.field("raw_body").as_str().unwrap_or_default().contains("1010") || server.contains("cloudflare")) {
            "transport_blocked".into()
        } else if status == Some(429) {
            "rate_limited".into()
        } else if !status.is_some_and(|status| (200..300).contains(&status)) {
            "http_error".into()
        } else if row.field("json_error").truthy() {
            "invalid_json".into()
        } else if row.field("evaluation_role").str_eq("unlabeled") {
            "unlabeled".into()
        } else {
            let assertions = case.field("assertions").as_arr().cloned().unwrap_or_default();
            let expected = match row.field("expected") {
                Json::Null => Json::Obj(Object::new()),
                other => other.clone(),
            };
            let mut differences = Vec::new();
            compare_subset(&expected, Some(row.field("response")), "$", &mut differences);
            let assertion_differences = compare_assertions(&assertions, row.field("response"));
            let failed = assertion_differences.len();
            differences.extend(assertion_differences);
            let matches = differences.is_empty();
            set(&mut row, "comparison", obj! {
                "matches" => matches, "differences" => differences,
                "assertions" => obj! {"total" => assertions.len(), "passed" => assertions.len() - failed},
            });
            if row.field("evaluation_role").str_eq("reference") {
                "reference".into()
            } else if matches {
                "matched_expectation".into()
            } else {
                "mismatch".into()
            }
        };
        set(&mut row, "outcome", Json::from(outcome));
        progress(index + 1, cases.len(), &row);
        results.push(row);
    }
    let summary = summarize(&results);
    obj! {
        "schema_version" => Json::Int(1),
        "metadata" => obj! {
            "timestamp_utc" => isoformat(chrono::Utc::now()),
            "base_url" => base_url,
            "default_endpoint" => settings.endpoint.as_str(),
            "transport" => transport.name(),
            "delay_seconds" => Json::Float(settings.delay),
            "timeout_seconds" => Json::Float(settings.timeout),
            "max_response_bytes" => settings.max_bytes,
            "rate_limit_retries" => settings.rate_limit_retries,
            "stop_reason" => stop_reason.map_or(Json::Null, Json::from),
        },
        "summary" => summary,
        "results" => results,
    }
}

/// HTTP(S) without credentials, query or fragment.
pub fn valid_base_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else { return false };
    let authority = rest.split('/').next().unwrap_or_default();
    matches!(scheme, "http" | "https") && !authority.is_empty() && !authority.contains('@') && !url.contains(['?', '#'])
}
