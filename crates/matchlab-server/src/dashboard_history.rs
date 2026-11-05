//! Persistent request history and dashboard statistics, separate from atomically refreshed metadata.
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use std::{path::Path, time::Duration};

fn open(path:&Path)->rusqlite::Result<Connection>{
    if let Some(parent)=path.parent(){std::fs::create_dir_all(parent).map_err(|_|rusqlite::Error::InvalidPath(parent.to_path_buf()))?;}
    let db=Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL;
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
        CREATE INDEX IF NOT EXISTS requests_created_at ON requests(created_at);
        CREATE INDEX IF NOT EXISTS requests_status ON requests(status);
        CREATE TABLE IF NOT EXISTS history_maintenance(
            key TEXT PRIMARY KEY, last_run INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO history_maintenance(key,last_run)
            VALUES('history_prune',0);
        CREATE TRIGGER IF NOT EXISTS requests_prune_on_insert
        AFTER INSERT ON requests
        WHEN (SELECT last_run FROM history_maintenance WHERE key='history_prune')
             <= CAST(strftime('%s','now') AS INTEGER)-300
        BEGIN
            DELETE FROM requests
             WHERE created_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-24 hours');
            UPDATE history_maintenance
                SET last_run=CAST(strftime('%s','now') AS INTEGER)
                WHERE key='history_prune';
        END;")?;
    Ok(db)
}

pub fn record(path:&Path,title:&str,status:&str,selected:Option<&Value>,details:&Value,duration_ms:u64){
    let result=(||->rusqlite::Result<()>{
        let db=open(path)?;
        db.execute("INSERT INTO requests(title,status,imdb_id,match_source,duration_ms,details)
            VALUES(?1,?2,?3,?4,?5,?6)",
            params![title,status,selected.and_then(|v|v["imdb_id"].as_str()),
                selected.and_then(|v|v["match_source"].as_str()),duration_ms as i64,details.to_string()])?;
        Ok(())
    })();
    if let Err(error)=result {tracing::warn!(%error,"could not persist matching history");}
}

/// Physically delete rows outside the rolling 24-hour window. Called by the
/// background maintenance worker (including once immediately at startup).
/// SQLite reuses freed pages; a manual VACUUM can reclaim old file size.
#[allow(dead_code)] // Manual maintenance helper; automatic expiry runs in the SQLite insert trigger.
pub fn prune(path:&Path)->Result<usize,String>{
    let db=open(path).map_err(|e|e.to_string())?;
    db.execute("DELETE FROM requests
        WHERE created_at < strftime('%Y-%m-%dT%H:%M:%fZ','now','-24 hours')",[])
        .map_err(|e|e.to_string())
}

pub fn history(path:&Path,limit:usize,offset:usize,status:Option<&str>)->Result<Value,String>{
    if status.is_some_and(|s|!matches!(s,"matched"|"unmatched"|"error")){
        return Err("invalid history status filter".into());
    }
    let db=open(path).map_err(|e|e.to_string())?;
    let mut stmt=db.prepare("SELECT id,created_at,title,status,imdb_id,match_source,duration_ms,details
        FROM requests
        WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now','-24 hours')
          AND (?1 IS NULL OR status=?1)
        ORDER BY id DESC LIMIT ?2 OFFSET ?3").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![status,limit.min(100) as i64,offset.min(100_000) as i64],|r|{
        let details:String=r.get(7)?;
        Ok(json!({"id":r.get::<_,i64>(0)?,"created_at":r.get::<_,String>(1)?,
            "title":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,
            "imdb_id":r.get::<_,Option<String>>(4)?,"match_source":r.get::<_,Option<String>>(5)?,
            "duration_ms":r.get::<_,i64>(6)?,
            "details":serde_json::from_str::<Value>(&details).unwrap_or(Value::Null)}))
    }).map_err(|e|e.to_string())?;
    let items=rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    Ok(json!({"items":items,"limit":limit.min(100),"offset":offset.min(100_000)}))
}

pub fn stats(path:&Path)->Result<Value,String>{
    let db=open(path).map_err(|e|e.to_string())?;
    let (total,matched,unmatched,errors,avg_ms):(i64,i64,i64,i64,f64)=db.query_row(
        "SELECT count(*),coalesce(sum(status='matched'),0),
         coalesce(sum(status='unmatched'),0),coalesce(sum(status='error'),0),
         coalesce(avg(duration_ms),0) FROM requests
         WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now','-24 hours')",[],|r|
        Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))
    ).map_err(|e|e.to_string())?;
    let mut stmt=db.prepare("SELECT substr(created_at,1,10) day,count(*) total,
        sum(status='matched') matched,sum(status='error') errors
        FROM requests
        WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now','-24 hours')
        GROUP BY day ORDER BY day DESC LIMIT 2").map_err(|e|e.to_string())?;
    let days=stmt.query_map([],|r|Ok(json!({"day":r.get::<_,String>(0)?,
        "total":r.get::<_,i64>(1)?,"matched":r.get::<_,i64>(2)?,
        "errors":r.get::<_,i64>(3)?}))).map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    Ok(json!({"total":total,"matched":matched,"unmatched":unmatched,"errors":errors,
        "average_duration_ms":avg_ms,"daily":days}))
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn stores_success_and_error(){
        let path=std::env::temp_dir().join(format!("matchlab-history-{}.sqlite3",std::process::id()));
        let _=std::fs::remove_file(&path);
        record(&path,"A S01E02","matched",Some(&json!({"imdb_id":"tt123","match_source":"imdb"})),&json!({"selected":{"imdb_id":"tt123"}}),10);
        record(&path,"broken","error",None,&json!({"error":"invalid"}),5);
        assert_eq!(stats(&path).unwrap()["total"],2);
        assert_eq!(stats(&path).unwrap()["errors"],1);
        assert_eq!(history(&path,10,0,None).unwrap()["items"][0]["title"],"broken");
        assert_eq!(history(&path,10,0,Some("matched")).unwrap()["items"].as_array().unwrap().len(),1);
        assert_eq!(history(&path,10,0,Some("matched")).unwrap()["items"][0]["title"],"A S01E02");
        assert_eq!(history(&path,10,0,Some("unmatched")).unwrap()["items"].as_array().unwrap().len(),0);
        assert_eq!(history(&path,10,0,Some("error")).unwrap()["items"][0]["title"],"broken");
        assert!(history(&path,10,0,Some("unknown")).is_err());
        let _=std::fs::remove_file(&path);
        let _=std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _=std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn retention_limits_visibility_and_reclaims_old_rows(){
        let path=std::env::temp_dir().join(format!(
            "matchlab-history-retention-{}.sqlite3",std::process::id()));
        let _=std::fs::remove_file(&path);
        {
            let db=open(&path).unwrap();
            db.execute_batch("
                INSERT INTO requests(created_at,title,status,duration_ms,details)
                VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now','-23 hours'),'recent','unmatched',30,'{}');
                INSERT INTO requests(created_at,title,status,duration_ms,details)
                VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now','-25 hours'),'expired','matched',10,'{}');
            ").unwrap();
        }
        // The dashboard must never display an expired request even before
        // the periodic physical cleanup has run.
        assert_eq!(history(&path,10,0,None).unwrap()["items"].as_array().unwrap().len(),1);
        assert_eq!(history(&path,10,0,None).unwrap()["items"][0]["title"],"recent");
        let before=stats(&path).unwrap();
        assert_eq!(before["total"],1);
        assert_eq!(before["unmatched"],1);
        assert_eq!(prune(&path).unwrap(),1);
        assert_eq!(prune(&path).unwrap(),0);
        let db=open(&path).unwrap();
        assert_eq!(db.query_row("SELECT COUNT(*) FROM requests",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        drop(db);
        let _=std::fs::remove_file(&path);
        let _=std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _=std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn write_trigger_prunes_expired_history_when_due(){
        let path=std::env::temp_dir().join(format!(
            "matchlab-history-trigger-{}.sqlite3",std::process::id()));
        let _=std::fs::remove_file(&path);
        {
            let db=open(&path).unwrap();
            db.execute_batch("
                INSERT INTO requests(created_at,title,status,duration_ms,details)
                VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now','-23 hours'),'recent','matched',10,'{}');
                INSERT INTO requests(created_at,title,status,duration_ms,details)
                VALUES(strftime('%Y-%m-%dT%H:%M:%fZ','now','-25 hours'),'expired','unmatched',20,'{}');
                UPDATE history_maintenance SET last_run=0 WHERE key='history_prune';
                INSERT INTO requests(title,status,duration_ms,details)
                VALUES('new','matched',5,'{}');
            ").unwrap();
            let expired:i64=db.query_row(
                "SELECT count(*) FROM requests WHERE title='expired'",[],|r|r.get(0)).unwrap();
            assert_eq!(expired,0);
            let live:i64=db.query_row("SELECT count(*) FROM requests",[],|r|r.get(0)).unwrap();
            assert_eq!(live,2);
        }
        let _=std::fs::remove_file(&path);
        let _=std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _=std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
