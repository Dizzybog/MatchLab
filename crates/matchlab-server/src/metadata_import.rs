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
    Ok(csv::ReaderBuilder::new().delimiter(b'\t').flexible(true).from_reader(input))
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
                if r.len()!=h.len() {
                    tracing::warn!(record=r.position().map(|p|p.record()), expected=h.len(), actual=r.len(), "skipping malformed metadata TSV record");
                    continue;
                }
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
                if r.len()!=h.len() {
                    tracing::warn!(record=r.position().map(|p|p.record()), expected=h.len(), actual=r.len(), "skipping malformed metadata TSV record");
                    continue;
                }
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
                if r.len()!=h.len() {
                    tracing::warn!(record=r.position().map(|p|p.record()), expected=h.len(), actual=r.len(), "skipping malformed metadata TSV record");
                    continue;
                }
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
                if r.len()!=headers.len() {
                    tracing::warn!(record=r.position().map(|p|p.record()), expected=headers.len(), actual=r.len(), "skipping malformed metadata TSV record");
                    continue;
                }
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

/// Import AniDB's official compressed title aliases into an existing staging DB.
/// The XML is processed incrementally to keep memory usage bounded.
pub fn import_anidb_titles(db_path:&Path,source:&Path)->Result<u64,Box<dyn Error>>{
    use quick_xml::{Reader,events::Event};
    let file=File::open(source)?;
    let input:Box<dyn Read>=if source.extension().is_some_and(|e|e=="gz"){
        Box::new(flate2::read::GzDecoder::new(BufReader::new(file)))
    }else{Box::new(BufReader::new(file))};
    let mut xml=Reader::from_reader(BufReader::new(input));
    xml.config_mut().trim_text(true);
    let mut db=Connection::open(db_path)?;
    let tx=db.transaction()?;
    let mut insert=tx.prepare_cached("INSERT OR IGNORE INTO anidb_titles VALUES(?1,?2,?3,?4,?5)")?;
    let mut buf=Vec::new();
    let mut anime_id=String::new();
    let mut title_type=String::new();
    let mut language=String::new();
    let mut inside_title=false;
    let mut count=0u64;
    loop {
        match xml.read_event_into(&mut buf)? {
            Event::Start(ref e) if e.name().as_ref()==b"anime"=>{
                anime_id.clear();
                for attr in e.attributes().flatten(){
                    if attr.key.as_ref()==b"aid"{
                        anime_id=attr.unescape_value()?.into_owned();
                    }
                }
            },
            Event::Start(ref e) if e.name().as_ref()==b"title"=>{
                inside_title=true;
                title_type.clear();
                language.clear();
                for attr in e.attributes().flatten(){
                    let key=attr.key.as_ref();
                    let value=attr.unescape_value()?.into_owned();
                    if key==b"type"{title_type=value;}
                    else if key==b"lang" || key==b"xml:lang"{language=value;}
                }
            },
            Event::Text(ref e) if inside_title && anime_id.parse::<u64>().unwrap_or(0)>0=>{
                let decoded=String::from_utf8_lossy(e.as_ref());
                let title=quick_xml::escape::unescape(&decoded)?.into_owned();
                let key=clean(&title);
                if !key.is_empty() && matches!(title_type.as_str(),"main"|"official"|"syn"|"synonym"|"short"){
                    insert.execute(params![anime_id,title,key,title_type,language])?;
                    count+=1;
                }
            },
            Event::End(ref e) if e.name().as_ref()==b"title"=>inside_title=false,
            Event::End(ref e) if e.name().as_ref()==b"anime"=>anime_id.clear(),
            Event::Eof=>break,
            _=>{}
        }
        buf.clear();
    }
    drop(insert);
    if count==0{return Err("AniDB titles contained no usable aliases".into());}
    let now=SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
    tx.execute("INSERT OR REPLACE INTO metadata_state VALUES('anidb_titles',?1,?2)",
        params![now,count])?;
    tx.commit()?;
    Ok(count)
}

