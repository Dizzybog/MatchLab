//! Opt-in GitHub-only community anomaly submission.
//! This does NOT train from self-reported predictions or affect match results.
use axum::Json;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, fs, path::{Path,PathBuf}, time::{Duration,SystemTime,UNIX_EPOCH}};

const REPO: &str = "Dizzybog/MatchLab";
const PROTOCOL: u32 = 1;
const LEARNING_EPOCH: u32 = 1;
const DAILY_LIMIT: u32 = 4;
const MAX_SCAN: i64 = 100;
const MAX_PAGES: usize = 5;
const POLL_SECONDS: u64 = 300;

// Build provenance is embedded by the official GitHub Docker workflow. A
// runtime environment variable must never be allowed to override it.
fn build_sha() -> &'static str {
    option_env!("MATCHLAB_BUILD_SHA").unwrap_or("")
}
fn valid_sha(sha: &str) -> bool {
    sha.len()==40 && sha.bytes().all(|b|b.is_ascii_hexdigit())
}
fn consent() -> bool {
    env::var("MATCHLAB_COMMUNITY_LEARNING_ENABLED").ok()
        .is_some_and(|s|s.eq_ignore_ascii_case("true"))
}
fn token() -> Option<String> {
    let value=env::var("MATCHLAB_COMMUNITY_GITHUB_TOKEN").ok()?;
    (!value.trim().is_empty()).then_some(value)
}
fn ready() -> bool { consent() && token().is_some() && valid_sha(build_sha()) }
pub async fn status() -> Json<Value> {
    Json(json!({
        "enabled": consent(),
        "ready":ready(),
        "protocol":PROTOCOL,
        "learning_epoch":LEARNING_EPOCH,
        "build_sha":if valid_sha(build_sha()){Some(build_sha())}else{None},
        "upload":"github_issues",
        "max_reports_per_utc_day":DAILY_LIMIT,
        "note":"Reported anomalies are unverified; they are never training labels"
    }))
}

#[derive(Serialize,Deserialize,Default,Debug)]
struct Cursor {
    protocol:u32,
    epoch:u32,
    build_sha:String,
    after_id:i64,
    utc_day:u64,
    sent_today:u32,
}

fn now_day() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()/86400
}
fn latest_id(path:&Path)->Result<i64,String>{
    let db=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e|e.to_string())?;
    db.query_row("SELECT COALESCE(MAX(id),0) FROM requests",[],|r|r.get(0))
        .map_err(|e|e.to_string())
}
fn load(path:&Path,history:&Path)->Result<Cursor,String>{
    if let Ok(bytes)=fs::read(path)
        && let Ok(mut saved)=serde_json::from_slice::<Cursor>(&bytes)
        && saved.protocol==PROTOCOL && saved.epoch==LEARNING_EPOCH
        && saved.build_sha==build_sha() && saved.after_id>=0 {
        let today=now_day();
        if saved.utc_day!=today {saved.utc_day=today;saved.sent_today=0;}
        return Ok(saved);
    }
    // Deliberately start at the newest request on first opt-in, protocol
    // upgrade, schema epoch change or new executable. Never export backlog.
    Ok(Cursor{
        protocol:PROTOCOL,epoch:LEARNING_EPOCH,
        build_sha:build_sha().into(),after_id:latest_id(history)?,
        utc_day:now_day(),sent_today:0
    })
}
fn save(path:&Path,cursor:&Cursor)->Result<(),String>{
    if let Some(parent)=path.parent(){fs::create_dir_all(parent).map_err(|e|e.to_string())?;}
    let tmp=path.with_extension("tmp");
    fs::write(&tmp,serde_json::to_vec(cursor).map_err(|e|e.to_string())?)
        .map_err(|e|e.to_string())?;
    fs::rename(tmp,path).map_err(|e|e.to_string())
}
fn client()->Result<reqwest::blocking::Client,String>{
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(12))
        .user_agent("MatchLab-Community-Learning/1")
        .build().map_err(|e|e.to_string())
}
fn is_current(client:&reqwest::blocking::Client)->Result<bool,String>{
    let response=client.get(format!("https://api.github.com/repos/{REPO}/commits/main"))
        .header("Accept","application/vnd.github+json")
        .send().map_err(|e|e.to_string())?;
    if !response.status().is_success(){return Err(format!("GitHub latest commit HTTP {}",response.status()));}
    let value:Value=response.json().map_err(|e|e.to_string())?;
    Ok(value["sha"].as_str()==Some(build_sha()))
}

