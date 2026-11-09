//! Opt-in persistent parser/matcher cache sharing a bounded local learning DB.
//! Direct SQLite primary-key lookup only; no SQLite text search and no I/O
//! whatsoever from this module when MATCHLAB_ML_ENABLED is disabled.
//!
//! Cached results are never ground-truth training labels. A build change,
//! a new verified local regex overlay or a metadata snapshot change gives
//! a different namespace. Stale entries are RETAINED as low-priority cache
//! evidence and evicted first only when the configured quota is exceeded.
use rusqlite::{params,Connection};
#[cfg(test)] use rusqlite::OptionalExtension;
use serde_json::{json,Value};
use std::collections::{HashMap,VecDeque};
use std::sync::{OnceLock,RwLock,mpsc::{sync_channel,SyncSender,TrySendError,RecvTimeoutError},atomic::{AtomicBool,AtomicU64,Ordering}};
use sha2::{Digest,Sha256};
use std::{env,fs,path::{Path,PathBuf},time::{Duration,SystemTime,UNIX_EPOCH}};

const GIB:u64=1024*1024*1024;
const DEFAULT_GB:u64=1;
const MAX_GB:u64=128;
const MAX_ENTRY_BYTES:usize=512*1024;
const CACHE_TTL_SECS:i64=30*24*3600;
const DEFAULT_TRAINING_EXAMPLES:i64=12_000;
const MAX_TRAINING_EXAMPLES:i64=2_000_000;
const DEFAULT_CACHE_ENTRIES:i64=100_000;
const MAX_CACHE_ENTRIES:i64=2_000_000;
static CACHE_HITS:AtomicU64=AtomicU64::new(0);
static CACHE_MISSES:AtomicU64=AtomicU64::new(0);
// Updated by background maintenance, never by individual request writes.
static CACHE_WRITES_ALLOWED:AtomicBool=AtomicBool::new(true);
 // Cache is acceleration only: never compete with the local learner for
// SQLite write locks on each public request.
const CACHE_QUEUE_SIZE:usize=4096;
const CACHE_BATCH_SIZE:usize=32;
// Keep disk reads and writes off the HTTP hot path. 32 MiB is the maximum
// memory footprint of the hot result tier; cold entries are re-parsed.
const HOT_MAX_ENTRIES:usize=2048;
const HOT_MAX_BYTES:usize=32*1024*1024;
const HOT_MAX_ENTRY_BYTES:usize=128*1024;
static HOT_CACHE:OnceLock<RwLock<HotStore>>=OnceLock::new();
static CACHE_MAINTENANCE_REQUESTED:AtomicBool=AtomicBool::new(false);
static CACHE_MAINTENANCE_METADATA:OnceLock<PathBuf>=OnceLock::new();

static CACHE_DROPPED:AtomicU64=AtomicU64::new(0);
static CACHE_WRITER:OnceLock<Option<SyncSender<CacheEvent>>>=OnceLock::new();

struct CacheEvent{
    cache_key:String,
    namespace:String,
    kind:String,
    payload:Vec<u8>,
    created_at:i64,
    expires_at:i64,
}

