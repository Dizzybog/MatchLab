//! GitHub-only, opt-in community learning. Untrusted batches, never labels.
//! One 24-hour batch COMMENT per installation; GitHub Actions owns one daily Issue.
use axum::Json;
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::{env,fs,path::{Path,PathBuf},time::{Duration,SystemTime,UNIX_EPOCH}};

const REPO:&str="Dizzybog/MatchLab";
const PROTOCOL:u32=3;
const EPOCH:u32=1;
const DAY_SECS:u64=86_400;
const POLL_SECS:u64=300;
const PAGE_SIZE:i64=100;
const MAX_PAGES:usize=5;
const MAX_BATCH:usize=16;
const MAX_RULES_PER_BATCH:usize=12;
const RECEIPT_RETRY_SECS:u64=900;
const MAX_RECEIPT_PAGES:usize=20;
const MAX_SEEN:i64=50_000;
const MAX_BODY:usize=16_384;
const MARKER:&str="<!-- matchlab-community-batch-v3 -->";

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
fn eligible(local:bool,share:bool,has_token:bool,official_build:bool)->bool{
    local && share && has_token && official_build
}
fn enabled()->bool{eligible(crate::local_learning::active(),opted_in(),
    github_token().is_some(),valid_sha(build_sha()))}
pub async fn status()->Json<Value>{
    let local=env::var("MATCHLAB_ML_STATE_FILE")
        .unwrap_or_else(|_|"/data/local-learning.sqlite3".into());
    let sharing=env::var("MATCHLAB_COMMUNITY_STATE_FILE")
        .unwrap_or_else(|_|"/data/community-learning.sqlite3".into());
    let archived=Connection::open_with_flags(
        local,OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
        .and_then(|db|crate::sharing_archive::stats(&db).ok())
        .unwrap_or(0);
    let sharing_db=Connection::open_with_flags(
        sharing,OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
    let count=|sql:&str|sharing_db.as_ref().and_then(|db|
        db.query_row(sql,[],|r|r.get::<_,i64>(0)).ok()).unwrap_or(0);
    Json(json!({"archived_regex_proposals":archived,
        "queued_reports_and_rules":count("SELECT COUNT(*) FROM pending"),
        "confirmed_deliveries":count("SELECT COUNT(*) FROM reported"),
        "unconfirmed_batches":count("SELECT COUNT(*) FROM inflight"),
        "enabled":opted_in(),"local_learning_enabled":crate::local_learning::active(),
        "ready":enabled(),
        "protocol":PROTOCOL,"learning_epoch":EPOCH,
        "build_sha":valid_sha(build_sha()).then_some(build_sha()),
        "upload":"one_daily_github_issue_with_opt_in_batch_comments",
        "batch_interval_hours":24,"max_reports_per_batch":MAX_BATCH,
        "uploads":"verified_new_and_source_regex_proposals_and_strict_anomalies",
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
// Guardrail: a difference is not necessarily a bad match. Require
// independently imported AniDB/TVDB episode crosswalk evidence before
// sharing a parsed-versus-matched coordinate discrepancy as a bug.
fn proven_coordinate_gap(meta:&Connection,parsed:&Value,selected:&Value,code:&str)->bool{
    if selected["category"]!="series"{return false;}
    let Some(imdb)=selected["imdb_id"].as_str()else{return false;};
    if !(9..=10).contains(&imdb.len())||!imdb.starts_with("tt")
        ||!imdb[2..].bytes().all(|b|b.is_ascii_digit()){return false;}
    let mapped_remap=selected["anime"].is_object()
        ||selected["episode_range"]["complete"]==true;
    if mapped_remap{return false;}
    if matches!(code,"field_missing_anime_episode"|"unresolved_absolute_anime_episode"){
        let Some(ep)=parsed["anime_episode_candidate"]["episode"].as_i64()
            .filter(|n|(1..=99_999).contains(n))else{return false;};
        return crate::episode_crosswalk::resolve_absolute(meta,imdb,ep)
            .ok().flatten().is_some()
            &&selected["anime"].is_null();
    }
    let Some(seasons)=parsed["seasons"].as_array().filter(|a|a.len()==1)else{return false;};
    let Some(episodes)=parsed["episodes"].as_array().filter(|a|a.len()==1)else{return false;};
    let (Some(season),Some(episode))=(seasons[0].as_i64(),episodes[0].as_i64())else{
        return false;
    };
    if !(1..=9999).contains(&season)||!(1..=99_999).contains(&episode){return false;}
    let mismatch=match code{
        "field_unresolved_season"=>selected["season"].is_null(),
        "field_unresolved_episode"=>selected["episode"].is_null(),
        "field_mismatch_season"=>selected["season"].as_i64()
            .is_some_and(|value|value!=season),
        "field_mismatch_episode"=>selected["episode"].as_i64()
            .is_some_and(|value|value!=episode),
        "explicit_episode_mismatch"=>selected["season"].as_i64()!=Some(season)
            ||selected["episode"].as_i64()!=Some(episode),
        _=>false,
    };
    mismatch && crate::episode_crosswalk::resolve_season(meta,imdb,season,episode)
        .ok().flatten().is_some()
}
fn safe_issue_code(code:&str,meta:Option<&Connection>,parsed:&Value,selected:&Value)->bool{
    if matches!(code,"field_unresolved_season"|"field_unresolved_episode"|
        "field_mismatch_season"|"field_mismatch_episode"|
        "field_missing_anime_episode"|"unresolved_absolute_anime_episode"|
        "explicit_episode_mismatch"){
        return meta.is_some_and(|db|proven_coordinate_gap(db,parsed,selected,code));
    }
    // Year differences are observations, not proof. They must be inspected
    // locally by the opt-in model and cannot create public issues on their own.
    if code=="field_mismatch_year"{return false;}
    matches!(code,
        "matching_error"|"invalid_selected_coordinate"|"unusual_season_number"|
        "unmatched_explicit_episode"|
        "parser_marker_disagreement"|"incomplete_anime_episode_range"|
        "reversed_selected_range"|"field_missing_title"|
        "field_missing_resolution"|"field_missing_codec"|
        "field_missing_quality"|"field_missing_audio"|
        "field_missing_bit_depth"|"field_missing_hdr"|
        "field_missing_channels"|"field_missing_languages"|
        "field_missing_subbed"|"field_missing_dubbed"|
        "field_missing_seasons"|"field_missing_episodes"|
        "field_missing_extension"|"field_missing_year")
}

fn report(title:&str,status:&str,details:&Value,metadata:Option<&Connection>)->Option<Value>{
    if !matches!(status,"matched"|"unmatched"|"error"){return None;}
    let title=safe_leaf(title)?;
    let flags=crate::debug_feed::community_alerts(&title,status,details);
    let parsed=&details["parsed"];
    let selected=&details["selected"];
    let mut codes:Vec<String>=flags.into_iter().filter_map(|flag|
        flag["code"].as_str().filter(|code|safe_issue_code(code,metadata,parsed,selected))
        .map(str::to_owned)).collect();
    codes.sort();codes.dedup();
    if codes.is_empty(){return None;}
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
           batch_id TEXT PRIMARY KEY,attempted_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS inflight(
           batch_id TEXT PRIMARY KEY,issue INTEGER NOT NULL,
           payload TEXT NOT NULL,fingerprints TEXT NOT NULL,
           created_at INTEGER NOT NULL,last_attempt INTEGER NOT NULL DEFAULT 0,
           attempts INTEGER NOT NULL DEFAULT 0);"
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
    // Preserve unacknowledged history across parser/image upgrades.
    let window=setting(db,"window_start")?
        .unwrap_or_else(||time.saturating_sub(DAY_SECS).to_string());
    let tx=db.transaction().map_err(|e|e.to_string())?;
    put(&tx,"version",&version)?;
    put(&tx,"after_id",&latest.to_string())?;
    put(&tx,"window_start",&window)?;
    tx.commit().map_err(|e|e.to_string())
}
fn scan(db:&Connection,history:&Path,metadata:&Path,time:u64)->Result<(),String>{
    let mut after=setting(db,"after_id")?.and_then(|v|v.parse::<i64>().ok()).unwrap_or(0);
    let seen:i64=db.query_row("SELECT count(*) FROM reported",[],|r|r.get(0))
        .map_err(|e|e.to_string())?;
    let mut pending:i64=db.query_row("SELECT count(*) FROM pending",[],|r|r.get(0))
        .map_err(|e|e.to_string())?;
    if seen>=MAX_SEEN{return Ok(());}
    let metadata_db=Connection::open_with_flags(metadata,OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
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
            if pending<(MAX_BATCH-2) as i64{
                let details:Value=serde_json::from_str(&details).unwrap_or(Value::Null);
                if let Some(candidate)=report(&title,&status,&details,metadata_db.as_ref()){
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
/// Gather high-evidence rule suggestions from the local offline learning DB.
/// No rule is sent merely because a model assigned a large confidence value.
/// Each proposal must already be locally active, evidence-diverse, and
/// independently revalidated using the current AniDB/TVDB mapping.
fn collect_rule_proposals(db:&Connection,metadata:&Path,time:u64)->Result<(),String>{
    let state=env::var("MATCHLAB_ML_STATE_FILE")
        .unwrap_or_else(|_|"/data/local-learning.sqlite3".into());
    let Ok(local)=Connection::open_with_flags(
        &state,OpenFlags::SQLITE_OPEN_READ_ONLY
    )else{return Ok(());};
    let _=local.busy_timeout(Duration::from_millis(250));
    // Durable archive first, oldest rule first. This survives changing
    // parser builds and turning community sharing on long after training.
    let mut rules=crate::sharing_archive::load(&local).unwrap_or_default();
    // Fresh current-build rules can be shared immediately even before the
    // background archiving worker finishes its first cycle.
    if let Ok(meta)=Connection::open_with_flags(
        metadata,OpenFlags::SQLITE_OPEN_READ_ONLY){
        rules.extend(crate::learned_regex::proposals(&local,&meta)
            .unwrap_or_default());
    }
    rules.extend(crate::field_learning::community_proposals(&local)
        .unwrap_or_default());
    rules.extend(crate::tv_learning::community_proposals(&local)
        .unwrap_or_default());
    rules.extend(crate::candidate_discovery::community_proposals(&local)
        .unwrap_or_default());
    let mut distinct=std::collections::HashSet::new();
    rules.retain(|rule|rule["fingerprint"].as_str()
        .is_some_and(|fp|distinct.insert(fp.to_owned())));
    let mut count:i64=db.query_row("SELECT COUNT(*) FROM pending",[],|r|r.get(0))
        .map_err(|e|e.to_string())?;
    let mut added=0usize;
    for proposal in rules {
        if count>=MAX_BATCH as i64 || added>=MAX_RULES_PER_BATCH{break;}
        let Some(key)=proposal["fingerprint"].as_str()else{continue;};
        let seen:i64=db.query_row(
            "SELECT EXISTS(SELECT 1 FROM reported WHERE fingerprint=?1)",
            [key],|r|r.get(0)).map_err(|e|e.to_string())?;
        if seen!=0{continue;}
        let rows=db.execute(
            "INSERT OR IGNORE INTO pending(fingerprint,payload,created_at)
             VALUES(?1,?2,?3)",params![key,proposal.to_string(),time as i64])
            .map_err(|e|e.to_string())?;
        count+=rows as i64;
        added+=rows;
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
        "SELECT fingerprint,payload FROM pending ORDER BY created_at,fingerprint LIMIT 64"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    let mut fingerprints=Vec::new();
    let mut reports=Vec::new();
    let mut rules=Vec::new();
    // Enforce byte limits on the actual serialized GitHub comment. Large
    // verified anime proposals must never block an entire archived backlog.
    for (key,raw) in rows{
        let item:Value=serde_json::from_str(&raw).map_err(|e|e.to_string())?;
        let is_rule=matches!(item["kind"].as_str(),
            Some("verified_regex_proposal"|"learned_field_regex"
            |"learned_tv_regex"|"learned_source_regex_patch"
            |"learned_field_regex_review"));
        if is_rule&&rules.len()>=MAX_RULES_PER_BATCH{continue;}
        if fingerprints.len()>=MAX_BATCH{break;}
        let mut candidate_reports=reports.clone();
        let mut candidate_rules=rules.clone();
        if is_rule{candidate_rules.push(item)}else{candidate_reports.push(item)}
        let mut keys=fingerprints.clone();
        keys.push(key.clone());
        keys.sort();
        let candidate=json!({"protocol":PROTOCOL,"learning_epoch":EPOCH,
            "build_sha":build_sha(),"source":"matchlab-community-batch",
            "batch_id":hex_digest(&format!("{}\n{}\n{}",
                build_sha(),EPOCH,keys.join("\n"))),
            "created_unix":time,"verified":false,
            "reports":candidate_reports,"rules":candidate_rules});
        if candidate.to_string().len()+MARKER.len()+2>MAX_BODY{
            if fingerprints.is_empty(){
                return Err("single archived sharing proposal exceeds GitHub comment limit".into());
            }
            break;
        }
        fingerprints.push(key);
        reports=candidate_reports;
        rules=candidate_rules;
    }
    if fingerprints.is_empty(){return Ok(None);}
    fingerprints.sort();
    let batch_id=hex_digest(&format!("{}\n{}\n{}",
        build_sha(),EPOCH,fingerprints.join("\n")));
    let batch=json!({"protocol":PROTOCOL,"learning_epoch":EPOCH,
        "build_sha":build_sha(),"source":"matchlab-community-batch",
        "batch_id":batch_id,"created_unix":time,"verified":false,
        "reports":reports,"rules":rules});
    Ok(Some((batch_id,batch,fingerprints)))
}
/// A fingerprint becomes "reported" only after confirmed receipt. If the
/// process dies between posting and recording, the next cycle searches the
/// original Issue for the exact batch before deciding whether to retry.
fn mark_delivered(db:&mut Connection,time:u64,batch_id:&str,
    fingerprints:&[String])->Result<(),String>{
    let tx=db.transaction().map_err(|e|e.to_string())?;
    for fingerprint in fingerprints{
        tx.execute("INSERT OR IGNORE INTO reported(fingerprint,first_reported_at)
            VALUES(?1,?2)",params![fingerprint,time as i64])
            .map_err(|e|e.to_string())?;
        tx.execute("DELETE FROM pending WHERE fingerprint=?1",[fingerprint])
            .map_err(|e|e.to_string())?;
    }
    tx.execute("DELETE FROM inflight WHERE batch_id=?1",[batch_id])
        .map_err(|e|e.to_string())?;
    tx.execute("INSERT OR IGNORE INTO batches(batch_id,attempted_at) VALUES(?1,?2)",
        params![batch_id,time as i64]).map_err(|e|e.to_string())?;
    put(&tx,"window_start",&time.to_string())?;
    tx.commit().map_err(|e|e.to_string())
}
fn stage_batch(db:&Connection,time:u64,issue:u64,batch_id:&str,
    batch:&Value,fingerprints:&[String])->Result<(),String>{
    let keys=serde_json::to_string(fingerprints).map_err(|e|e.to_string())?;
    db.execute("INSERT OR IGNORE INTO inflight(
        batch_id,issue,payload,fingerprints,created_at)
        VALUES(?1,?2,?3,?4,?5)",
        params![batch_id,issue as i64,batch.to_string(),keys,time as i64])
        .map_err(|e|e.to_string())?;
    Ok(())
}
/// Read the entire relevant issue history before concluding that an
/// ambiguous HTTP POST did not arrive. On paging overflow, fail closed.
fn remote_has_batch(client:&reqwest::blocking::Client,
    issue:u64,batch_id:&str,payload:&str)->Result<bool,String>{
    let expected=format!("{MARKER}\n{payload}\n");
    for page in 1..=MAX_RECEIPT_PAGES{
        let number=page.to_string();
        let data=request_json(client.get(format!(
            "https://api.github.com/repos/{REPO}/issues/{issue}/comments"))
            .query(&[("per_page","100"),("page",number.as_str())])
            .header("Accept","application/vnd.github+json")
            .send().map_err(|e|e.to_string())?)?;
        let comments=data.as_array().ok_or("invalid GitHub comments response")?;
        if comments.iter().any(|item|
            item["body"].as_str()==Some(expected.as_str())
            && item["body"].as_str().is_some_and(|body|body.contains(batch_id))){
            return Ok(true);
        }
        if comments.len()<100{return Ok(false);}
    }
    Err("GitHub receipt search exceeded page limit; refusing duplicate upload".into())
}
/// A retry verifies the old daily Issue first. If it is no longer the
/// ingestion target, repost only after proving absence there and in today's
/// Issue. This avoids losing reports when a daily Action already ran.
fn drive_delivery(db:&mut Connection,client:&reqwest::blocking::Client,
    token:&str,time:u64)->Result<bool,String>{
    let pending:Option<(String,u64,String,String,i64)>=db.query_row(
        "SELECT batch_id,issue,payload,fingerprints,last_attempt
         FROM inflight ORDER BY created_at LIMIT 1",[],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))
    ).optional().map_err(|e|e.to_string())?;
    let Some((batch_id,old_issue,payload,keys,last_attempt))=pending else{
        return Ok(false);
    };
    if time<(last_attempt.max(0) as u64).saturating_add(RECEIPT_RETRY_SECS){
        return Ok(true);
    }
    let fingerprints:Vec<String>=serde_json::from_str(&keys).map_err(|e|e.to_string())?;
    if remote_has_batch(client,old_issue,&batch_id,&payload)? {
        mark_delivered(db,time,&batch_id,&fingerprints)?;
        return Ok(true);
    }
    let Some(today_issue)=daily_thread(client,time)?else{return Ok(true);};
    if today_issue!=old_issue
        && remote_has_batch(client,today_issue,&batch_id,&payload)?{
        mark_delivered(db,time,&batch_id,&fingerprints)?;
        return Ok(true);
    }
    // Persist the target and attempt timestamp BEFORE sending. If GitHub
    // returns an ambiguous error, the same batch is checked for a receipt.
    db.execute("UPDATE inflight SET issue=?2,last_attempt=?3,
        attempts=attempts+1 WHERE batch_id=?1",
        params![batch_id,today_issue as i64,time as i64])
        .map_err(|e|e.to_string())?;
    let batch:Value=serde_json::from_str(&payload).map_err(|e|e.to_string())?;
    submit_comment(client,token,today_issue,&batch)?;
    mark_delivered(db,time,&batch_id,&fingerprints)?;
    Ok(true)
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
fn cycle(history:&Path,metadata:&Path,storage:&Path,token:&str)->Result<(),String>{
    let time=now();
    let mut db=connect(storage)?;
    ensure_version(&mut db,history,time)?;
    collect_rule_proposals(&db,metadata,time)?;
    scan(&db,history,metadata,time)?;
    let client=github_client()?;
    if !known_main_ancestor(&client)?{return Ok(());}
    // Reconcile unconfirmed batches first. A timeout is NOT delivery.
    if drive_delivery(&mut db,&client,token,time)?{return Ok(());}
    let started=setting(&db,"window_start")?
        .and_then(|v|v.parse::<u64>().ok()).unwrap_or(time);
    if time<started.saturating_add(DAY_SECS){return Ok(());}
    let Some((id,batch,fingerprints))=build_batch(&db,time)?else{
        put(&db,"window_start",&time.to_string())?;
        return Ok(());
    };
    let Some(issue)=daily_thread(&client,time)?else{return Ok(());};
    stage_batch(&db,time,issue,&id,&batch,&fingerprints)?;
    let _=drive_delivery(&mut db,&client,token,time)?;
    Ok(())
}
pub fn start(history:PathBuf,metadata:PathBuf){
    if !enabled(){
        if opted_in(){
            tracing::warn!("community learning unavailable: requires local ML opt-in, GitHub token and official SHA-stamped build");
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
            let metadata_path=metadata.clone();
            let local=storage.clone();
            let key=token.clone();
            match tokio::task::spawn_blocking(move||cycle(&db,&metadata_path,&local,&key)).await{
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
    fn community_upload_requires_two_distinct_opt_ins(){
        for (local,share,token,official,allowed) in [
            (false,false,false,false,false),
            (true,false,false,true,false),
            (false,true,true,true,false),
            (true,true,false,true,false),
            (true,true,true,false,false),
            (true,true,true,true,true),
        ]{
            assert_eq!(eligible(local,share,token,official),allowed);
        }
    }
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
        mark_delivered(&mut db,now(),"batch-1",&[key.clone()]).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM pending",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(db.query_row("SELECT COUNT(*) FROM reported WHERE fingerprint=?1",
            [&key],|r|r.get::<_,i64>(0)).unwrap(),1);
        drop(db);
        let _=fs::remove_file(path);
    }
    #[test]
    fn only_independently_verified_episode_discrepancies_are_uploadable(){
        let meta=Connection::open_in_memory().unwrap();
        meta.execute_batch("CREATE TABLE anime_series_map(
            imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,
                end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','100','200',0);
            INSERT INTO episode_map VALUES('100','200',1,2,1,12,0,'');").unwrap();
        let details=json!({"parsed":{"title":"Test Show",
            "seasons":[2],"episodes":[7]},
            "selected":{"category":"series","imdb_id":"tt1234567",
                "season":null,"episode":null}});
        assert!(proven_coordinate_gap(&meta,&details["parsed"],
            &details["selected"],"field_unresolved_episode"));
        assert!(proven_coordinate_gap(&meta,&details["parsed"],
            &details["selected"],"field_unresolved_season"));
        let submitted=report("Test.Show.S02E07","matched",&details,Some(&meta))
            .expect("verified missing matcher episode should qualify");
        assert!(submitted["alerts"].as_array().unwrap().iter().any(|code|
            code=="field_unresolved_episode"));
        assert!(report("Test.Show.S02E07","matched",&details,None).is_none());
        let other=json!({"parsed":{"year":2025,"title":"Test Show"},
            "selected":{"year":2020,"category":"movie","imdb_id":"tt1234567"}});
        let maybe_report=report("Test.Show.2025.WEB-DL","matched",&other,Some(&meta));
        if let Some(candidate)=maybe_report {
            assert!(!candidate["alerts"].as_array().unwrap().iter().any(|code|
                code=="field_mismatch_year"),
                "year disagreement alone cannot be called a verified error");
        }
    }
    #[test]
    fn unverified_reports_never_include_full_diagnostics(){
        let details=json!({"parsed":{"title":"Bleach","episodes":[338],
                "api_key":"SECRET"},"selected":{"imdb_id":"tt0434665",
                "private_path":"/private/file.mkv"},
                "reason":"free form text that should not be exported"});
        // A reporting alert is needed: normal successful matches are not uploaded.
        assert!(report("Bleach_338","matched",&details,None).is_none());
        assert!(!valid_sha("latest"));
    }
}
