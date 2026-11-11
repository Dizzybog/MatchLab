//! Deterministic IMDb title matcher (independent of AniDB identity selection).
//! Ordered stages prioritize exact aliases, then title and season eligibility,
//! year proximity and stable tie-breaking. Source metadata comes from Tantivy.
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashSet};
use serde::Serialize;

pub use crate::title_normalization::clean_title;

#[derive(Clone,Debug)]
struct Aka {
    imdb_id:String, title:String, clean:String, year:i32, category:String,
    region:Option<String>, original:bool, ordering:i32, media_type:String,
}
impl From<crate::tantivy_search::TitleAlias> for Aka {
    fn from(a:crate::tantivy_search::TitleAlias)->Self{
        Self{imdb_id:a.imdb_id,title:a.title,clean:a.clean,year:a.year,
            category:a.category,region:a.region,original:a.original,
            ordering:a.ordering,media_type:a.media_type}
    }
}
fn index_error(error:String)->rusqlite::Error{
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error)))
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
fn tantivy_aliases(db:&Connection,key:&str,suffix:bool)->rusqlite::Result<Vec<Aka>>{
    crate::tantivy_search::title_aliases(db,key,suffix)
        .map(|rows|rows.into_iter().map(Aka::from).collect()).map_err(index_error)
}
fn exact(db:&Connection,key:&str)->rusqlite::Result<Vec<Aka>>{
    crate::tantivy_search::title_aliases_exact(db,key)
        .map(|rows|rows.into_iter().map(Aka::from).collect()).map_err(index_error)
}
fn prefix(db:&Connection,key:&str,release:&str,series:bool,year:Option<i32>,season:i64)->rusqlite::Result<Vec<Aka>>{
    let matching=filter(db,exact(db,key)?,release,series,year,season)?;
    if !matching.is_empty(){return Ok(matching);}
    filter(db,tantivy_aliases(db,key,false)?,release,series,year,season)
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
            let found=exact(db,&candidate)?;
            for row in found {
                if is_series(&row.media_type) && eligible(db,&row,required_season)? {
                    rows.push(row);
                }
            }
        }
        if !rows.is_empty(){stage="country_exact";}
    }
    if rows.is_empty(){
        // "S" or "Th" can be a damaged parser output, but prefix matching
        // these against millions of Tantivy aliases is prohibitively broad.
        // Keep legitimate short titles (Up, It, Her) eligible as exact hits.
        rows=if key.len()<4 {
            filter(db,exact(db,&key)?,&release,series,year,required_season)?
        }else{
            prefix(db,&key,&release,series,year,required_season)?
        };
    }
    if rows.is_empty()&&key.len()>=4{
        for size in ((std::cmp::max(2,key.len()/2)+1)..key.len()).rev(){
            if !key.is_char_boundary(size){continue;}
            let short=&key[..size];
            rows=filter(db,exact(db,short)?,&release,series,year,required_season)?;
            if !rows.is_empty(){stage="shorter_prefix";break;}
        }
    }
    if rows.is_empty()&&key.len()>=12{
        rows=filter(db,tantivy_aliases(db,&key,true)?,
            &release,series,year,required_season)?;
        if !rows.is_empty(){stage="reverse_suffix";}
    }
    if rows.is_empty()&&key.len()>=6{
        let half=&key[..(key.len()+1)/2];
        rows=prefix(db,half,&release,series,year,required_season)?;
        if !rows.is_empty(){stage="half_prefix";}
    }
    if rows.is_empty()&&series&&key.len()>=4{
        // IMDb's English primary title need not appear in the release at
        // all: e.g. Shinigami Bocchan to Kuro Maid is indexed under its
        // English title. The exact/longest AniDB alias provides identity;
        // requiring release.contains(imdb_alias) silently rejects it.
        // Search via the existing Tantivy anime index, not SQLite title scans.
        let anime_aliases=crate::tantivy_search::anime_aliases(db,&key,4)
            .map_err(index_error)?;
        let best_len=anime_aliases.first().map(|alias|alias.clean_title.len())
            .unwrap_or(0);
        let ids=anime_aliases.into_iter()
            .filter(|alias|alias.clean_title.len()==best_len
                && key.starts_with(&alias.clean_title))
            .map(|alias|alias.imdb_id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter().collect::<Vec<_>>();
        if !ids.is_empty(){
            let aliases=crate::tantivy_search::titles_by_ids(db,&ids).map_err(index_error)?;
            let mut eligibility=std::collections::HashMap::new();
            for row in aliases.into_iter().map(Aka::from) {
                if !is_series(&row.media_type){continue;}
                let yes=if let Some(&cached)=eligibility.get(&row.imdb_id){cached}
                    else {
                        let value=eligible(db,&row,required_season)?;
                        eligibility.insert(row.imdb_id.clone(),value);
                        value
                    };
                if yes{rows.push(row);}
            }
        }
        if rows.is_empty(){
            // Keep the original exact indexed anime-ID discovery path as
            // a conservative fallback, for legacy datasets and alias
            // shapes that the prefix lookup cannot represent.
            let ids=crate::tantivy_search::anime_candidates(db,&key)
                .map_err(index_error)?;
            if !ids.is_empty(){
                let aliases=crate::tantivy_search::titles_by_ids(db,&ids)
                    .map_err(index_error)?;
                rows=filter(db,aliases.into_iter().map(Aka::from).collect(),
                    &release,series,year,required_season)?;
            }
        }
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
    
    for r in groups.values().flatten(){
        if checked.insert(r.clean.clone()) {
            if crate::tantivy_search::has_duplicate_title(db,&r.clean).map_err(index_error)?{
                duplicates.insert(r.clean.clone());
            }
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
/// A matching English IMDb alias can belong to an older OVA with too few
/// episodes, while the modern TV series is linked only by AniDB's English
/// alias (e.g. JoJo 1993 versus JoJo 2012 S02E37).
///
/// Keep normal title ranking untouched when an already-ranked candidate has
/// independently confirmed episode coverage. Only on an episode-evidence
/// miss consult Tantivy's *exact* anime aliases and promote a UNIQUE
/// verified series identity. No SQLite title scan, no guessed offsets, and
/// no additional index query on ordinary successful matches.
pub fn prefer_verified_episode_identity(
    db:&Connection,mut candidates:Vec<Match>,title:&str,season:i64,episode:i64,
)->rusqlite::Result<Vec<Match>>{
    if !(1..=9_999).contains(&season)||!(1..=99_999).contains(&episode){
        return Ok(candidates);
    }
    let proven=|id:&str|->rusqlite::Result<bool>{
        let max:Option<i64>=db.query_row(
            "SELECT max_episode FROM season_episodes WHERE imdb_id=?1 AND season=?2",
            params![id,season],|r|r.get(0)).optional()?;
        if max.is_some_and(|max|max>=episode){return Ok(true);}
        Ok(crate::episode_crosswalk::resolve_season(
            db,id,season,episode)?.is_some())
    };
    // A canonical IMDb episode is stronger than an unrelated exact title.
    // Preserve relative ranking among equally supported candidates.
    let mut support=Vec::with_capacity(candidates.len());
    for candidate in &candidates{support.push(proven(&candidate.imdb_id)?);}
    if support.iter().any(|supported|*supported){
        let mut confirmed=Vec::new();
        let mut unconfirmed=Vec::new();
        for (candidate,supported) in candidates.into_iter().zip(support) {
            if supported {confirmed.push(candidate)}else{unconfirmed.push(candidate)}
        }
        confirmed.extend(unconfirmed);
        return Ok(confirmed);
    }
    let key=clean_title(title);
    if key.len()<4{return Ok(candidates);}
    let exact_anime_ids=crate::tantivy_search::anime_aliases(db,&key,4)
        .map_err(index_error)?.into_iter()
        .filter(|alias|alias.clean_title==key)
        .map(|alias|alias.imdb_id)
        .collect::<std::collections::BTreeSet<_>>();
    // Never choose between multiple independently plausible anime works
    // with identical aliases solely because of a database iteration order.
    let mut verified=Vec::new();
    for id in exact_anime_ids {
        if candidates.iter().any(|c|c.imdb_id==id) || !proven(&id)?{continue;}
        verified.push(id);
    }
    if verified.len()!=1{return Ok(candidates);}
    let id=verified.pop().unwrap();
    let aliases=crate::tantivy_search::titles_by_ids(db,&[id.clone()])
        .map_err(index_error)?;
    let Some(best)=aliases.into_iter()
        .filter(|alias|is_series(&alias.media_type))
        .min_by_key(|alias|(
            if alias.original{0}else{1},
            alias.ordering,
            alias.title.len()
        )) else{return Ok(candidates)};
    let mut anime_ids=BTreeMap::new();
    let mut stmt=db.prepare("SELECT type,value FROM anime_ids WHERE group_id=?1")?;
    for row in stmt.query_map([&id],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
        let (kind,value)=row?;
        anime_ids.insert(kind,value);
    }
    if !anime_ids.contains_key("anidb"){
        let mut stmt=db.prepare(
            "SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?1")?;
        let linked=stmt.query_map([&id],|r|r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if linked.len()==1{anime_ids.insert("anidb".into(),linked[0].clone());}
    }
    candidates.insert(0,Match{
        imdb_id:id,title:best.title.clone(),year:best.year,
        category:best.category,matched_title:best.title,
        anime_ids,media_type:best.media_type
    });
    Ok(candidates)
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
    #[test] fn short_titles_require_exact_match_instead_of_broad_tantivy_prefix(){
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
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES('tt0000001','Up','up',2009,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('tt0000002','Upgrade','upgrade',2018,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('tt0000003','The 100','the100',2014,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0000003',7);").unwrap();
        let up=search(&db,"Up",Some(2009),false,0,"Up.2009",false,5).unwrap();
        assert_eq!(up.len(),1);
        assert_eq!(up[0].imdb_id,"tt0000001");
        let partial=search(&db,"Th",None,true,1,"The100.s01",false,5).unwrap();
        assert!(partial.is_empty(),"broken short title must not trigger broad prefix matching");
        let full=search(&db,"The100",None,true,1,"The100.s01",false,5).unwrap();
        assert_eq!(full[0].imdb_id,"tt0000003");
    }
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
    #[test]
    fn monster_dahmer_release_never_becomes_monster_2004_anime(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            INSERT INTO titles VALUES('tt0434706','Monster','monster',2004,
                'series','JP',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0434706',1);
            INSERT INTO anidb_titles VALUES('100','monster');
            INSERT INTO anime_series_map VALUES('tt0434706','100','100',0);").unwrap();
        let wrong=search(&db,"Monster The Jeffrey Dahmer Story",None,true,1,
            "monster.the.jeffrey.dahmer.story.s01.e01",true,10).unwrap();
        assert!(wrong.is_empty(),
            "an unrelated long live-action title must not return the 2004 Monster anime");
        let exact=search(&db,"Monster",None,true,1,"Monster.S01E01",true,10)
            .unwrap();
        assert_eq!(exact.len(),1);
        assert_eq!(exact[0].imdb_id,"tt0434706",
            "the exact anime title should still match normally");
    }
    #[test] fn imdb_exact_year_and_series_cap() {
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