// Memory-only fast path. The persistent cache is written by one background
// actor and optionally hydrated at startup, never opened by HTTP threads.
struct HotEntry{
    namespace:String,
    value:Value,
    expires_at:i64,
    bytes:usize,
    sequence:u64,
}
#[derive(Default)]
struct HotStore{
    rows:HashMap<String,HotEntry>,
    fifo:VecDeque<(String,u64)>,
    occupied:usize,
    sequence:u64,
}
impl HotStore{
    fn insert(&mut self,key:String,namespace:String,value:Value,
        expires_at:i64,bytes:usize)
    {
        if bytes>HOT_MAX_ENTRY_BYTES || expires_at<=now(){return;}
        if let Some(old)=self.rows.remove(&key){
            self.occupied=self.occupied.saturating_sub(old.bytes);
        }
        self.sequence=self.sequence.wrapping_add(1);
        let stamp=self.sequence;
        self.rows.insert(key.clone(),HotEntry{
            namespace,value,expires_at,bytes,sequence:stamp,
        });
        self.occupied=self.occupied.saturating_add(bytes);
        self.fifo.push_back((key,stamp));
        // A repeated key can leave obsolete FIFO markers. Ignore them, but
        // also cap marker count so identical requests cannot leak memory.
        while self.rows.len()>HOT_MAX_ENTRIES
            ||self.occupied>HOT_MAX_BYTES
            ||self.fifo.len()>HOT_MAX_ENTRIES*2
        {
            let Some((old_key,old_stamp))=self.fifo.pop_front()else{break;};
            if self.rows.get(&old_key).is_some_and(|v|v.sequence==old_stamp){
                if let Some(old)=self.rows.remove(&old_key){
                    self.occupied=self.occupied.saturating_sub(old.bytes);
                }
            }
        }
    }
}
fn hot_cache()->&'static RwLock<HotStore>{
    HOT_CACHE.get_or_init(||RwLock::new(HotStore::default()))
}
fn hot_insert(key:String,namespace:String,value:Value,expires_at:i64,bytes:usize){
    if let Ok(mut cache)=hot_cache().try_write(){
        cache.insert(key,namespace,value,expires_at,bytes);
    }
}
/// Startup warmup must never overwrite a newer result already submitted by
/// an API request while the cache actor was still opening SQLite.
fn hot_restore(key:String,namespace:String,value:Value,expires_at:i64,bytes:usize){
    if let Ok(mut cache)=hot_cache().try_write(){
        if !cache.rows.contains_key(&key){
            cache.insert(key,namespace,value,expires_at,bytes);
        }
    }
}
fn hot_lookup(key:&str,ns:&str,time:i64)->Option<Value>{
    let cache=hot_cache().try_read().ok()?;
    let entry=cache.rows.get(key)?;
    (entry.namespace==ns && entry.expires_at>time).then(||entry.value.clone())
}
/// Cache now has its own database and quota. Sharing a writer with training
/// caused repeated SQLITE_BUSY, stalled local learning and saturated writes.
fn cache_storage_gb()->u64{
    env::var("MATCHLAB_ML_CACHE_MAX_STORAGE_GB")
        .ok().and_then(|raw|raw.parse::<u64>().ok())
        .filter(|value|(1..=MAX_GB).contains(value))
        .unwrap_or(DEFAULT_GB)
}
pub fn storage_gb()->u64 {
    env::var("MATCHLAB_ML_MAX_STORAGE_GB")
        .ok().and_then(|value|value.parse::<u64>().ok())
        .filter(|value|(1..=MAX_GB).contains(value))
        .unwrap_or(DEFAULT_GB)
}
/// The number of unique, validated field observations retained for each
/// current source build. This is deliberately separate from on-disk GiB:
/// disk quota and sample diversity are different constraints.
fn bounded_count(raw:Option<&str>,default:i64,min:i64,max:i64)->i64{
    raw.and_then(|s|s.parse::<i64>().ok())
        .filter(|value|(*value>=min)&&(*value<=max))
        .unwrap_or(default)
}
pub fn training_max_examples()->i64{
    bounded_count(env::var("MATCHLAB_ML_TRAINING_MAX_EXAMPLES").ok().as_deref(),
        DEFAULT_TRAINING_EXAMPLES,100,MAX_TRAINING_EXAMPLES)
}
fn cache_max_entries()->i64{
    bounded_count(env::var("MATCHLAB_ML_CACHE_MAX_ENTRIES").ok().as_deref(),
        DEFAULT_CACHE_ENTRIES,100,MAX_CACHE_ENTRIES)
}
fn cache_ttl_secs()->i64{
    bounded_count(env::var("MATCHLAB_ML_CACHE_TTL_HOURS").ok().as_deref(),
        CACHE_TTL_SECS/3600,1,30*24)*3600
}
fn miss_ttl_secs()->i64{
    bounded_count(env::var("MATCHLAB_ML_UNMATCHED_CACHE_TTL_MINUTES")
        .ok().as_deref(),30,1,24*60)*60
}
pub fn status()->Value{
    json!({"enabled":enabled(),"learning_storage_limit_gb":storage_gb(),
        "cache_storage_limit_gb":cache_storage_gb(),
        "cache_isolated_sqlite":true,
        "cache_lookup":"nonblocking_in_memory",
        "hot_cache_entries":HOT_CACHE.get().and_then(|cache|cache.try_read().ok())
            .map(|cache|cache.rows.len()).unwrap_or(0),
        "training_max_examples":training_max_examples(),
        "cache_max_entries":cache_max_entries(),
        "matched_cache_ttl_hours":cache_ttl_secs()/3600,
        "unmatched_cache_ttl_minutes":miss_ttl_secs()/60,
        "hits":CACHE_HITS.load(Ordering::Relaxed),
        "misses":CACHE_MISSES.load(Ordering::Relaxed),
        "cache_queue_dropped":CACHE_DROPPED.load(Ordering::Relaxed),
        "cache_writes_allowed":CACHE_WRITES_ALLOWED.load(Ordering::Relaxed),
        "stale_entries_reparsed_and_rematched":true})
}
pub fn enabled()->bool{
    crate::local_learning::active() && env::var("MATCHLAB_ML_CACHE_ENABLED")
        .ok().is_none_or(|flag|flag.eq_ignore_ascii_case("true"))
}
fn state_file()->PathBuf{
    PathBuf::from(env::var("MATCHLAB_ML_CACHE_STATE_FILE")
        .unwrap_or_else(|_|"/data/ml-result-cache.sqlite3".to_owned()))
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
    format!("{}:{}:{:016x}:{:016x}:{:016x}:{:016x}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"),
        3,crate::learned_regex::revision(),crate::field_learning::revision(),
        crate::tv_learning::revision(),
        matchlab_parser::handler_patch_revision())
}
fn namespace(kind:&str,metadata:Option<&Path>)->Option<String>{
    let stamp=if kind=="match" {metadata_stamp(metadata?)?}else{String::new()};
    Some(digest(&format!("{kind}:{}:{stamp}",revision())))
}
/// Snapshot of the exact parser/regex/metadata version that produced a
/// match. Callers use it to avoid publishing or caching an answer computed
/// across a concurrent ruleset or metadata refresh.
pub fn match_namespace(metadata:&Path)->Option<String>{
    if !enabled(){return None;}
    namespace("match",Some(metadata))
}
fn key(namespace:&str,title:&str,year:Option<i32>,series:Option<bool>)->String{
    digest(&format!("{namespace}\0{title}\0{year:?}\0{series:?}"))
}
/// Obsolete entries are retained ONLY as historical evidence. Neither an
/// expired result nor one from an old parser/regex/metadata namespace may
/// serve a request: both must be parsed/matched again, then cached with
/// current provenance. Recheck the namespace after reading to close the race
/// against a concurrent local-rule or metadata refresh.
/// Never open SQLite on a live /parse or /match request. Cold entries are
/// recomputed once, then stored in the hot tier and a background disk queue.
/// The cache is only acceleration; stale generations are never reused.
fn read(kind:&str,title:&str,year:Option<i32>,series:Option<bool>,
    metadata:Option<&Path>)->Option<Value>{
    if !enabled(){return None;}
    // Start a single worker lazily; it hydrates recent durable cache entries
    // without blocking the caller or competing with the ML training writer.
    let _=CACHE_WRITER.get_or_init(cache_worker);
    let ns=namespace(kind,metadata)?;
    let value=hot_lookup(&key(&ns,title,year,series),&ns,now());
    let valid=if namespace(kind,metadata).as_deref()==Some(ns.as_str()){
        value
    }else{None};
    if valid.is_some(){CACHE_HITS.fetch_add(1,Ordering::Relaxed);}
    else{CACHE_MISSES.fetch_add(1,Ordering::Relaxed);}
    valid
}
fn hydrate_hot_cache(db:&Connection)->rusqlite::Result<()>{
    let time=now();
    let mut stmt=db.prepare(
        "SELECT cache_key,namespace,payload,expires_at,bytes
         FROM learning_cache WHERE obsolete=0 AND expires_at>?1
           AND bytes<=?2
         ORDER BY created_at DESC LIMIT 256")?;
    let rows=stmt.query_map(params![time,HOT_MAX_ENTRY_BYTES as i64],|row|Ok((
        row.get::<_,String>(0)?,row.get::<_,String>(1)?,
        row.get::<_,Vec<u8>>(2)?,row.get::<_,i64>(3)?,
        row.get::<_,i64>(4)?
    )))?;
    for row in rows{
        let (key,ns,raw,expires,bytes)=row?;
        if bytes<0 || bytes as usize>HOT_MAX_ENTRY_BYTES{continue;}
        if let Ok(value)=serde_json::from_slice::<Value>(&raw){
            hot_restore(key,ns,value,expires,raw.len());
        }
    }
    Ok(())
}

fn put_batch(db:&mut Connection,events:&[CacheEvent])->rusqlite::Result<()>{
    if events.is_empty(){return Ok(());}
    let tx=db.transaction()?;
    {
        let mut stmt=tx.prepare_cached(
            "INSERT INTO learning_cache
              (cache_key,namespace,kind,payload,bytes,created_at,expires_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(cache_key) DO UPDATE SET
               payload=excluded.payload,bytes=excluded.bytes,
               created_at=excluded.created_at,expires_at=excluded.expires_at,
               obsolete=0"
        )?;
        for item in events {
            stmt.execute(params![
                item.cache_key,item.namespace,item.kind,item.payload,
                item.payload.len() as i64,item.created_at,item.expires_at
            ])?;
        }
    }
    tx.commit()
}

/// One StremThru-style bounded background worker owns the cache DB.
/// Work is keyed/idempotent via cache_key, batched, and never makes the
/// parsing/matching worker wait for a SQLite writer or checkpoint.
fn cache_worker()->Option<SyncSender<CacheEvent>>{
    let (tx,rx)=sync_channel::<CacheEvent>(CACHE_QUEUE_SIZE);
    let thread=std::thread::Builder::new()
        .name("matchlab-cache-writer".into()).spawn(move||{
            let path=state_file();
            let mut connection:Option<Connection>=None;
            let mut hydrated=false;
            loop{
                let first=match rx.recv_timeout(Duration::from_secs(5)){
                    Ok(item)=>Some(item),
                    Err(RecvTimeoutError::Timeout)=>None,
                    Err(RecvTimeoutError::Disconnected)=>break,
                };
                let mut batch=Vec::with_capacity(CACHE_BATCH_SIZE);
                if let Some(item)=first{
                    batch.push(item);
                    while batch.len()<CACHE_BATCH_SIZE{
                        match rx.try_recv(){
                            Ok(next)=>batch.push(next),
                            Err(_)=>break,
                        }
                    }
                }
                let maintenance=CACHE_MAINTENANCE_REQUESTED
                    .swap(false,Ordering::AcqRel);
                if batch.is_empty()&&!maintenance{continue;}
                if connection.is_none(){
                    connection=(||->rusqlite::Result<Connection>{
                        if let Some(parent)=path.parent(){
                            std::fs::create_dir_all(parent).map_err(|_|{
                                rusqlite::Error::InvalidPath(parent.to_path_buf())
                            })?;
                        }
                        let db=Connection::open(&path)?;
                        db.busy_timeout(Duration::from_millis(150))?;
                        db.execute_batch("PRAGMA journal_mode=WAL;")?;
                        initialize(&db)?;
                        Ok(db)
                    })().ok();
                    hydrated=false;
                }
                let Some(db)=connection.as_mut()else{
                    if maintenance{
                        CACHE_MAINTENANCE_REQUESTED.store(true,Ordering::Release);
                    }
                    continue;
                };
                if !hydrated{
                    if let Err(error)=hydrate_hot_cache(db){
                        tracing::debug!(%error,"ML disk cache warmup deferred");
                    }
                    hydrated=true;
                }
                if !batch.is_empty() && CACHE_WRITES_ALLOWED.load(Ordering::Acquire){
                    if let Err(error)=put_batch(db,&batch){
                        tracing::debug!(%error,
                            "ML cache batch deferred; cache acceleration only");
                        connection=None;
                    }
                }
                if maintenance{
                    if let Some(metadata)=CACHE_MAINTENANCE_METADATA.get(){
                        if let Some(db)=connection.as_ref(){
                            if let Err(error)=maintain_with_budget(
                                db,metadata,cache_storage_gb()*GIB){
                                tracing::debug!(%error,
                                    "ML cache maintenance will retry");
                                CACHE_MAINTENANCE_REQUESTED
                                    .store(true,Ordering::Release);
                            }
                        }else{
                            CACHE_MAINTENANCE_REQUESTED
                                .store(true,Ordering::Release);
                        }
                    }
                }
            }
        });
    if thread.is_err(){
        tracing::warn!("cache writer unavailable; matching continues uncached");
        None
    }else{Some(tx)}
}
fn write(kind:&str,title:&str,year:Option<i32>,series:Option<bool>,
    metadata:Option<&Path>,value:&Value){
    if !enabled() || !CACHE_WRITES_ALLOWED.load(Ordering::Acquire){return;}
    let Some(ns)=namespace(kind,metadata)else{return;};
    let Ok(raw)=serde_json::to_vec(value)else{return;};
    if raw.len()>MAX_ENTRY_BYTES{return;}
    let time=now();
    let event=CacheEvent{
        cache_key:key(&ns,title,year,series),namespace:ns,
        kind:kind.to_owned(),payload:raw,created_at:time,
        expires_at:time+if kind=="match"&&value["selected"].is_null(){
            miss_ttl_secs()
        }else{cache_ttl_secs()}
    };
    // A small in-memory hot tier prevents every repeated request from
    // reopening SQLite. The disk queue is best-effort and never blocks matching.
    hot_insert(event.cache_key.clone(),event.namespace.clone(),value.clone(),
        event.expires_at,event.payload.len());
    let Some(tx)=CACHE_WRITER.get_or_init(cache_worker).as_ref()else{return;};
    if let Err(e)=tx.try_send(event){
        let n=CACHE_DROPPED.fetch_add(1,Ordering::Relaxed)+1;
        if n==1||n%4096==0 {
            tracing::warn!(dropped=n,disconnected=matches!(e,TrySendError::Disconnected(_)),
                "ML result cache queue saturated; matching unaffected");
        }
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
/// The separate ML evidence database still needs its own quota maintenance.
/// Only the single learning worker calls this (never API/cache threads), and
/// only after all shareable historical rules have been archived. Active
/// rules are protected: evict obsolete candidates, then oldest samples.
pub fn maintain_learning(db:&Connection)->Result<(),String>{
    let budget=storage_gb()*GIB;
    let mut occupied=used_bytes(db)?;
    if occupied<=budget{return Ok(());}
    for _ in 0..16{
        let regex_removed=crate::learned_regex::evict_obsolete(db,32)?;
        let fields_removed=crate::field_learning::evict_obsolete(db,512)?;
        if regex_removed+fields_removed==0{break;}
        occupied=used_bytes(db)?;
        if occupied<=budget{return Ok(());}
    }
    for _ in 0..16{
        let removed=db.execute(
            "DELETE FROM samples WHERE key IN (
                SELECT key FROM samples ORDER BY observed_at ASC LIMIT 256
            )",[]).map_err(|e|e.to_string())?;
        if removed==0{break;}
        occupied=used_bytes(db)?;
        if occupied<=budget{return Ok(());}
    }
    // A persistent archive or active regex must never be discarded to
    // force the disk below quota. Keep the model running but report pressure.
    tracing::warn!(occupied_bytes=occupied,budget_bytes=budget,
        "ML learning evidence remains over quota; active rules and history preserved");
    Ok(())
}

/// Called off the hot path. Cache maintenance never touches training evidence.
/// Request cache maintenance without touching SQLite in the ML learner.
/// The cache's sole writer performs maintenance between bounded write batches.
pub fn maintain(_db:&Connection,metadata:&Path)->Result<(),String>{
    if !enabled(){return Ok(());}
    let _=CACHE_MAINTENANCE_METADATA.set(metadata.to_path_buf());
    CACHE_MAINTENANCE_REQUESTED.store(true,Ordering::Release);
    let _=CACHE_WRITER.get_or_init(cache_worker);
    Ok(())
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
    // This SQLite database only contains cached responses. Rule evidence and
    // the offline sharing archive remain untouched in local-learning.sqlite3.
    let mut occupied=used_bytes(db)?;
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
    // Bound the number of repeat-query entries separately from the corpus
    // size. This runs off-path, pruning stale records first, never training
    // evidence. Cache misses reparse and rematch with current source data.
    let max_entries=cache_max_entries();
    let cached:i64=db.query_row("SELECT COUNT(*) FROM learning_cache",[],
        |row|row.get(0)).map_err(|e|e.to_string())?;
    if cached>max_entries{
        let excess=cached-max_entries;
        db.execute("DELETE FROM learning_cache WHERE cache_key IN (
            SELECT cache_key FROM learning_cache ORDER BY obsolete DESC,
                created_at ASC,cache_key LIMIT ?1)",[excess])
            .map_err(|e|e.to_string())?;
    }
    // Reuse freed pages and checkpoint opportunistically, without a full
    // VACUUM that could cause CPU/IO spikes on weak hardware.
    let _=db.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    // Compute the final quota state after cache-count pruning as well.
    // Only the background worker executes this SQLite page accounting.
    let occupied=used_bytes(db)?;
    CACHE_WRITES_ALLOWED.store(occupied<=budget,Ordering::Release);
    if occupied>budget {
        tracing::warn!(occupied_bytes=occupied,budget_bytes=budget,
            "learning storage remains over budget; future cache writes blocked");
    }
    Ok(())
}
/// Disk-only regression helper; production never calls this from requests.
#[cfg(test)]
fn read_current(db:&Connection,cache_key:&str,ns:&str,time:i64)->Option<Value>{
    let raw:Option<Vec<u8>>=db.query_row(
        "SELECT payload FROM learning_cache
         WHERE cache_key=?1 AND namespace=?2 AND obsolete=0
           AND expires_at>?3",
        params![cache_key,ns,time],|r|r.get(0)
    ).optional().ok()?;
    serde_json::from_slice(raw?.as_slice()).ok()
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn bounded_cache_batch_preserves_current_namespace(){
        let mut db=Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let event=CacheEvent{cache_key:"key".into(),namespace:"new".into(),
            kind:"parse".into(),payload:br#"{"title":"Bleach"}"#.to_vec(),
            created_at:1,expires_at:i64::MAX};
        put_batch(&mut db,&[event]).unwrap();
        assert_eq!(read_current(&db,"key","new",2).unwrap()["title"],"Bleach");
        assert!(read_current(&db,"key","old",2).is_none());
    }

    #[test]
    fn hot_cache_ignores_old_versions_and_stays_bounded(){
        let mut cache=HotStore::default();
        let expires=now()+300;
        cache.insert("episode".into(),"v1".into(),json!({"episodes":[1]}),
            expires,40);
        assert_eq!(cache.rows["episode"].namespace,"v1");
        for index in 0..(HOT_MAX_ENTRIES+100){
            cache.insert(format!("key-{index}"),"v2".into(),json!(index),
                expires,40);
        }
        assert!(cache.rows.len()<=HOT_MAX_ENTRIES);
        assert!(cache.occupied<=HOT_MAX_BYTES);
        assert!(!cache.rows.contains_key("episode"));
        assert!(cache.fifo.len()<=HOT_MAX_ENTRIES*2);
    }
    #[test]
    fn hot_cache_rewrites_keep_fifo_memory_bounded(){
        let mut cache=HotStore::default();
        for i in 0..(HOT_MAX_ENTRIES*5){
            cache.insert("same".into(),"stable".into(),json!(i),
                now()+60,40);
        }
        assert_eq!(cache.rows.len(),1);
        assert!(cache.fifo.len()<=HOT_MAX_ENTRIES*2);
    }
    #[test]
    fn configurable_corpus_size_and_cache_limits_reject_invalid_values(){
        assert_eq!(bounded_count(Some("250000"),12_000,100,2_000_000),250_000);
        for raw in ["0","-2","2000001","not-a-number","1.5"]{
            assert_eq!(bounded_count(Some(raw),12_000,100,2_000_000),12_000);
        }
        assert_eq!(bounded_count(None,12_000,100,2_000_000),12_000);
    }
    #[test]
    fn unmatched_entries_are_short_lived_and_stale_namespaces_never_hit(){
        let db=Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let ns="snapshot";
        let k=key(ns,"Film.S02E07",None,None);
        db.execute("INSERT INTO learning_cache(cache_key,namespace,kind,payload,bytes,
            created_at,expires_at) VALUES(?1,?2,'match',?3,2,1,?4)",
            params![k,ns,b"{}".to_vec(),now()+30*60]).unwrap();
        assert!(read_current(&db,&k,ns,now()+30*60+1).is_none());
        assert!(read_current(&db,&k,"new-regex",now()).is_none());
    }
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
