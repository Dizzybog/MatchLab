//! GitHub-only, opt-in community learning. Untrusted batches, never labels.
//! One 24-hour batch COMMENT per installation; GitHub Actions owns one daily Issue.
use axum::Json;
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::{env,fs,path::{Path,PathBuf},time::{Duration,SystemTime,UNIX_EPOCH}};

const REPO:&str="Dizzybog/MatchLab";
const PROTOCOL:u32=2;
const EPOCH:u32=1;
const DAY_SECS:u64=86_400;
const POLL_SECS:u64=300;
const PAGE_SIZE:i64=100;
const MAX_PAGES:usize=5;
const MAX_BATCH:usize=16;
const MAX_SEEN:i64=50_000;
const MAX_BODY:usize=16_384;
const MARKER:&str="<!-- matchlab-community-batch-v2 -->";

fn now()->u64{
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
fn build_sha()->&'static str{option_env!("MATCHLAB_BUILD_SHA").unwrap_or("")}
fn valid_sha(sha:&str)->bool{
    sha.len()==40 && sha.bytes().all(|b|b.is_ascii_hexdigit())
}
fn opted_in()->bool{
    env::var("MATCHLAB_COMMUNITY_LEARNING_ENABLED").ok()
        .is_some_and(|v|v.eq_ignore_ascii_case("true"))
}
fn github_token()->Option<String>{
    env::var("MATCHLAB_COMMUNITY_GITHUB_TOKEN").ok()
        .filter(|v|!v.trim().is_empty())
}
fn enabled()->bool{opted_in()&&github_token().is_some()&&valid_sha(build_sha())}
pub async fn status()->Json<Value>{
    Json(json!({"enabled":opted_in(),"ready":enabled(),
        "protocol":PROTOCOL,"learning_epoch":EPOCH,
        "build_sha":valid_sha(build_sha()).then_some(build_sha()),
        "upload":"one_daily_github_issue_with_opt_in_batch_comments",
        "batch_interval_hours":24,"max_reports_per_batch":MAX_BATCH,
        "older_build_reports":"accepted_as_unverified_until_current_replay",
        "note":"Reports are untrusted signals, never verified training labels"}))
}
fn hex_digest(value:&str)->String{format!("{:x}",Sha256::digest(value.as_bytes()))}
fn normalized(title:&str)->String{
    let mut out=String::new();
    let mut space=false;
    for ch in title.trim().to_lowercase().chars(){
        if ch.is_alphanumeric(){out.push(ch);space=false;}
        else if !space && !out.is_empty(){out.push(' ');space=true;}
    }
    out.trim_end().to_owned()
}
fn fingerprint(title:&str,alerts:&[String])->String{
    let mut sorted=alerts.to_vec();
    sorted.sort();sorted.dedup();
    hex_digest(&format!("{}\n{}",normalized(title),sorted.join(",")))
}
fn safe_leaf(raw:&str)->Option<String>{
    let text=raw.trim();
    if text.is_empty() || text.len()>240
        || text.contains(['/', '\\','@','\n','\r','\0'])
        || text.chars().any(char::is_control){return None;}
    let lower=text.to_ascii_lowercase();
    if ["http:","https:","www.","magnet:","token=","apikey=","password=",
        "secret=","bearer ","private=","api_key="].iter()
        .any(|bad|lower.contains(bad)){return None;}
    Some(text.to_owned())
}
fn trim_numbers(value:&Value,key:&str)->Value{
    json!(value.get(key).and_then(Value::as_array).into_iter().flatten()
        .take(12).filter_map(Value::as_i64)
        .filter(|n|(0..=99_999).contains(n)).collect::<Vec<_>>())
}
fn report(title:&str,status:&str,details:&Value)->Option<Value>{
    if !matches!(status,"matched"|"unmatched"|"error"){return None;}
    let title=safe_leaf(title)?;
    let flags=crate::debug_feed::community_alerts(&title,status,details);
    let mut codes:Vec<String>=flags.into_iter().filter_map(|flag|
        flag["code"].as_str().filter(|code|matches!(*code,
            "matching_error"|"invalid_selected_coordinate"|"unusual_season_number"|
            "unmatched_explicit_episode"|"explicit_episode_mismatch"|
            "parser_marker_disagreement"|"unresolved_absolute_anime_episode"|
            "incomplete_anime_episode_range"|"reversed_selected_range")).map(str::to_owned)).collect();
    codes.sort();codes.dedup();
    if codes.is_empty(){return None;}
    let parsed=&details["parsed"];
    let selected=&details["selected"];
    let parsed_title=parsed["title"].as_str().and_then(safe_leaf);
    let imdb=selected["imdb_id"].as_str().filter(|id|
        (9..=10).contains(&id.len())&&id.starts_with("tt")
        &&id[2..].bytes().all(|b|b.is_ascii_digit()));
    let key=fingerprint(&title,&codes);
    Some(json!({"protocol":PROTOCOL,"learning_epoch":EPOCH,
        "build_sha":build_sha(),"source":"matchlab-community",
        "fingerprint":key,"title":title,"status":status,"alerts":codes,
        "parsed":{"title":parsed_title,
            "seasons":trim_numbers(parsed,"seasons"),
            "episodes":trim_numbers(parsed,"episodes")},
        "selected":{"imdb_id":imdb,"season":selected["season"].as_i64(),
            "episode":selected["episode"].as_i64()},
        "verified":false}))
}
fn connect(path:&Path)->Result<Connection,String>{
    if let Some(parent)=path.parent(){
        fs::create_dir_all(parent).map_err(|e|e.to_string())?;
    }
    let db=Connection::open(path).map_err(|e|e.to_string())?;
    db.busy_timeout(Duration::from_secs(5)).map_err(|e|e.to_string())?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS pending(
           fingerprint TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS reported(
           fingerprint TEXT PRIMARY KEY,first_reported_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS batches(
           batch_id TEXT PRIMARY KEY,attempted_at INTEGER NOT NULL);"
    ).map_err(|e|e.to_string())?;
    Ok(db)
}
fn setting(db:&Connection,key:&str)->Result<Option<String>,String>{
    db.query_row("SELECT v FROM settings WHERE k=?1",[key],|r|r.get(0))
        .optional().map_err(|e|e.to_string())
}
fn put(db:&Connection,key:&str,value:&str)->Result<(),String>{
    db.execute("INSERT INTO settings(k,v) VALUES(?1,?2)
        ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        params![key,value]).map_err(|e|e.to_string())?;
    Ok(())
}
fn latest_id(path:&Path)->Result<i64,String>{
    let db=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e|e.to_string())?;
    db.query_row("SELECT COALESCE(MAX(id),0) FROM requests",[],|r|r.get(0))
        .map_err(|e|e.to_string())
}
// On upgrade/protocol/epoch change: reset unverified pending reports and
// jump cursor to the newest request; preserve previously reported digests.
fn ensure_version(db:&mut Connection,history:&Path,time:u64)->Result<(),String>{
    let version=format!("{PROTOCOL}:{EPOCH}:{}",build_sha());
    if setting(db,"version")?.as_deref()==Some(version.as_str()){return Ok(());}
    let latest=latest_id(history)?;
    let tx=db.transaction().map_err(|e|e.to_string())?;
    tx.execute("DELETE FROM pending",[]).map_err(|e|e.to_string())?;
    put(&tx,"version",&version)?;
    put(&tx,"after_id",&latest.to_string())?;
    put(&tx,"window_start",&time.to_string())?;
    tx.commit().map_err(|e|e.to_string())
}
fn scan(db:&Connection,history:&Path,time:u64)->Result<(),String>{
    let mut after=setting(db,"after_id")?.and_then(|v|v.parse::<i64>().ok()).unwrap_or(0);
    let seen:i64=db.query_row("SELECT count(*) FROM reported",[],|r|r.get(0))
        .map_err(|e|e.to_string())?;
    let mut pending:i64=db.query_row("SELECT count(*) FROM pending",[],|r|r.get(0))
        .map_err(|e|e.to_string())?;
    if seen>=MAX_SEEN{return Ok(());}
    let history_db=Connection::open_with_flags(history,OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e|e.to_string())?;
    for _ in 0..MAX_PAGES{
        let mut stmt=history_db.prepare(
            "SELECT id,title,status,details FROM requests
             WHERE id>?1 ORDER BY id ASC LIMIT ?2"
        ).map_err(|e|e.to_string())?;
        let rows=stmt.query_map(params![after,PAGE_SIZE],|r|
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,
                r.get::<_,String>(2)?,r.get::<_,String>(3)?)))
            .map_err(|e|e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e|e.to_string())?;
        if rows.is_empty(){break;}
        let row_count=rows.len();
        for (id,title,status,details) in rows{
            if pending<MAX_BATCH as i64{
                let details:Value=serde_json::from_str(&details).unwrap_or(Value::Null);
                if let Some(candidate)=report(&title,&status,&details){
                    let key=candidate["fingerprint"].as_str().unwrap_or_default();
                    let exists=db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reported WHERE fingerprint=?1)",
                        [key],|r|r.get::<_,i64>(0)).map_err(|e|e.to_string())?;
                    if exists==0{
                        let changed=db.execute(
                            "INSERT OR IGNORE INTO pending(fingerprint,payload,created_at)
                             VALUES(?1,?2,?3)",params![key,candidate.to_string(),time as i64])
                            .map_err(|e|e.to_string())?;
                        pending+=changed as i64;
                    }
                }
            }
            after=id;
        }
        put(db,"after_id",&after.to_string())?;
        if row_count<PAGE_SIZE as usize{break;}
    }
    Ok(())
}
fn github_client()->Result<reqwest::blocking::Client,String>{
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("MatchLab-Community-Learning/2")
        .build().map_err(|e|e.to_string())
}
fn request_json(response:reqwest::blocking::Response)->Result<Value,String>{
    if !response.status().is_success(){
        return Err(format!("GitHub HTTP {}",response.status()));
    }
    let text=response.text().map_err(|e|e.to_string())?;
    serde_json::from_str(&text).map_err(|e|e.to_string())
}
// Older official source revisions are allowed: they may still expose bugs
// not fixed in the latest parser. Eligibility does not mean correctness.
// Intake independently replays each report using the current source.
fn known_main_ancestor(client:&reqwest::blocking::Client)->Result<bool,String>{
    if !valid_sha(build_sha()){return Ok(false);}
    let data=request_json(client.get(format!(
        "https://api.github.com/repos/{REPO}/compare/{}...main",build_sha()))
        .header("Accept","application/vnd.github+json")
        .send().map_err(|e|e.to_string())?)?;
    Ok(data["behind_by"].as_i64()==Some(0)
       && matches!(data["status"].as_str(),Some("identical"|"ahead")))
}
// GitHub Actions creates ONE daily issue for all installations. Clients never
// create Issues, only one batch comment in the current daily issue.
// Only use the issue opened by the repository's GitHub Actions bot.
// A public contributor could otherwise spoof the daily Issue title.
fn daily_thread(client:&reqwest::blocking::Client,time:u64)->Result<Option<u64>,String>{
    let day=chrono::DateTime::from_timestamp(time as i64,0)
        .ok_or("invalid clock")?.format("%Y-%m-%d").to_string();
    let expected=format!("[community-learning] {day}");
    let data=request_json(client.get(format!("https://api.github.com/repos/{REPO}/issues"))
        .query(&[("state","open"),("sort","created"),("direction","desc"),
            ("per_page","100")])
        .header("Accept","application/vnd.github+json")
        .send().map_err(|e|e.to_string())?)?;
    let items=data.as_array().ok_or("invalid GitHub issues response")?;
    Ok(items.iter().find(|v|v["title"].as_str()==Some(expected.as_str())
        && v["state"]=="open" && v["user"]["login"]=="github-actions[bot]"
        && v.get("pull_request").is_none())
        .and_then(|v|v["number"].as_u64()))
}

