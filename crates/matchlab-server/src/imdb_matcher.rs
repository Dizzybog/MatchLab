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
    region:Option<String>, ordering:i32, media_type:String,
}
impl From<crate::tantivy_search::TitleAlias> for Aka {
    fn from(a:crate::tantivy_search::TitleAlias)->Self{
        Self{imdb_id:a.imdb_id,title:a.title,clean:a.clean,year:a.year,
            category:a.category,region:a.region,ordering:a.ordering,
            media_type:a.media_type}
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
/// Prefer exact or complete-word edge alignment over raw alias length.
#[derive(Clone,Copy,Debug,PartialEq,Eq,PartialOrd,Ord)]
struct TitleFit{
    tier:u8,               // exact/country, full-word edge, last-resort fallback
    alias_exact:u8,        // exact title.basics entry before exact AKA, if undated
    unmatched_bp:u32,     // relative unmatched share (basis points)
    edge:u8,              // prefix before suffix only when coverage is equal
    longest_first:i64,    // length only breaks otherwise equivalent ties
}
impl TitleFit{
    fn exact()->Self{Self{tier:0,alias_exact:0,unmatched_bp:0,edge:0,longest_first:0}}
    fn exact_aka()->Self{Self{tier:0,alias_exact:1,unmatched_bp:0,edge:0,longest_first:0}}
    fn fallback(length:usize)->Self{
        Self{tier:2,alias_exact:0,unmatched_bp:0,edge:0,longest_first:-(length as i64)}
    }
}
fn title_fit(key:&str,words:&[String],release_words:&[String],row:&Aka,
    prefer_primary:bool)->Option<TitleFit>{
    if row.clean==key{
        // An IMDb AKA can abbreviate a different work to the same short
        // title as an established series (Bleach 2004 vs TYBW 2022).
        // Match the full title.basics identity before the abbreviated AKA
        // only for undated, country-unqualified releases. An explicit year
        // or country still disambiguates using the established ranking.
        let from_basics=row.ordering==0 && row.region.as_deref().unwrap_or("").is_empty();
        return Some(if prefer_primary && !from_basics {
            TitleFit::exact_aka()
        }else{TitleFit::exact()});
    }
    let short=key.len().min(row.clean.len());
    let long=key.len().max(row.clean.len());
    if short<4||short.saturating_mul(2)<long||long-short>16{return None;}
    let unmatched_bp=((long-short) as u64*10_000/long as u64) as u32;
    // The parser can treat an immediately trailing number as technical
    // metadata even when it is part of an IMDb title: Televisionseries1.
    // Reattach ONLY a complete ASCII numeric suffix already present in the
    // source release (checked by filter). Never accept a partial word.
    if row.clean.strip_prefix(key).is_some_and(|tail|
        !tail.is_empty() && tail.bytes().all(|b|b.is_ascii_digit())){
        return Some(TitleFit{tier:1,alias_exact:0,unmatched_bp,edge:0,
            longest_first:-(short as i64)});
    }
    let alias_words=crate::title_normalization::clean_words(&row.title);
    if words.is_empty()||alias_words.is_empty(){return None;}
    // The parser sometimes consumes part of a *real* title as release
    // metadata: Televisionseries1 -> Television. Restore a longer alias
    // only when its FULL normalized words occur in the original release.
    // A character-only prefix (Dune vs Duneland) is never sufficient.
    if row.clean.starts_with(key)
        && release_words.windows(alias_words.len())
            .any(|window|window==alias_words.as_slice()){
        return Some(TitleFit{tier:1,alias_exact:0,unmatched_bp,edge:0,
            longest_first:-(short as i64)});
    }
    let prefix=words.starts_with(&alias_words)||alias_words.starts_with(words);
    let suffix=words.ends_with(&alias_words)||alias_words.ends_with(words);
    if !prefix&&!suffix{return None;}
    Some(TitleFit{tier:1,alias_exact:0,unmatched_bp,edge:if prefix{0}else{1},
        longest_first:-(short as i64)})
}

/// Title relevance is ranked by the caller. Then: requested-year distance,
/// IMDb votes before region (unless a country is explicitly supplied), and
/// earliest candidate year only after every other semantic criterion ties.
/// IMDb ID provides deterministic output for otherwise identical candidates.
fn rank(r:&Aka,year:Option<i32>,duplicate:bool,country:Option<&str>,votes:i64)
    -> (i64,i32,i64,i32,i32,String) {
    let distance=year.map_or(0,|y|(i64::from(r.year)-i64::from(y)).abs());
    // IMDb uses GB for UK AKA regions, although our explicit country
    // release marker uses UK. Region matters only for duplicate titles.
    let preferred=country.map(|code|if code=="UK"{"GB"}else{code});
    let region=if !duplicate
        || r.region.as_deref()==Some(preferred.unwrap_or("US")){0}else{1};
    // With an explicit country, region takes precedence over popularity.
    // Without one, popularity leads and legacy US preference is a fallback.
    let (region_before_votes,region_after_votes)=
        if country.is_some(){(region,0)}else{(0,region)};
    // Unknown IMDb year (0) is not the "earliest" known year.
    let earliest_year=if r.year>0{r.year}else{i32::MAX};
    (distance,region_before_votes,-votes,region_after_votes,
        earliest_year,r.imdb_id.clone())
}
pub fn search(db:&Connection,title:&str,year:Option<i32>,series:bool,required_season:i64,
    release_title:&str,_has_episode:bool,limit:usize)->rusqlite::Result<Vec<Match>>{
    let key=clean_title(title);
    let release=clean_title(release_title);
    if key.is_empty()||limit==0{return Ok(vec![]);}
    // Most lookups resolve exactly: avoid two extra Unicode folds and word
    // vector allocations on that hot path.
    let mut words=Vec::new();
    let mut release_words=Vec::new();
    let mut stage="aligned";
    let mut rows=Vec::new();
    // Detect explicit country qualifiers once. Reuse the same detection for
    // canonical country-title aliases and the regional tie-breaker.
    let country_qualified=crate::country_aliases::qualified(release_title);
    let country=country_qualified.as_ref().map(|(code,_)|*code);
    let prefer_primary=year.is_none() && country.is_none();
    // Exact country-qualified aliases precede generic prefix matching.
    if series {
        if let Some((_,aliases))=&country_qualified {
            for candidate in aliases {
                let found=exact(db,candidate)?;
                for row in found {
                    if is_series(&row.media_type) && eligible(db,&row,required_season)? {
                        rows.push(row);
                    }
                }
            }
        }
        if !rows.is_empty(){stage="country_exact";}
    }
    if rows.is_empty(){
        // Exact aliases are authoritative and fast, including "Up" and "It".
        rows=filter(db,exact(db,&key)?,&release,series,year,required_season)?;
        if rows.is_empty()&&key.len()>=4{
            words=crate::title_normalization::clean_words(title);
            release_words=crate::title_normalization::clean_words(release_title);
            // Compare forward, reverse and embedded edge matches together;
            // never let the first nonempty strategy decide the winning title.
            let mut discovered=tantivy_aliases(db,&key,false)?;
            if key.len()>=8{
                discovered.extend(tantivy_aliases(db,&key,true)?);
            }
            discovered.extend(crate::tantivy_search::title_aliases_inside(db,&key)
                .map_err(index_error)?.into_iter().map(Aka::from));
            // Avoid eligibility SQL joins for candidates that only match
            // fragments of a word (e.g. Dune vs Duneland).
            discovered.retain(|row|title_fit(&key,&words,&release_words,row,prefer_primary).is_some());
            rows=filter(db,discovered,&release,series,year,required_season)?;
        }
    }
    // Do not fall back to a half-word prefix: such matches are not
    // independently meaningful, and caused short titles to hijack long ones.
    if rows.is_empty()&&series&&key.len()>=4{
        // IMDb's English primary title need not appear in the release at
        // all: e.g. Shinigami Bocchan to Kuro Maid is indexed under its
        // English title. The exact/longest AniDB alias provides identity;
        // requiring release.contains(imdb_alias) silently rejects it.
        // Search via the existing Tantivy anime index, not SQLite title scans.
        let anime_aliases=crate::tantivy_search::anime_aliases_for_title(db,title,4)
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
    // Keep all plausible identities until ranked. Unrelated word fragments
    // are rejected before season eligibility and expensive ID lookups.
    let mut groups:BTreeMap<String,Vec<(Aka,TitleFit)>>=BTreeMap::new();
    for row in rows{
        let fit=if stage=="country_exact"{TitleFit::exact()}
            else if stage=="aligned"{
                title_fit(&key,&words,&release_words,&row,prefer_primary)
                    .unwrap_or(TitleFit::fallback(row.clean.len()))
            }else{TitleFit::fallback(row.clean.len())};
        groups.entry(row.imdb_id.clone()).or_default().push((row,fit));
    }
    // Title fit is primary (including canonical vs abbreviated AKA for
    // undated searches), then year distance, conditional country preference
    // and votes, followed by earliest year as the final semantic tie-breaker.
    // Candidates below the lexical rank at `limit` cannot be returned.
    // Retain the entire tied boundary for the remaining criteria.
    // This avoids most per-IMDb rating, anime-ID and duplicate-title lookups.
    let mut ranked_groups=groups.into_iter().map(|(id,aliases)|{
        let best_fit=aliases.iter().map(|(_,fit)|*fit).min().unwrap();
        (best_fit,id,aliases)
    }).collect::<Vec<_>>();
    ranked_groups.sort_by_key(|(fit,_,_)|*fit);
    if ranked_groups.len()>limit {
        let cutoff=ranked_groups[limit-1].0;
        ranked_groups.retain(|(fit,_,_)|*fit<=cutoff);
    }
    let mut duplicates=HashSet::new();
    let mut checked=HashSet::new();
    for (_,_,aliases) in &ranked_groups{
        for (r,_) in aliases {
            if checked.insert(r.clean.clone())
                && crate::tantivy_search::has_duplicate_title(db,&r.clean)
                    .map_err(index_error)? {
                duplicates.insert(r.clean.clone());
            }
        }
    }
    // Missing release year means zero distance for every eligible work;
    // votes precede region unless the release explicitly names a country.
    // Season/episode tokens never alter this ranking order.
    let mut output=Vec::new();
    for (_,id,aliases) in ranked_groups{
        let votes:i64=db.query_row("SELECT num_votes FROM imdb_ratings WHERE imdb_id=?1",[&id],|r|r.get(0))
            .optional().unwrap_or(None).unwrap_or(0);
        let (best,fit)=aliases.into_iter().min_by_key(|(r,fit)|
            (*fit,rank(r,year,duplicates.contains(&r.clean),country,votes))
        ).unwrap();
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
        let score=(fit,rank(&best,year,duplicates.contains(&best.clean),country,votes));
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
    let exact_anime_ids=crate::tantivy_search::anime_aliases_for_title(db,title,4)
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
        let found=search(&db,"Example",Some(2020),false,0,"Example.2020",false,1).unwrap();
        assert_eq!(found.len(),1);
        assert_eq!(found[0].imdb_id,"tthigh",
            "lexical cutoff must retain all tied identities for vote ranking");
    }
    #[test]
    fn votes_precede_year_when_release_year_is_missing_even_with_episode_coordinates(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES
                ('ttold','Example','example',1993,'series','US',1,0,'tvSeries'),
                ('ttnew','Example','example',2012,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttold',5),('ttnew',5);
            INSERT INTO imdb_ratings VALUES('ttold',100),('ttnew',90000);")
            .unwrap();
        // A missing year must choose popularity regardless of whether the
        // parsed torrent has a season, an episode, both, or neither.
        for (season,has_episode,release) in [
            (0,false,"Example"),
            (0,true,"Example.E37"),
            (2,false,"Example.S02"),
            (2,true,"Example.S02E37"),
        ] {
            let found=search(&db,"Example",None,true,season,release,has_episode,1).unwrap();
            assert_eq!(found.len(),1,"release: {release}");
            assert_eq!(found[0].imdb_id,"ttnew","release: {release}");
        }
        // An explicit year is still authoritative ahead of popularity.
        let with_year=search(&db,"Example",Some(1993),true,2,
            "Example.1993.S02E37",true,1).unwrap();
        assert_eq!(with_year[0].imdb_id,"ttold");
        // Eligibility continues to outrank popularity even with no year.
        db.execute("UPDATE seasons SET max_season=1 WHERE imdb_id='ttnew'",[])
            .unwrap();
        let eligible=search(&db,"Example",None,true,4,
            "Example.S04E37",true,1).unwrap();
        assert_eq!(eligible[0].imdb_id,"ttold");
    }
    #[test]
    fn ranking_orders_year_votes_and_region_conditionally(){
        let candidate=|id:&str,year:i32,region:&str|{
            Aka{imdb_id:id.into(),title:"Example".into(),clean:"example".into(),
                year,category:"movie".into(),region:Some(region.into()),
                media_type:"movie".into()}
        };
        let us_old=candidate("ttus",2018,"US");
        let no_exact=candidate("ttno",2020,"NO");
        let no_old=candidate("ttno",2018,"NO");
        // An explicit release year always beats popularity and region.
        assert!(rank(&no_exact,Some(2020),true,None,1)
            <rank(&us_old,Some(2020),true,None,1_000_000));
        // No country qualifier: votes are ahead of the region preference.
        assert!(rank(&no_old,Some(2018),true,None,1_000_000)
            <rank(&us_old,Some(2018),true,None,1));
        assert!(rank(&no_old,None,true,None,1_000_000)
            <rank(&us_old,None,true,None,1));
        // Explicit country qualifier: prefer the requested region first.
        assert!(rank(&us_old,Some(2018),true,Some("US"),1)
            <rank(&no_old,Some(2018),true,Some("US"),1_000_000));
        assert!(rank(&no_old,None,true,Some("NO"),1)
            <rank(&us_old,None,true,Some("NO"),1_000_000));
        // British releases use IMDb's GB regional title code.
        let british=candidate("ttgb",2018,"GB");
        assert!(rank(&british,None,true,Some("UK"),1)
            <rank(&us_old,None,true,Some("UK"),1_000_000));
        // If region is not applicable, votes alone lead.
        assert!(rank(&no_old,Some(2018),false,None,2)
            <rank(&us_old,Some(2018),false,None,1));
        // A complete vote/region/year-distance tie favors the earliest year.
        assert!(rank(&us_old,None,true,None,100)
            <rank(&candidate("ttnew",2020,"US"),None,true,None,100));
        // The same applies when two years are equally close to the given year.
        assert!(rank(&us_old,Some(2019),true,None,100)
            <rank(&candidate("ttnew",2020,"US"),Some(2019),true,None,100));
        // An unknown metadata year is not preferred as the earliest known.
        assert!(rank(&us_old,None,true,None,100)
            <rank(&candidate("ttmissing",0,"US"),None,true,None,100));
        // IMDb ID makes exact ties deterministic, but is not a semantic score.
        assert!(rank(&candidate("ttaaa",2020,"US"),None,true,None,100)
            <rank(&candidate("ttbbb",2020,"US"),None,true,None,100));
    }

    #[test]
    fn explicit_country_changes_votes_vs_region_order_for_duplicate_titles(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttus','Example','example',2020,'movie','US',0,99,'movie'),
                ('ttno','Example','example',2020,'movie','NO',1,0,'tvMovie'),
                ('ttgb','Example','example',2020,'movie','GB',1,0,'movie');
            INSERT INTO imdb_ratings VALUES('ttus',10),('ttno',100000),('ttgb',1000);")
            .unwrap();
        for specified_year in [None,Some(2020)] {
            // Neither a region marker nor IMDb's original-title/AKA ordering
            // is allowed to overrule a more popular candidate.
            let found=search(&db,"Example",specified_year,false,0,
                "Example.1080p",false,3).unwrap();
            assert_eq!(found.iter().map(|m|m.imdb_id.as_str())
                .collect::<Vec<_>>(),vec!["ttno","ttgb","ttus"]);
            // Country explicitly supplied: requested region wins before votes.
            let us=search(&db,"Example",specified_year,false,0,
                "Example.US.1080p",false,3).unwrap();
            assert_eq!(us[0].imdb_id,"ttus");
            let gb=search(&db,"Example",specified_year,false,0,
                "Example.UK.1080p",false,3).unwrap();
            assert_eq!(gb[0].imdb_id,"ttgb");
            let no=search(&db,"Example",specified_year,false,0,
                "Example.Norge.1080p",false,3).unwrap();
            assert_eq!(no[0].imdb_id,"ttno");
        }
    }

    #[test]
    fn earliest_year_breaks_complete_vote_region_and_year_distance_ties(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttold','Example','example',2018,'movie','US',1,4,'movie'),
                ('ttnew','Example','example',2020,'movie','US',0,0,'movie');
            INSERT INTO imdb_ratings VALUES('ttold',100),('ttnew',100);")
            .unwrap();
        let old=search(&db,"Example",None,false,0,"Example.1080p",false,2)
            .unwrap();
        assert_eq!(old[0].imdb_id,"ttold");
        // The release year 2019 is equidistant from 2018 and 2020.
        let equidistant=search(&db,"Example",Some(2019),false,0,
            "Example.2019",false,2).unwrap();
        assert_eq!(equidistant[0].imdb_id,"ttold");
        // More votes still outrank the earliest candidate year.
        db.execute("UPDATE imdb_ratings SET num_votes=101 WHERE imdb_id='ttnew'",[])
            .unwrap();
        let popular=search(&db,"Example",None,false,0,
            "Example.1080p",false,2).unwrap();
        assert_eq!(popular[0].imdb_id,"ttnew");
    }

    #[test]
    fn votes_break_equal_movie_type_and_title_alias_ties(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttmovie','Same','same',2020,'movie','US',1,0,'movie'),
                ('tttv','Same','same',2020,'movie','US',0,99,'tvMovie');
            INSERT INTO imdb_ratings VALUES('ttmovie',10),('tttv',10000);")
            .unwrap();
        let found=search(&db,"Same",Some(2020),false,0,
            "Same.2020",false,1).unwrap();
        assert_eq!(found[0].imdb_id,"tttv",
            "media type, original flag and alias order are not ranking factors");
    }
    #[test]
    fn bleach_s01_uses_canonical_title_not_shortened_sequel_aka(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES
                ('tt0434665','Bleach','bleach',2004,'series','',0,0,'tvSeries'),
                ('tt14986406','Bleach: Thousand-Year Blood War',
                    'bleachthousandyearbloodwar',2022,'series','',0,0,'tvSeries'),
                ('tt14986406','Bleach','bleach',2022,'series','US',0,42,'tvSeries');
            INSERT INTO imdb_ratings VALUES('tt0434665',90000),('tt14986406',120000);
            INSERT INTO seasons VALUES('tt0434665',16),('tt14986406',4);")
            .unwrap();
        // IMDb's sequel has more votes, but 'Bleach' is only an abbreviated
        // AKA of the sequel and is the main title of the 2004 series.
        for release in ["bleach.s1","Bleach.S01","Bleach.S01E01"] {
            let hits=search(&db,"Bleach",None,true,1,release,
                release.contains('E'),1).unwrap();
            assert_eq!(hits[0].imdb_id,"tt0434665","release: {release}");
        }
        // A full sequel title still resolves to the sequel.
        let sequel=search(&db,"Bleach Thousand Year Blood War",None,true,1,
            "Bleach.Thousand.Year.Blood.War.S01",false,1).unwrap();
        assert_eq!(sequel[0].imdb_id,"tt14986406");
        // An explicitly supplied production year must remain authoritative.
        let year=search(&db,"Bleach",Some(2022),true,1,
            "Bleach.2022.S01",false,1).unwrap();
        assert_eq!(year[0].imdb_id,"tt14986406");
        // The older series still survives the 1-season import tolerance;
        // the newer series must not be selected for the older show's S17.
        let season=search(&db,"Bleach",None,true,17,
            "Bleach.S17E01",true,1).unwrap();
        assert_eq!(season[0].imdb_id,"tt0434665");
    }

    #[test]
    fn equal_primary_titles_still_choose_imdb_votes(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1984','Dune','dune',1984,'movie','',0,0,'movie'),
                ('tt2021','Dune','dune',2021,'movie','',0,0,'movie');
            INSERT INTO imdb_ratings VALUES('tt1984',2000),('tt2021',200000);")
            .unwrap();
        let hits=search(&db,"Dune",None,false,0,
            "Dune.1080p",false,1).unwrap();
        assert_eq!(hits[0].imdb_id,"tt2021",
            "popularity must win when both candidates actually share the primary title");
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
    #[test]
    fn lexical_alignment_ranks_whole_word_prefix_over_equal_suffix(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Alpha Beta','alphabeta',2021,'movie','US',1,0,'movie'),
                ('tt2222222','Beta Gamma','betagamma',2021,'movie','US',1,0,'movie');
            INSERT INTO imdb_ratings VALUES('tt1111111',1),('tt2222222',90000);")
            .unwrap();
        let found=search(&db,"Alpha Beta Gamma",None,false,0,
            "Alpha.Beta.Gamma.2021.WEB-DL",false,10).unwrap();
        assert_eq!(found.len(),2);
        assert_eq!(found[0].imdb_id,"tt1111111",
            "equal-coverage whole-word prefix must beat suffix, not popularity");
    }
    #[test]
    fn title_number_mistaken_for_parser_suffix_is_preserved(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES
                ('tt0000123','Televisionseries1','televisionseries1',2020,
                 'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0000123',10);").unwrap();
        for title in ["Televisionseries","Television"] {
            let hits=search(&db,title,None,true,1,
                "Televisionseries1.S01E02.1080p.WEB-DL",true,10).unwrap();
            assert_eq!(hits[0].imdb_id,"tt0000123","parser title: {title}");
        }
    }
    #[test]
    fn suffix_of_release_title_is_discovered_without_reverse_fallback(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Beyond Dune','beyonddune',2021,'movie','US',1,0,'movie'),
                ('tt2222222','Mystery','mystery',2021,'movie','US',1,0,'movie');")
            .unwrap();
        let found=search(&db,"Mystery Beyond Dune",None,false,0,
            "Mystery.Beyond.Dune.2021.1080p",false,10).unwrap();
        assert_eq!(found.len(),1);
        assert_eq!(found[0].imdb_id,"tt1111111");
    }
    #[test]
    fn word_fragments_and_short_generic_titles_are_not_identity(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Dune','dune',2021,'movie','US',1,0,'movie'),
                ('tt2222222','Monster','monster',2004,'movie','US',1,0,'movie');")
            .unwrap();
        assert!(search(&db,"Duneland",None,false,0,"Duneland.2026",false,10)
            .unwrap().is_empty(),"a substring of a word cannot be an alias");
        assert!(search(&db,"Monster The Jeffrey Dahmer Story",None,false,0,
            "Monster.The.Jeffrey.Dahmer.Story",false,10).unwrap().is_empty(),
            "a short generic prefix cannot override an unrelated longer title");
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