// Never publish paths, network addresses, usernames or likely credentials.
// GitHub Issues in a public repository are public and can be permanent.
fn safe_leaf(title:&str)->Option<String>{
    let title=title.trim();
    if title.is_empty()||title.len()>240
        || title.contains(['/', '\\','@','\n','\r','\0'])
        || title.chars().any(char::is_control){return None;}
    let lower=title.to_ascii_lowercase();
    if ["http:","https:","www.","magnet:","token=","apikey=",
        "password=","secret=","bearer ","private=","api_key="]
        .iter().any(|needle|lower.contains(needle)) {return None;}
    Some(title.to_owned())
}
fn trim_numbers(value:&Value,key:&str)->Value{
    let arr=value.get(key).and_then(Value::as_array);
    let nums:Vec<i64>=arr.into_iter().flatten()
        .take(12).filter_map(Value::as_i64).filter(|n|(0..=99_999).contains(n)).collect();
    json!(nums)
}
fn safe_report(item:&Value)->Option<Value>{
    if item["suspected"]!=true {return None;}
    let title=safe_leaf(item["title"].as_str()?)?;
    let alert_codes:Vec<&str>=item["alerts"].as_array()?
        .iter().filter_map(|a|a["code"].as_str())
        .filter(|code|matches!(*code,
            "matching_error"|"invalid_selected_coordinate"|"unusual_season_number"|
            "unmatched_explicit_episode"|"explicit_episode_mismatch"|
            "parser_marker_disagreement"|"unresolved_absolute_anime_episode"|
            "reversed_selected_range")).take(8).collect();
    if alert_codes.is_empty(){return None;}
    let parsed_title=item["parsed"]["title"].as_str()
        .and_then(safe_leaf);
    let selected_id=item["selected"]["imdb_id"].as_str()
        .filter(|s|s.len()>=9&&s.len()<=10&&s.starts_with("tt")
            && s[2..].bytes().all(|b|b.is_ascii_digit()));
    let status=item["status"].as_str()?;
    if !matches!(status,"matched"|"unmatched"|"error"){return None;}
    Some(json!({
        "protocol":PROTOCOL,"learning_epoch":LEARNING_EPOCH,
        "build_sha":build_sha(),"source":"matchlab-community",
        "title":title,"status":status,
        "alerts":alert_codes,
        "parsed":{"title":parsed_title,
            "seasons":trim_numbers(&item["parsed"],"seasons"),
            "episodes":trim_numbers(&item["parsed"],"episodes")},
        "selected":{"imdb_id":selected_id,
            "season":item["selected"]["season"].as_i64(),
            "episode":item["selected"]["episode"].as_i64()},
        "verified":false
    }))
}
fn submit(client:&reqwest::blocking::Client,auth:&str,report:&Value)->Result<(),String>{
    let body=json!({
        "title":"[community-learning] parser anomaly",
        "body":format!("<!-- matchlab-community-v1 -->\n{}\n",
            serde_json::to_string(report).map_err(|e|e.to_string())?)
    });
    let result=client.post(format!("https://api.github.com/repos/{REPO}/issues"))
        .header("Accept","application/vnd.github+json")
        .bearer_auth(auth).json(&body).send().map_err(|e|e.to_string())?;
    if result.status()!=reqwest::StatusCode::CREATED {
        return Err(format!("GitHub Issues HTTP {}",result.status()));
    }
    Ok(())
}
fn run_cycle(history:&Path,storage:&Path,auth:&str)->Result<(),String>{
    if !valid_sha(build_sha()){return Ok(());}
    let client=client()?;
    // The client fails closed on offline/old builds. GitHub must also reject
    // stale and spoofed reports; version claims alone are not proof of origin.
    if !is_current(&client)?{return Ok(());}
    let mut cursor=load(storage,history)?;
    if cursor.sent_today>=DAILY_LIMIT {
        save(storage,&cursor)?;
        return Ok(());
    }
    for _ in 0..MAX_PAGES {
        let db=Connection::open_with_flags(history,OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e|e.to_string())?;
        let mut stmt=db.prepare(
            "SELECT id,title,status,details FROM requests WHERE id>?1 ORDER BY id ASC LIMIT ?2"
        ).map_err(|e|e.to_string())?;
        let rows=stmt.query_map(rusqlite::params![cursor.after_id,MAX_SCAN],|r|
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,
                r.get::<_,String>(3)?))).map_err(|e|e.to_string())?;
        let entries=rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e|e.to_string())?;
        if entries.is_empty(){break;}
        let count=entries.len();
        for (id,title,status,details) in entries{
            let parsed:Value=serde_json::from_str(&details).unwrap_or(Value::Null);
            let flags=crate::debug_feed::community_alerts(&title,&status,&parsed);
            if !flags.is_empty() {
                let item=json!({"title":title,"status":status,
                    "suspected":true,"alerts":flags,
                    "parsed":parsed["parsed"],"selected":parsed["selected"]});
                if let Some(report)=safe_report(&item) {
                    submit(&client,auth,&report)?;
                    cursor.sent_today+=1;
                }
            }
            cursor.after_id=id;
            save(storage,&cursor)?;
            if cursor.sent_today>=DAILY_LIMIT{return Ok(());}
        }
        if count<MAX_SCAN as usize{break;}
    }
    Ok(())
}
pub fn start(history:PathBuf){
    if !ready(){
        if consent(){tracing::warn!("community learning disabled: requires GitHub token and official SHA-stamped image");}
        return;
    }
    let storage=env::var("MATCHLAB_COMMUNITY_STATE_FILE")
        .unwrap_or_else(|_|"/data/community-learning-state.json".to_string());
    let storage=PathBuf::from(storage);
    let Some(auth)=token()else{return};
    tokio::spawn(async move {
        loop {
            let h=history.clone();
            let state=storage.clone();
            let key=auth.clone();
            match tokio::task::spawn_blocking(move||run_cycle(&h,&state,&key)).await{
                Ok(Ok(()))=>{},
                Ok(Err(error))=>tracing::warn!(%error,"community report cycle unavailable"),
                Err(error)=>tracing::warn!(%error,"community report worker unavailable"),
            };
            tokio::time::sleep(Duration::from_secs(POLL_SECONDS)).await;
        }
    });
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn filenames_and_credentials_are_never_eligible_for_public_issues(){
        assert!(safe_leaf("Anime S01E02").is_some());
        for bad in ["Show/Season 1/file.mkv","C:\\Users\\me\\file.mkv",
            "user@example.com","https://example.com/a","token=secret",
            "magnet:?xt=123","   ","A\nB"]{
            assert!(safe_leaf(bad).is_none(),"{bad}");
        }
    }
    #[test]
    fn unverified_reports_never_embed_full_diagnostics(){
        let sample=json!({"title":"Bleach_338","status":"matched",
            "suspected":true,"alerts":[{"code":"unresolved_absolute_anime_episode"}],
            "parsed":{"title":"Bleach","seasons":[],"episodes":[338],"api_key":"DO_NOT_SHARE"},
            "selected":{"imdb_id":"tt0434665","season":null,"episode":null,
                "raw_path":"/private/path"}});
        let report=safe_report(&sample).unwrap();
        assert_eq!(report["verified"],false);
        assert!(report.to_string().contains("Bleach_338"));
        assert!(!report.to_string().contains("DO_NOT_SHARE"));
        assert!(!report.to_string().contains("/private/path"));
    }
    #[test]
    fn build_hash_is_strictly_a_full_commit_id(){
        assert!(!valid_sha("latest"));
        assert!(!valid_sha("unknown"));
        assert!(valid_sha("0000000000000000000000000000000000000000"));
    }
}