fn build_batch(db:&Connection,time:u64)->Result<Option<(String,Value,Vec<String>)>,String>{
    let mut stmt=db.prepare(
        "SELECT fingerprint,payload FROM pending ORDER BY fingerprint LIMIT ?1"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([MAX_BATCH as i64],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    if rows.is_empty(){return Ok(None);}
    let fingerprints=rows.iter().map(|(k,_)|k.clone()).collect::<Vec<_>>();
    let reports=rows.iter().map(|(_,v)|
        serde_json::from_str::<Value>(v).map_err(|e|e.to_string()))
        .collect::<Result<Vec<_>,_>>()?;
    let batch_id=hex_digest(&format!("{}\n{}\n{}",
        build_sha(),EPOCH,fingerprints.join("\n")));
    let batch=json!({"protocol":PROTOCOL,"learning_epoch":EPOCH,
        "build_sha":build_sha(),"source":"matchlab-community-batch",
        "batch_id":batch_id,"created_unix":time,"verified":false,
        "reports":reports});
    Ok(Some((batch_id,batch,fingerprints)))
}
// Commit report digests BEFORE a network write. On a timeout/crash we may
// lose one batch, but never spam GitHub by retrying an ambiguous POST.
fn mark_attempted(
    db:&mut Connection,time:u64,batch_id:&str,fingerprints:&[String],
)->Result<(),String>{
    let tx=db.transaction().map_err(|e|e.to_string())?;
    for fingerprint in fingerprints{
        tx.execute("INSERT OR IGNORE INTO reported(fingerprint,first_reported_at)
            VALUES(?1,?2)",params![fingerprint,time as i64])
            .map_err(|e|e.to_string())?;
        tx.execute("DELETE FROM pending WHERE fingerprint=?1",[fingerprint])
            .map_err(|e|e.to_string())?;
    }
    tx.execute("INSERT OR IGNORE INTO batches(batch_id,attempted_at) VALUES(?1,?2)",
        params![batch_id,time as i64]).map_err(|e|e.to_string())?;
    put(&tx,"window_start",&time.to_string())?;
    tx.commit().map_err(|e|e.to_string())
}
fn submit_comment(client:&reqwest::blocking::Client,token:&str,
    issue:u64,batch:&Value)->Result<(),String>{
    let text=serde_json::to_string(batch).map_err(|e|e.to_string())?;
    let comment=format!("{MARKER}\n{text}\n");
    if comment.len()>MAX_BODY{return Err("learning batch too large".into());}
    let request=json!({"body":comment}).to_string();
    let result=client.post(format!("https://api.github.com/repos/{REPO}/issues/{issue}/comments"))
        .bearer_auth(token)
        .header("Accept","application/vnd.github+json")
        .header("Content-Type","application/json")
        .body(request).send().map_err(|e|e.to_string())?;
    if result.status()!=reqwest::StatusCode::CREATED{
        return Err(format!("GitHub comment HTTP {}",result.status()));
    }
    Ok(())
}
fn cycle(history:&Path,storage:&Path,token:&str)->Result<(),String>{
    let time=now();
    let mut db=connect(storage)?;
    ensure_version(&mut db,history,time)?;
    scan(&db,history,time)?;
    let started=setting(&db,"window_start")?
        .and_then(|v|v.parse::<u64>().ok()).unwrap_or(time);
    if time<started.saturating_add(DAY_SECS){return Ok(());}
    let Some((id,batch,fingerprints))=build_batch(&db,time)?else{
        put(&db,"window_start",&time.to_string())?;
        return Ok(());
    };
    // Older builds may report; GitHub Action replays their observations against
    // the current parser and excludes resolved errors before review/training.
    // Network eligibility is checked only once for the daily submission.
    let client=github_client()?;
    if !known_main_ancestor(&client)?{return Ok(());}
    let Some(issue)=daily_thread(&client,time)?else{
        // If GitHub Actions missed the daily thread, do not create issues.
        return Ok(());
    };
    // Validate batch size *before* persistence, so a rejected oversized batch
    // can be diagnosed instead of discarded.
    if batch.to_string().len()+MARKER.len()+2>MAX_BODY{
        return Err("community batch exceeds comment size bound".into());
    }
    mark_attempted(&mut db,time,&id,&fingerprints)?;
    submit_comment(&client,token,issue,&batch)
}
pub fn start(history:PathBuf){
    if !enabled(){
        if opted_in(){
            tracing::warn!("community learning unavailable: token and official SHA-stamped build required");
        }
        return;
    }
    let location=env::var("MATCHLAB_COMMUNITY_STATE_FILE")
        .unwrap_or_else(|_|"/data/community-learning.sqlite3".into());
    let storage=PathBuf::from(location);
    let Some(token)=github_token()else{return;};
    tokio::spawn(async move{
        loop{
            let db=history.clone();
            let local=storage.clone();
            let key=token.clone();
            match tokio::task::spawn_blocking(move||cycle(&db,&local,&key)).await{
                Ok(Ok(()))=>{},
                Ok(Err(error))=>tracing::warn!(%error,
                    "community learning batch unavailable; no automatic retry of posted reports"),
                Err(error)=>tracing::warn!(%error,"community learning worker failed"),
            }
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
        }
    });
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn canonical_fingerprints_ignore_release_separator_variations(){
        let code=vec!["unresolved_absolute_anime_episode".into()];
        assert_eq!(fingerprint("Bleach_338",&code),fingerprint("bleach.338",&code));
        assert_eq!(fingerprint("Bleach_338",&code),fingerprint("Bleach - 338",&code));
    }
    #[test]
    fn secrets_paths_and_urls_are_rejected(){
        assert!(safe_leaf("Bleach_338").is_some());
        for bad in ["Some/Show/file.mkv","C:\\Users\\me\\file.mkv","a@b.com",
            "http://example.com","token=secret","magnet:?xt=test","A\nB"]{
            assert!(safe_leaf(bad).is_none(),"{bad}");
        }
    }
    #[test]
    fn batch_is_bounded_and_same_report_is_deduplicated_across_windows(){
        let path=std::env::temp_dir().join(format!(
            "matchlab-community-{}-{}.sqlite3",std::process::id(),now()));
        let _=fs::remove_file(&path);
        let mut db=connect(&path).unwrap();
        let key=fingerprint("Bleach_338",&["unresolved_absolute_anime_episode".into()]);
        let sample=json!({"title":"Bleach_338","fingerprint":key});
        db.execute("INSERT INTO pending VALUES(?1,?2,1)",params![key,sample.to_string()]).unwrap();
        db.execute("INSERT OR IGNORE INTO pending VALUES(?1,?2,1)",params![key,sample.to_string()]).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM pending",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        // Tests cannot rely on an official build SHA; version-independent
        // persistence logic still guards repeated submission.
        mark_attempted(&mut db,now(),"batch-1",&[key.clone()]).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM pending",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(db.query_row("SELECT COUNT(*) FROM reported WHERE fingerprint=?1",
            [&key],|r|r.get::<_,i64>(0)).unwrap(),1);
        drop(db);
        let _=fs::remove_file(path);
    }
    #[test]
    fn unverified_reports_never_include_full_diagnostics(){
        let details=json!({"parsed":{"title":"Bleach","episodes":[338],
                "api_key":"SECRET"},"selected":{"imdb_id":"tt0434665",
                "private_path":"/private/file.mkv"},
                "reason":"free form text that should not be exported"});
        // A reporting alert is needed: normal successful matches are not uploaded.
        assert!(report("Bleach_338","matched",&details).is_none());
        assert!(!valid_sha("latest"));
    }
}
