//! Persistent, bounded request history and independent 14-day statistics.
//! The metadata database is separate and is never modified here.
use rusqlite::{params, Connection};
use sha2::{Digest,Sha256};
use serde_json::{json, Value};
use std::{
    path::{Path,PathBuf},
    sync::{mpsc::{sync_channel,SyncSender,TrySendError},OnceLock,
           atomic::{AtomicUsize,Ordering}},
    time::Duration,
};

const MAX_HISTORY: i64 = 1_000;
const PRUNE_BATCH: i64 = 1_000;
const HISTORY_BATCH_SIZE:usize = 64;
// Dedicated, bounded SQLite job spool (separate from the 1,000-row dashboard
// retention window). Producers never synchronously touch this DB from API.
static ML_QUEUE_EVICTED:AtomicUsize=AtomicUsize::new(0);
pub fn ml_queue_evicted()->usize{ML_QUEUE_EVICTED.load(Ordering::Relaxed)}
pub fn history_telemetry_dropped()->usize{HISTORY_DROPPED.load(Ordering::Relaxed)}
fn ml_queue_max_items()->i64{
    crate::ml_config::get().queue_max_items
}

/// O(number of evicted rows), not O(capacity) per write: a trigger-maintained
/// counter avoids a deep OFFSET scan on every history batch. A large cap
/// reduction is trimmed gradually to keep foreground logging responsive.
fn trim_ml_queue_with_budget(
    tx:&rusqlite::Transaction<'_>,capacity:i64,byte_budget:u64
)->rusqlite::Result<usize>{
    let (queued,payload_bytes):(i64,i64)=tx.query_row(
        "SELECT total,payload_bytes FROM ml_queue_metrics WHERE only_row=1",[],
        |row|Ok((row.get(0)?,row.get(1)?)))?;
    // One approximate average-size calculation replaces a deep OFFSET scan.
    // If the queue is oversize after changing budgets, prune up to 4096 old
    // jobs per transaction and resume on the next history batch.
    let excess_count=queued.saturating_sub(capacity);
    let excess_bytes=if byte_budget>0 && (payload_bytes as u64)>byte_budget{
        let average=(payload_bytes/queued.max(1)).max(1);
        (payload_bytes-(byte_budget.min(i64::MAX as u64) as i64))
            .saturating_add(average-1)/average
    }else{0};
    let excess=excess_count.max(excess_bytes).min(4_096);
    if excess==0{return Ok(0);}
    tx.execute("DELETE FROM ml_training_queue WHERE id IN (
        SELECT id FROM ml_training_queue ORDER BY id ASC LIMIT ?1
    )",[excess])
}
fn trim_ml_queue(tx:&rusqlite::Transaction<'_>,capacity:i64)->rusqlite::Result<usize>{
    trim_ml_queue_with_budget(tx,capacity,crate::ml_config::get().queue_storage_bytes)
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
        }
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}

