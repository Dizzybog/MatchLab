//! GitHub-facing steps with a fake API: Release state, issue intake and
//! bot-only cleanup.
mod common;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::*;
use matchlab_community::cleanup as cc;
use matchlab_community::github::{quote, valid_repo, GitHub, BOT};
use matchlab_community::issue_intake as intake;
use matchlab_community::json::Json;
use matchlab_community::state;
use matchlab_community::util::Result;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

const REPO: &str = "Dizzybog/MatchLab";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
}

fn iso(days: i64) -> String {
    (now() - Duration::days(days)).to_rfc3339()
}

fn day(days: i64) -> String {
    (now() - Duration::days(days)).date_naive().format("%Y-%m-%d").to_string()
}

#[test]
fn repository_names_and_url_quoting() {
    assert!(valid_repo(REPO));
    assert!(!valid_repo("owner"));
    assert!(!valid_repo("owner/repo/extra"));
    assert!(!valid_repo("owner/re po"));
    assert_eq!(quote("bot/community-regex-1", "/"), "bot/community-regex-1");
    assert_eq!(quote("Dizzybog:bot/x y", ":"), "Dizzybog:bot%2Fx%20y");
}

// ---- versioned Release state --------------------------------------------

#[derive(Default)]
struct FakeState {
    releases: BTreeMap<String, Json>,
    assets: BTreeMap<String, Vec<u8>>,
    next_id: i64,
    deleted: Vec<i64>,
    fail_upload: bool,
}

impl FakeState {
    fn new(missing: bool) -> FakeState {
        let mut fake = FakeState { next_id: 10, ..FakeState::default() };
        if !missing {
            fake.releases.insert("community-learning-ledger".into(), j(json!({"assets": []})));
        }
        fake
    }

    fn assets_of(&mut self, tag: &str) -> &mut Vec<Json> {
        match self.releases.get_mut(tag) {
            Some(Json::Obj(map)) => match map.get_mut("assets") {
                Some(Json::Arr(items)) => items,
                _ => panic!("no assets"),
            },
            _ => panic!("no release {tag}"),
        }
    }

    fn set_assets(&mut self, tag: &str, assets: serde_json::Value) {
        self.releases.insert(tag.into(), j(json!({"assets": assets})));
    }
}

impl GitHub for FakeState {
    fn repo(&self) -> &str {
        REPO
    }

    fn send(&mut self, method: &str, route: &str, _: Option<&Json>, missing_ok: bool) -> Result<Json> {
        if method == "GET"
            && let Some(tag) = route.strip_prefix("/releases/tags/") {
                return match self.releases.get(tag) {
                    Some(release) => Ok(release.clone()),
                    None if missing_ok => Ok(Json::Null),
                    None => Err("404".into()),
                };
            }
        if method == "DELETE"
            && let Some(number) = route.strip_prefix("/releases/assets/") {
                let number: i64 = number.parse().unwrap();
                self.deleted.push(number);
                for release in self.releases.values_mut() {
                    if let Json::Obj(map) = release
                        && let Some(Json::Arr(items)) = map.get_mut("assets") {
                            items.retain(|asset| asset.field("id") != &Json::Int(i128::from(number)));
                        }
                }
                return Ok(Json::Null);
            }
        panic!("unexpected {method} {route}");
    }

    fn gh(&mut self, args: &[String]) -> Result<()> {
        let value = |flag: &str| args[args.iter().position(|arg| arg == flag).unwrap() + 1].clone();
        match (args[0].as_str(), args[1].as_str()) {
            ("release", "create") => {
                self.set_assets(&args[2], json!([]));
            }
            ("release", "download") => {
                let name = value("--pattern");
                std::fs::write(Path::new(&value("--dir")).join(&name), &self.assets[&name]).unwrap();
            }
            ("release", "upload") => {
                if self.fail_upload {
                    return Err("simulated failed upload".into());
                }
                let source = Path::new(&args[3]);
                let name = source.file_name().unwrap().to_string_lossy().into_owned();
                let bytes = std::fs::read(source).unwrap();
                let size = bytes.len();
                self.assets.insert(name.clone(), bytes);
                let id = self.next_id;
                self.next_id += 1;
                self.assets_of(&args[2]).push(j(json!({"id": id, "name": name, "size": size})));
            }
            _ => panic!("unexpected gh {args:?}"),
        }
        Ok(())
    }
}

