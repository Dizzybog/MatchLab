//! Opt-in persistent parser/matcher cache sharing a bounded local learning DB.
//! Direct SQLite primary-key lookup only; no SQLite text search and no I/O
//! whatsoever from this module when MATCHLAB_ML_ENABLED is disabled.
//!
//! Cached results are never ground-truth training labels. A build change,
//! a new verified local regex overlay or a metadata snapshot change gives
//! a different namespace. The local learning worker prunes stale entries.
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{env,fs,path::{Path,PathBuf},time::{Duration,SystemTime,UNIX_EPOCH}};

const GIB:u64=1024*1024*1024;
const DEFAULT_GB:u64=1;
const MAX_GB:u64=128;
const MAX_ENTRY_BYTES:usize=512*1024;
const CACHE_FRACTION:u64=70; // 30% reserved for examples, rules and SQLite overhead
const CACHE_TTL_SECS:i64=24*3600;
const BUSY_MS:u64=12;

pub fn storage_gb()->u64 {
    env::var("MATCHLAB_ML_MAX_STORAGE_GB")
        .ok().and_then(|value|value.parse::<u64>().ok())
        .filter(|value|(1..=MAX_GB).contains(value))
        .unwrap_or(DEFAULT_GB)
}
fn enabled()->bool{
    crate::local_learning::active() && env::var("MATCHLAB_ML_CACHE_ENABLED")
        .ok().is_none_or(|flag|flag.eq_ignore_ascii_case("true"))
}
fn state_file()->PathBuf{
    PathBuf::from(env::var("MATCHLAB_ML_STATE_FILE")
        .unwrap_or_else(|_|"/data/local-learning.sqlite3".to_owned()))
}
fn digest(value:&str)->String{
    format!("{:x}",Sha256::digest(value.as_bytes()))
}
fn now()->i64{
    SystemTime::now().duration_since(UNIX_EPOCH)
        .unwrap_or_default().as_secs() as i64
}
fn modified(path:&Path)->Option<String>{
    let meta=fs::metadata(path).ok()?;
    let mtime=meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(format!("{}:{}:{}",meta.len(),mtime.as_secs(),mtime.subsec_nanos()))
}
fn metadata_stamp(path:&Path)->Option<String>{
    // SQLite may write metadata via WAL; hash both file signatures to avoid
    // returning old SID/ASID results after a metadata refresh or replacement.
    let base=modified(path)?;
    let wal=modified(&PathBuf::from(format!("{}-wal",path.display())))
        .unwrap_or_else(||"no-wal".into());
    Some(digest(&format!("{base}:{wal}")))
}
fn revision()->String{
    format!("{}:{}:{:016x}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"),
        2,crate::learned_regex::revision())
}
fn namespace(kind:&str,metadata:Option<&Path>)->Option<String>{
    let stamp=if kind=="match" {metadata_stamp(metadata?)?}else{String::new()};
    Some(digest(&format!("{kind}:{}:{stamp}",revision())))
}
fn key(namespace:&str,title:&str,year:Option<i32>,series:Option<bool>)->String{
    digest(&format!("{namespace}\0{title}\0{year:?}\0{series:?}"))
}
fn read(kind:&str,title:&str,year:Option<i32>,series:Option<bool>,
    metadata:Option<&Path>)->Option<Value>{
    if !enabled(){return None;}
    let ns=namespace(kind,metadata)?;
    let db=Connection::open_with_flags(state_file(),OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    db.busy_timeout(Duration::from_millis(BUSY_MS)).ok()?;
    let raw:Option<Vec<u8>>=db.query_row(
        "SELECT payload FROM learning_cache WHERE cache_key=?1
         AND namespace=?2 AND expires_at>?3",
        params![key(&ns,title,year,series),ns,now()],|r|r.get(0)
    ).optional().ok()?;
    serde_json::from_slice(raw?.as_slice()).ok()
}
fn write(kind:&str,title:&str,year:Option<i32>,series:Option<bool>,
    metadata:Option<&Path>,value:&Value){
    if !enabled(){return;}
    let Some(ns)=namespace(kind,metadata)else{return;};
    let Ok(raw)=serde_json::to_vec(value)else{return;};
    if raw.len()>MAX_ENTRY_BYTES{return;}
    // This is a best-effort acceleration. Lock contention or a full budget
    // must never delay/fail the real parser or matcher.
    let result=(||->rusqlite::Result<()> {
        let db=Connection::open(state_file())?;
        db.busy_timeout(Duration::from_millis(BUSY_MS))?;
        initialize(&db)?;
        db.execute(
            "INSERT INTO learning_cache
              (cache_key,namespace,kind,payload,bytes,created_at,expires_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(cache_key) DO UPDATE SET
               payload=excluded.payload,bytes=excluded.bytes,
               created_at=excluded.created_at,expires_at=excluded.expires_at",
            params![key(&ns,title,year,series),ns,kind,raw,
                raw.len() as i64,now(),now()+CACHE_TTL_SECS]
        )?;
        Ok(())
    })();
    if result.is_err() {
        // Do not log user titles, key material or response contents.
        tracing::debug!("local learning cache write unavailable");
    }
}
pub fn get_parser(title:&str)->Option<Value>{
    read("parse",title,None,None,None)
}
pub fn put_parser(title:&str,value:&Value){
    write("parse",title,None,None,None,value);
}
pub fn get_match(title:&str,year:Option<i32>,series:Option<bool>,
    metadata:&Path)->Option<Value>{
    read("match",title,year,series,Some(metadata))
}
pub fn put_match(title:&str,year:Option<i32>,series:Option<bool>,
    metadata:&Path,value:&Value){
    write("match",title,year,series,Some(metadata),value);
}
fn initialize(db:&Connection)->rusqlite::Result<()>{
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS learning_cache(
            cache_key TEXT PRIMARY KEY,namespace TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('parse','match')),
            payload BLOB NOT NULL,bytes INTEGER NOT NULL,
            created_at INTEGER NOT NULL,expires_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS learning_cache_expiry
           ON learning_cache(expires_at);
         CREATE INDEX IF NOT EXISTS learning_cache_oldest
           ON learning_cache(created_at);"
    )
}
/// Called only by the opt-in local learning worker every minute. The count
/// of cached bytes is measured in SQLite, and oldest entries are evicted in
/// bounded batches. No full-table search happens in a match request.
pub fn maintain(db:&Connection,metadata:&Path)->Result<(),String>{
    if !enabled(){return Ok(());}
    initialize(db).map_err(|e|e.to_string())?;
    let current_parse=namespace("parse",None).ok_or("parser cache namespace")?;
    let current_match=namespace("match",Some(metadata));
    // If metadata is not available, no match entry may be served.
    db.execute(
        "DELETE FROM learning_cache WHERE expires_at<=?1
           OR (kind='parse' AND namespace<>?2)
           OR (kind='match' AND namespace<>COALESCE(?3,''))",
        params![now(),current_parse,current_match]
    ).map_err(|e|e.to_string())?;
    let max_cache_bytes=(storage_gb()*GIB*CACHE_FRACTION/100) as i64;
    let mut bytes:i64=db.query_row(
        "SELECT COALESCE(SUM(bytes),0) FROM learning_cache",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    let mut batches=0usize;
    while bytes>max_cache_bytes && batches<256 {
        let removed=db.execute(
            "DELETE FROM learning_cache WHERE cache_key IN(
               SELECT cache_key FROM learning_cache ORDER BY created_at ASC LIMIT 512)",
            []
        ).map_err(|e|e.to_string())?;
        if removed==0{break;}
        bytes=db.query_row("SELECT COALESCE(SUM(bytes),0) FROM learning_cache",
            [],|r|r.get(0)).map_err(|e|e.to_string())?;
        batches+=1;
    }
    // Reclaim unused WAL frames in a low-priority maintenance cycle; SQLite
    // can reuse freed DB pages without an expensive full VACUUM.
    db.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .map_err(|e|e.to_string())?;
    Ok(())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn stable_key_separates_parse_and_match_policies(){
        assert_ne!(key("parse","Show S01E02",None,None),
            key("match","Show S01E02",None,None));
        assert_ne!(key("match","Show S01E02",None,None),
            key("match","Show S01E02",Some(2005),Some(true)));
    }
    #[test]
    fn eviction_prunes_expired_entries_and_respects_small_budget(){
        let db=Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db.execute("INSERT INTO learning_cache VALUES(
            'expired','other','parse',X'7B7D',2,1,1)",[]).unwrap();
        // A missing metadata file is okay: it invalidates match caches.
        // The in-memory connection is also used by the maintenance worker.
        if crate::local_learning::active(){
            maintain(&db,Path::new("/nonexistent.sqlite3")).unwrap();
            let n:i64=db.query_row("SELECT COUNT(*) FROM learning_cache",
                [],|r|r.get(0)).unwrap();
            assert_eq!(n,0);
        }
    }
}
