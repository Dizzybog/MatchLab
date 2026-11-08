//! Read-only, bearer-protected polling feed for production parser diagnostics.
//! Reads the existing bounded dashboard history; never triggers parsing.
use axum::{
    extract::{Path as UrlPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, path::Path, time::Duration};

use crate::AppState;

const MAX_PAGE: usize = 100;
const MIN_TOKEN_BYTES: usize = 32;

#[derive(Debug, Deserialize)]
pub struct FeedQuery {
    after_id: Option<i64>,
    limit: Option<usize>,
    suspect_only: Option<bool>,
    include_details: Option<bool>,
}

fn response(status: StatusCode, value: Value) -> Response {
    let mut res = (status, Json(value)).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

// A missing or weak key leaves this optional route unavailable. No keys are
// accepted in URLs, where they would leak into access logs and browser history.
fn authorized(headers: &HeaderMap) -> Result<(), StatusCode> {
    let Ok(expected) = env::var("MATCHLAB_DEBUG_API_TOKEN") else {
        return Err(StatusCode::NOT_FOUND);
    };
    if expected.as_bytes().len() < MIN_TOKEN_BYTES {
        return Err(StatusCode::NOT_FOUND);
    }
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided.is_some_and(|value| constant_time_eq(value.as_bytes(), expected.as_bytes())) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (&a, &b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn guard(headers: &HeaderMap) -> Result<(), Response> {
    authorized(headers).map_err(|status| {
        let message = if status == StatusCode::NOT_FOUND {
            "debug feed is disabled"
        } else {
            "bearer token required"
        };
        response(status, json!({"error":message}))
    })
}

fn open_history(path: &Path) -> Result<Connection, String> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    Ok(db)
}

fn bounds(db: &Connection) -> Result<(Option<i64>, Option<i64>, i64), String> {
    db.query_row("SELECT MIN(id),MAX(id),COUNT(*) FROM requests", [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .map_err(|error| error.to_string())
}

// Deliberately conservative: anomalies are triage hints, not proof of an
// incorrect identity. Verified anime episode remapping is not classified as
// an explicit-marker mismatch, because it legitimately changes coordinates.
fn explicit_sxe(title: &str) -> Option<(i64, i64)> {
    let bytes = title.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i].to_ascii_uppercase() != b'S'
            || (i > 0 && bytes[i - 1].is_ascii_alphanumeric())
        {
            continue;
        }
        let mut cursor = i + 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() && cursor - i <= 3 {
            cursor += 1;
        }
        if cursor == i + 1 || cursor - i > 4
            || bytes.get(cursor).map(|b| b.to_ascii_uppercase()) != Some(b'E')
        {
            continue;
        }
        let start = cursor + 1;
        cursor = start;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() && cursor - start < 5 {
            cursor += 1;
        }
        if cursor == start || bytes.get(cursor).is_some_and(u8::is_ascii_alphanumeric) {
            continue;
        }
        if let (Ok(season), Ok(episode)) = (
            title[i + 1..start - 1].parse::<i64>(),
            title[start..cursor].parse::<i64>(),
        ) {
            if season > 0 && episode > 0 {
                return Some((season, episode));
            }
        }
    }
    None
}

// A compact Spanish caption marker: Cap.103 -> S01E03, Cap.1004 -> S10E04.
// Interpret only complete 3- or 4-digit chapter codes, not arbitrary numbers.
fn explicit_spanish_cap(title: &str) -> Option<(i64, i64)> {
    let bytes = title.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if (i > 0 && bytes[i - 1].is_ascii_alphanumeric())
            || bytes[i].to_ascii_lowercase() != b'c'
            || bytes[i + 1].to_ascii_lowercase() != b'a'
            || bytes[i + 2].to_ascii_lowercase() != b'p'
        {
            continue;
        }
        let mut cursor = i + 3;
        while cursor < bytes.len()
            && matches!(bytes[cursor], b'.' | b':' | b'_' | b'-' | b' ')
            && cursor < i + 7
        {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit()
            && cursor - start < 5
        {
            cursor += 1;
        }
        if !(3..=4).contains(&(cursor - start))
            || bytes.get(cursor).is_some_and(u8::is_ascii_alphanumeric)
        {
            continue;
        }
        if let Ok(number) = title[start..cursor].parse::<i64>() {
            let (season, episode) = (number / 100, number % 100);
            if (1..=99).contains(&season) && (1..=99).contains(&episode) {
                return Some((season, episode));
            }
        }
    }
    None
}

pub(crate) fn community_alerts(title:&str,status:&str,details:&Value)->Vec<Value>{
    alerts(title,status,details)
}

fn alerts(title: &str, status: &str, details: &Value) -> Vec<Value> {
    let mut findings = Vec::new();
    if status == "error" {
        findings.push(json!({
            "code":"matching_error","severity":"error",
            "message":"The request failed before a match was returned"
        }));
    }
    let selected = &details["selected"];
    let parsed = &details["parsed"];
    let season = selected["season"].as_i64();
    let episode = selected["episode"].as_i64();
    let is_series = selected["category"] == "series";
    if is_series && (season.is_some_and(|v| v <= 0) || episode.is_some_and(|v| v <= 0)) {
        findings.push(json!({
            "code":"invalid_selected_coordinate","severity":"high",
            "message":"Selected series has a nonpositive season or episode"
        }));
    }
    if is_series && season.is_some_and(|v| v > 60) {
        findings.push(json!({
            "code":"unusual_season_number","severity":"review",
            "message":"Selected season exceeds 60; check for an absolute anime episode"
        }));
    }
    if let Some((explicit_season, explicit_episode)) =
        explicit_sxe(title).or_else(|| explicit_spanish_cap(title))
    {
        if status == "unmatched" {
            findings.push(json!({
                "code":"unmatched_explicit_episode","severity":"review",
                "message":"An explicitly numbered release did not match",
                "raw_season":explicit_season,"raw_episode":explicit_episode
            }));
        }
        let verified_remap = selected.get("anime")
            .is_some_and(|anime| !anime.is_null())
            || selected.get("episode_range")
                .and_then(|range| range.get("complete"))
                .and_then(Value::as_bool) == Some(true);
        if is_series && !verified_remap
            && (season != Some(explicit_season) || episode != Some(explicit_episode))
        {
            findings.push(json!({
                "code":"explicit_episode_mismatch","severity":"review",
                "message":"Selected season/episode disagrees with an explicit release episode marker",
                "raw_season":explicit_season,"raw_episode":explicit_episode,
                "selected_season":season,"selected_episode":episode
            }));
        }
        let parsed_seasons = parsed["seasons"].as_array();
        let parsed_episodes = parsed["episodes"].as_array();
        let parser_missing = parsed_seasons.is_some_and(|a|
            !a.iter().any(|v| v.as_i64() == Some(explicit_season)))
            || parsed_episodes.is_some_and(|a|
                !a.iter().any(|v| v.as_i64() == Some(explicit_episode)));
        if parser_missing && !verified_remap && status != "error" {
            findings.push(json!({
                "code":"parser_marker_disagreement","severity":"review",
                "message":"The parser's season/episode arrays disagree with an explicit release episode marker",
                "raw_season":explicit_season,"raw_episode":explicit_episode
            }));
        }
    }
    // Unverified absolute-episode hints (e.g. "Stone Ocean 27") are useful
    // review candidates when a series was matched without episode coordinates.
    if is_series && episode.is_none()
        && parsed["anime_episode_candidate"]["episode"].as_i64().is_some()
        && selected.get("anime").is_none_or(Value::is_null)
    {
        findings.push(json!({
            "code":"unresolved_absolute_anime_episode","severity":"review",
            "message":"An absolute anime episode hint was not resolved to a selected episode",
            "candidate_episode":parsed["anime_episode_candidate"]["episode"]
        }));
    }
    if is_series {
        let end_season = selected["season_end"].as_i64();
        let end_episode = selected["episode_end"].as_i64();
        if let (Some(s), Some(e), Some(es), Some(ee)) =
            (season, episode, end_season, end_episode)
        {
            if (es, ee) < (s, e) {
                findings.push(json!({
                    "code":"reversed_selected_range","severity":"high",
                    "message":"Selected episode range ends before it starts"
                }));
            }
        }
    }
    findings
}

fn diagnostic_row(
    id: i64, created_at: String, title: String, status: String,
    imdb_id: Option<String>, match_source: Option<String>,
    duration_ms: i64, details: Value, include_details: bool,
) -> Value {
    let flags = alerts(&title, &status, &details);
    let mut item = json!({
        "id":id,"created_at":created_at,"title":title,"status":status,
        "imdb_id":imdb_id,"match_source":match_source,
        "duration_ms":duration_ms,"suspected":!flags.is_empty(),
        "alerts":flags,
        "parsed":details.get("parsed"),"selected":details.get("selected"),
        "reason":details.get("reason"),
    });
    if include_details {
        item["details"] = details;
    }
    item
}

fn feed(
    path: &Path, after_id: i64, limit: usize, suspect_only: bool,
    include_details: bool,
) -> Result<Value, String> {
    let db = open_history(path)?;
    let (oldest_id, latest_id, retained) = bounds(&db)?;
    let mut stmt = db.prepare(
        "SELECT id,created_at,title,status,imdb_id,match_source,duration_ms,details
         FROM requests WHERE id > ?1 ORDER BY id ASC LIMIT ?2"
    ).map_err(|error| error.to_string())?;
    let rows = stmt.query_map(params![after_id, (limit + 1) as i64], |row| {
        let details: String = row.get(7)?;
        Ok((
            row.get::<_, i64>(0)?,row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, i64>(6)?,
            serde_json::from_str::<Value>(&details).unwrap_or(Value::Null),
        ))
    }).map_err(|error| error.to_string())?;
    let collected = rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    let has_more = collected.len() > limit;
    let mut next_after_id = after_id;
    let mut items = Vec::new();
    for (id, created_at, title, status, imdb_id, match_source, duration_ms, details)
        in collected.into_iter().take(limit)
    {
        next_after_id = id;
        let item = diagnostic_row(
            id, created_at, title, status, imdb_id, match_source,
            duration_ms, details, include_details,
        );
        if !suspect_only || item["suspected"] == true {
            items.push(item);
        }
    }
    Ok(json!({
        "items":items,"after_id":after_id,"next_after_id":next_after_id,
        "has_more":has_more,"latest_id":latest_id,"oldest_id":oldest_id,
        "retained":retained,"retention_limit":1000,
        "cursor_expired":after_id > 0 &&
            oldest_id.is_some_and(|oldest| oldest > after_id.saturating_add(1)),
        "notice":"Anomaly flags are heuristics, not verified parser defects"
    }))
}

fn single(path: &Path, id: i64) -> Result<Option<Value>, String> {
    let db = open_history(path)?;
    db.query_row(
        "SELECT id,created_at,title,status,imdb_id,match_source,duration_ms,details
         FROM requests WHERE id=?1",
        params![id],
        |row| {
            let details: String = row.get(7)?;
            Ok(diagnostic_row(
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?,
                serde_json::from_str(&details).unwrap_or(Value::Null),
                true,
            ))
        },
    ).optional().map_err(|error| error.to_string())
}

pub async fn list(
    State(state): State<AppState>, headers: HeaderMap, Query(query): Query<FeedQuery>,
) -> Response {
    if let Err(denied) = guard(&headers) {
        return denied;
    }
    let after_id = query.after_id.unwrap_or(0);
    let limit = query.limit.unwrap_or(100);
    if after_id < 0 || !(1..=MAX_PAGE).contains(&limit) {
        return response(StatusCode::BAD_REQUEST, json!({
            "error":"after_id must be nonnegative and limit must be between 1 and 100"
        }));
    }
    let path = state.history;
    let result = tokio::task::spawn_blocking(move || {
        feed(&path, after_id, limit,
            query.suspect_only.unwrap_or(false),
            query.include_details.unwrap_or(false))
    }).await;
    match result {
        Ok(Ok(value)) => response(StatusCode::OK, value),
        Ok(Err(error)) => {
            tracing::warn!(%error, "parsing debug feed failed");
            response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"history unavailable"}))
        }
        Err(error) => {
            tracing::warn!(%error, "parsing debug feed worker failed");
            response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"history unavailable"}))
        }
    }
}

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(denied) = guard(&headers) {
        return denied;
    }
    let path = state.history;
    let result = tokio::task::spawn_blocking(move || {
        let db = open_history(&path)?;
        let (oldest_id, latest_id, retained) = bounds(&db)?;
        Ok::<_, String>(json!({
            "oldest_id":oldest_id,"latest_id":latest_id,
            "retained":retained,"retention_limit":1000
        }))
    }).await;
    match result {
        Ok(Ok(value)) => response(StatusCode::OK, value),
        Ok(Err(error)) => {
            tracing::warn!(%error, "parsing debug status failed");
            response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"history unavailable"}))
        }
        Err(error) => {
            tracing::warn!(%error, "parsing debug status worker failed");
            response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"history unavailable"}))
        }
    }
}

