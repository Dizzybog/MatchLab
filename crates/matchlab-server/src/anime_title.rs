//! Port of anime_title_matcher.py: part-specific titles and local seasons.
use rusqlite::{params,Connection,OptionalExtension};
use crate::{anime_resolver,imdb_v4};
use std::collections::BTreeSet;

#[derive(Debug,Clone)]
struct Part {id:String}

pub fn resolve(db:&Connection,imdb:&str,title:&str,season:i64,episode:i64)
    -> rusqlite::Result<Option<anime_resolver::Episode>> {
    if season<=0||episode<=0{return Ok(None);}
    let key=imdb_v4::clean_title(title);
    if key.len()<4{return Ok(None);}
    let mut stmt=db.prepare("SELECT DISTINCT a.anidb_id,a.clean_title
        FROM anidb_titles a JOIN anime_series_map l ON l.anidb_id=a.anidb_id
        WHERE l.imdb_id=?1 AND (a.clean_title=?2 OR
        (length(a.clean_title)>=4 AND ?2 LIKE a.clean_title||'%'))")?;
    let mut aliases=stmt.query_map(params![imdb,key],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if aliases.is_empty(){return Ok(None);}
    aliases.sort_by_key(|(id,alias)|(if alias==&key{0}else{1},
        -(alias.len() as i64),id.clone(),alias.clone()));
    let best=&aliases[0];
    if aliases.iter().any(|(id,alias)|id!=&best.0&&
        (alias==&key)==(best.1==key)&&alias.len()==best.1.len()){return Ok(None);}
    let selected=&best.0;
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
        return anime_resolver::resolve_season(db,imdb,season,episode);
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
    Ok(Some(anime_resolver::Episode{imdb_id:imdb.into(),season:tv,
        episode:ep,anidb_id:target.id.clone(),anidb_episode:local}))
}

pub fn has_specific_alias(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<bool>{
    let key=imdb_v4::clean_title(title);
    if key.len()<4{return Ok(false);}
    let mut stmt=db.prepare("SELECT DISTINCT a.anidb_id,a.clean_title FROM anidb_titles a
        JOIN anime_series_map l ON l.anidb_id=a.anidb_id WHERE l.imdb_id=?1 AND
        (a.clean_title=?2 OR (length(a.clean_title)>=4 AND ?2 LIKE a.clean_title||'%'))")?;
    let mut aliases=stmt.query_map(params![imdb,key],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
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
