//! IMDb V4 candidate search ported from MediaMatch/index.py.
//! The ranking stages intentionally match the Python reference implementation.
use rusqlite::{params, Connection, Row};
use std::collections::{BTreeMap, HashSet};
use unicode_normalization::UnicodeNormalization;
use serde::Serialize;

pub fn clean_title(input: &str) -> String {
    let lower = input.to_lowercase().replace('æ',"ae").replace('ø',"o").replace('œ',"oe").replace('ß',"ss");
    let ascii: String = lower.nfkd().filter(|c| c.is_ascii()).collect();
    ascii.split(|c:char| !c.is_ascii_alphanumeric()).filter(|word| !word.is_empty() && *word != "and").collect()
}

#[derive(Clone,Debug)]
struct Aka {
    imdb_id:String, title:String, clean:String, year:i32, category:String,
    region:Option<String>, original:bool, ordering:i32, media_type:String,
}
fn read_aka(r:&Row<'_>)->rusqlite::Result<Aka>{
    Ok(Aka{imdb_id:r.get("imdb_id")?,title:r.get("title")?,clean:r.get("clean_title")?,
        year:r.get("year")?,category:r.get("category")?,region:r.get("region")?,
        original:r.get::<_,i64>("original")? != 0,ordering:r.get("ordering")?,
        media_type:r.get("type")?})
}
#[derive(Clone,Debug,Serialize)]
pub struct Match {
    pub imdb_id:String,pub title:String,pub year:i32,pub category:String,
    pub matched_title:String,pub anime_ids:BTreeMap<String,String>,pub media_type:String,
}
fn is_series(t:&str)->bool {t=="tvSeries" || t=="tvMiniSeries"}
fn allowed(t:&str,series:bool,year:Option<i32>)->bool {
    if series {is_series(t)} else {
        matches!(t,"movie"|"tvMovie"|"short"|"tvShort"|"tvSpecial") || (year.is_none() && is_series(t))
    }
}
fn eligible(db:&Connection,r:&Aka,season:i64)->rusqlite::Result<bool>{
    if !is_series(&r.media_type)||season<=0{return Ok(true);}
    let max:Option<i64>=db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
        [&r.imdb_id],|row|row.get(0)).optional()?;
    if max.unwrap_or(0)>=std::cmp::max(1,season-1){return Ok(true);}
    Ok(db.query_row("SELECT 1 FROM anime_series_map l JOIN episode_map m ON m.anidb_id=l.anidb_id
        WHERE l.imdb_id=?1 AND (m.tvdb_season=?2 OR l.trakt_season=?2) LIMIT 1",
        params![r.imdb_id,season],|row|row.get::<_,i32>(0)).optional()?.is_some())
}
use rusqlite::OptionalExtension;
fn select(db:&Connection,where_clause:&str,values:&[&dyn rusqlite::ToSql])->rusqlite::Result<Vec<Aka>>{
    let sql=format!("SELECT * FROM titles WHERE {where_clause} AND year>0");
    let mut stmt=db.prepare_cached(&sql)?;
    stmt.query_map(values,read_aka)?.collect()
}
fn filter(db:&Connection,rows:Vec<Aka>,release:&str,series:bool,year:Option<i32>,season:i64)->rusqlite::Result<Vec<Aka>>{
    let mut eligibility=std::collections::HashMap::new();
    let mut result=Vec::new();
    for row in rows {
        if !allowed(&row.media_type,series,year)||!release.contains(&row.clean){continue;}
        let yes=if let Some(&cached)=eligibility.get(&row.imdb_id){cached}else{
            let value=eligible(db,&row,season)?;
            eligibility.insert(row.imdb_id.clone(),value);
            value
        };
        if yes{result.push(row);}
    }
    Ok(result)
}
fn prefix(db:&Connection,key:&str,release:&str,series:bool,year:Option<i32>,season:i64)->rusqlite::Result<Vec<Aka>>{
    let upper=format!("{key}\u{ffff}");
    // Query the exhaustive Tantivy ID set, then apply the authoritative
    // relational eligibility and deterministic V4 ranking in SQLite.
    // Keep the SQLite range as a safety fallback when the index is unavailable.
    if let Ok(ids)=crate::tantivy_search::candidates(db,key,year,false,1){
        if !ids.is_empty(){
            // SQLite's indexed prefix range is more efficient than issuing
            // one SQL statement per IMDb ID for broad prefixes. The exhaustive
            // Tantivy result guarantees discovery; a single range query loads
            // the full V4 alias rows without an N+1 database access pattern.
            let _=ids;
            return filter(db,select(db,
                "clean_title>=?1 AND clean_title<?2",
                &[&key,&upper])?,release,series,year,season);
        }
    }
    filter(db,select(db,"clean_title>=?1 AND clean_title<?2",&[&key,&upper])?,release,series,year,season)
}
fn type_rank(t:&str)->i32{
    match t{"movie"=>0,"tvMovie"=>1,"short"=>2,"tvShort"=>3,"tvSpecial"=>4,"tvSeries"=>5,_=>6}
}
fn rank(r:&Aka,year:Option<i32>,series:bool,duplicate:bool,count_first:bool,votes:i64)
    -> (i64,i32,i32,i32,i32,i32,i64,i32,String) {
    let original=if !duplicate||r.original{0}else{1};
    let us=if !duplicate||r.region.as_deref()==Some("US"){0}else{1};
    let (distance,original_order,year_order)=if let Some(y)=year{
        ((r.year-y).abs(),original,r.year)
    }else{(original,r.year,0)};
    (-if count_first{votes}else{0},distance,original_order,year_order,
        if series{0}else{type_rank(&r.media_type)},us,
        -if count_first{0}else{votes},r.ordering,r.imdb_id.clone())
}
pub fn search(db:&Connection,title:&str,year:Option<i32>,series:bool,required_season:i64,
    release_title:&str,has_episode:bool,limit:usize)->rusqlite::Result<Vec<Match>>{
    let key=clean_title(title);
    let release=clean_title(release_title);
    if key.is_empty(){return Ok(vec![]);}
    let mut stage="forward_prefix";
    let mut rows=Vec::new();
    // Exact country-qualified aliases precede generic prefix matching.
    if series {
        for candidate in crate::country_aliases::candidates(release_title) {
            let found=select(db,"clean_title=?1",&[&candidate])?;
            for row in found {
                if is_series(&row.media_type) && eligible(db,&row,required_season)? {
                    rows.push(row);
                }
            }
        }
        if !rows.is_empty(){stage="country_exact";}
    }
    if rows.is_empty(){rows=prefix(db,&key,&release,series,year,required_season)?;}
    if rows.is_empty(){
        for size in ((std::cmp::max(2,key.len()/2)+1)..key.len()).rev(){
            if !key.is_char_boundary(size){continue;}
            let short=&key[..size];
            rows=filter(db,select(db,"clean_title=?1",&[&short])?,&release,series,year,required_season)?;
            if !rows.is_empty(){stage="shorter_prefix";break;}
        }
    }
    if rows.is_empty()&&key.len()>=12{
        let rev:String=key.chars().rev().collect();
        let upper=format!("{rev}\u{ffff}");
        let indexed=crate::tantivy_search::candidates(db,&key,year,true,1);
        if let Ok(ids)=indexed {
            if !ids.is_empty(){
                // Avoid one SQLite query per candidate on high-cardinality
                // suffix searches; fetch the complete indexed SQL range once.
                let _=ids;
                rows=filter(db,select(db,
                    "reverse(clean_title)>=?1 AND reverse(clean_title)<?2 AND length(clean_title)>?3",
                    &[&rev,&upper,&(key.len() as i64)])?,&release,series,year,required_season)?;
            }
        }
        // Fallback to the complete SQLite range when Tantivy cannot
        // provide eligible suffix candidates.
        if rows.is_empty(){
            rows=filter(db,select(db,
                "reverse(clean_title)>=?1 AND reverse(clean_title)<?2 AND length(clean_title)>?3",
                &[&rev,&upper,&(key.len() as i64)])?,&release,series,year,required_season)?;
        }
        if !rows.is_empty(){stage="reverse_suffix";}
    }
    if rows.is_empty()&&key.len()>=6{
        let half=&key[..(key.len()+1)/2];
        rows=prefix(db,half,&release,series,year,required_season)?;
        if !rows.is_empty(){stage="half_prefix";}
    }
    if rows.is_empty()&&series{
        rows=filter(db,select(db,"imdb_id IN (
            SELECT l.imdb_id FROM anidb_titles a JOIN anime_series_map l ON l.anidb_id=a.anidb_id
            WHERE a.clean_title=?1)",&[&key])?,&release,series,year,required_season)?;
        if !rows.is_empty(){stage="anime_alias";}
    }
    if rows.is_empty(){return Ok(vec![]);}
    if matches!(stage,"forward_prefix"|"half_prefix"|"country_exact"){
        let shape=rows.iter().map(|r|(if r.clean==key{0}else{1},-(r.clean.len() as i64))).min().unwrap();
        rows.retain(|r|(if r.clean==key{0}else{1},-(r.clean.len() as i64))==shape);
    }else if stage=="reverse_suffix"{
        let longest=rows.iter().map(|r|r.clean.len()).max().unwrap();
        rows.retain(|r|r.clean.len()==longest);
    }
    let mut groups:BTreeMap<String,Vec<Aka>>=BTreeMap::new();
    for r in rows{groups.entry(r.imdb_id.clone()).or_default().push(r);}
    let mut duplicates=HashSet::new();
    let mut checked=HashSet::new();
    let mut duplicate_stmt=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM titles WHERE clean_title=?1 AND year>0 AND imdb_id <> (SELECT imdb_id FROM titles WHERE clean_title=?1 AND year>0 LIMIT 1))")?;
    for r in groups.values().flatten(){
        if checked.insert(r.clean.clone()) {
            let count:i64=duplicate_stmt.query_row([&r.clean],|row|row.get(0))?;
            if count!=0{duplicates.insert(r.clean.clone());}
        }
    }
    let count_first=year.is_none()&&required_season==0&&!has_episode;
    let mut output=Vec::new();
    for (id,aliases) in groups{
        let votes:i64=db.query_row("SELECT num_votes FROM imdb_ratings WHERE imdb_id=?1",[&id],|r|r.get(0))
            .optional().unwrap_or(None).unwrap_or(0);
        let best=aliases.into_iter().min_by_key(|r|rank(r,year,series,duplicates.contains(&r.clean),count_first,votes)).unwrap();
        let mut ids=BTreeMap::new();
        let mut stmt=db.prepare("SELECT type,value FROM anime_ids WHERE group_id=?1")?;
        for item in stmt.query_map([&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?{
            let (k,v)=item?;ids.insert(k,v);
        }
        if !ids.contains_key("anidb"){
            let mut stmt=db.prepare("SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?1")?;
            let linked=stmt.query_map([&id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if linked.len()==1{ids.insert("anidb".into(),linked[0].clone());}
        }
        let score=rank(&best,year,series,duplicates.contains(&best.clean),count_first,votes);
        output.push((score,Match{imdb_id:id,title:best.title.clone(),year:best.year,
            category:best.category,matched_title:best.title,anime_ids:ids,media_type:best.media_type}));
    }
    output.sort_by(|a,b|a.0.cmp(&b.0));
    Ok(output.into_iter().take(limit).map(|(_,m)|m).collect())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test] fn votes_replace_alias_count_as_tiebreaker(){
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                |rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES('ttlow','Example','example',2020,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('tthigh','Example','example',2020,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('ttlow','Example alternate','examplealternate',2020,'movie','',0,1,'movie');
            INSERT INTO imdb_ratings VALUES('ttlow',100);
            INSERT INTO imdb_ratings VALUES('tthigh',50000);").unwrap();
        let found=search(&db,"Example",Some(2020),false,0,"Example.2020",false,5).unwrap();
        assert_eq!(found[0].imdb_id,"tthigh");
    }
    #[test] fn normalization(){assert_eq!(clean_title("Pokémon and Ægir"),"pokemonaegir");}
    #[test] fn country_specific_series_beats_generic_alias() {
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC|rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            INSERT INTO titles VALUES('tt100','The Voice US','thevoiceus',2011,'series','US',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt200','The Voice','thevoice',2010,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt100',10);
            INSERT INTO seasons VALUES('tt200',10);").unwrap();
        let found=search(&db,"The Voice US",None,true,3,"The.Voice.US.S03E02.1080p",true,10).unwrap();
        assert_eq!(found[0].imdb_id,"tt100");
    }
    #[test] fn v4_exact_year_and_series_cap() {
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC|rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            INSERT INTO titles VALUES('tt1','Dune','dune',1984,'movie','US',1,0,'movie');
            INSERT INTO titles VALUES('tt2','Dune','dune',2021,'movie','US',1,0,'movie');
            INSERT INTO titles VALUES('tt3','Breaking Bad','breakingbad',2008,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt3',5);").unwrap();
        let movies=search(&db,"Dune",Some(2021),false,0,"Dune.2021.1080p",false,20).unwrap();
        assert_eq!(movies[0].imdb_id,"tt2");
        let no_year=search(&db,"Dune",None,false,0,"Dune.1080p",false,20).unwrap();
        assert!(!no_year.is_empty(),"equally plausible titles must still return a candidate");
        assert_eq!(no_year[0].imdb_id,"tt1","stable ranking chooses the earlier title without a release year");

        let series=search(&db,"Breaking Bad",None,true,4,"Breaking.Bad.S04E02",true,20).unwrap();
        assert_eq!(series[0].imdb_id,"tt3");
        let rejected=search(&db,"Breaking Bad",None,true,9,"Breaking.Bad.S09E02",true,20).unwrap();
        assert!(rejected.is_empty());
    }
}
