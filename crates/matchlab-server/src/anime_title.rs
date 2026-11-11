//! MatchLab's anime part-title matcher and local-season resolver.
use rusqlite::{params,Connection,OptionalExtension};
use crate::{episode_crosswalk,imdb_matcher};
use std::collections::BTreeSet;

#[derive(Debug,Clone)]
struct Part {id:String}

fn aliases(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<Vec<(String,String)>>{
    crate::tantivy_search::anime_aliases_for_title(db,title,4)
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))
        .map(|found|found.into_iter().filter(|a|a.imdb_id==imdb)
            .map(|a|(a.anidb_id,a.clean_title)).collect())
}

/// An AniDB title with an offset-zero mapping names the beginning of
/// an IMDb/TVDB season. Season-relative episode coordinates may continue
/// into later AniDB parts with nonzero offsets (Stone Ocean episodes 25+).
fn begins_tvdb_season(
    db:&Connection,id:&str,season:i64,
)->rusqlite::Result<bool>{
    Ok(db.query_row("SELECT 1 FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season=?2
            AND offset=0 AND start<=1 LIMIT 1",
        params![id,season],|r|r.get::<_,i64>(0)).optional()?.is_some())
}

fn shares_tvdb_season(
    db:&Connection,base:&str,candidate:&str,season:i64,
)->rusqlite::Result<bool>{
    Ok(db.query_row("SELECT 1 FROM episode_map b
        JOIN episode_map m ON m.tvdb_id=b.tvdb_id
        WHERE b.anidb_id=?1 AND m.anidb_id=?2
          AND b.tvdb_season=?3 AND m.tvdb_season=?3
          AND b.tvdb_id<>'' LIMIT 1",
        params![base,candidate,season],|r|r.get::<_,i64>(0))
        .optional()?.is_some())
}

/// Historical IMDb episode counts can be stale or absent. Use available
/// upper bounds to reject impossible named-part episode numbers; missing
/// bounds do not authorize guessing an unrelated AniDB identity.
fn within_known_season_bounds(
    db:&Connection,imdb:&str,season:i64,episode:i64,
)->rusqlite::Result<bool>{
    let max:Option<i64>=db.query_row(
        "SELECT MAX(max_episode) FROM season_episodes
        WHERE imdb_id=?1 AND season=?2",
        params![imdb,season],|r|r.get(0)).optional()?.flatten();
    Ok(episode>0 && max.is_none_or(|end|end>=episode))
}