pub async fn detail(
    State(state): State<AppState>, headers: HeaderMap, UrlPath(id): UrlPath<i64>,
) -> Response {
    if let Err(denied) = guard(&headers) {
        return denied;
    }
    if id < 1 {
        return response(StatusCode::BAD_REQUEST, json!({"error":"id must be positive"}));
    }
    let result = tokio::task::spawn_blocking(move || single(&state.history, id)).await;
    match result {
        Ok(Ok(Some(value))) => response(StatusCode::OK, value),
        Ok(Ok(None)) => response(StatusCode::NOT_FOUND, json!({"error":"request expired or not found"})),
        Ok(Err(error)) => {
            tracing::warn!(%error, "parsing debug detail failed");
            response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"history unavailable"}))
        }
        Err(error) => {
            tracing::warn!(%error, "parsing debug detail worker failed");
            response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":"history unavailable"}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_history;
    use std::path::PathBuf;

    fn history_db() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "matchlab-debug-feed-{}-{nanos}.sqlite3",std::process::id()
        ));
        dashboard_history::initialize(&path).unwrap();
        path
    }

    #[test]
    fn bearer_comparison_requires_exact_bytes() {
        assert!(constant_time_eq(b"long-secret", b"long-secret"));
        assert!(!constant_time_eq(b"long-secret", b"long-SeCreT"));
        assert!(!constant_time_eq(b"long-secret", b"short"));
    }

    #[test]
    fn explicit_marker_recognizes_release_boundaries() {
        assert_eq!(explicit_sxe("Show.S02E041.1080p"),Some((2,41)));
        assert_eq!(explicit_sxe("Show S36E41.mkv"),Some((36,41)));
        assert_eq!(explicit_sxe("ABCShowS02E01"),None);
        assert_eq!(explicit_sxe("Show S02E01extra"),None);
        assert_eq!(explicit_sxe("No season marker"),None);
    }

    #[test]
    fn explicit_coordinate_changes_are_flagged_without_verified_crosswalk() {
        let example=json!({
            "parsed":{"seasons":[3],"episodes":[9]},
            "selected":{"category":"series","season":3,"episode":9}
        });
        let findings=alerts("Example.S03E08.mkv","matched",&example);
        assert!(findings.iter().any(|a|a["code"]=="explicit_episode_mismatch"));
        assert!(findings.iter().any(|a|a["code"]=="parser_marker_disagreement"));
        let verified=json!({
            "parsed":{"seasons":[3],"episodes":[9]},
            "selected":{"category":"series","season":3,"episode":9,
                "anime":{"asid":"123:8"}}
        });
        assert!(alerts("Example.S03E08.mkv","matched",&verified).is_empty());
    }

    #[test]
    fn feed_cursor_advances_over_nonsuspects_and_is_read_only() {
        let path=history_db();
        for (title,season,episode) in [
            ("Show.S01E01",1,1),("Show.S01E02",1,1),("Show.S01E03",1,3),
        ] {
            let selected=json!({"imdb_id":"tt1234567","category":"series",
                "season":season,"episode":episode});
            let details=json!({"parsed":{"seasons":[1],"episodes":[episode]},
                "selected":selected,"reason":"fixture"});
            dashboard_history::record(&path,title,"matched",Some(&selected),&details,4);
        }
        let first=feed(&path,0,1,true,false).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(),0);
        assert_eq!(first["next_after_id"],1);
        assert_eq!(first["has_more"],true);
        let second=feed(&path,1,2,true,false).unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(),1);
        assert_eq!(second["items"][0]["id"],2);
        assert_eq!(second["items"][0]["suspected"],true);
        assert_eq!(second["next_after_id"],3);
        assert_eq!(second["has_more"],false);
        assert_eq!(single(&path,2).unwrap().unwrap()["details"]["reason"],"fixture");
        assert!(single(&path,9999).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn spanish_cap_markers_are_audited() {
        assert_eq!(explicit_spanish_cap("Show [Cap.103]"),Some((1,3)));
        assert_eq!(explicit_spanish_cap("The Walking Dead Cap.1004"),Some((10,4)));
        assert_eq!(explicit_spanish_cap("Captain America 1004"),None);
        let wrong=json!({"parsed":{"seasons":[10],"episodes":[3]},
            "selected":{"category":"series","season":10,"episode":3}});
        let flags=alerts("Show.Temporada.10.Cap.1004","matched",&wrong);
        assert!(flags.iter().any(|a|a["code"]=="explicit_episode_mismatch"));
    }

    #[test]
    fn unresolved_absolute_anime_episode_hint_is_reported() {
        let details=json!({
            "parsed":{"anime_episode_candidate":{"title":"Stone Ocean","episode":27}},
            "selected":{"category":"series","season":1}
        });
        assert!(alerts("Stone Ocean 27","matched",&details).iter()
            .any(|a|a["code"]=="unresolved_absolute_anime_episode"));
    }

    #[test]
    fn range_and_invalid_coordinates_are_detected() {
        let details=json!({"selected":{"category":"series","season":61,"episode":2,
            "season_end":60,"episode_end":5}});
        let found=alerts("Show season pack","matched",&details);
        assert!(found.iter().any(|a|a["code"]=="unusual_season_number"));
        assert!(found.iter().any(|a|a["code"]=="reversed_selected_range"));
    }
}