/// One-time startup migration, before accepting HTTP requests.
///
/// Import the existing last-14-day history into minute counters BEFORE the
/// 1,000-row retention task can remove raw data. The migration marker, data
/// backfill and insert trigger are installed in one write transaction.
pub fn initialize(path: &Path) -> Result<(), String> {
    let db = connect(path).map_err(|e| e.to_string())?;
    db.execute_batch(r#"
        PRAGMA journal_mode=WAL;
        BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS requests(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
            title TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('matched','unmatched','error')),
            imdb_id TEXT,
            match_source TEXT,
            duration_ms INTEGER NOT NULL,
            details TEXT NOT NULL
        );
        -- INTEGER PRIMARY KEY is the rowid B-tree. Reverse rowid scans
        -- already serve ORDER BY id DESC; drop the legacy duplicate index.
        DROP INDEX IF EXISTS requests_recent;
        CREATE TABLE IF NOT EXISTS request_minute_stats(
            minute TEXT PRIMARY KEY NOT NULL,
            total INTEGER NOT NULL DEFAULT 0,
            matched INTEGER NOT NULL DEFAULT 0,
            unmatched INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0,
            duration_sum_ms INTEGER NOT NULL DEFAULT 0
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS history_migrations(
            name TEXT PRIMARY KEY NOT NULL
        ) WITHOUT ROWID;
        -- Durable ML observation queue: the learning cursor lives in its
        -- own SQLite DB and commits atomically with trained weights. Each
        -- request ID is an idempotency key for safe restart/replay.
        CREATE TABLE IF NOT EXISTS ml_training_queue(
            id INTEGER PRIMARY KEY NOT NULL,
            title_hash TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            details TEXT NOT NULL
        );
        -- One-time O(N) count during migration, then constant-time counter
        -- updates on inserts, deletes and duplicate-title coalescing.
        CREATE TABLE IF NOT EXISTS ml_queue_metrics(
            only_row INTEGER PRIMARY KEY CHECK(only_row=1),
            total INTEGER NOT NULL DEFAULT 0 CHECK(total>=0),
            payload_bytes INTEGER NOT NULL DEFAULT 0 CHECK(payload_bytes>=0)
        );
        INSERT OR IGNORE INTO ml_queue_metrics(only_row,total,payload_bytes)
        SELECT 1,COUNT(*),COALESCE(SUM(
            length(CAST(title AS BLOB))+length(CAST(details AS BLOB))+256
        ),0) FROM ml_training_queue;
        CREATE TRIGGER IF NOT EXISTS ml_queue_insert_count
        AFTER INSERT ON ml_training_queue BEGIN
            UPDATE ml_queue_metrics SET total=total+1,
                payload_bytes=payload_bytes+length(CAST(NEW.title AS BLOB))
                    +length(CAST(NEW.details AS BLOB))+256
            WHERE only_row=1;
        END;
        CREATE TRIGGER IF NOT EXISTS ml_queue_delete_count
        AFTER DELETE ON ml_training_queue BEGIN
            UPDATE ml_queue_metrics SET total=total-1,
                payload_bytes=payload_bytes-length(CAST(OLD.title AS BLOB))
                    -length(CAST(OLD.details AS BLOB))-256
            WHERE only_row=1;
        END;
        CREATE TRIGGER IF NOT EXISTS ml_queue_update_bytes
        AFTER UPDATE OF title,details ON ml_training_queue BEGIN
            UPDATE ml_queue_metrics SET
                payload_bytes=payload_bytes
                    +length(CAST(NEW.title AS BLOB))+length(CAST(NEW.details AS BLOB))
                    -length(CAST(OLD.title AS BLOB))-length(CAST(OLD.details AS BLOB))
            WHERE only_row=1;
        END;

        -- Backfill exactly once for existing installations. Other connections
        -- cannot write while this transaction is active.
        INSERT INTO request_minute_stats(
            minute,total,matched,unmatched,errors,duration_sum_ms
        )
        SELECT substr(created_at,1,16),COUNT(*),
               SUM(status='matched'),SUM(status='unmatched'),
               SUM(status='error'),COALESCE(SUM(duration_ms),0)
        FROM requests
        WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now','-14 days')
          AND NOT EXISTS (
              SELECT 1 FROM history_migrations WHERE name='minute_stats_v1'
          )
        GROUP BY substr(created_at,1,16);
        INSERT OR IGNORE INTO history_migrations(name) VALUES('minute_stats_v1');

        -- The old draft implementation pruned on writes; remove it if present.
        DROP TRIGGER IF EXISTS requests_prune_on_insert;
        CREATE TRIGGER IF NOT EXISTS request_stats_on_insert
        AFTER INSERT ON requests
        BEGIN
            INSERT INTO request_minute_stats(
                minute,total,matched,unmatched,errors,duration_sum_ms
            ) VALUES(
                substr(NEW.created_at,1,16),1,
                (NEW.status='matched'),(NEW.status='unmatched'),
                (NEW.status='error'),NEW.duration_ms
            )
            ON CONFLICT(minute) DO UPDATE SET
                total=total+1,
                matched=matched+(NEW.status='matched'),
                unmatched=unmatched+(NEW.status='unmatched'),
                errors=errors+(NEW.status='error'),
                duration_sum_ms=duration_sum_ms+NEW.duration_ms;
        END;

        -- Older versions executed a 1,000-row offset lookup and DELETE on
        -- EVERY insert. The single background writer now trims once per
        -- transaction of at most 64 requests, keeping writes bounded.
        DROP TRIGGER IF EXISTS request_history_cap;

        -- The history view uses recent IDs; status filtering only scans 1,000.
        DROP INDEX IF EXISTS requests_status;
        DROP INDEX IF EXISTS requests_created_at;
        COMMIT;
    "#).map_err(|e| e.to_string())
}

/// Persist one bounded history batch in a single SQLite transaction. The
/// minute-level statistics trigger runs for every row; raw-detail retention
/// runs just once per batch instead of scanning the last 1,000 IDs per row.
fn persist_batch(db:&mut Connection,items:&[HistoryEvent])->rusqlite::Result<()>{
    persist_batch_with_learning(db,items,crate::local_learning::active())
}
fn persist_batch_with_learning(db:&mut Connection,items:&[HistoryEvent],
    train:bool)->rusqlite::Result<()>
{
    if items.is_empty(){return Ok(());}
    let tx=db.transaction()?;
    {
        let mut insert=tx.prepare_cached(
            "INSERT INTO requests(title,status,imdb_id,match_source,duration_ms,details)
             VALUES(?1,?2,?3,?4,?5,?6) RETURNING id"
        )?;
        let mut enqueue=tx.prepare_cached(
            "INSERT INTO ml_training_queue(id,title_hash,title,details)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(title_hash) DO UPDATE SET
               id=excluded.id,title=excluded.title,details=excluded.details"
        )?;
        for item in items{
            let details=item.details.to_string();
            let id:i64=insert.query_row(params![
                item.title,item.status,
                item.selected.as_ref().and_then(|v|v["imdb_id"].as_str()),
                item.selected.as_ref().and_then(|v|v["match_source"].as_str()),
                i64::try_from(item.duration_ms).unwrap_or(i64::MAX),
                details
            ],|r|r.get(0))?;
            if train && item.status!="error"{
                // Key by the exact release title: identical pending requests
                // coalesce to the newest observation before training. The
                // request's increasing ID keeps the durable cursor correct.
                let identity=format!("{:x}",Sha256::digest(item.title.as_bytes()));
                // The learner reparses the source title with the ML-free
                // canonical parser. It only needs the selected identity and
                // original parsed diagnostics, not ranked candidates/traces.
                // Keep the durable queue small and independent of UI history.
                let training=json!({
                    "selected":item.details["selected"],
                    "parsed":item.details["parsed"],
                });
                enqueue.execute(params![id,identity,item.title,training.to_string()])?;
            }
        }
    }
    // Exact cap even when rows have gaps. The transaction commits both the
    // request aggregates and retention trim, so readers see consistent data.
    tx.execute("DELETE FROM requests WHERE id < (
        SELECT id FROM requests ORDER BY id DESC LIMIT 1 OFFSET ?1
    )",[MAX_HISTORY-1])?;
    // Counter tracks actual rows and estimated payload bytes across dedup.
    // Trim at most 4,096 oldest rows per batch; never scan a huge OFFSET.
    let evicted=if train{trim_ml_queue(&tx,ml_queue_max_items())?}else{0};
    tx.commit()?; 
    if evicted>0{
        let total=ML_QUEUE_EVICTED.fetch_add(evicted,Ordering::Relaxed)+evicted;
        if total==evicted||total%4096<evicted{
            tracing::debug!(evicted_total=total,
                "ML queue retention limit evicted older observations; matching unaffected");
        }
    }
    Ok(())
}

/// Synchronous single-event writer, also used in tests and fallback paths.
/// Production requests use the independently bounded asynchronous queue.
pub fn record(
    path:&Path,title:&str,status:&str,
    selected:Option<&Value>,details:&Value,duration_ms:u64,
){
    let item=HistoryEvent{
        path:path.to_path_buf(),title:title.to_owned(),status:status.to_owned(),
        selected:selected.cloned(),details:details.clone(),duration_ms
    };
    let result=connect(path).and_then(|mut db|persist_batch(&mut db,&[item]));
    if let Err(error)=result{
        tracing::debug!(%error,"could not persist matching history and statistics");
    }
}

// History is best-effort telemetry. Never block matching requests on the
// history SQLite writer's busy timeout: a busy metrics database used to
// consume most of StremThru's 10-second client deadline.
struct HistoryEvent {
    path:PathBuf,
    title:String,
    status:String,
    selected:Option<Value>,
    details:Value,
    duration_ms:u64,
}
// A burst buffer, not a substitute for durable metrics. The single writer
// drains up to 64 entries per SQLite transaction and never blocks matching.
const HISTORY_QUEUE_SIZE:usize=4096;
static HISTORY_QUEUE:OnceLock<Option<SyncSender<HistoryEvent>>>=OnceLock::new();
static HISTORY_DROPPED:AtomicUsize=AtomicUsize::new(0);

/// Hand the completed diagnostic to one bounded background writer. A full
/// queue drops only telemetry, never the verified SID/ASID response. The
/// writer serializes history transactions and cannot block Tokio or matching
/// worker threads.
pub fn record_async(
    path:&Path,title:&str,status:&str,
    selected:Option<&Value>,details:&Value,duration_ms:u64,
){
    let Some(tx)=HISTORY_QUEUE.get_or_init(||{
        let (tx,rx)=sync_channel::<HistoryEvent>(HISTORY_QUEUE_SIZE);
        let spawned=std::thread::Builder::new()
            .name("matchlab-history".into()).spawn(move||{
                // Keep SQLite connection and prepared statements hot across
                // batches. Reopening it for every request exhausted the
                // bounded queue and lost telemetry under sustained load.
                let mut connection:Option<(PathBuf,Connection)>=None;
                while let Ok(first)=rx.recv(){
                    let mut batch=vec![first];
                    while batch.len()<HISTORY_BATCH_SIZE {
                        match rx.try_recv() {
                            Ok(next) if next.path==batch[0].path =>batch.push(next),
                            Ok(next)=>{
                                // Tests and embedded callers may use a second
                                // DB path; do not mix paths in one transaction.
                                record(&next.path,&next.title,&next.status,
                                    next.selected.as_ref(),&next.details,next.duration_ms);
                            },
                            Err(_)=>break,
                        }
                    }
                    let path=batch[0].path.clone();
                    if connection.as_ref().is_none_or(|(p,_)|*p!=path){
                        connection=connect(&path).ok().map(|db|(path.clone(),db));
                    }
                    if let Some((_,db))=connection.as_mut(){
                        if let Err(error)=persist_batch(db,&batch){
                            tracing::debug!(%error,events=batch.len(),
                                "history writer batch failed; telemetry skipped");
                            connection=None; // discard failed connection/transaction
                        }
                    }else{
                        tracing::debug!(events=batch.len(),
                            "history database unavailable; telemetry skipped");
                    }
                }
            });
        if let Err(error)=spawned{
            tracing::debug!(%error,"history writer unavailable");
            None
        }else{Some(tx)}
    }).as_ref()else{return};
    let event=HistoryEvent{
        path:path.to_path_buf(),
        title:title.to_owned(),
        status:status.to_owned(),
        selected:selected.cloned(),
        details:details.clone(),
        duration_ms,
    };
    match tx.try_send(event){
        Ok(())=>{},
        Err(TrySendError::Full(_))=>{
            let n=HISTORY_DROPPED.fetch_add(1,Ordering::Relaxed)+1;
            if n==1 || n%128==0 {
                tracing::debug!(dropped=n,"history queue full; skipping telemetry, not match result");
            }
        },
        Err(TrySendError::Disconnected(_))=>{
            let n=HISTORY_DROPPED.fetch_add(1,Ordering::Relaxed)+1;
            if n==1 || n%128==0{
                tracing::debug!(dropped=n,"history writer unavailable; skipping telemetry");
            }
        },
    }
}

/// Trim raw details to 1,000 most recent requests and minute statistics to
/// 14 days. Small delete transactions avoid holding a long write lock while
/// removing a legacy history file with hundreds of thousands of rows.
pub fn prune(path: &Path) -> Result<usize, String> {
    let db = connect(path).map_err(|e| e.to_string())?;
    let mut removed = 0usize;
    loop {
        let deleted = db.execute(
            "DELETE FROM requests WHERE id IN (
                SELECT id FROM requests
                WHERE id < (
                    SELECT id FROM requests ORDER BY id DESC LIMIT 1 OFFSET 999
                )
                ORDER BY id ASC LIMIT ?1
             )",
            params![PRUNE_BATCH],
        ).map_err(|e| e.to_string())?;
        removed += deleted;
        if (deleted as i64) < PRUNE_BATCH {
            break;
        }
    }
    db.execute(
        "DELETE FROM request_minute_stats
         WHERE minute < strftime('%Y-%m-%dT%H:%M','now','-14 days')", [],
    ).map_err(|e| e.to_string())?;
    Ok(removed)
}

