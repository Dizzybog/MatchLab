//! Offline, native-Rust field-learning worker.
//! It collects independent evidence, trains a bounded online FTRL model,
//! and stores all observations in a single transaction per history batch.
//! Model scores are *shadow-only*: they never authorize regex or SID/ASID.
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{env,path::{Path,PathBuf},time::Duration};

const POLL_SECONDS:u64=15;
const MAX_PATTERNS:i64=2_000;
const MIN_EXAMPLES:i64=5;
fn max_examples()->i64{
    (crate::learning_cache::storage_gb() as i64*20_000).min(2_000_000)
}
pub(crate) fn active()->bool{
    env::var("MATCHLAB_ML_ENABLED").ok()
        .is_some_and(|v|v.eq_ignore_ascii_case("true"))
}
fn digest(raw:&str)->String{format!("{:x}",Sha256::digest(raw.as_bytes()))}
fn normalized(title:&str)->String{
    title.chars().flat_map(char::to_lowercase)
        .map(|c|if c.is_alphanumeric(){c}else{' '})
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}
fn connect(path:&Path)->Result<Connection,String>{
    if let Some(dir)=path.parent(){
        std::fs::create_dir_all(dir).map_err(|e|e.to_string())?;
    }
    let db=Connection::open(path).map_err(|e|e.to_string())?;
    db.busy_timeout(Duration::from_secs(3)).map_err(|e|e.to_string())?;
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS patterns(
            key TEXT PRIMARY KEY,field_code TEXT NOT NULL,
            distinct_examples INTEGER NOT NULL DEFAULT 0,
            model_score REAL,first_seen INTEGER NOT NULL,last_seen INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS samples(
            key TEXT PRIMARY KEY,pattern_key TEXT NOT NULL,observed_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS pattern_samples_idx ON samples(pattern_key);")
        .map_err(|e|e.to_string())?;
    crate::online_model::OnlineModel::init(&db)?;
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
fn count(db:&Connection,table:&str)->Result<i64,String>{
    let sql=match table {
        "patterns"=>"SELECT count(*) FROM patterns",
        "samples"=>"SELECT count(*) FROM samples",
        _=>return Err("unknown statistics table".into())
    };
    db.query_row(sql,[],|r|r.get(0)).map_err(|e|e.to_string())
}
fn pattern(flag:&Value,details:&Value)->Option<(String,String)>{
    let code=flag["code"].as_str()?;
    if !code.starts_with("field_"){return None;}
    let parsed=&details["parsed"];
    let selected=&details["selected"];
    let feature=if code=="field_mismatch_year"{
        let (p,s)=(parsed["year"].as_i64()?,selected["year"].as_i64()?);
        if !(1880..=2100).contains(&p)||!(1880..=2100).contains(&s){return None;}
        format!("year_delta:{}",p-s)
    }else if code.starts_with("field_missing_"){
        flag["signal"].as_str().unwrap_or("unknown").to_owned()
    }else{
        let imdb=selected["imdb_id"].as_str()?;
        if !(9..=10).contains(&imdb.len())||!imdb.starts_with("tt")
            ||!imdb[2..].bytes().all(|b|b.is_ascii_digit()){return None;}
        imdb.to_owned()
    };
    Some((code.to_owned(),digest(&format!("{code}:{feature}"))))
}
fn scan(history:&Path,metadata:&Path,local:&mut Connection)->Result<(),String>{
    // Both learners reconcile only between batches: no active-registry
    // mutation occurs inside the evidence transaction.
    crate::field_learning::reconcile(local)?;
    let history_db=Connection::open_with_flags(history,OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e|e.to_string())?;
    history_db.busy_timeout(Duration::from_secs(2)).map_err(|e|e.to_string())?;
    let newest:i64=history_db.query_row("SELECT COALESCE(MAX(id),0) FROM requests",
        [],|r|r.get(0)).map_err(|e|e.to_string())?;
    let Some(cursor)=setting(local,"after_id")?else{
        // Do not silently train on requests made before the opt-in.
        put(local,"after_id",&newest.to_string())?;
        return Ok(());
    };
    let mut after=cursor.parse::<i64>().unwrap_or(newest);
    if newest<after{put(local,"after_id",&newest.to_string())?;return Ok(());}
    if newest==after{return Ok(());}
    let mut stmt=history_db.prepare("SELECT id,title,details FROM requests
        WHERE id>?1 ORDER BY id ASC LIMIT 1000").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([after],|r|Ok((r.get::<_,i64>(0)?,
        r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    let meta=Connection::open_with_flags(metadata,OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
    if let Some(ref metadata)=meta{
        crate::learned_regex::reconcile(local,metadata)?;
    }
    // Load a committed snapshot every batch. If a batch fails, all evidence,
    // model weights, and the history cursor roll back together.
    let mut model=crate::online_model::OnlineModel::load(local)?;
    let tx=local.transaction().map_err(|e|e.to_string())?;
    let mut evidence=crate::field_learning::count_evidence(&tx)?;
    let mut samples=count(&tx,"samples")?;
    let mut patterns=count(&tx,"patterns")?;
    let now=chrono::Utc::now().timestamp();
    let mut newly_trained=false;
    for (id,title,raw) in rows{
        after=id;
        let Ok(details)=serde_json::from_str::<Value>(&raw)else{continue};
        if let Some(ref metadata)=meta{
            crate::learned_regex::observe(&tx,metadata,&title,&details)?;
        }
        for (field,signal) in crate::field_learning::observe_with_budget(
            &tx,&title,&details,&mut evidence)?{
            newly_trained|=model.observe(&tx,&title,&field,&signal)?;
        }
        let flags=crate::field_audit::audit(
            &title,&details["parsed"],&details["selected"]);
        for flag in flags{
            let Some((code,group))=pattern(&flag,&details)else{continue};
            if samples>=max_examples(){continue;}
            let sample=digest(&format!("{group}:{}",normalized(&title)));
            // The PRIMARY KEY is the dedup check. No preliminary SELECT
            // and no per-observation transactions.
            let known:i64=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM patterns WHERE key=?1)",
                [&group],|r|r.get(0)).map_err(|e|e.to_string())?;
            if known==0&&patterns>=MAX_PATTERNS{continue;}
            let inserted=tx.execute(
                "INSERT OR IGNORE INTO samples(key,pattern_key,observed_at)
                 VALUES(?1,?2,?3)",params![sample,group,now])
                .map_err(|e|e.to_string())?;
            if inserted==0{continue;}
            let n:i64=tx.query_row(
                "INSERT INTO patterns(key,field_code,distinct_examples,first_seen,last_seen)
                 VALUES(?1,?2,1,?3,?3)
                 ON CONFLICT(key) DO UPDATE SET
                   distinct_examples=distinct_examples+1,last_seen=excluded.last_seen
                 RETURNING distinct_examples",
                params![group,code,now],|r|r.get(0)).map_err(|e|e.to_string())?;
            samples+=1;
            if known==0{patterns+=1;}
            if n>=MIN_EXAMPLES{
                let field=flag["field"].as_str().unwrap_or("");
                let signal=flag["signal"].as_str().unwrap_or("");
                if let Some(score)=model.score(field,signal,&title){
                    tx.execute("UPDATE patterns SET model_score=?1 WHERE key=?2",
                        params![score,group]).map_err(|e|e.to_string())?;
                }
            }
        }
    }
    if newly_trained{model.save(&tx)?;}
    // The cursor and learned model are committed in the same transaction.
    put(&tx,"after_id",&after.to_string())?;
    tx.commit().map_err(|e|e.to_string())?;
    crate::online_model::publish_status(&model);
    if let Some(ref metadata)=meta{
        crate::learned_regex::reconcile(local,metadata)?;
    }
    crate::field_learning::reconcile(local)?;
    Ok(())
}
pub fn verify_native_model()->Result<(),String>{
    let db=Connection::open_in_memory().map_err(|e|e.to_string())?;
    let mut model=crate::online_model::OnlineModel::load(&db)?;
    for n in 0..64{
        model.observe(&db,&format!("Test{n}.2160p.WEB-DL"),"resolution","2160p")?;
    }
    let yes=model.score("resolution","2160p","Other.2160p.WEB-DL");
    let no=model.score("codec","2160p","Other.2160p.WEB-DL");
    match (yes,no) {
        (Some(a),Some(b)) if a>b=>Ok(()),
        _=>Err("native FTRL self-test failed".into())
    }
}
pub fn start(history:PathBuf,metadata:PathBuf){
    if !active(){return;}
    let state=PathBuf::from(env::var("MATCHLAB_ML_STATE_FILE")
        .unwrap_or_else(|_|"/data/local-learning.sqlite3".into()));
    let worker=std::thread::Builder::new()
        .name("matchlab-local-learning".into()).spawn(move||{
            let mut maintained=std::time::Instant::now()
                .checked_sub(Duration::from_secs(300))
                .unwrap_or_else(std::time::Instant::now);
            loop{
                if let Err(error)=connect(&state).and_then(|mut db|{
                    if maintained.elapsed()>=Duration::from_secs(300){
                        crate::learning_cache::maintain(&db,&metadata)?;
                        maintained=std::time::Instant::now();
                    }
                    scan(&history,&metadata,&mut db)
                }){
                    tracing::warn!(%error,"local learning cycle skipped");
                }
                std::thread::sleep(Duration::from_secs(POLL_SECONDS));
            }
        });
    if let Err(error)=worker{tracing::warn!(%error,"failed to start local learning");}
}
#[cfg(test)]
mod tests{
    use super::*;
    use serde_json::json;
    #[test]
    fn year_offset_patterns_are_hashed_not_saved_as_films(){
        let d=json!({"parsed":{"year":2024},"selected":{"year":2020}});
        let a=json!({"code":"field_mismatch_year"});
        let (code,key)=pattern(&a,&d).unwrap();
        assert_eq!(code,"field_mismatch_year");
        assert_eq!(key.len(),64);
    }
    #[test]
    fn episode_gap_patterns_separate_distinct_imdb_works(){
        let a=json!({"code":"field_unresolved_episode"});
        let d=json!({"selected":{"imdb_id":"tt1234567"}});
        let e=json!({"selected":{"imdb_id":"tt7654321"}});
        assert_ne!(pattern(&a,&d).unwrap().1,pattern(&a,&e).unwrap().1);
    }
    #[test]
    fn native_inference_is_finite_and_requires_training(){
        verify_native_model().unwrap();
    }
}