pub fn resolve(db:&Connection,imdb:&str,title:&str,season:i64,episode:i64)
    -> rusqlite::Result<Option<episode_crosswalk::Episode>> {
    if season<=0||episode<=0{return Ok(None);}
    let key=imdb_matcher::clean_title(title);
    if key.len()<4{return Ok(None);}
    let mut aliases=aliases(db,imdb,title)?;
    if aliases.is_empty(){return Ok(None);}
    aliases.sort_by_key(|(id,alias)|(if alias==&key{0}else{1},
        -(alias.len() as i64),id.clone(),alias.clone()));
    let best=&aliases[0];
    if aliases.iter().any(|(id,alias)|id!=&best.0&&
        (alias==&key)==(best.1==key)&&alias.len()==best.1.len()){return Ok(None);}
    let selected=&best.0;
    // Explicit S01E27 under a *named* anime part is frequently the
    // part-local first season, not season 1 of the canonical IMDb show.
    // The exact AniDB part may map to TVDB/IMDb season 5 (Stone Ocean).
    // Remap only when this specific alias has one verified season and
    // the selected IMDb title's crosswalk confirms the same AniDB ID.
    if season==1 && best.1==key
        && let Some(mapped_season)=season_from_title(db,imdb,title)?
        && mapped_season>1
        && let Some(mapped)=episode_crosswalk::resolve_season(
            db,imdb,mapped_season,episode)?
        && within_known_season_bounds(db,imdb,mapped.season,mapped.episode)?
        && (mapped.anidb_id==*selected
            || (begins_tvdb_season(db,selected,mapped_season)?
                && shares_tvdb_season(
                    db,selected,&mapped.anidb_id,mapped_season)?)) {
        return Ok(Some(mapped));
    }
    let family:Option<String>=db.query_row(
        "SELECT tvdb_id FROM episode_map WHERE anidb_id=?1 AND anidb_season>0
         AND tvdb_season>0 ORDER BY tvdb_season,offset,start LIMIT 1",
        [selected],|r|r.get(0)).optional()?;
    let Some(family)=family else{return Ok(None)};
    let base:Option<String>=db.query_row(
        "SELECT anidb_id FROM episode_map WHERE tvdb_id=?1 AND anidb_season>0
         AND tvdb_season>0 ORDER BY tvdb_season,offset,start,
         CASE WHEN anidb_id GLOB '[0-9]*' THEN CAST(anidb_id AS INTEGER)
         ELSE 2147483647 END LIMIT 1",
        [&family],|r|r.get(0)).optional()?;
    if base.as_deref()==Some(selected){
        return episode_crosswalk::resolve_season(db,imdb,season,episode);
    }
    let mut stmt=db.prepare(
        "SELECT m.anidb_id,MIN(m.tvdb_season),MIN(m.offset),MIN(m.start)
         FROM episode_map m JOIN anime_series_map l ON l.anidb_id=m.anidb_id
         WHERE l.imdb_id=?1 AND m.tvdb_id=?2 AND m.anidb_season>0 AND m.tvdb_season>0
         GROUP BY m.anidb_id ORDER BY 2,3,4,1")?;
    let parts=stmt.query_map(params![imdb,family],|r|Ok(Part{id:r.get(0)?}))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(position)=parts.iter().position(|p|&p.id==selected)else{return Ok(None)};
    let Some(target)=parts.get(position.saturating_add(season as usize-1))else{return Ok(None)};
    let mut stmt=db.prepare("SELECT tvdb_season,offset,start,end FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season>0
        ORDER BY tvdb_season,offset,start")?;
    let rows=stmt.query_map([&target.id],|r|Ok((r.get::<_,i64>(0)?,
        r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut locals=BTreeSet::new();
    for (_,_,start,end) in &rows {
        let local=if *start>1{episode+start.max(&1)-1}else{episode};
        if local>=(*start).max(1)&&(*end<=0||local<=*end){locals.insert(local);}
    }
    if locals.len()!=1{return Ok(None);}
    let local=*locals.iter().next().unwrap();
    let coordinates:BTreeSet<_>=rows.iter().filter(|(_,_,start,end)|
        local>=(*start).max(1)&&(*end<=0||local<=*end))
        .map(|(tv,offset,_,_)|(*tv,local+offset)).collect();
    if coordinates.len()!=1{return Ok(None);}
    let (tv,ep)=*coordinates.iter().next().unwrap();
    // A named anime part can use an open-ended or offset crosswalk too.
    // Refuse out-of-range AniDB episodes before creating an ASID.
    if episode_crosswalk::canonical_asid(&target.id,local).is_none()
        || !(1..=9_999).contains(&tv)
        || !(1..=99_999).contains(&ep){return Ok(None);}
    Ok(Some(episode_crosswalk::Episode{imdb_id:imdb.into(),season:tv,
        episode:ep,anidb_id:target.id.clone(),anidb_episode:local}))
}

pub fn has_specific_alias(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<bool>{
    let key=imdb_matcher::clean_title(title);
    if key.len()<4{return Ok(false);}
    let mut aliases=aliases(db,imdb,title)?;
    if aliases.is_empty(){return Ok(false);}
    aliases.sort_by_key(|(id,alias)|(if alias==&key{0}else{1},-(alias.len() as i64),id.clone()));
    let best=&aliases[0];
    if aliases.iter().any(|(id,alias)|id!=&best.0&&(alias==&key)==(best.1==key)
        &&alias.len()==best.1.len()){return Ok(true);}
    let tvdb:Option<String>=db.query_row("SELECT tvdb_id FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season>0
        ORDER BY tvdb_season,offset,start LIMIT 1",[&best.0],|r|r.get(0)).optional()?;
    let Some(tvdb)=tvdb else{return Ok(true)};
    let base:Option<String>=db.query_row("SELECT anidb_id FROM episode_map
        WHERE tvdb_id=?1 AND anidb_season>0 AND tvdb_season>0
        ORDER BY tvdb_season,offset,start LIMIT 1",[tvdb],|r|r.get(0)).optional()?;
    Ok(base.as_deref()!=Some(&best.0))
}

/// Resolve a single local episode using an exact anime *part* title when
/// no explicit Sxx season is present. Unlike absolute episode numbering,
/// "[Group] Amanchu! Advance - 01" starts again at episode 1 in this
/// part. Do not guess: the alias must identify exactly one AniDB entry
/// tied to this IMDb series and one mapped positive TVDB season.
pub fn resolve_episode_from_exact_part_title(
    db:&Connection,imdb:&str,title:&str,episode:i64,
)->rusqlite::Result<Option<episode_crosswalk::Episode>>{
    if episode<=0{return Ok(None);}
    let Some(inferred_season)=season_from_title(db,imdb,title)? else{return Ok(None)};
    let key=imdb_matcher::clean_title(title);
    let ids=aliases(db,imdb,title)?.into_iter()
        .filter_map(|(id,alias)|(alias==key).then_some(id))
        .collect::<BTreeSet<_>>();
    if ids.len()!=1{return Ok(None);}
    let named_id=ids.iter().next().unwrap();
    // A named part's TVDB/IMDb season can be 5 while its AniDB local
    // episodes start at 1 (JoJo Stone Ocean), even when the only metadata
    // row is an open-ended default. Resolve the known season directly,
    // and require the mapped AniDB ID to equal the exact named part.
    if let Some(mapped)=episode_crosswalk::resolve_season(
        db,imdb,inferred_season,episode)?
        && within_known_season_bounds(db,imdb,mapped.season,mapped.episode)?
        && (mapped.anidb_id==*named_id
            || (begins_tvdb_season(db,named_id,inferred_season)?
                && shares_tvdb_season(
                    db,named_id,&mapped.anidb_id,inferred_season)?)) {
        return Ok(Some(mapped));
    }
    // Other split-cour layouts require interpreting season=1 as the first
    // season *within* the named AniDB part. Keep the existing verified
    // split-part mapping rather than assuming every part has one season.
    // Preserve the previous resolver's eligibility and split-cour
    // behavior exactly when the new named-part shortcut cannot prove a
    // result. Some valid cours map to another AniDB ID within the family,
    // and IMDb episode counts may lag behind verified crosswalk metadata.
    let mapped=resolve(db,imdb,title,1,episode)?;
    Ok(mapped.filter(|m|m.season==inferred_season))
}

/// Infer an IMDb/TVDB season from a title-specific AniDB alias without an episode.
/// Only publish a season when one uniquely identified AniDB work maps to exactly
/// one positive TVDB season. Never manufacture an episode or an ASID.
pub fn season_from_title(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<Option<i64>>{
    let key=imdb_matcher::clean_title(title);
    if key.len()<4{return Ok(None);}
    let ids=aliases(db,imdb,title)?.into_iter().filter(|(_,alias)|alias==&key)
        .collect::<Vec<_>>();
    let unique:BTreeSet<_>=ids.into_iter().map(|(id,_)|id).collect();
    if unique.len()!=1{return Ok(None);}
    let selected=unique.iter().next().unwrap();
    let mut stmt=db.prepare("SELECT DISTINCT tvdb_season FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season>0")?;
    let seasons=stmt.query_map([selected],|r|r.get::<_,i64>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    if seasons.len()!=1{return Ok(None);}
    Ok(seasons.iter().next().copied())
}

/// Recognize a title-only anime part only when an exact alias has a unique
/// IMDb identity and a uniquely mapped positive season.
pub fn season_from_any_exact_title(db:&Connection,key:&str)->rusqlite::Result<Option<i64>>{
    if key.len()<4{return Ok(None);}
    let ids=crate::tantivy_search::anime_aliases(db,key,4)
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?
        .into_iter().filter(|a|a.clean_title==key).map(|a|a.imdb_id)
        .collect::<BTreeSet<_>>();
    if ids.len()!=1{return Ok(None);}
    let imdb=ids.iter().next().unwrap();
    season_from_title(db,imdb,key)
}

#[cfg(test)]
mod title_only_season_tests {
    use super::*;
    #[test]
    fn exact_part_alias_infers_season_but_ambiguous_alias_does_not() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            CREATE TABLE episode_map(anidb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER);
            INSERT INTO anidb_titles VALUES('101','swordartonlinealicization');
            INSERT INTO anime_series_map VALUES('101','tt0300001');
            INSERT INTO episode_map VALUES('101',1,3);").unwrap();
        assert_eq!(season_from_any_exact_title(&db,"swordartonlinealicization").unwrap(),Some(3));
        assert_eq!(season_from_title(&db,"tt0300001","Sword Art Online Alicization").unwrap(),Some(3));
        assert_eq!(season_from_any_exact_title(&db,"swordartonline").unwrap(),None);
        db.execute_batch("INSERT INTO episode_map VALUES('101',1,4);").unwrap();
        assert_eq!(season_from_any_exact_title(&db,"swordartonlinealicization").unwrap(),None);
    }
}