pub fn history(path: &Path, limit: usize, offset: usize, status: Option<&str>)
    -> Result<Value, String>
{
    if status.is_some_and(|s| !matches!(s, "matched" | "unmatched" | "error")) {
        return Err("invalid history status filter".into());
    }
    let db = connect(path).map_err(|e| e.to_string())?;
    let mut stmt = db.prepare(
        "SELECT id,created_at,title,status,imdb_id,match_source,duration_ms,details
         FROM requests
         WHERE id IN (SELECT id FROM requests ORDER BY id DESC LIMIT 1000)
           AND (?1 IS NULL OR status=?1)
         ORDER BY id DESC LIMIT ?2 OFFSET ?3"
    ).map_err(|e| e.to_string())?;
    let limit = limit.min(100);
    let offset = offset.min(MAX_HISTORY as usize);
    let rows = stmt.query_map(params![status,limit as i64,offset as i64], |r| {
        let details: String = r.get(7)?;
        Ok(json!({
            "id":r.get::<_,i64>(0)?,"created_at":r.get::<_,String>(1)?,
            "title":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,
            "imdb_id":r.get::<_,Option<String>>(4)?,
            "match_source":r.get::<_,Option<String>>(5)?,
            "duration_ms":r.get::<_,i64>(6)?,
            "details":serde_json::from_str::<Value>(&details).unwrap_or(Value::Null)
        }))
    }).map_err(|e| e.to_string())?;
    let items = rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    Ok(json!({"items":items,"limit":limit,"offset":offset,"max_records":MAX_HISTORY}))
}

