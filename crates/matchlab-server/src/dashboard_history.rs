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
        CREATE INDEX IF NOT EXISTS requests_status ON requests(status);")?;
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

pub fn history(path:&Path,limit:usize,offset:usize)->Result<Value,String>{
    let db=open(path).map_err(|e|e.to_string())?;
    let mut stmt=db.prepare("SELECT id,created_at,title,status,imdb_id,match_source,duration_ms,details
        FROM requests ORDER BY id DESC LIMIT ?1 OFFSET ?2").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![limit.min(100) as i64,offset.min(100_000) as i64],|r|{
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
         coalesce(avg(duration_ms),0) FROM requests",[],|r|
        Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))
    ).map_err(|e|e.to_string())?;
    let mut stmt=db.prepare("SELECT substr(created_at,1,10) day,count(*) total,
        sum(status='matched') matched,sum(status='error') errors
        FROM requests GROUP BY day ORDER BY day DESC LIMIT 14").map_err(|e|e.to_string())?;
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
        assert_eq!(history(&path,10,0).unwrap()["items"][0]["title"],"broken");
        let _=std::fs::remove_file(&path);
        let _=std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _=std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
