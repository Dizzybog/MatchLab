//! Opt-in persistent parser/matcher cache sharing a bounded local learning DB.
//! Direct SQLite primary-key lookup only; no SQLite text search and no I/O
//! whatsoever from this module when MATCHLAB_ML_ENABLED is disabled.
//!
//! Cached results are never ground-truth training labels. A build change,
//! a new verified local regex overlay or a metadata snapshot change gives
//! a different namespace. Stale entries are RETAINED as low-priority cache
//! evidence and evicted first only when the configured quota is exceeded.
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{env,fs,path::{Path,PathBuf},time::{Duration,SystemTime,UNIX_EPOCH}};

const GIB:u64=1024*1024*1024;
const DEFAULT_GB:u64=1;
const MAX_GB:u64=128;
const MAX_ENTRY_BYTES:usize=512*1024;
const CACHE_TTL_SECS:i64=30*24*3600;
const BUSY_MS:u64=12;

pub fn storage_gb()->u64 {
    env::var("MATCHLAB_ML_MAX_STORAGE_GB")
        .ok().and_then(|value|value.parse::<u64>().ok())
        .filter(|value|(1..=MAX_GB).contains(value))
        .unwrap_or(DEFAULT_GB)
}
pub fn enabled()->bool{
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
/// Obsolete entries are retained ONLY as historical evidence. Neither an
/// expired result nor one from an old parser/regex/metadata namespace may
/// serve a request: both must be parsed/matched again, then cached with
/// current provenance. Recheck the namespace after reading to close the race
/// against a concurrent local-rule or metadata refresh.
fn read_current(db:&Connection,cache_key:&str,ns:&str,time:i64)->Option<Value>{
    let raw:Option<Vec<u8>>=db.query_row(
        "SELECT payload FROM learning_cache
         WHERE cache_key=?1 AND namespace=?2 AND obsolete=0
           AND expires_at>?3",
        params![cache_key,ns,time],|r|r.get(0)
    ).optional().ok()?;
    serde_json::from_slice(raw?.as_slice()).ok()
}
fn read(kind:&str,title:&str,year:Option<i32>,series:Option<bool>,
    metadata:Option<&Path>)->Option<Value>{
    if !enabled(){return None;}
    let ns=namespace(kind,metadata)?;
    let db=Connection::open_with_flags(state_file(),OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    db.busy_timeout(Duration::from_millis(BUSY_MS)).ok()?;
    let value=read_current(&db,&key(&ns,title,year,series),&ns,now())?;
    (namespace(kind,metadata).as_deref()==Some(ns.as_str())).then_some(value)
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
        // Fail closed when the entire local learning DB is at its quota.
        // The low-priority worker will reclaim obsolete rows first.
        if used_bytes(&db).unwrap_or(u64::MAX)>=storage_gb()*GIB {
            return Ok(());
        }
        db.execute(
            "INSERT INTO learning_cache
              (cache_key,namespace,kind,payload,bytes,created_at,expires_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(cache_key) DO UPDATE SET
               payload=excluded.payload,bytes=excluded.bytes,
               created_at=excluded.created_at,expires_at=excluded.expires_at,
               obsolete=0",
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
            created_at INTEGER NOT NULL,expires_at INTEGER NOT NULL,
            obsolete INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS learning_cache_expiry
           ON learning_cache(expires_at);
         CREATE INDEX IF NOT EXISTS learning_cache_oldest
           ON learning_cache(created_at);
         CREATE INDEX IF NOT EXISTS learning_cache_priority
           ON learning_cache(obsolete,created_at);"
    )
}
/// Approximate actual occupied SQLite pages, not only stored payload bytes.
/// The freelist is reusable immediately without a blocking full VACUUM.
fn used_bytes(db:&Connection)->Result<u64,String>{
    let pages:u64=db.pragma_query_value(None,"page_count",|r|r.get(0))
        .map_err(|e|e.to_string())?;
    let reusable:u64=db.pragma_query_value(None,"freelist_count",|r|r.get(0))
        .map_err(|e|e.to_string())?;
    let size:u64=db.pragma_query_value(None,"page_size",|r|r.get(0))
        .map_err(|e|e.to_string())?;
    Ok(pages.saturating_sub(reusable)*size)
}
/// Called off the hot path. Version/expiry changes mark old cache rows as
/// obsolete, never delete them immediately. Under the configured quota,
/// they remain available for historical analysis (but not live cache hits).
pub fn maintain(db:&Connection,metadata:&Path)->Result<(),String>{
    if !enabled(){return Ok(());}
    maintain_with_budget(db,metadata,storage_gb()*GIB)
}
fn maintain_with_budget(db:&Connection,metadata:&Path,budget:u64)->Result<(),String>{
    initialize(db).map_err(|e|e.to_string())?;
    let current_parse=namespace("parse",None).ok_or("parser namespace unavailable")?;
    let current_match=namespace("match",Some(metadata));
    db.execute(
        "UPDATE learning_cache SET obsolete=1 WHERE obsolete=0
           AND (expires_at<=?1
            OR (kind='parse' AND namespace<>?2)
            OR (kind='match' AND namespace<>COALESCE(?3,'')))",
        params![now(),current_parse,current_match]
    ).map_err(|e|e.to_string())?;
    // One shared quota for both learning evidence and cached responses.
    // Never evict merely because one class of data reached an arbitrary
    // sub-budget: eviction starts only when actual DB usage is over the cap.
    let mut occupied=used_bytes(db)?;
    // Only when the WHOLE learning DB exceeds its cap may obsolete learned
    // regex patterns and their old evidence be reclaimed. Foreign keys
    // enforce atomic deletion of the rule's supporting evidence.
    if occupied>budget{
        for _ in 0..64 {
            if occupied<=budget{break;}
            let deleted=crate::learned_regex::evict_obsolete(db,32)?;
            if deleted==0{break;}
            occupied=used_bytes(db)?;
        }
    }
    // Prefer obsolete cache data. Only after all obsolete entries are
    // exhausted may we evict the oldest still-valid cache responses.
    for _ in 0..1024 {
        if occupied<=budget{break;}
        let mut removed=db.execute(
            "DELETE FROM learning_cache WHERE cache_key IN(
               SELECT cache_key FROM learning_cache WHERE obsolete=1
               ORDER BY created_at ASC LIMIT 512)",[]
        ).map_err(|e|e.to_string())?;
        if removed==0{
            removed=db.execute(
                "DELETE FROM learning_cache WHERE cache_key IN(
                   SELECT cache_key FROM learning_cache
                   ORDER BY created_at ASC LIMIT 512)",[]
            ).map_err(|e|e.to_string())?;
        }
        if removed==0{break;}
        occupied=used_bytes(db)?;
    }
    if occupied>budget {
        // Samples hold hashes only and are less important than current
        // validated regex evidence. Delete old unreferenced observations
        // last, never remove active learned rules to satisfy cache quotas.
        for _ in 0..256 {
            if occupied<=budget{break;}
            let removed=db.execute("DELETE FROM samples WHERE key IN (
                SELECT key FROM samples ORDER BY observed_at ASC LIMIT 256)",[])
                .unwrap_or(0);
            if removed==0{break;}
            occupied=used_bytes(db)?;
        }
    }
    // Reuse freed pages and checkpoint opportunistically, without a full
    // VACUUM that could cause CPU/IO spikes on weak hardware.
    let _=db.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    if occupied>budget {
        tracing::warn!(occupied_bytes=occupied,budget_bytes=budget,
            "learning storage remains over budget; future cache writes blocked");
    }
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
    fn stale_cache_must_miss_and_cannot_skip_reparse_or_rematch(){
        let db=Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let ns="current-verified";
        let k=key(ns,"Bleach_300",None,None);
        db.execute("INSERT INTO learning_cache
          (cache_key,namespace,kind,payload,bytes,created_at,expires_at,obsolete)
          VALUES(?1,?2,'match',?3,2,1,?4,0)",
          params![k,ns,b"{}".to_vec(),now()+3600]).unwrap();
        assert_eq!(read_current(&db,&k,ns,now()),Some(serde_json::json!({})));
        db.execute("UPDATE learning_cache SET obsolete=1 WHERE cache_key=?1",
            [&k]).unwrap();
        assert!(read_current(&db,&k,ns,now()).is_none(),
            "an obsolete match must be recomputed, not treated as a hit");
        db.execute("UPDATE learning_cache SET obsolete=0 WHERE cache_key=?1",
            [&k]).unwrap();
        assert!(read_current(&db,&k,"new-parser-build",now()).is_none(),
            "a different parser/rules/metadata namespace must recompute");
        assert!(read_current(&db,&k,ns,now()+3601).is_none(),
            "expired results must recompute");
    }
    #[test]
    fn obsolete_cache_survives_until_pressure_then_is_evicted_first(){
        let db=Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db.execute("INSERT INTO learning_cache(
          cache_key,namespace,kind,payload,bytes,created_at,expires_at)
          VALUES('old','old-parser','parse',X'7B7D',2,1,1)",[]).unwrap();
        // Don't purge stale historical cache while there is spare space.
        maintain_with_budget(&db,Path::new("/nonexistent.sqlite3"),1024*1024).unwrap();
        let inactive:i64=db.query_row("SELECT obsolete FROM learning_cache
            WHERE cache_key='old'",[],|r|r.get(0)).unwrap();
        assert_eq!(inactive,1);
        maintain_with_budget(&db,Path::new("/nonexistent.sqlite3"),0).unwrap();
        let remaining:i64=db.query_row("SELECT count(*) FROM learning_cache",
            [],|r|r.get(0)).unwrap();
        assert_eq!(remaining,0);
    }
}
