//! Native SQLite metadata schema and streaming IMDb import.
//! This module is intentionally separate from the HTTP request path.
use std::{error::Error,fs::File,path::Path,io::BufReader};
use rusqlite::{params,Connection};
use flate2::read::GzDecoder;
type Result<T> = std::result::Result<T,Box<dyn Error+Send+Sync>>;

pub fn initialize(db:&Connection)->rusqlite::Result<()> {
    db.create_scalar_function("reverse",1,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC |
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |ctx| { let s:String=ctx.get(0)?; Ok(s.chars().rev().collect::<String>()) })?;
    db.execute_batch("
        CREATE TABLE IF NOT EXISTS titles (
          imdb_id TEXT NOT NULL,title TEXT NOT NULL,clean_title TEXT NOT NULL,
          year INTEGER NOT NULL,category TEXT NOT NULL,region TEXT,
          original INTEGER NOT NULL DEFAULT 0,ordering INTEGER NOT NULL DEFAULT 0,
          type TEXT NOT NULL DEFAULT '',
          PRIMARY KEY(imdb_id,clean_title,year,region,ordering));
        CREATE INDEX IF NOT EXISTS title_lookup ON titles(clean_title,year);
        CREATE INDEX IF NOT EXISTS title_suffix ON titles(clean_title);
        CREATE INDEX IF NOT EXISTS title_reverse_suffix ON titles(reverse(clean_title),type);
        CREATE TABLE IF NOT EXISTS seasons(imdb_id TEXT PRIMARY KEY,max_season INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS anime_ids(group_id TEXT NOT NULL,type TEXT NOT NULL,value TEXT NOT NULL,
          PRIMARY KEY(group_id,type,value));
        CREATE TABLE IF NOT EXISTS episode_map(anidb_id TEXT NOT NULL,tvdb_id TEXT NOT NULL,
          anidb_season INTEGER NOT NULL,tvdb_season INTEGER NOT NULL,start INTEGER NOT NULL,
          end INTEGER NOT NULL,offset INTEGER NOT NULL DEFAULT 0,map TEXT NOT NULL DEFAULT '');
        CREATE TABLE IF NOT EXISTS anidb_titles(anidb_id TEXT NOT NULL,title TEXT NOT NULL,
          clean_title TEXT NOT NULL,title_type TEXT NOT NULL,language TEXT NOT NULL,
          PRIMARY KEY(anidb_id,clean_title,title_type,language));
        CREATE INDEX IF NOT EXISTS anidb_title_lookup ON anidb_titles(clean_title);
        CREATE TABLE IF NOT EXISTS anime_series_map(imdb_id TEXT NOT NULL,anidb_id TEXT NOT NULL,
          tvdb_id TEXT NOT NULL DEFAULT '',trakt_season INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
        CREATE INDEX IF NOT EXISTS anime_series_by_anidb ON anime_series_map(anidb_id);
        CREATE INDEX IF NOT EXISTS anime_series_by_imdb ON anime_series_map(imdb_id);
        CREATE INDEX IF NOT EXISTS anime_lookup_idx ON anime_ids(type,value);
        CREATE INDEX IF NOT EXISTS episode_lookup_idx ON episode_map(anidb_id,anidb_season,start,end);
        CREATE TABLE IF NOT EXISTS metadata_state(source TEXT PRIMARY KEY,
          updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);")
}

fn reader(path:&Path)->Result<csv::Reader<Box<dyn std::io::Read>>> {
    let file=File::open(path)?;
    let input:Box<dyn std::io::Read>=if path.extension().is_some_and(|e|e=="gz"){
        Box::new(GzDecoder::new(BufReader::new(file)))
    }else{Box::new(BufReader::new(file))};
    Ok(csv::ReaderBuilder::new().delimiter(b'\t').flexible(true).from_reader(input))
}
fn field<'a>(row:&'a csv::StringRecord,headers:&csv::StringRecord,name:&str)->&'a str {
    headers.iter().position(|h|h==name).and_then(|i|row.get(i)).unwrap_or("")
}
fn title_type(t:&str)->bool{
    matches!(t,"movie"|"tvMovie"|"short"|"tvShort"|"tvSpecial"|"tvSeries"|"tvMiniSeries")
}
fn add_title(db:&Connection,id:&str,title:&str,year:i32,kind:&str,original:i32,ordering:i32,region:&str)->Result<()> {
    let key=crate::normalize(title);
    if !key.is_empty() {
        db.execute("INSERT OR IGNORE INTO titles VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id,title,key,year,if matches!(kind,"tvSeries"|"tvMiniSeries"){"series"}else{"movie"},
            region,original,ordering,kind])?;
    }
    Ok(())
}
/// Import a local set of official IMDb .tsv.gz exports. Caller must use a staging
/// SQLite file; never run this against a live database.
pub fn import_imdb(db:&mut Connection,basics:&Path,akas:&Path,episodes:&Path)->Result<usize>{
    initialize(db)?;
    let tx=db.transaction()?;
    let mut count=0usize;
    {
        let mut csv=reader(basics)?;
        let headers=csv.headers()?.clone();
        for row in csv.records(){
            let row=row?;
            let kind=field(&row,&headers,"titleType");
            if !title_type(kind){continue;}
            let id=field(&row,&headers,"tconst");
            let year=field(&row,&headers,"startYear").parse::<i32>().unwrap_or(0);
            let primary=field(&row,&headers,"primaryTitle");
            add_title(&tx,id,primary,year,kind,0,0,"")?;
            let original=field(&row,&headers,"originalTitle");
            if crate::normalize(primary)!=crate::normalize(original){
                add_title(&tx,id,original,year,kind,1,0,"")?;
            }
            count+=1;
        }
    }
    // AKAs are streamed; unsupported IMDb IDs are ignored by the indexed lookup.
    {
        let mut csv=reader(akas)?;
        let headers=csv.headers()?.clone();
        let mut find=tx.prepare_cached("SELECT year,type FROM titles WHERE imdb_id=?1 AND ordering=0 LIMIT 1")?;
        for row in csv.records(){
            let row=row?;
            let id=field(&row,&headers,"titleId");
            let info=find.query_row([id],|r|Ok((r.get::<_,i32>(0)?,r.get::<_,String>(1)?)));
            if let Ok((year,kind))=info {
                let title=field(&row,&headers,"title");
                let region=field(&row,&headers,"region");
                let region=if region==r"\N"{""}else{region};
                let ordering=field(&row,&headers,"ordering").parse::<i32>().unwrap_or(0);
                let original=i32::from(field(&row,&headers,"isOriginalTitle")=="1");
                add_title(&tx,id,title,year,&kind,original,ordering,region)?;
            }
        }
    }
    {
        let mut csv=reader(episodes)?;
        let headers=csv.headers()?.clone();
        let mut update=tx.prepare_cached("INSERT INTO seasons(imdb_id,max_season)
          SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM titles WHERE imdb_id=?1
          AND type IN ('tvSeries','tvMiniSeries'))
          ON CONFLICT(imdb_id) DO UPDATE SET max_season=max(max_season,excluded.max_season)")?;
        for row in csv.records(){
            let row=row?;
            let season=field(&row,&headers,"seasonNumber").parse::<i32>().unwrap_or(0);
            if season>0 {
                update.execute(params![field(&row,&headers,"parentTconst"),season])?;
            }
        }
    }
    tx.execute("INSERT OR REPLACE INTO metadata_state VALUES('imdb',datetime('now'),?1)",[count as i64])?;
    tx.commit()?;
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creates_matching_schema(){
        let db=Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        initialize(&db).unwrap();
        assert_eq!(db.query_row("SELECT reverse('abc')",[],|r|r.get::<_,String>(0)).unwrap(),"cba");
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name='episode_map'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
}