/// Stream Anime-Lists mappings into the existing staging database.
/// Explicit XML mapping entries are retained; implicit part-boundary inference
/// and upstream override merging are handled separately.
pub fn import_animelists(db_path:&Path,source:&Path)->Result<u64,Box<dyn Error>>{
    use quick_xml::{Reader,events::Event};
    use rusqlite::OptionalExtension;
    let file=File::open(source)?;
    let mut xml=Reader::from_reader(BufReader::new(file));
    xml.config_mut().trim_text(true);
    let mut db=Connection::open(db_path)?;
    let tx=db.transaction()?;
    let mut insert=tx.prepare_cached("INSERT INTO episode_map VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?;
    let mut links=tx.prepare_cached("INSERT OR IGNORE INTO anime_series_map VALUES(?1,?2,?3,0)")?;
    let mut buf=Vec::new();
    let (mut aid,mut tvdb,mut imdb)=(String::new(),String::new(),String::new());
    let mut last_parts:std::collections::HashMap<(String,i64),(String,i64)>=std::collections::HashMap::new();
    let mut valid=false;
    let mut pending:Option<(i64,i64,i64,i64,i64,String)>=None;
    let mut count=0u64;
    loop{
        let event=xml.read_event_into(&mut buf)?;
        let empty_mapping=matches!(&event,Event::Empty(e) if e.name().as_ref()==b"mapping");
        match event{
            Event::Start(ref e) if e.name().as_ref()==b"anime"=>{
                aid.clear();tvdb.clear();imdb.clear();
                let (mut default_season,mut default_offset)=(None,0i64);
                for a in e.attributes(){
                    let a=a?;
                    let value=a.unescape_value()?.into_owned();
                    match a.key.as_ref(){
                        b"anidbid"=>aid=value,
                        b"tvdbid"=>tvdb=value,
                        b"imdbid"=>imdb=value,
                        b"defaulttvdbseason"=>default_season=if value=="a"{Some(-1)}else{value.parse().ok()},
                        b"episodeoffset"=>default_offset=value.parse().unwrap_or(0),
                        _=>{}
                    }
                }
                valid=aid.parse::<u64>().unwrap_or(0)>0 && tvdb.parse::<u64>().unwrap_or(0)>0;
                if valid{
                    if imdb.starts_with("tt"){links.execute(params![imdb,aid,tvdb])?;}
                    if let Some(season)=default_season{
                        let mut start=if season == -1{1}else{0};
                        if season>0 {
                            let key=(tvdb.clone(),season);
                            if let Some((previous_aid,previous_offset))=last_parts.get(&key){
                                // A second AniDB entry in the same TVDB season starts a new part.
                                // Bound the preceding part by the difference in absolute offsets.
                                tx.execute("UPDATE episode_map SET start=CASE WHEN start=0 THEN 1 ELSE start END,
                                    end=CASE WHEN end=0 THEN max(0,?1-offset) ELSE end END
                                    WHERE anidb_id=?2 AND tvdb_id=?3 AND anidb_season=1 AND tvdb_season=?4",
                                    params![default_offset,previous_aid,tvdb,season])?;
                                let _=previous_offset;
                                start=1;
                            }
                            last_parts.insert(key,(aid.clone(),default_offset));
                        }
                        insert.execute(params![aid,tvdb,1,season,start,0,default_offset,""])?;
                        count+=1;
                    }
                }
            },
            Event::Start(ref e) | Event::Empty(ref e) if e.name().as_ref()==b"mapping"=>{
                if !valid{buf.clear();continue;}
                let (mut ani,mut tv,mut start,mut end,mut offset)=(None,None,0i64,0i64,0i64);
                let mut has_start=false;
                let mut has_end=false;
                let mut has_offset=false;
                for a in e.attributes(){
                    let a=a?;
                    let value=a.unescape_value()?.into_owned();
                    match a.key.as_ref(){
                        b"anidbseason"=>ani=value.parse::<i64>().ok(),
                        b"tvdbseason"=>tv=value.parse::<i64>().ok(),
                        b"start"=>{start=value.parse().unwrap_or(0);has_start=true;},
                        b"end"=>{end=value.parse().unwrap_or(0);has_end=true;},
                        b"offset"=>{offset=value.parse().unwrap_or(0);has_offset=true;},
                        _=>{}
                    }
                }
                if let (Some(ani),Some(tv))=(ani,tv){
                    if ani==0||ani==1{
                        let existing:Option<(i64,i64,i64)>=tx.query_row(
                            "SELECT start,end,offset FROM episode_map WHERE anidb_id=?1 AND tvdb_id=?2 AND anidb_season=?3 AND tvdb_season=?4 ORDER BY rowid DESC LIMIT 1",
                            params![aid,tvdb,ani,tv],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
                        ).optional()?;
                        if let Some((old_start,old_end,old_offset))=existing{
                            tx.execute("DELETE FROM episode_map WHERE anidb_id=?1 AND tvdb_id=?2 AND anidb_season=?3 AND tvdb_season=?4",
                                params![aid,tvdb,ani,tv])?;
                            if !has_start{start=old_start;}
                            if !has_end{end=old_end;}
                            if !has_offset{offset=old_offset;}
                            count=count.saturating_sub(1);
                        }
                        if empty_mapping{
                            insert.execute(params![aid,tvdb,ani,tv,start,end,offset,""])?;
                            count+=1;
                        }else{
                            pending=Some((ani,tv,start,end,offset,String::new()));
                        }
                    }
                }
            },
            Event::Text(ref e) if pending.is_some()=>{
                let decoded=String::from_utf8_lossy(e.as_ref());
                let value=quick_xml::escape::unescape(&decoded)?;
                if let Some(ref mut mapping)=pending{mapping.5.push_str(&value);}
            },
            Event::End(ref e) if e.name().as_ref()==b"mapping"=>{
                if let Some((ani,tv,start,end,offset,mapping))=pending.take(){
                    insert.execute(params![aid,tvdb,ani,tv,start,end,offset,mapping.trim()])?;
                    count+=1;
                }
            },
            Event::Eof=>{
                if valid || pending.is_some() {
                    return Err("truncated Anime-Lists XML: unclosed anime or mapping".into());
                }
                break;
            },
            Event::End(ref e) if e.name().as_ref()==b"anime"=>{
                if pending.is_some(){return Err("unclosed Anime-Lists mapping".into());}
                valid=false;
            },
            _=>{}
        }
        buf.clear();
    }
    drop(insert);drop(links);
    if count==0{return Err("Anime-Lists contained no episode mappings".into());}
    let now=SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
    tx.execute("INSERT OR REPLACE INTO metadata_state VALUES('animelists',?1,?2)",params![now,count])?;
    tx.commit()?;
    Ok(count)
}

/// Build all metadata in a disposable database, validate each required dataset,
/// then atomically replace the live SQLite file only after success.
/// Inputs are local files so callers can download and verify sources independently.
pub fn refresh_from_files(
    target:&Path,basics:&Path,akas:&Path,episodes:&Path,
    animeapi:&Path,animelists:&Path,anidb_titles:&Path,
)->Result<(u64,u64,u64,u64),Box<dyn Error>>{
    let parent=target.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let stamp=SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stage=parent.join(format!(".matchlab-full-{}-{stamp}.sqlite3",std::process::id()));
    let result=(||{
        let imdb=import_imdb(&stage,basics,akas,episodes)?;
        let links=import_animeapi(&stage,animeapi)?;
        let episodes_count=import_animelists(&stage,animelists)?;
        let aliases=import_anidb_titles(&stage,anidb_titles)?;
        if imdb==0||links==0||episodes_count==0||aliases==0{
            return Err("incomplete metadata: refusing to replace live database".into());
        }
        {
            let db=Connection::open(&stage)?;
            let now=SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
            db.execute("INSERT OR REPLACE INTO metadata_state VALUES('full_refresh',?1,?2)",
                params![now,imdb+links+episodes_count+aliases])?;
            // Verify the complete SQLite image before publishing it.
            let integrity:String=db.query_row("PRAGMA integrity_check",[],|row|row.get(0))?;
            if integrity!="ok"{
                return Err(format!("staging SQLite integrity check failed: {integrity}").into());
            }
        }
        // Persist the completed staging file before atomically publishing it.
        File::open(&stage)?.sync_all()?;
        fs::rename(&stage,target)?;
        // The rename is atomic on the same filesystem. Sync the directory as
        // well so the new directory entry survives a sudden power loss.
        File::open(parent)?.sync_all()?;
        Ok((imdb,links,episodes_count,aliases))
    })();
    if result.is_err(){let _=fs::remove_file(&stage);}
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn animelists_preserves_explicit_mapping_text_and_overrides_default() {
        let folder=std::env::temp_dir().join(format!("matchlab-mapping-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let db_path=folder.join("test.sqlite3");
        let xml_path=folder.join("anime-list.xml");
        fs::write(&xml_path,br#"<anime-list><anime anidbid="123" tvdbid="456" defaulttvdbseason="1" episodeoffset="12"><mapping-list><mapping anidbseason="1" tvdbseason="1" start="3" end="7">1-3;2-4</mapping></mapping-list></anime></anime-list>"#).unwrap();
        let db=Connection::open(&db_path).unwrap();
        db.execute_batch("CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER,PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);").unwrap();
        drop(db);
        assert_eq!(import_animelists(&db_path,&xml_path).unwrap(),1);
        let db=Connection::open(&db_path).unwrap();
        let row:(i64,i64,i64,String)=db.query_row("SELECT start,end,offset,map FROM episode_map WHERE anidb_id='123'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(row,(3,7,12,"1-3;2-4".to_owned()));
        drop(db);
        fs::remove_dir_all(&folder).unwrap();
    }
    #[test]
    fn animelists_imports_basic_season_and_episode_mapping() {
        let folder=std::env::temp_dir().join(format!("matchlab-anime-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let db_path=folder.join("test.sqlite3");
        let xml_path=folder.join("anime-list.xml");
        fs::write(&xml_path,br#"<anime-list><anime anidbid="123" tvdbid="456" imdbid="tt1234567" defaulttvdbseason="2" episodeoffset="12"><mapping-list><mapping anidbseason="1" tvdbseason="3" start="5" end="10" offset="4"/></mapping-list></anime></anime-list>"#).unwrap();
        let db=Connection::open(&db_path).unwrap();
        db.execute_batch("CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER,PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);").unwrap();
        drop(db);
        let count=import_animelists(&db_path,&xml_path).unwrap();
        assert_eq!(count,2);
        let db=Connection::open(&db_path).unwrap();
        let rows:i64=db.query_row("SELECT count(*) FROM episode_map WHERE anidb_id='123' AND tvdb_id='456'",[],|r|r.get(0)).unwrap();
        assert_eq!(rows,2);
        let links:i64=db.query_row("SELECT count(*) FROM anime_series_map WHERE imdb_id='tt1234567'",[],|r|r.get(0)).unwrap();
        assert_eq!(links,1);
        drop(db);
        fs::remove_dir_all(&folder).unwrap();
    }
    #[test]
    fn malformed_animelists_rolls_back_all_rows_and_metadata_state() {
        let folder=std::env::temp_dir().join(format!("matchlab-rollback-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let db_path=folder.join("test.sqlite3");
        let xml_path=folder.join("anime-list.xml");
        fs::write(&xml_path,br#"<anime-list><anime anidbid="123" tvdbid="456" defaulttvdbseason="1" episodeoffset="0"/></anime-list><broken"#).unwrap();
        let db=Connection::open(&db_path).unwrap();
        db.execute_batch("CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER,PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);
            INSERT INTO episode_map VALUES('existing','999',1,1,1,10,0,'');").unwrap();
        drop(db);
        assert!(import_animelists(&db_path,&xml_path).is_err());
        let db=Connection::open(&db_path).unwrap();
        let rows:i64=db.query_row("SELECT count(*) FROM episode_map",[],|r|r.get(0)).unwrap();
        assert_eq!(rows,1,"failed XML import must not publish partial mappings");
        let old:String=db.query_row("SELECT anidb_id FROM episode_map",[],|r|r.get(0)).unwrap();
        assert_eq!(old,"existing");
        let state:i64=db.query_row("SELECT count(*) FROM metadata_state",[],|r|r.get(0)).unwrap();
        assert_eq!(state,0);
        drop(db);
        fs::remove_dir_all(&folder).unwrap();
    }
    #[test]
    fn failed_refresh_preserves_existing_database() {
        let folder=std::env::temp_dir().join(format!(
            "matchlab-refresh-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let target=folder.join("live.sqlite3");
        fs::write(&target,b"original database bytes").unwrap();
        let missing=folder.join("missing.tsv.gz");
        let result=refresh_from_files(&target,&missing,&missing,&missing,
            &missing,&missing,&missing);
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(),b"original database bytes");
        let names:Vec<_>=fs::read_dir(&folder).unwrap()
            .map(|entry|entry.unwrap().file_name()).collect();
        assert_eq!(names.len(),1,"failed refresh must remove staging database");
        fs::remove_dir_all(&folder).unwrap();
    }
    #[test]
    fn truncated_animelists_does_not_publish_partial_rows(){
        let folder=std::env::temp_dir().join(format!("matchlab-truncated-{}-{}",
            std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let path=folder.join("metadata.sqlite3");
        let xml=folder.join("anime-list.xml");
        fs::write(&xml,br#"<anime-list><anime anidbid="123" tvdbid="456" defaulttvdbseason="1">"#).unwrap();
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER,PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);").unwrap();
        drop(db);
        assert!(import_animelists(&path,&xml).is_err());
        let db=Connection::open(&path).unwrap();
        let rows:i64=db.query_row("SELECT count(*) FROM episode_map",[],|r|r.get(0)).unwrap();
        assert_eq!(rows,0);
        let state:i64=db.query_row("SELECT count(*) FROM metadata_state",[],|r|r.get(0)).unwrap();
        assert_eq!(state,0);
        drop(db);
        fs::remove_dir_all(&folder).unwrap();
    }
    #[test]
    fn malformed_tsv_rows_are_skipped_without_shifting_columns(){
        let folder=std::env::temp_dir().join(format!("matchlab-tsv-{}-{}",
            std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let path=folder.join("records.tsv");
        fs::write(&path,b"first\\tsecond\\tthird\\nvalid\\ta\\tb\\nmissing\\tfield\\nvalid2\\tc\\td\\n").unwrap();
        let mut input=reader(&path).unwrap();
        let h=input.headers().unwrap().clone();
        let mut valid=Vec::new();
        for row in input.records(){
            let r=row.unwrap();
            if r.len()!=h.len(){continue;}
            valid.push(r.get(0).unwrap().to_owned());
        }
        assert_eq!(valid,vec!["valid","valid2"]);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn normalization(){
        assert_eq!(clean("Pokémon and Friends!"),"pokemonfriends");
        assert_eq!(clean("Bobs Burgers"),"bobsburgers");
    }
}
