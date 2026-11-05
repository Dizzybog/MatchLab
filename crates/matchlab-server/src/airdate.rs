//! Keyless Cinemeta airdate lookup, adapted from Comet's episode index approach.
//! The cache is separate from the atomically replaced IMDb metadata database.
use rusqlite::{params, Connection};
use serde_json::Value;
use std::{path::{Path,PathBuf},time::{SystemTime,UNIX_EPOCH,Duration}};

const TTL:i64=7*24*3600;
const MAX_BYTES:usize=16*1024*1024;

pub fn extract(input:&str)->Option<(String,String)> {
    // Recognize YYYY.MM.DD, YYYY-MM-DD, YYYY_MM_DD without confusing SxxExx.
    let leaf=input.rsplit(['/', '\\']).next()?;
    let parts:Vec<(usize,&str)>=leaf.match_indices(|c:char|!c.is_ascii_digit())
        .map(|(i,_)|i).chain(std::iter::once(leaf.len()))
        .scan(0,|start,end|{let p=(*start,end);*start=end+1;Some(p)})
        .filter_map(|(a,b)|leaf.get(a..b).filter(|s|!s.is_empty()).map(|s|(a,s)))
        .collect();
    for w in parts.windows(3){
        let (offset,y)=w[0];let (_,m)=w[1];let (_,d)=w[2];
        if y.len()!=4||m.len()!=2||d.len()!=2{continue;}
        let (Ok(y),Ok(m),Ok(d))=(y.parse::<i32>(),m.parse::<u32>(),d.parse::<u32>())else{continue;};
        if !(1900..=2100).contains(&y)||!(1..=12).contains(&m){continue;}
        let leap=y%4==0&&(y%100!=0||y%400==0);
        let max=[31,if leap{29}else{28},31,30,31,30,31,31,30,31,30,31][m as usize-1];
        if d==0||d>max{continue;}
        let title=leaf[..offset].trim_end_matches(|c:char|!c.is_ascii_alphanumeric())
            .replace(['.','_','-']," ");
        if title.trim().is_empty(){continue;}
        return Some((title,format!("{y:04}-{m:02}-{d:02}")));
    }
    None
}