const LEDGER: &str = "community-learning-ledger";

#[test]
fn empty_release_bootstrap_and_upload_is_never_zero_bytes() {
    let mut store = FakeState::new(false);
    let temp = Temp::new();
    let path = temp.0.join("fingerprints.txt");
    state::load(&mut store, "ledger", &path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    state::save(&mut store, "ledger", &path, "ledger", "123", "1").unwrap();
    assert_eq!(store.assets["fingerprints-123-1-ledger.txt"], format!("{}\n", state::HEADER).into_bytes());
    state::load(&mut store, "ledger", &path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"");
}

#[test]
fn legacy_asset_and_versioned_state_roundtrip() {
    let mut store = FakeState::new(false);
    let old = format!("{}\n", "a".repeat(64));
    store.assets.insert("fingerprints.txt".into(), old.clone().into_bytes());
    store.set_assets(LEDGER, json!([{"id": 1, "name": "fingerprints.txt", "size": old.len()}]));
    let temp = Temp::new();
    let path = temp.0.join("fingerprints.txt");
    state::load(&mut store, "ledger", &path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    state::save(&mut store, "ledger", &path, "ledger", "123", "2").unwrap();
    state::load(&mut store, "ledger", &path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
}

#[test]
fn failed_upload_preserves_last_confirmed_asset() {
    let mut store = FakeState { fail_upload: true, ..FakeState::new(false) };
    store.assets.insert("fingerprints.txt".into(), format!("{}\n", "a".repeat(64)).into_bytes());
    store.set_assets(LEDGER, json!([{"id": 1, "name": "fingerprints.txt", "size": 65}]));
    let temp = Temp::new();
    let path = temp.0.join("fingerprints.txt");
    state::load(&mut store, "ledger", &path).unwrap();
    assert!(state::save(&mut store, "ledger", &path, "ledger", "99", "1").is_err());
    assert!(store.deleted.is_empty());
    assert!(store.assets.contains_key("fingerprints.txt"));
}

#[test]
fn invalid_latest_fails_closed_and_does_not_replace_file() {
    let mut store = FakeState::new(false);
    store.assets.insert("fingerprints-1-1-ledger.txt".into(), b"not-a-hash\n".to_vec());
    store.set_assets(LEDGER, json!([{"id": 1, "name": "fingerprints-1-1-ledger.txt", "size": 11}]));
    let temp = Temp::new();
    let path = temp.file("fingerprints.txt", "existing");
    assert!(state::load(&mut store, "ledger", &path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing");
}

#[test]
fn empty_latest_asset_is_never_a_silent_reset() {
    let mut store = FakeState::new(false);
    store.set_assets(LEDGER, json!([{"id": 1, "name": "fingerprints.txt", "size": 0}]));
    let temp = Temp::new();
    assert!(state::load(&mut store, "ledger", &temp.0.join("x.txt")).is_err());
}

#[test]
fn backlog_prepared_final_crash_recovery_and_retention() {
    let mut store = FakeState::new(true);
    let temp = Temp::new();
    let path = temp.0.join("backlog.json");
    state::load(&mut store, "backlog", &path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"pending\":[]}\n");
    for n in 0..9 {
        let phase = if n % 2 == 0 { "prepared" } else { "final" };
        state::save(&mut store, "backlog", &path, phase, &(n + 1).to_string(), "1").unwrap();
    }
    assert_eq!(store.assets_of("community-learning-backlog").len(), state::KEEP);
    assert_eq!(store.deleted.len(), 9 - state::KEEP);
    state::load(&mut store, "backlog", &path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"pending\":[]}\n");
}

#[test]
fn versioned_decisions_round_trip_and_corruption_guards() {
    let mut store = FakeState::new(true);
    let temp = Temp::new();
    let path = temp.0.join("decisions.json");
    state::load(&mut store, "decisions", &path).unwrap();
    assert_eq!(matchlab_community::util::read_json(&path).unwrap(), j(json!({"decisions": {}})));
    let payload = json!({"decisions": {"a".repeat(64): {"status": "regression", "signature": "b".repeat(64),
                                                         "checked_at": "2026-11-11T12:00:00+00:00"}}});
    std::fs::write(&path, payload.to_string()).unwrap();
    state::save(&mut store, "decisions", &path, "final", "901", "1").unwrap();
    state::load(&mut store, "decisions", &path).unwrap();
    assert_eq!(matchlab_community::util::read_json(&path).unwrap(), j(payload));
    assert!(state::normalize("decisions", br#"{"decisions":{"bad":{}}}"#).is_err());
    assert!(state::normalize("decisions", br#"{"pending":[]}"#).is_err());
}

#[test]
fn corrupt_inputs_are_not_uploaded() {
    let doubled = format!("{0}\n{0}\n", "a".repeat(64));
    for raw in [b"\n".as_slice(), b"nothex\n", doubled.as_bytes()] {
        assert!(state::normalize("ledger", raw).is_err());
    }
    for raw in [b"{}".as_slice(), br#"{"pending":"bad"}"#, br#"{"pending":[{"fingerprint":"bad"}]}"#] {
        assert!(state::normalize("backlog", raw).is_err());
    }
}

#[test]
fn normalized_state_matches_python_bytes() {
    let raw = format!("{}\n{}\n{}\n", state::HEADER, "b".repeat(64), "a".repeat(64));
    assert_eq!(state::normalize("ledger", raw.as_bytes()).unwrap(), format!("{}\n{}\n", "a".repeat(64), "b".repeat(64)).into_bytes());
    let backlog = format!(r#"{{"pending": [{{"z": "é", "fingerprint": "{}"}}]}}"#, "a".repeat(64));
    assert_eq!(
        String::from_utf8(state::normalize("backlog", backlog.as_bytes()).unwrap()).unwrap(),
        format!("{{\"pending\":[{{\"fingerprint\":\"{}\",\"z\":\"é\"}}]}}\n", "a".repeat(64))
    );
    assert!(state::asset_name("backlog", "backlog-12-1-final.json"));
    assert!(state::asset_name("backlog", "backlog.json"));
    assert!(!state::asset_name("backlog", "backlog-12-1-other.json"));
    assert!(!state::asset_name("backlog", "backlog--1-final.json"));
}

// ---- issue intake -------------------------------------------------------

fn issue(number: i64, days: i64, author: &str, comments: i64, title: Option<&str>) -> Json {
    let title = title.map(str::to_owned).unwrap_or_else(|| format!("[community-learning] {}", day(days)));
    j(json!({"number": number, "title": title, "user": {"login": author}, "state": "open",
             "updated_at": iso(days), "comments": comments}))
}

struct IntakeApi {
    rows: Vec<Json>,
    comments: BTreeMap<i64, Vec<Json>>,
    writes: Vec<(String, String, Json)>,
    paths: Vec<String>,
}

impl GitHub for IntakeApi {
    fn repo(&self) -> &str {
        REPO
    }

    fn send(&mut self, method: &str, route: &str, payload: Option<&Json>, _: bool) -> Result<Json> {
        self.writes.push((method.into(), route.into(), payload.cloned().unwrap_or(Json::Null)));
        Ok(Json::Null)
    }

    fn pages(&mut self, path: &str) -> Result<Vec<Json>> {
        self.paths.push(path.into());
        if path.starts_with("/issues?") {
            return Ok(self.rows.clone());
        }
        let number: i64 = path.split('/').nth(2).unwrap().parse().unwrap();
        Ok(self.comments[&number].clone())
    }

    fn gh(&mut self, _: &[String]) -> Result<()> {
        unreachable!()
    }
}

fn intake_api(rows: Vec<Json>, comments: Vec<(i64, Vec<Json>)>) -> IntakeApi {
    IntakeApi { rows, comments: comments.into_iter().collect(), writes: vec![], paths: vec![] }
}

#[test]
fn reads_today_and_old_active_thread_without_first_page_limit() {
    let mut rows: Vec<Json> = (0..100).map(|n| issue(n, 10, BOT, 0, Some("Unrelated PR"))).collect();
    rows.extend([issue(101, 0, BOT, 1, None), issue(102, 33, BOT, 1, None), issue(103, 70, BOT, 1, None), issue(104, 10, "not-bot", 1, None)]);
    let mut api = intake_api(
        rows,
        vec![
            (101, vec![j(json!({"body": "today", "created_at": iso(0)}))]),
            (102, vec![j(json!({"body": "late", "created_at": iso(33)}))]),
        ],
    );
    let result = intake::collect(&mut api, now(), true).unwrap();
    let bodies: Vec<&str> = result.iter().map(|part| part[0].field("body").as_str().unwrap()).collect();
    assert_eq!(bodies, ["today", "late"]);
    assert!(api.writes.is_empty());
    assert!(api.paths.contains(&"/issues/102/comments".to_owned()));
}

#[test]
fn creates_daily_thread_only_when_no_official_one_exists() {
    let mut api = intake_api(vec![issue(1, 0, "someone-else", 0, None)], vec![]);
    assert!(intake::collect(&mut api, now(), true).unwrap().is_empty());
    assert_eq!((api.writes[0].0.as_str(), api.writes[0].1.as_str()), ("POST", "/issues"));
    assert!(api.writes[0].2.field("title").str_eq("[community-learning] 2026-11-11"));
}

#[test]
fn oversized_intake_fails_without_partial_output() {
    let spam = vec![j(json!({"body": "spam", "created_at": iso(0)})); intake::MAX_COMMENTS + 1];
    let mut api = intake_api(vec![issue(1, 10, BOT, 1, None)], vec![(1, spam)]);
    assert!(intake::collect(&mut api, now(), false).is_err());
}

#[test]
fn no_fake_bot_issues_or_pull_requests() {
    let legitimate = issue(1, 10, BOT, 1, None);
    assert!(intake::official(&legitimate));
    for bad in [
        with(&legitimate, "pull_request", j(json!({"url": "example"}))),
        with(&legitimate, "user", j(json!({"login": "imposter"}))),
        with(&legitimate, "title", Json::from("[community-learning] tomorrow")),
        with(&legitimate, "number", Json::from("1")),
    ] {
        assert!(!intake::official(&bad));
    }
}

// ---- cleanup ------------------------------------------------------------

fn cleanup_issue(number: i64, days: i64, extra: serde_json::Value) -> Json {
    let created = iso(days);
    let mut row = j(json!({"number": number, "title": format!("{}{}", cc::ISSUE_PREFIX, day(days)), "state": "open",
                           "user": {"login": BOT}, "created_at": created, "updated_at": created,
                           "comments": 0, "labels": []}));
    for (key, value) in j(extra).as_obj().unwrap() {
        row = with(&row, key, value.clone());
    }
    row
}

fn pull(number: i64, branch: &str, state: &str, extra: serde_json::Value) -> Json {
    let mut row = j(json!({
        "number": number, "title": cc::PR_TITLE, "body": cc::PR_SIGNATURE, "user": {"login": BOT},
        "state": state, "draft": false, "created_at": iso(60), "updated_at": iso(40),
        "closed_at": if state == "closed" { Some(iso(9)) } else { None }, "labels": [], "requested_reviewers": [],
        "head": {"ref": branch, "sha": SHA, "repo": {"full_name": REPO}}, "base": {"ref": "main"},
    }));
    for (key, value) in j(extra).as_obj().unwrap() {
        row = with(&row, key, value.clone());
    }
    row
}

const BRANCH: &str = "bot/community-regex-20260901";

fn open_pull() -> Json {
    pull(3, BRANCH, "open", json!({}))
}

fn branch(name: &str) -> Json {
    j(json!({"name": name, "protected": false, "commit": {"sha": SHA}}))
}

#[derive(Default)]
struct CleanupApi {
    issues: Vec<Json>,
    prs: Vec<Json>,
    branches: Vec<Json>,
    reviews: Vec<Json>,
    writes: Vec<(String, String)>,
    edited_issue: Option<Json>,
    edited_pull: Option<Json>,
    reopened: bool,
}

impl GitHub for CleanupApi {
    fn repo(&self) -> &str {
        REPO
    }

    fn pages(&mut self, path: &str) -> Result<Vec<Json>> {
        Ok(if path.starts_with("/issues?") {
            self.issues.clone()
        } else if path.starts_with("/pulls?") {
            self.prs.clone()
        } else if path == "/branches" {
            self.branches.clone()
        } else if path.starts_with("/pulls/") && path.ends_with("/reviews") {
            self.reviews.clone()
        } else {
            panic!("{path}")
        })
    }

    fn send(&mut self, method: &str, path: &str, _: Option<&Json>, _: bool) -> Result<Json> {
        if method != "GET" {
            self.writes.push((method.into(), path.into()));
            return Ok(j(json!({})));
        }
        let number = |prefix: &str| path.strip_prefix(prefix).and_then(|n| n.parse::<i128>().ok());
        if let Some(number) = number("/issues/") {
            if let Some(edited) = &self.edited_issue {
                return Ok(edited.clone());
            }
            return Ok(self.issues.iter().find(|row| row.field("number") == &Json::Int(number)).unwrap().clone());
        }
        if let Some(number) = number("/pulls/") {
            if let Some(edited) = &self.edited_pull {
                return Ok(edited.clone());
            }
            return Ok(self.prs.iter().find(|row| row.field("number") == &Json::Int(number)).unwrap().clone());
        }
        if let Some(name) = path.strip_prefix("/branches/") {
            return Ok(self.branches.iter().find(|row| row.field("name").str_eq(name)).unwrap().clone());
        }
        if path.starts_with("/pulls?state=open&head=") {
            return Ok(if self.reopened { j(json!([{"number": 3}])) } else { j(json!([])) });
        }
        if path.starts_with("/git/ref/heads/") {
            return Ok(j(json!({"object": {"sha": SHA}})));
        }
        if path == format!("/commits/{SHA}") {
            return Ok(j(json!({"author": {"login": BOT},
                "commit": {"author": {"name": BOT, "date": iso(40)}, "committer": {"name": BOT, "date": iso(40)}}})));
        }
        panic!("{method} {path}");
    }

    fn gh(&mut self, _: &[String]) -> Result<()> {
        unreachable!()
    }
}

#[test]
fn only_old_bot_daily_threads_and_no_unread_late_comments() {
    let base = || cleanup_issue(1, 28, json!({}));
    assert!(cc::daily_issue(&base(), now()));
    assert!(!cc::daily_issue(&cleanup_issue(1, 14, json!({})), now()));
    for (key, value) in [
        ("user", json!({"login": "Dizzybog"})),
        ("title", json!("Parser bug")),
        ("labels", json!([{"name": cc::KEEP_LABEL}])),
        ("updated_at", json!(iso(1))),
        ("pull_request", json!({"url": "..."})),
    ] {
        assert!(!cc::daily_issue(&with(&base(), key, j(value)), now()), "{key}");
    }
    assert!(!cc::daily_issue(&cleanup_issue(1, 28, json!({"comments": 2, "updated_at": iso(7)})), now()));
    assert!(cc::daily_issue(&cleanup_issue(1, 28, json!({"comments": 2, "updated_at": iso(16)})), now()));
}

#[test]
fn only_unreviewed_signed_stale_bot_parser_prs() {
    assert!(cc::stale_pr(&open_pull(), REPO, now()));
    for (key, value) in [
        ("user", json!({"login": "Dizzybog"})),
        ("title", json!("feat(parser): human change")),
        ("body", json!("arbitrary text")),
        ("updated_at", json!(iso(4))),
        ("draft", json!(true)),
        ("requested_reviewers", json!([{"login": "maintainer"}])),
        ("head", json!({"ref": "fix/parser", "sha": SHA, "repo": {"full_name": REPO}})),
        ("labels", json!([{"name": cc::KEEP_LABEL}])),
        ("head", json!({"ref": BRANCH, "sha": SHA, "repo": {"full_name": "attacker/fork"}})),
    ] {
        assert!(!cc::stale_pr(&with(&open_pull(), key, j(value)), REPO, now()), "{key}");
    }
}

#[test]
fn branch_rules_protect_active_and_manual_refs() {
    let old = branch(BRANCH);
    let closed = pull(3, BRANCH, "closed", json!({}));
    assert_eq!(cc::stale_branch(&old, &[open_pull()], REPO, now()), None);
    assert_eq!(cc::stale_branch(&old, std::slice::from_ref(&closed), REPO, now()), Some("review-closed"));
    assert_eq!(cc::stale_branch(&old, &[with(&closed, "closed_at", Json::from(iso(3)))], REPO, now()), None);
    let moved = with(&closed, "head", j(json!({"ref": BRANCH, "sha": "b".repeat(40), "repo": {"full_name": REPO}})));
    assert_eq!(cc::stale_branch(&old, &[moved], REPO, now()), None);
    assert_eq!(cc::stale_branch(&old, &[], REPO, now()), Some("abandoned"));
    assert_eq!(cc::stale_branch(&branch("bot/community-regex-20261110"), &[], REPO, now()), None);
    assert_eq!(cc::stale_branch(&branch("fix/parser-thing"), &[], REPO, now()), None);
    assert_eq!(cc::stale_branch(&with(&old, "protected", Json::Bool(true)), &[], REPO, now()), None);
    assert_eq!(cc::bot_branch_day("bot/community-regex-20260901-123-1"), Some("20260901"));
    assert_eq!(cc::bot_branch_day("bot/community-regex-20260901-1-2-3"), None);
    assert_eq!(cc::bot_branch_day("bot/community-regex-20260901-"), None);
}

fn writes(api: &CleanupApi) -> Vec<(&str, &str)> {
    api.writes.iter().map(|(method, path)| (method.as_str(), path.as_str())).collect()
}

#[test]
fn no_side_effects_in_dry_run_and_exact_apply_targets() {
    let mut api = CleanupApi {
        issues: vec![cleanup_issue(1, 28, json!({})), cleanup_issue(2, 2, json!({}))],
        prs: vec![open_pull(), pull(4, "fix/parser", "open", json!({})), pull(5, BRANCH, "closed", json!({}))],
        branches: vec![branch(BRANCH), branch("fix/parser"), branch("bot/community-regex-20261110")],
        ..CleanupApi::default()
    };
    let planned = cc::run(&mut api, now(), false).unwrap();
    assert_eq!(planned.field("daily_issues"), &j(json!([1])));
    assert_eq!(planned.field("stale_review_prs"), &j(json!([3])));
    assert_eq!(planned.field("bot_branches"), &j(json!([])));
    assert!(api.writes.is_empty());
    let result = cc::run(&mut api, now(), true).unwrap();
    assert_eq!(result.field("daily_issues"), &j(json!([1])));
    assert_eq!(writes(&api), [("PATCH", "/issues/1"), ("PATCH", "/pulls/3"), ("POST", "/issues/3/comments")]);
}

#[test]
fn merged_bot_branch_deletes_only_after_grace() {
    let mut api = CleanupApi {
        prs: vec![pull(3, BRANCH, "closed", json!({}))],
        branches: vec![branch(BRANCH)],
        ..CleanupApi::default()
    };
    let plan = cc::run(&mut api, now(), true).unwrap();
    assert_eq!(plan.field("bot_branches"), &j(json!([BRANCH])));
    assert_eq!(writes(&api), [("DELETE", "/git/refs/heads/bot/community-regex-20260901")]);
}

#[test]
fn latest_issue_update_prevents_stale_snapshot_closure() {
    let mut api = CleanupApi {
        issues: vec![cleanup_issue(1, 28, json!({}))],
        edited_issue: Some(cleanup_issue(1, 28, json!({"updated_at": iso(1)}))),
        ..CleanupApi::default()
    };
    cc::run(&mut api, now(), true).unwrap();
    assert!(api.writes.is_empty());
}

#[test]
fn latest_pr_update_prevents_stale_snapshot_closure() {
    let mut api = CleanupApi {
        prs: vec![open_pull()],
        edited_pull: Some(with(&open_pull(), "updated_at", Json::from(iso(1)))),
        ..CleanupApi::default()
    };
    cc::run(&mut api, now(), true).unwrap();
    assert!(api.writes.is_empty());
}

#[test]
fn reopened_pr_blocks_branch_deletion() {
    let mut api = CleanupApi {
        prs: vec![pull(3, BRANCH, "closed", json!({}))],
        branches: vec![branch(BRANCH)],
        reopened: true,
        ..CleanupApi::default()
    };
    cc::run(&mut api, now(), true).unwrap();
    assert!(api.writes.is_empty());
}

#[test]
fn human_approval_blocks_automatic_pr_close() {
    let mut api = CleanupApi {
        prs: vec![open_pull()],
        reviews: vec![j(json!({"state": "APPROVED", "user": {"login": "Dizzybog"}}))],
        ..CleanupApi::default()
    };
    let plan = cc::run(&mut api, now(), true).unwrap();
    assert_eq!(plan.field("stale_review_prs"), &j(json!([])));
    assert!(api.writes.is_empty());
}
