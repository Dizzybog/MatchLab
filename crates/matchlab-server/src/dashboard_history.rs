//! Persistent, bounded request history and independent 14-day statistics.
//! The metadata database is separate and is never modified here.
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

const MAX_HISTORY: i64 = 1_000;
const PRUNE_BATCH: i64 = 1_000;

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
        CREATE INDEX IF NOT EXISTS requests_recent ON requests(id DESC);
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

        -- Keep the physical table bounded as new requests arrive. The
        -- subquery uses the descending primary-key order and evicts only
        -- one row; legacy bulk cleanup is handled by the background worker.
        CREATE TRIGGER IF NOT EXISTS request_history_cap
        AFTER INSERT ON requests
        BEGIN
            DELETE FROM requests WHERE id = (
                SELECT id FROM requests ORDER BY id DESC LIMIT 1 OFFSET 1000
            );
        END;

        -- The history view uses recent IDs; status filtering only scans 1,000.
        DROP INDEX IF EXISTS requests_status;
        DROP INDEX IF EXISTS requests_created_at;
        COMMIT;
    "#).map_err(|e| e.to_string())
}

/// Record details and increment minute aggregates atomically through triggers.
/// A single oldest detail row is evicted once the 1,000-row cap is exceeded;
/// large legacy purges and aggregate expiry remain background-only.
pub fn record(
    path: &Path, title: &str, status: &str,
    selected: Option<&Value>, details: &Value, duration_ms: u64,
) {
    let result = (|| -> rusqlite::Result<()> {
        let db = connect(path)?;
        db.execute(
            "INSERT INTO requests(title,status,imdb_id,match_source,duration_ms,details)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                title, status,
                selected.and_then(|v| v["imdb_id"].as_str()),
                selected.and_then(|v| v["match_source"].as_str()),
                i64::try_from(duration_ms).unwrap_or(i64::MAX),
                details.to_string()
            ],
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "could not persist matching history and statistics");
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
        // Even before maintenance the physical table holds only 1,000.
        let db = connect(&path).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM requests",[],|r|r.get::<_,i64>(0)).unwrap(),1000);
        drop(db);
        assert_eq!(history(&path,100,999,None).unwrap()["items"].as_array().unwrap().len(),1);
        assert_eq!(history(&path,100,1000,None).unwrap()["items"].as_array().unwrap().len(),0);
        assert_eq!(prune(&path).unwrap(),0);
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
