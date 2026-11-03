//! Native IMDb metadata ingestion. Accepts locally downloaded IMDb TSV/TSV.GZ files.
//! Builds a staging SQLite database and publishes it only after a complete import.
use std::{error::Error,fs::{self,File},io::{Read,BufReader},path::Path,time::{SystemTime,UNIX_EPOCH}};
use rusqlite::{params,Connection};
use unicode_normalization::UnicodeNormalization;

fn clean(s:&str)->String {
    s.to_lowercase().replace('æ',"ae").replace('ø',"o").replace('œ',"oe").replace('ß',"ss")
        .nfkd().filter(|c|c.is_ascii()).collect::<String>()
        .split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|x|!x.is_empty() && *x!="and").collect()
}
fn reader(path:&Path)->Result<csv::Reader<Box<dyn Read>>,Box<dyn Error>>{
    let file=File::open(path)?;
    let input:Box<dyn Read>=if path.extension().is_some_and(|e|e=="gz"){
        Box::new(flate2::read::GzDecoder::new(BufReader::new(file)))
    }else{Box::new(BufReader::new(file))};
    Ok(csv::ReaderBuilder::new().delimiter(b'\t').from_reader(input))
}
fn col(headers:&csv::StringRecord,name:&str)->Result<usize,Box<dyn Error>>{
    headers.iter().position(|h|h==name).ok_or_else(||format!("missing TSV header: {name}").into())
}
pub fn import_imdb(target:&Path,basics:&Path,akas:&Path,episodes:&Path)->Result<u64,Box<dyn Error>>{
    let parent=target.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let stamp=SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stage=parent.join(format!(".matchlab-{}-{stamp}.sqlite3",std::process::id()));
    let result=(||->Result<u64,Box<dyn Error>>{
        let mut db=Connection::open(&stage)?;
        db.create_scalar_function("reverse",1,rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,|ctx|{
            let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())
        })?;
        db.execute_batch("PRAGMA journal_mode=DELETE;
            CREATE TABLE titles(imdb_id TEXT NOT NULL,title TEXT NOT NULL,clean_title TEXT NOT NULL,
                year INTEGER NOT NULL,category TEXT NOT NULL,region TEXT,
                original INTEGER NOT NULL DEFAULT 0,ordering INTEGER NOT NULL DEFAULT 0,
                type TEXT NOT NULL DEFAULT '',
                PRIMARY KEY(imdb_id,clean_title,year,region,ordering));
            CREATE TABLE seasons(imdb_id TEXT PRIMARY KEY,max_season INTEGER NOT NULL);
            CREATE TABLE anime_ids(group_id TEXT NOT NULL,type TEXT NOT NULL,value TEXT NOT NULL,
                PRIMARY KEY(group_id,type,value));
            CREATE TABLE anime_series_map(imdb_id TEXT NOT NULL,anidb_id TEXT NOT NULL,
                tvdb_id TEXT NOT NULL DEFAULT '',trakt_season INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
            CREATE TABLE episode_map(anidb_id TEXT NOT NULL,tvdb_id TEXT NOT NULL,
                anidb_season INTEGER NOT NULL,tvdb_season INTEGER NOT NULL,
                start INTEGER NOT NULL,end INTEGER NOT NULL,offset INTEGER NOT NULL DEFAULT 0,
                map TEXT NOT NULL DEFAULT '');
            CREATE TABLE anidb_titles(anidb_id TEXT NOT NULL,title TEXT NOT NULL,
                clean_title TEXT NOT NULL,title_type TEXT NOT NULL,language TEXT NOT NULL,
                PRIMARY KEY(anidb_id,clean_title,title_type,language));
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
            CREATE INDEX title_lookup ON titles(clean_title,year);
            CREATE INDEX title_suffix ON titles(clean_title);
            CREATE INDEX title_reverse_suffix ON titles(reverse(clean_title),type);
            CREATE INDEX anidb_title_lookup ON anidb_titles(clean_title);
            CREATE INDEX anime_series_by_anidb ON anime_series_map(anidb_id);
            CREATE INDEX anime_series_by_imdb ON anime_series_map(imdb_id);
            CREATE INDEX anime_lookup_idx ON anime_ids(type,value);
            CREATE INDEX episode_lookup_idx ON episode_map(anidb_id,anidb_season,start,end);")?;
        let tx=db.transaction()?;
        let mut count=0u64;
        {
            let mut input=reader(basics)?;
            let h=input.headers()?.clone();
            let (id,kind,primary,original,year)=(col(&h,"tconst")?,col(&h,"titleType")?,
                col(&h,"primaryTitle")?,col(&h,"originalTitle")?,col(&h,"startYear")?);
            let mut insert=tx.prepare_cached("INSERT OR IGNORE INTO titles VALUES(?1,?2,?3,?4,?5,'',?6,0,?7)")?;
            for row in input.records(){
                let r=row?;
                let t=r.get(kind).unwrap_or("");
                if !matches!(t,"movie"|"tvMovie"|"short"|"tvShort"|"tvSpecial"|"tvSeries"|"tvMiniSeries"){continue}
                let id=r.get(id).unwrap_or("");
                let yr=r.get(year).unwrap_or("").parse::<i32>().unwrap_or(0);
                let category=if t=="tvSeries"||t=="tvMiniSeries"{"series"}else{"movie"};
                for (title,is_original) in [(r.get(primary).unwrap_or(""),0),(r.get(original).unwrap_or(""),1)]{
                    let key=clean(title);
                    if !key.is_empty(){
                        insert.execute(params![id,title,key,yr,category,is_original,t])?;
                    }
                }
                count+=1;
            }
        }
        if count==0{return Err("IMDb basics contained no supported titles".into())}
        {
            let mut input=reader(akas)?;
            let h=input.headers()?.clone();
            let (id,ordering,title,region,original)=(col(&h,"titleId")?,col(&h,"ordering")?,
                col(&h,"title")?,col(&h,"region")?,col(&h,"isOriginalTitle")?);
            let mut insert=tx.prepare_cached("INSERT OR IGNORE INTO titles
                SELECT imdb_id,?2,?3,year,category,?4,?5,?6,type FROM titles
                WHERE imdb_id=?1 AND ordering=0 AND region='' LIMIT 1")?;
            for row in input.records(){
                let r=row?;
                let title=r.get(title).unwrap_or("");
                let key=clean(title);
                if key.is_empty(){continue}
                let reg=r.get(region).unwrap_or("");
                let reg=if reg==r"\N"{""}else{reg};
                insert.execute(params![r.get(id).unwrap_or(""),title,key,reg,
                    i32::from(r.get(original)==Some("1")),
                    r.get(ordering).unwrap_or("0").parse::<i64>().unwrap_or(0)])?;
            }
        }
        {
            let mut input=reader(episodes)?;
            let h=input.headers()?.clone();
            let (parent,season)=(col(&h,"parentTconst")?,col(&h,"seasonNumber")?);
            let mut insert=tx.prepare_cached("INSERT INTO seasons(imdb_id,max_season)
                SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM titles
                WHERE imdb_id=?1 AND type IN ('tvSeries','tvMiniSeries'))
                ON CONFLICT(imdb_id) DO UPDATE SET max_season=max(max_season,excluded.max_season)")?;
            for row in input.records(){
                let r=row?;
                let se=r.get(season).unwrap_or("").parse::<i64>().unwrap_or(0);
                if se>0{insert.execute(params![r.get(parent).unwrap_or(""),se])?;}
            }
        }
        let now=SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
        tx.execute("INSERT INTO metadata_state VALUES('imdb',?1,?2)",params![now,count])?;
        // Do not mark full_refresh: anime datasets must also be imported and validated.
        tx.commit()?;
        drop(db);
        fs::rename(&stage,target)?;
        Ok(count)
    })();
    if result.is_err(){let _=fs::remove_file(&stage);}
    result
}

/// Import a locally downloaded animeApi v3 TSV into an existing staging database.
/// Never publish a partially imported anime dataset.
pub fn import_animeapi(db_path:&Path,source:&Path)->Result<u64,Box<dyn Error>>{
    let mut db=Connection::open(db_path)?;
    let tx=db.transaction()?;
    let mut input=reader(source)?;
    let headers=input.headers()?.clone();
    let imdb=col(&headers,"imdb")?;
    let anidb=col(&headers,"anidb")?;
    let tvdb=col(&headers,"thetvdb")?;
    let trakt=col(&headers,"trakt_season")?;
    let mut links=tx.prepare_cached("INSERT OR IGNORE INTO anime_series_map VALUES(?1,?2,?3,?4)")?;
    let mut identities=tx.prepare_cached("INSERT OR IGNORE INTO anime_ids VALUES(?1,?2,?3)")?;
    let mut count=0u64;
    for row in input.records(){
        let r=row?;
        let id=r.get(imdb).unwrap_or("").trim();
        let aid=r.get(anidb).unwrap_or("").trim();
        if !id.starts_with("tt") || !id[2..].bytes().all(|b|b.is_ascii_digit())
            || aid.parse::<u64>().unwrap_or(0)==0 {continue;}
        let tv=r.get(tvdb).unwrap_or("").trim();
        let tv=if tv.parse::<u64>().unwrap_or(0)>0 {tv}else{""};
        let may_invalid=headers.iter().position(|h|h=="trakt_may_invalid")
            .and_then(|i|r.get(i)).is_some_and(|v|v.eq_ignore_ascii_case("true"));
        let season=if may_invalid {0}else{r.get(trakt).unwrap_or("").parse::<i64>().unwrap_or(0)};
        links.execute(params![id,aid,tv,season])?;
        identities.execute(params![id,"anidb",aid])?;
        for (column,source) in [("thetvdb","tvdb"),("myanimelist","mal"),
            ("anilist","anilist"),("kitsu","kitsu")]{
            if let Some(value)=headers.iter().position(|h|h==column).and_then(|i|r.get(i)){
                let value=value.trim();
                if !value.is_empty() && value!="0" && value!="unknown"{
                    identities.execute(params![id,source,value])?;
                }
            }
        }
        count+=1;
    }
    drop(links);
    drop(identities);
    if count==0{return Err("animeApi contained no valid IMDb/AniDB links".into());}
    let now=SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
    tx.execute("INSERT OR REPLACE INTO metadata_state VALUES('animeapi',?1,?2)",
        params![now,count])?;
    tx.commit()?;
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization(){
        assert_eq!(clean("Pokémon and Friends!"),"pokemonfriends");
        assert_eq!(clean("Bobs Burgers"),"bobsburgers");
    }
}
