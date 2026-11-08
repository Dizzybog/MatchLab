//! Local opt-in learning: hashed field-pattern frequencies, no raw filenames
//! persisted, no network, no changes to production SID/ASID. Optional ONNX
//! scoring is entirely outside request handling and limited to one CPU thread.
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{env,path::{Path,PathBuf},time::Duration};

const POLL_SECONDS:u64=15;
const MAX_PATTERNS:i64=2_000;
const MAX_EXAMPLES:i64=20_000;
const MIN_EXAMPLES:i64=5;
const FEATURES:usize=8;

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
fn bit(value:bool)->f32{if value{1.0}else{0.0}}
fn vector(code:&str,count:i64)->[f32;FEATURES]{
    [
        bit(code=="field_mismatch_year"),
        bit(code=="field_unresolved_episode"),
        bit(code=="field_unresolved_season"),
        bit(code=="field_mismatch_episode"),
        bit(code=="field_mismatch_season"),
        bit(code=="field_missing_anime_episode"),
        bit(code.starts_with("field_missing_")),
        ((count as f32).ln_1p()/10.0).min(1.0)
    ]
}
fn scan(history:&Path,metadata:&Path,local:&mut Connection,model:&mut OptionalModel)->Result<(),String>{
    let history_db=Connection::open_with_flags(history,OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e|e.to_string())?;
    history_db.busy_timeout(Duration::from_secs(2)).map_err(|e|e.to_string())?;
    let newest:i64=history_db.query_row("SELECT COALESCE(MAX(id),0) FROM requests",
        [],|r|r.get(0)).map_err(|e|e.to_string())?;
    // New opt-in does not backfill or export old request history.
    let Some(cursor)=setting(local,"after_id")?else{
        put(local,"after_id",&newest.to_string())?;
        return Ok(());
    };
    let mut after=cursor.parse::<i64>().unwrap_or(newest);
    if newest<after{put(local,"after_id",&newest.to_string())?;return Ok(());}
    let mut stmt=history_db.prepare("SELECT id,title,details FROM requests
        WHERE id>?1 ORDER BY id ASC LIMIT 1000").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([after],|r|Ok((r.get::<_,i64>(0)?,
        r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    let meta=Connection::open_with_flags(metadata,OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
    if let Some(ref metadata)=meta {
        crate::learned_regex::reconcile(local,metadata)?;
    }
    let now=chrono::Utc::now().timestamp();
    let mut samples=count(local,"samples")?;
    let mut patterns=count(local,"patterns")?;
    for (id,title,raw) in rows {
        after=id;
        let Ok(details)=serde_json::from_str::<Value>(&raw)else{continue};
        if let Some(ref metadata)=meta {
            crate::learned_regex::observe(local,metadata,&title,&details)?;
        }
        let flags=crate::field_audit::audit(&title,&details["parsed"],&details["selected"]);
        for flag in flags {
            let Some((code,group))=pattern(&flag,&details)else{continue};
            if samples>=MAX_EXAMPLES{continue;}
            let sample=digest(&format!("{group}:{}",normalized(&title)));
            let exists:i64=local.query_row(
                "SELECT EXISTS(SELECT 1 FROM samples WHERE key=?1)",
                [&sample],|r|r.get(0)).map_err(|e|e.to_string())?;
            if exists>0{continue;}
            let known:i64=local.query_row(
                "SELECT EXISTS(SELECT 1 FROM patterns WHERE key=?1)",
                [&group],|r|r.get(0)).map_err(|e|e.to_string())?;
            if known==0&&patterns>=MAX_PATTERNS{continue;}
            let tx=local.transaction().map_err(|e|e.to_string())?;
            tx.execute("INSERT OR IGNORE INTO samples(key,pattern_key,observed_at)
                VALUES(?1,?2,?3)",params![sample,group,now])
                .map_err(|e|e.to_string())?;
            tx.execute("INSERT INTO patterns(key,field_code,distinct_examples,first_seen,last_seen)
                VALUES(?1,?2,1,?3,?3)
                ON CONFLICT(key) DO UPDATE SET distinct_examples=distinct_examples+1,
                    last_seen=excluded.last_seen",
                params![group,code,now]).map_err(|e|e.to_string())?;
            tx.commit().map_err(|e|e.to_string())?;
            samples+=1;
            patterns+=i64::from(known==0);
            let n:i64=local.query_row("SELECT distinct_examples FROM patterns
                WHERE key=?1",[&group],|r|r.get(0)).map_err(|e|e.to_string())?;
            if n>=MIN_EXAMPLES && let Some(score)=model.score(vector(&code,n)){
                local.execute("UPDATE patterns SET model_score=?1 WHERE key=?2",
                    params![score,group]).map_err(|e|e.to_string())?;
            }
        }
    }
    if let Some(ref metadata)=meta {
        crate::learned_regex::reconcile(local,metadata)?;
    }
    put(local,"after_id",&after.to_string())
}
#[cfg(feature="ml-ort")]
struct OptionalModel{session:Option<ort::session::Session>}
#[cfg(not(feature="ml-ort"))]
struct OptionalModel;
impl OptionalModel {
    fn new()->Self{
        #[cfg(not(feature="ml-ort"))]{
            tracing::info!("local learning: Rust frequency analysis only; optional ort feature is not compiled");
            Self
        }
        #[cfg(feature="ml-ort")]{
            use ort::session::Session;
            let result=(||->Result<Session,String>{
                let library=env::var("ORT_DYLIB_PATH")
                    .map_err(|_|"ORT_DYLIB_PATH is unset")?;
                let location=env::var("MATCHLAB_ML_MODEL_PATH")
                    .map_err(|_|"MATCHLAB_ML_MODEL_PATH is unset")?;
                let path=Path::new(&location);
                if !path.is_file()||path.metadata().map_err(|e|e.to_string())?.len()>4*1024*1024{
                    return Err("ONNX model must be a local file <=4MiB".into());
                }
                // CPU only, single thread, no downloads, no runtime loading
                // unless the user explicitly enabled local learning.
                let env=ort::init_from(&library).map_err(|e|e.to_string())?
                    .with_intra_threads(1).map_err(|e|e.to_string())?
                    .with_inter_threads(1).map_err(|e|e.to_string())?
                    .build().map_err(|e|e.to_string())?;
                Session::builder(&env).map_err(|e|e.to_string())?
                    .with_intra_threads(1).map_err(|e|e.to_string())?
                    .with_inter_threads(1).map_err(|e|e.to_string())?
                    .commit_from_file(path).map_err(|e|e.to_string())
            })();
            match result{
                Ok(session)=>Self{session:Some(session)},
                Err(error)=>{
                    tracing::warn!(%error,"optional ONNX model unavailable; Rust pattern statistics remain active");
                    Self{session:None}
                }
            }
        }
    }
    fn score(&mut self,x:[f32;FEATURES])->Option<f32>{
        #[cfg(not(feature="ml-ort"))]{let _=x;None}
        #[cfg(feature="ml-ort")]{
            let session=self.session.as_mut()?;
            let tensor=ort::value::Tensor::from_array((
                [1usize,FEATURES],x.to_vec().into_boxed_slice())).ok()?;
            let result=session.run(ort::inputs![tensor]).ok()?;
            let (_,values)=result[0].try_extract_tensor::<f32>().ok()?;
            values.first().copied().filter(|v|v.is_finite()&&(0.0..=1.0).contains(v))
        }
    }
}
pub fn start(history:PathBuf,metadata:PathBuf){
    if !active(){return;}
    let state=PathBuf::from(env::var("MATCHLAB_ML_STATE_FILE")
        .unwrap_or_else(|_|"/data/local-learning.sqlite3".into()));
    let worker=std::thread::Builder::new()
        .name("matchlab-local-learning".into()).spawn(move||{
            let mut model=OptionalModel::new();
            loop{
                if let Err(error)=connect(&state).and_then(|mut db|
                    scan(&history,&metadata,&mut db,&mut model)){
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
        assert_eq!(vector(&code,5)[0],1.0);
    }
    #[test]
    fn episode_gap_patterns_separate_distinct_imdb_works(){
        let a=json!({"code":"field_unresolved_episode"});
        let d=json!({"selected":{"imdb_id":"tt1234567"}});
        let e=json!({"selected":{"imdb_id":"tt7654321"}});
        assert_ne!(pattern(&a,&d).unwrap().1,pattern(&a,&e).unwrap().1);
    }
}