pub fn stats(path: &Path) -> Result<Value, String> {
    let db = connect(path).map_err(|e| e.to_string())?;
    let (total,matched,unmatched,errors,avg_ms):
        (i64,i64,i64,i64,f64) = db.query_row(
        "SELECT COALESCE(SUM(total),0),COALESCE(SUM(matched),0),
                COALESCE(SUM(unmatched),0),COALESCE(SUM(errors),0),
                CASE WHEN SUM(total)>0
                     THEN 1.0*SUM(duration_sum_ms)/SUM(total) ELSE 0.0 END
         FROM request_minute_stats
         WHERE minute >= strftime('%Y-%m-%dT%H:%M','now','-14 days')",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))
    ).map_err(|e| e.to_string())?;
    let mut stmt = db.prepare(
        "SELECT substr(minute,1,10) AS day,
                SUM(total),SUM(matched),SUM(errors)
         FROM request_minute_stats
         WHERE minute >= strftime('%Y-%m-%dT%H:%M','now','-14 days')
         GROUP BY day ORDER BY day DESC LIMIT 15"
    ).map_err(|e| e.to_string())?;
    let daily = stmt.query_map([], |r| Ok(json!({
        "day":r.get::<_,String>(0)?,"total":r.get::<_,i64>(1)?,
        "matched":r.get::<_,i64>(2)?,"errors":r.get::<_,i64>(3)?
    }))).map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?;
    Ok(json!({
        "total":total,"matched":matched,"unmatched":unmatched,"errors":errors,
        "average_duration_ms":avg_ms,"daily":daily,
        "period_days":14,"period":"rolling 14 days (UTC; minute precision)"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_history_uses_rowid_without_duplicated_index(){
        let path=path();
        initialize(&path).unwrap();
        // Upgrade a database created by the previous build.
        {
            let db=connect(&path).unwrap();
            db.execute_batch("CREATE INDEX requests_recent ON requests(id DESC);").unwrap();
        }
        initialize(&path).unwrap();
        let db=connect(&path).unwrap();
        let mut index_stmt=db.prepare("PRAGMA index_list('requests')").unwrap();
        let indexes=index_stmt.query_map([],|r|r.get::<_,String>(1)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(!indexes.contains(&"requests_recent".to_owned()));
        drop(index_stmt);
        let plan:String=db.query_row(
            "EXPLAIN QUERY PLAN SELECT id FROM requests ORDER BY id DESC LIMIT 1000",
            [],|r|r.get(3)).unwrap();
        assert!(!plan.contains("INDEX"),"rowid scan should not require a secondary index: {plan}");
        drop(db);
        record(&path,"new","matched",None,&json!({}),1);
        assert_eq!(history(&path,10,0,None).unwrap()["items"][0]["title"],"new");
        cleanup(&path);
    }
    use std::{path::PathBuf, sync::atomic::{AtomicU64, Ordering}};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    fn path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "matchlab-stats-{}-{}.sqlite3",
            std::process::id(), NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn persistent_ml_queue_survives_history_retention(){
        let path=path();
        initialize(&path).unwrap();
        let mut db=connect(&path).unwrap();
        let events=(0..1200).map(|i|HistoryEvent{
            path:path.clone(),title:format!("Series S01E{i:04}"),
            status:"matched".into(),selected:None,
            details:json!({"selected":{"imdb_id":"tt1234567"}}),
            duration_ms:10
        }).collect::<Vec<_>>();
        for batch in events.chunks(HISTORY_BATCH_SIZE){
            persist_batch_with_learning(&mut db,batch,true).unwrap();
        }
        let raw:i64=db.query_row("SELECT COUNT(*) FROM requests",[],
            |r|r.get(0)).unwrap();
        let jobs:i64=db.query_row("SELECT COUNT(*) FROM ml_training_queue",[],
            |r|r.get(0)).unwrap();
        assert_eq!(raw,1000,"dashboard must retain only most recent requests");
        assert_eq!(jobs,1200,"training backlog must survive dashboard cap");
        drop(db);
        initialize(&path).unwrap(); // restart/migration is idempotent
        let db=connect(&path).unwrap();
        let jobs:i64=db.query_row("SELECT COUNT(*) FROM ml_training_queue",[],
            |r|r.get(0)).unwrap();
        assert_eq!(jobs,1200);
        drop(db);
        cleanup(&path);
    }
    #[test]
    fn repeated_title_coalesces_to_newest_pending_training_job(){
        let path=path();
        initialize(&path).unwrap();
        let mut db=connect(&path).unwrap();
        let events=[HistoryEvent{
            path:path.clone(),title:"Bleach.EPIS326".into(),
            status:"matched".into(),selected:None,
            details:json!({"selected":{"imdb_id":"tt0000001"}}),
            duration_ms:1
        },HistoryEvent{
            path:path.clone(),title:"Bleach.EPIS326".into(),
            status:"matched".into(),selected:None,
            details:json!({"selected":{"imdb_id":"tt0000002"}}),
            duration_ms:1
        }];
        persist_batch_with_learning(&mut db,&events,true).unwrap();
        let (count,id,details):(i64,i64,String)=db.query_row(
            "SELECT COUNT(*),MAX(id),MAX(details) FROM ml_training_queue",
            [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(count,1);
        let queued:i64=db.query_row("SELECT total FROM ml_queue_metrics WHERE only_row=1",
            [],|row|row.get(0)).unwrap();
        assert_eq!(queued,1);
        assert_eq!(id,2);
        assert_eq!(serde_json::from_str::<Value>(&details).unwrap()
            ["selected"]["imdb_id"],"tt0000002");
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn ml_queue_retention_counts_rows_not_gapped_request_ids(){
        // Coalescing produces large ID gaps. The count is actual queued rows.
        let path=path();
        initialize(&path).unwrap();
        let mut db=connect(&path).unwrap();
        db.execute_batch("INSERT INTO ml_training_queue(id,title_hash,title,details)
            VALUES(1,'a','old','{}'),(10001,'b','old2','{}'),(40000,'c','new','{}');").unwrap();
        let retention=|db:&mut Connection,cap:i64|{
            let tx=db.transaction().unwrap();
            let removed=trim_ml_queue(&tx,cap).unwrap();
            tx.commit().unwrap();
            removed
        };
        assert_eq!(retention(&mut db,3),0);
        let ids=db.prepare("SELECT id FROM ml_training_queue ORDER BY id").unwrap()
            .query_map([],|row|row.get::<_,i64>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(ids,vec![1,10001,40000]);
        assert_eq!(retention(&mut db,2),1);
        let ids=db.prepare("SELECT id FROM ml_training_queue ORDER BY id").unwrap()
            .query_map([],|row|row.get::<_,i64>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(ids,vec![10001,40000]);
        assert_eq!(retention(&mut db,5),0);
        let queued:i64=db.query_row("SELECT total FROM ml_queue_metrics WHERE only_row=1",
            [],|row|row.get(0)).unwrap();
        assert_eq!(queued,2);
        // A duplicate title updates its ID without increasing the count.
        db.execute("INSERT INTO ml_training_queue(id,title_hash,title,details)
            VALUES(50000,'c','new','{}')
            ON CONFLICT(title_hash) DO UPDATE SET id=excluded.id",[]).unwrap();
        let queued:i64=db.query_row("SELECT total FROM ml_queue_metrics WHERE only_row=1",
            [],|row|row.get(0)).unwrap();
        assert_eq!(queued,2);
        drop(db);
        initialize(&path).unwrap(); // repeated migration must not re-count
        let db=connect(&path).unwrap();
        let queued:i64=db.query_row("SELECT total FROM ml_queue_metrics WHERE only_row=1",
            [],|row|row.get(0)).unwrap();
        assert_eq!(queued,2);
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn queue_byte_budget_reclaims_oldest_jobs(){
        let path=path();
        initialize(&path).unwrap();
        let mut db=connect(&path).unwrap();
        db.execute_batch("INSERT INTO ml_training_queue(id,title_hash,title,details)
            VALUES (1,'one','long episode one','{}'),
                   (2,'two','long episode two','{}'),
                   (3,'three','long episode three','{}');").unwrap();
        let payload:i64=db.query_row(
            "SELECT payload_bytes FROM ml_queue_metrics WHERE only_row=1",[],
            |row|row.get(0)).unwrap();
        let tx=db.transaction().unwrap();
        let removed=trim_ml_queue_with_budget(&tx,100,(payload/2) as u64)
            .unwrap();
        assert!(removed>0);
        tx.commit().unwrap();
        let first:i64=db.query_row(
            "SELECT MIN(id) FROM ml_training_queue",[],|row|row.get(0))
            .unwrap();
        assert!(first>1,"oldest job must be evicted under byte pressure");
        let count:i64=db.query_row(
            "SELECT total FROM ml_queue_metrics WHERE only_row=1",[],
            |row|row.get(0)).unwrap();
        let actual:i64=db.query_row(
            "SELECT count(*) FROM ml_training_queue",[],|row|row.get(0))
            .unwrap();
        assert_eq!(count,actual);
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn bulk_writer_preserves_all_minute_counts_and_caps_details(){
        let path=path();
        initialize(&path).unwrap();
        let mut db=connect(&path).unwrap();
        let events=(0..1200).map(|i|HistoryEvent{
            path:path.clone(),title:format!("Episode {i}"),
            status:"matched".into(),selected:None,details:json!({}),
            duration_ms:10
        }).collect::<Vec<_>>();
        for chunk in events.chunks(HISTORY_BATCH_SIZE){
            persist_batch(&mut db,chunk).unwrap();
        }
        let raw:i64=db.query_row("SELECT COUNT(*) FROM requests",[],
            |r|r.get(0)).unwrap();
        assert_eq!(raw,MAX_HISTORY);
        let total=stats(&path).unwrap();
        assert_eq!(total["total"],1200);
        assert_eq!(total["matched"],1200);
        assert_eq!(total["average_duration_ms"],10.0);
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn records_and_filters_counts_independently() {
        let path = path();
        initialize(&path).unwrap();
        record(&path,"A S01E02","matched",
            Some(&json!({"imdb_id":"tt123","match_source":"imdb"})),
            &json!({"selected":{"imdb_id":"tt123"}}),10);
        record(&path,"broken","error",None,&json!({"error":"invalid"}),5);
        assert_eq!(stats(&path).unwrap()["total"],2);
        assert_eq!(stats(&path).unwrap()["errors"],1);
        assert_eq!(stats(&path).unwrap()["average_duration_ms"],7.5);
        assert_eq!(history(&path,10,0,None).unwrap()["items"][0]["title"],"broken");
        assert_eq!(history(&path,10,0,Some("matched")).unwrap()["items"][0]["title"],"A S01E02");
        assert_eq!(history(&path,10,0,Some("unmatched")).unwrap()["items"].as_array().unwrap().len(),0);
        assert!(history(&path,10,0,Some("unknown")).is_err());
        cleanup(&path);
    }

    #[test]
    fn retains_14_day_aggregates_after_trimming_raw_history() {
        let path = path();
        initialize(&path).unwrap();
        let db = connect(&path).unwrap();
        let tx_count = 1015;
        for _ in 0..tx_count {
            db.execute(
                "INSERT INTO requests(title,status,duration_ms,details)
                 VALUES('repeat','matched',20,'{}')", []
            ).unwrap();
        }
        drop(db);
        // Bulk out-of-band SQL inserts bypass the bounded history writer;
        // scheduled retention trims them. Normal record_async batches
        // enforce the exact same cap inside their write transaction.
        let db = connect(&path).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM requests",[],|r|r.get::<_,i64>(0)).unwrap(),1015);
        drop(db);
        assert_eq!(prune(&path).unwrap(),15);
        assert_eq!(history(&path,100,999,None).unwrap()["items"].as_array().unwrap().len(),1);
        assert_eq!(history(&path,100,1000,None).unwrap()["items"].as_array().unwrap().len(),0);
        let db = connect(&path).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM requests",[],|r|r.get::<_,i64>(0)).unwrap(),1000);
        drop(db);
        let s = stats(&path).unwrap();
        assert_eq!(s["total"],1015);
        assert_eq!(s["matched"],1015);
        assert_eq!(s["average_duration_ms"],20.0);
        cleanup(&path);
    }

    #[test]
    fn backfills_legacy_history_once_before_pruning() {
        let path = path();
        {
            let db = connect(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE requests(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    title TEXT NOT NULL,
                    status TEXT NOT NULL,
                    imdb_id TEXT,match_source TEXT,
                    duration_ms INTEGER NOT NULL,details TEXT NOT NULL
                );
                INSERT INTO requests(title,status,duration_ms,details)
                VALUES('legacy','matched',100,'{}');
                INSERT INTO requests(created_at,title,status,duration_ms,details)
                VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now','-15 days'),
                       'old','unmatched',100,'{}');"
            ).unwrap();
        }
        initialize(&path).unwrap();
        initialize(&path).unwrap(); // migration must be idempotent
        let s=stats(&path).unwrap();
        assert_eq!(s["total"],1);
        assert_eq!(s["matched"],1);
        assert_eq!(s["unmatched"],0);
        cleanup(&path);
    }

    #[test]
    fn old_statistics_expire_while_recent_history_is_visible() {
        let path = path();
        initialize(&path).unwrap();
        {
            let db=connect(&path).unwrap();
            db.execute_batch(
                "INSERT INTO requests(created_at,title,status,duration_ms,details)
                 VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now','-15 days'),'old','error',50,'{}');"
            ).unwrap();
        }
        assert_eq!(stats(&path).unwrap()["total"],0);
        assert_eq!(history(&path,10,0,None).unwrap()["items"][0]["title"],"old");
        assert_eq!(prune(&path).unwrap(),0);
        let db=connect(&path).unwrap();
        let count:i64=db.query_row("SELECT count(*) FROM request_minute_stats",[],|r|r.get(0)).unwrap();
        assert_eq!(count,0);
        cleanup(&path);
    }
}