fn cache_path(metadata:&Path)->PathBuf{
    metadata.with_file_name("matchlab_airdates.sqlite3")
}
fn now()->i64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64}
fn open_cache(path:&Path)->Result<Connection,String>{
    let conn=Connection::open(cache_path(path)).map_err(|e|e.to_string())?;
    conn.busy_timeout(Duration::from_secs(3)).map_err(|e|e.to_string())?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS episode_airdates(
        imdb_id TEXT NOT NULL,air_date TEXT NOT NULL,season INTEGER NOT NULL,episode INTEGER NOT NULL,
        PRIMARY KEY(imdb_id,air_date,season,episode));
        CREATE TABLE IF NOT EXISTS episode_airdate_refresh(imdb_id TEXT PRIMARY KEY,refreshed_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS episode_airdates_lookup ON episode_airdates(imdb_id,air_date);")
        .map_err(|e|e.to_string())?;
    Ok(conn)
}
fn lookup(conn:&Connection,id:&str,date:&str)->Result<Option<(i64,i64)>,String>{
    let mut stmt=conn.prepare("SELECT DISTINCT season,episode FROM episode_airdates
        WHERE imdb_id=?1 AND air_date=?2 LIMIT 2").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![id,date],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))
        .map_err(|e|e.to_string())?;
    let values=rows.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    Ok(if values.len()==1{Some(values[0])}else{None})
}
fn normalize_date(s:&str)->Option<&str>{
    let date=s.split('T').next()?;
    let parts:Vec<_>=date.split('-').collect();
    if parts.len()==3&&parts[0].len()==4&&parts[1].len()==2&&parts[2].len()==2
        &&parts.iter().all(|p|p.bytes().all(|c|c.is_ascii_digit())){Some(date)}else{None}
}
fn fetch(id:&str)->Result<Vec<(String,i64,i64)>,String>{
    if !id.starts_with("tt")||!id[2..].bytes().all(|c|c.is_ascii_digit()){return Err("invalid IMDb ID".into());}
    let url=format!("https://v3-cinemeta.strem.io/meta/series/{id}.json");
    let client=reqwest::blocking::Client::builder().timeout(Duration::from_secs(8))
        .build().map_err(|e|e.to_string())?;
    let response=client.get(url).send().map_err(|e|e.to_string())?
        .error_for_status().map_err(|e|e.to_string())?;
    if response.content_length().is_some_and(|n|n>MAX_BYTES as u64){return Err("Cinemeta response too large".into());}
    let bytes=response.bytes().map_err(|e|e.to_string())?;
    if bytes.len()>MAX_BYTES{return Err("Cinemeta response too large".into());}
    let json:Value=serde_json::from_slice(&bytes).map_err(|e|e.to_string())?;
    let videos=json.pointer("/meta/videos").and_then(Value::as_array)
        .ok_or("Cinemeta response missing meta.videos")?;
    let mut out=Vec::new();
    for video in videos{
        let Some(season)=video["season"].as_i64() else{continue;};
        let Some(episode)=video.get("episode").or_else(||video.get("number")).and_then(Value::as_i64)else{continue;};
        let date=["released","firstAired","air_date","first_aired"].iter()
            .find_map(|key|video.get(*key).and_then(Value::as_str).and_then(normalize_date));
        if let Some(date)=date{
            if season>=0&&episode>0{out.push((date.to_string(),season,episode));}
        }
    }
    if out.is_empty(){return Err("Cinemeta returned no dated episodes".into());}
    Ok(out)
}
pub fn resolve(metadata:&Path,id:&str,date:&str)->Result<Option<(i64,i64)>,String>{
    let mut db=open_cache(metadata)?;
    let refreshed:Option<i64>=match db.query_row("SELECT refreshed_at FROM episode_airdate_refresh WHERE imdb_id=?1",
        [id],|r|r.get(0)){
        Ok(v)=>Some(v),Err(rusqlite::Error::QueryReturnedNoRows)=>None,Err(e)=>return Err(e.to_string())
    };
    if refreshed.is_some_and(|at|now()-at<TTL){return lookup(&db,id,date);}
    match fetch(id){
        Ok(rows)=>{
            let tx=db.transaction().map_err(|e|e.to_string())?;
            tx.execute("DELETE FROM episode_airdates WHERE imdb_id=?1",[id]).map_err(|e|e.to_string())?;
            {
                let mut stmt=tx.prepare("INSERT OR IGNORE INTO episode_airdates VALUES(?1,?2,?3,?4)")
                    .map_err(|e|e.to_string())?;
                for (air_date,season,episode) in rows{
                    stmt.execute(params![id,air_date,season,episode]).map_err(|e|e.to_string())?;
                }
            }
            tx.execute("INSERT INTO episode_airdate_refresh VALUES(?1,?2)
                ON CONFLICT(imdb_id) DO UPDATE SET refreshed_at=excluded.refreshed_at",
                params![id,now()]).map_err(|e|e.to_string())?;
            tx.commit().map_err(|e|e.to_string())?;
        }
        Err(e)=>{
            tracing::warn!(imdb_id=%id,error=%e,"Cinemeta airdate refresh failed");
            // Stale cache is preferable to failing the ordinary matcher.
        }
    }
    lookup(&db,id,date)
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn date_and_title(){
        assert_eq!(extract("The.Daily.Show.2022.01.20.Francis.Ngannou.1080p"),
            Some(("The Daily Show".into(),"2022-01-20".into())));
        assert_eq!(extract("The_Daily_Show_2022-01-20.mkv"),
            Some(("The Daily Show".into(),"2022-01-20".into())));
        assert!(extract("The.Daily.Show.2022.02.30").is_none());
        assert!(extract("Supernatural.S02E03.1080p").is_none());
    }
    #[test]fn ambiguous_dates_do_not_choose_first(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE episode_airdates(imdb_id TEXT,air_date TEXT,season INTEGER,episode INTEGER);
            INSERT INTO episode_airdates VALUES('tt1','2022-01-20',27,45);
            INSERT INTO episode_airdates VALUES('tt1','2022-01-20',27,46);").unwrap();
        assert_eq!(lookup(&db,"tt1","2022-01-20").unwrap(),None);
    }
}
