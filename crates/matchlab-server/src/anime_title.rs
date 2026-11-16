//! MatchLab's anime part-title matcher and local-season resolver.
use rusqlite::{params,Connection,OptionalExtension};
use crate::{episode_crosswalk,imdb_matcher};
use std::collections::BTreeSet;

#[derive(Debug,Clone)]
struct Part {id:String}

fn aliases(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<Vec<(String,String)>>{
    let aliases=if crate::strict_match_mode(){
        crate::tantivy_search::anime_aliases_exact(db,title)
    }else{crate::tantivy_search::anime_aliases_for_title(db,title,4)};
    aliases.map_err(|error|rusqlite::Error::ToSqlConversionFailure(
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

/// Resolve a batch of named-anime-part episodes from one immutable SQLite
/// snapshot. Unlike repeated resolve() calls, alias selection, part ordering,
/// crosswalk links and episode-map rows are loaded at most once per range.
/// All tie/ambiguity and boundedness checks remain episode-specific.
pub fn resolve_range(
    db:&Connection,imdb:&str,title:&str,season:i64,start:i64,end:i64,
)->rusqlite::Result<Vec<Option<episode_crosswalk::Episode>>>{
    resolve_range_inner(db,imdb,title,season,start,end,false)
}

/// Only local S/P markers use this path. A family-first AniDB work may
/// refer to its second cour while all cours still occupy one IMDb season.
/// The ordinary resolver deliberately retains its global-season shortcut.
pub fn resolve_local_part_range(
    db:&Connection,imdb:&str,title:&str,season:i64,start:i64,end:i64,
)->rusqlite::Result<Vec<Option<episode_crosswalk::Episode>>>{
    resolve_range_inner(db,imdb,title,season,start,end,true)
}

fn resolve_range_inner(
    db:&Connection,imdb:&str,title:&str,season:i64,start:i64,end:i64,
    force_local:bool,
)->rusqlite::Result<Vec<Option<episode_crosswalk::Episode>>>{
    if !(1..=9_999).contains(&season)||!(1..=99_999).contains(&start)
        ||!(start..=99_999).contains(&end){return Ok(Vec::new());}
    // Preserve very large interval behavior without allocating an unbounded
    // batch context. Ordinary packs use the optimized shared-snapshot path.
    if end-start>=1_000 {
        return (start..=end).map(|episode|
            resolve_range_inner(db,imdb,title,season,episode,episode,force_local)
                .map(|episodes|episodes.into_iter().next().flatten())).collect();
    }
    let mut results=vec![None;(end-start+1) as usize];
    let key=imdb_matcher::clean_title(title);
    if key.is_empty()||(!crate::strict_match_mode()&&key.len()<4){
        return Ok(results);
    }
    let mut title_aliases=aliases(db,imdb,title)?;
    if title_aliases.is_empty(){return Ok(results);}
    title_aliases.sort_by_key(|(id,alias)|(if alias==&key{0}else{1},
        -(alias.len() as i64),id.clone(),alias.clone()));
    let best=&title_aliases[0];
    if title_aliases.iter().any(|(id,alias)|id!=&best.0
        &&(alias==&key)==(best.1==key)&&alias.len()==best.1.len()){
        return Ok(results);
    }
    let selected=&best.0;

    // A named AniDB part may begin an IMDb season other than S01 (JoJo).
    // Preload its whole crosswalk once, but keep every episode's independent
    // eligibility and ambiguous-mapping check.
    if best.1==key
        && let Some(mapped_season)=season_from_title(db,imdb,title)?
        && mapped_season>1
        && (season==1 || (!force_local && season==mapped_season)) {
        let mapped=episode_crosswalk::resolve_season_range(
            db,imdb,mapped_season,start,end)?;
        let mut bounds=std::collections::HashMap::<i64,Option<i64>>::new();
        let mut shared=std::collections::HashMap::<String,bool>::new();
        let mut begins:Option<bool>=None;
        for (slot,candidate) in results.iter_mut().zip(mapped) {
            let Some(candidate)=candidate else{continue};
            let upper=if let Some(upper)=bounds.get(&candidate.season){
                *upper
            }else{
                let upper:Option<i64>=db.query_row(
                    "SELECT MAX(max_episode) FROM season_episodes
                     WHERE imdb_id=?1 AND season=?2",
                    params![imdb,candidate.season],|row|row.get(0))
                    .optional()?.flatten();
                bounds.insert(candidate.season,upper);
                upper
            };
            if upper.is_some_and(|max|candidate.episode>max){continue;}
            let valid=if candidate.anidb_id==*selected {true}else{
                let begins=if let Some(value)=begins {value}else{
                    let value=begins_tvdb_season(db,selected,mapped_season)?;
                    begins=Some(value);
                    value
                };
                if !begins {false}else if let Some(&value)=shared.get(&candidate.anidb_id){
                    value
                }else{
                    let value=shares_tvdb_season(
                        db,selected,&candidate.anidb_id,mapped_season)?;
                    shared.insert(candidate.anidb_id.clone(),value);
                    value
                }
            };
            if valid{*slot=Some(candidate);}
        }
        // Explicit S03 on a part that starts in global S03 stays S03 for
        // the complete interval, just as it does for a single episode.
        // A hole or an out-of-bounds endpoint must not fall through to the
        // third local part. P3 still uses force_local and retains its meaning.
        if (!force_local && season==mapped_season)
            || results.iter().all(Option::is_some){return Ok(results);}
    }

    let family:Option<String>=db.query_row(
        "SELECT tvdb_id FROM episode_map WHERE anidb_id=?1 AND anidb_season>0
         AND tvdb_season>0 ORDER BY tvdb_season,offset,start LIMIT 1",
        [selected],|r|r.get(0)).optional()?;
    let Some(family)=family else{return Ok(results)};
    let base:Option<String>=db.query_row(
        "SELECT anidb_id FROM episode_map WHERE tvdb_id=?1 AND anidb_season>0
         AND tvdb_season>0 ORDER BY tvdb_season,offset,start,
         CASE WHEN anidb_id GLOB '[0-9]*' THEN CAST(anidb_id AS INTEGER)
         ELSE 2147483647 END LIMIT 1",
        [&family],|r|r.get(0)).optional()?;
    if !force_local && base.as_deref()==Some(selected){
        let fallback=episode_crosswalk::resolve_season_range(db,imdb,season,start,end)?;
        for (slot,mapped) in results.iter_mut().zip(fallback){
            if slot.is_none(){*slot=mapped;}
        }
        return Ok(results);
    }
    let mut stmt=db.prepare(
        "SELECT m.anidb_id,MIN(m.tvdb_season),MIN(m.offset),MIN(m.start)
         FROM episode_map m JOIN anime_series_map l ON l.anidb_id=m.anidb_id
         WHERE l.imdb_id=?1 AND m.tvdb_id=?2 AND m.anidb_season>0 AND m.tvdb_season>0
         GROUP BY m.anidb_id ORDER BY 2,3,4,1")?;
    let parts=stmt.query_map(params![imdb,family],|r|Ok(Part{id:r.get(0)?}))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(position)=parts.iter().position(|p|&p.id==selected)else{
        return Ok(results);
    };
    let Some(target)=parts.get(position.saturating_add(season as usize-1))else{
        return Ok(results);
    };
    let mut stmt=db.prepare("SELECT tvdb_season,offset,start,end FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season>0
        ORDER BY tvdb_season,offset,start")?;
    let rows=stmt.query_map([&target.id],|r|Ok((r.get::<_,i64>(0)?,
        r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (index,slot) in results.iter_mut().enumerate(){
        if slot.is_some(){continue;}
        let episode=start+index as i64;
        let mut locals=BTreeSet::new();
        for (_,_,range_start,range_end) in &rows {
            let local=if *range_start>1{
                let Some(local)=episode.checked_add(range_start-1) else{continue;};
                local
            }else{episode};
            if local>=(*range_start).max(1)
                &&(*range_end<=0||local<=*range_end){locals.insert(local);}
        }
        if locals.len()!=1{continue;}
        let local=*locals.iter().next().unwrap();
        let coordinates:BTreeSet<_>=rows.iter().filter(|(_,_,s,e)|
            local>=(*s).max(1)&&(*e<=0||local<=*e))
            .filter_map(|(tv,offset,_,_)|
                local.checked_add(*offset).map(|episode|(*tv,episode))).collect();
        if coordinates.len()!=1{continue;}
        let (tv,ep)=*coordinates.iter().next().unwrap();
        if episode_crosswalk::canonical_asid(&target.id,local).is_none()
            ||!(1..=9_999).contains(&tv)||!(1..=99_999).contains(&ep){
            continue;
        }
        *slot=Some(episode_crosswalk::Episode{
            imdb_id:imdb.into(),season:tv,episode:ep,
            anidb_id:target.id.clone(),anidb_episode:local,
        });
    }
    Ok(results)
}

pub fn resolve(db:&Connection,imdb:&str,title:&str,season:i64,episode:i64)
    ->rusqlite::Result<Option<episode_crosswalk::Episode>>{
    Ok(resolve_range(db,imdb,title,season,episode,episode)?
        .into_iter().next().flatten())
}

pub fn has_specific_alias(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<bool>{
    let key=imdb_matcher::clean_title(title);
    if key.is_empty()||(!crate::strict_match_mode()&&key.len()<4){
        return Ok(false);
    }
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

/// The single AniDB work of this IMDb series that the title names exactly.
pub fn exact_alias_work(
    db:&Connection,imdb:&str,title:&str,
)->rusqlite::Result<Option<String>>{
    let key=imdb_matcher::clean_title(title);
    if key.is_empty(){return Ok(None);}
    let ids=aliases(db,imdb,title)?.into_iter()
        .filter_map(|(id,alias)|(alias==key).then_some(id))
        .collect::<BTreeSet<_>>();
    Ok(if ids.len()==1{ids.into_iter().next()}else{None})
}

/// Does an exact numbered alias ("Title 2", "Title Part 2") name IMDb season
/// `season`? True when its unique AniDB work is mapped to that season, or
/// when it is a later cour of the previous TVDB season and IMDb ends that
/// previous season exactly where the cour begins (IMDb split the cours into
/// seasons). An alias without one uniquely mapped season is left to the
/// caller's own bounds check.
pub fn numbered_alias_names_season(
    db:&Connection,imdb:&str,title:&str,season:i64,
)->rusqlite::Result<bool>{
    let key=imdb_matcher::clean_title(title);
    let ids=aliases(db,imdb,title)?.into_iter()
        .filter_map(|(id,alias)|(alias==key).then_some(id))
        .collect::<BTreeSet<_>>();
    if ids.len()!=1{return Ok(true);}
    let id=ids.iter().next().expect("unique AniDB alias");
    let Some(mapped)=unique_mapped_season(db,id)? else{return Ok(true)};
    if mapped==season{return Ok(true);}
    if mapped+1!=season{return Ok(false);}
    Ok(imdb_split_offset(db,imdb,id,mapped)?.is_some())
}

/// The cour's positive episode offset inside TVDB season `mapped`, when
/// IMDb's season of that number ends exactly at this offset.
pub(crate) fn imdb_split_offset(
    db:&Connection,imdb:&str,anidb:&str,mapped:i64,
)->rusqlite::Result<Option<i64>>{
    let offset:Option<i64>=db.query_row("SELECT MIN(offset) FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season=?2 AND map=''",
        params![anidb,mapped],|row|row.get(0)).optional()?.flatten();
    let previous_end:Option<i64>=db.query_row(
        "SELECT MAX(max_episode) FROM season_episodes WHERE imdb_id=?1 AND season=?2",
        params![imdb,mapped],|row|row.get(0)).optional()?.flatten();
    Ok(offset.filter(|offset|*offset>0 && previous_end==Some(*offset)))
}

/// Resolve "<first part title> Part N - EE". The base title must be an exact
/// alias of ONE AniDB work that begins a TVDB season (offset 0); part N is
/// the N-th AniDB work of this IMDb series in that same season, ordered by
/// its episode offset. The episode is local to the part ("Part 2 - 05") or
/// continues the season's numbering ("Part 2 - 25" when part two is 13..25).
/// Part lengths come from the next part's offset or IMDb's season bound;
/// without a bound nothing is published.
pub fn resolve_numbered_part(
    db:&Connection,imdb:&str,base_title:&str,part:i64,episode:i64,
)->rusqlite::Result<Option<episode_crosswalk::Episode>>{
    if !(2..=9).contains(&part)||!(1..=9_999).contains(&episode){return Ok(None);}
    let key=imdb_matcher::clean_title(base_title);
    let ids=aliases(db,imdb,base_title)?.into_iter()
        .filter_map(|(id,alias)|(alias==key).then_some(id))
        .collect::<BTreeSet<_>>();
    if ids.len()!=1{return Ok(None);}
    let first=ids.iter().next().expect("unique AniDB alias");
    let Some(season)=unique_mapped_season(db,first)? else{return Ok(None)};
    if !begins_tvdb_season(db,first,season)?{return Ok(None);}
    let mut stmt=db.prepare("SELECT DISTINCT m.anidb_id,m.offset FROM episode_map base
        JOIN episode_map m ON m.tvdb_id=base.tvdb_id AND m.tvdb_season=base.tvdb_season
        JOIN anime_series_map linked ON linked.anidb_id=m.anidb_id
        WHERE base.anidb_id=?1 AND base.tvdb_season=?2 AND base.tvdb_id<>''
          AND m.anidb_season>0 AND m.map='' AND linked.imdb_id=?3
        ORDER BY m.offset,m.anidb_id")?;
    let parts=stmt.query_map(params![first,season,imdb],|row|
        Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Every part needs its own distinct, increasing offset.
    if parts.first().is_none_or(|(id,offset)|id!=first||*offset!=0)
        || parts.windows(2).any(|pair|pair[0].1>=pair[1].1){return Ok(None);}
    let Some((anidb,offset))=parts.get((part-1) as usize) else{return Ok(None)};
    let season_end:Option<i64>=db.query_row(
        "SELECT MAX(max_episode) FROM season_episodes WHERE imdb_id=?1 AND season=?2",
        params![imdb,season],|row|row.get(0)).optional()?.flatten();
    // IMDb may end this season exactly where the part begins and list the
    // part as its own next season; its local numbering then restarts at 1.
    if *offset>0 && imdb_split_offset(db,imdb,anidb,season)?==Some(*offset) {
        let next_end:Option<i64>=db.query_row(
            "SELECT MAX(max_episode) FROM season_episodes WHERE imdb_id=?1 AND season=?2",
            params![imdb,season+1],|row|row.get(0)).optional()?.flatten();
        let Some(next_end)=next_end else{return Ok(None)};
        let local=if episode<=next_end{episode}
            else if episode>*offset && episode-offset<=next_end{episode-offset}
            else{return Ok(None)};
        return Ok(Some(episode_crosswalk::Episode{imdb_id:imdb.to_owned(),
            season:season+1,episode:local,anidb_id:anidb.clone(),anidb_episode:local}));
    }
    let end=match parts.get(part as usize){
        Some((_,next))=>*next,
        None=>match season_end{Some(end)=>end,None=>return Ok(None)},
    };
    let length=end-offset;
    if length<=0{return Ok(None);}
    let (tv,local)=if episode<=length{(episode+offset,episode)}
        else if episode>*offset && episode<=end{(episode,episode-offset)}
        else{return Ok(None)};
    if season_end.is_some_and(|max|tv>max){return Ok(None);}
    Ok(Some(episode_crosswalk::Episode{imdb_id:imdb.to_owned(),season,episode:tv,
        anidb_id:anidb.clone(),anidb_episode:local}))
}

/// Independently verified local S/P coordinates for a named AniDB work.
/// They are never derived from arithmetic alone: the established part
/// crosswalk supplies the real IMDb and AniDB episode IDs.
pub struct LocalPartCoordinate {
    pub base_season:i64,
    pub mapped:episode_crosswalk::Episode,
}

/// Resolve S2/P2 *within* an exact named anime work. A global S2 release
/// stays global, and a Part 3 S01E03 keeps its established part/episode
/// offset. The requested equality exception applies to S3 when the named
/// work starts at IMDb S03, not to P3 (which means a third part).
/// A named part must have a *unique exact* AniDB alias, start beyond
/// IMDb S01, and share a TVDB family with another linked AniDB work.
/// A single generic anime title indexed only against S03 is NOT enough
/// to prove that its explicit S2 is relative.
pub fn named_part_base(
    db:&Connection,imdb:&str,title:&str,
)->rusqlite::Result<Option<i64>>{
    let key=imdb_matcher::clean_title(title);
    if key.len()<7{return Ok(None);}
    // Title evidence is served only by the immutable Tantivy index.
    // Never scan the large IMDb titles table from a matching request.
    let canonical=crate::tantivy_search::title_aliases_exact(db,&key)
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?
        .into_iter().any(|alias|alias.imdb_id==imdb && alias.original
            && alias.clean==key
            && matches!(alias.media_type.as_str(),"tvSeries"|"tvMiniSeries"));
    if canonical{return Ok(None);}
    let ids=aliases(db,imdb,title)?.into_iter()
        .filter_map(|(id,alias)|(alias==key).then_some(id))
        .collect::<BTreeSet<_>>();
    if ids.len()!=1{return Ok(None);}
    let selected=ids.iter().next().expect("unique AniDB alias");
    let Some(base)=unique_mapped_season(db,selected)? else{return Ok(None)};
    if base<=1{return Ok(None);}
    let related:i64=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM episode_map mine
            JOIN episode_map sibling
                ON sibling.tvdb_id=mine.tvdb_id
            JOIN anime_series_map linked
                ON linked.anidb_id=sibling.anidb_id
            WHERE mine.anidb_id=?1 AND mine.tvdb_id<>''
                AND mine.tvdb_season>0
                AND sibling.anidb_id<>mine.anidb_id
                AND sibling.tvdb_season>0 AND linked.imdb_id=?2
            LIMIT 1)",
        params![selected,imdb],|row|row.get(0))?;
    Ok((related!=0).then_some(base))
}

/// Verify local S2/P2 via the original AniDB part resolver. S3 when the
/// work starts at S03 is *absolute* S03; P3 still denotes the third part.
/// No episode or season is emitted merely from an arithmetic prediction.
pub fn resolve_local_part(
    db:&Connection,imdb:&str,title:&str,marker:&str,local:i64,episode:i64,
)->rusqlite::Result<Option<LocalPartCoordinate>>{
    if local<2 || local>99 || episode<=0 || episode>99_999
        || !matches!(marker,"S"|"P"){return Ok(None);}
    let Some(base)=named_part_base(db,imdb,title)? else{return Ok(None)};
    let effective=if marker=="S" && local==base {1}else{local};
    let Some(mapped)=resolve_local_part_range(
        db,imdb,title,effective,episode,episode)?
            .into_iter().next().flatten() else{
        return Ok(None);
    };
    if !within_known_season_bounds(db,imdb,mapped.season,mapped.episode)?{
        return Ok(None);
    }
    let valid=if marker=="S" && local==base {
        mapped.season==base
    }else if marker=="S" {
        base.checked_add(local-1)==Some(mapped.season)
    }else{
        // Parts can occupy the same IMDb season or different seasons.
        mapped.season>=base
    };
    Ok(valid.then_some(LocalPartCoordinate{base_season:base,mapped}))
}

// One prepared query and at most two DISTINCT rows are enough to
// prove a unique positive season. No per-request BTreeSet or full
// result materialization is necessary.
fn unique_mapped_season(db:&Connection,id:&str)->rusqlite::Result<Option<i64>>{
    let mut stmt=db.prepare("SELECT DISTINCT tvdb_season FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season>0 LIMIT 2")?;
    let mut rows=stmt.query([id])?;
    let first=match rows.next()?{
        Some(row)=>row.get::<_,i64>(0)?,
        None=>return Ok(None),
    };
    if rows.next()?.is_some(){return Ok(None);}
    Ok(Some(first))
}

/// Infer an IMDb/TVDB season from a title-specific AniDB alias without an episode.
/// Only publish a season when one uniquely identified AniDB work maps to exactly
/// one positive TVDB season. Never manufacture an episode or an ASID.
pub fn season_from_title(db:&Connection,imdb:&str,title:&str)->rusqlite::Result<Option<i64>>{
    let key=imdb_matcher::clean_title(title);
    if key.is_empty()||(!crate::strict_match_mode()&&key.len()<4){
        return Ok(None);
    }
    let ids=aliases(db,imdb,title)?.into_iter().filter(|(_,alias)|alias==&key)
        .collect::<Vec<_>>();
    let unique:BTreeSet<_>=ids.into_iter().map(|(id,_)|id).collect();
    if unique.len()!=1{return Ok(None);}
    let selected=unique.iter().next().unwrap();
    unique_mapped_season(db,selected)
}

/// Recognize a title-only anime part only when an exact alias has a unique
/// IMDb identity and a uniquely mapped positive season.
pub fn season_from_any_exact_title(db:&Connection,title:&str)->rusqlite::Result<Option<i64>>{
    let key=imdb_matcher::clean_title(title);
    let strict=crate::strict_match_mode();
    if key.is_empty()||(!strict&&key.len()<4){return Ok(None);}
    let aliases=if strict{
        crate::tantivy_search::anime_aliases_exact(db,title)
    }else{crate::tantivy_search::anime_aliases(db,&key,4)};
    let found=aliases.map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?;
    // An exact title must name one IMDb identity. In strict mode we
    // already have the FULL AniDB alias postings at this point: the
    // former call to season_from_title repeated the very same Tantivy
    // lookup, Unicode word checks and alias materialization.
    let mut selected_imdb:Option<&str>=None;
    let mut selected_anidb:Option<&str>=None;
    for alias in found.iter().filter(|a|a.clean_title==key){
        if selected_imdb.is_some_and(|id|id!=alias.imdb_id){
            return Ok(None);
        }
        selected_imdb=Some(&alias.imdb_id);
        if strict {
            if selected_anidb.is_some_and(|id|id!=alias.anidb_id){
                return Ok(None);
            }
            selected_anidb=Some(&alias.anidb_id);
        }
    }
    let Some(imdb)=selected_imdb else{return Ok(None)};
    if strict {
        return match selected_anidb{
            Some(id)=>unique_mapped_season(db,id),
            None=>Ok(None),
        };
    }
    // Non-strict season_from_title intentionally performs a broader
    // source-boundary alias lookup. Retain that behavior exactly.
    season_from_title(db,imdb,&key)
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

#[cfg(test)]
mod named_part_range_tests {
    use super::*;

    fn fixture()->Connection{
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(
                anidb_id TEXT,imdb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,max_episode INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO titles VALUES
                ('tt0000001','Example Anime','exampleanime',2022,'series','',1,0,'tvSeries');
            INSERT INTO anidb_titles VALUES
                ('100','Part One','partone','main','en'),
                ('200','Part Two','parttwo','main','en');
            INSERT INTO anime_series_map VALUES
                ('100','tt0000001','700',0),('200','tt0000001','700',0);
            INSERT INTO season_episodes VALUES
                ('tt0000001',1,3),('tt0000001',2,3);
            INSERT INTO episode_map VALUES
                ('100','700',1,1,1,3,0,''),
                ('200','700',1,2,1,3,0,'');").unwrap();
        db
    }

    #[test]
    fn named_part_batch_keeps_verified_season_and_rejects_out_of_range(){
        let db=fixture();
        let base=resolve_range(&db,"tt0000001","Part One",1,1,3).unwrap();
        let sequel=resolve_range(&db,"tt0000001","Part Two",1,1,3).unwrap();
        for (number,episode) in (1..=3).zip(base){
            let item=episode.expect("verified first season");
            assert_eq!((item.season,item.episode,item.anidb_id.as_str()),
                (1,number,"100"));
        }
        for (number,episode) in (1..=3).zip(sequel){
            let item=episode.expect("verified second season via named part");
            assert_eq!((item.season,item.episode,item.anidb_id.as_str()),
                (2,number,"200"));
            assert_eq!(item.anidb_episode,number);
        }
        assert!(resolve_range(&db,"tt0000001","Part Two",1,4,5)
            .unwrap().iter().all(Option::is_none));
        let single=resolve(&db,"tt0000001","Part Two",1,2).unwrap().unwrap();
        assert_eq!((single.season,single.episode,single.anidb_id.as_str()),
            (2,2,"200"));
    }

    #[test]
    fn named_part_rejects_overflowing_metadata_coordinates(){
        let db=fixture();
        db.execute("UPDATE episode_map SET offset=?1 WHERE anidb_id='200'",
            [i64::MAX]).unwrap();
        assert!(resolve_local_part_range(&db,"tt0000001","Part One",2,1,2)
            .unwrap().iter().all(Option::is_none));
        db.execute("UPDATE episode_map SET offset=0,start=?1,end=0
            WHERE anidb_id='200'",[i64::MAX]).unwrap();
        assert!(resolve_local_part_range(&db,"tt0000001","Part One",2,2,2)
            .unwrap().iter().all(Option::is_none));
    }

    #[test]
    fn invalid_named_part_ranges_are_rejected_before_metadata_queries(){
        let db=Connection::open_in_memory().unwrap();
        for (season,start,end) in [(1,1,i64::MAX),(1,100_000,100_000),
            (10_000,1,1),(1,0,1)]{
            assert!(resolve_range(&db,"tt0000001","Part One",season,start,end)
                .unwrap().is_empty());
        }
    }
}

#[cfg(test)]
mod explicit_global_named_part_range_tests {
    use super::*;

    // Real AnimeAPI links and Anime-Lists coordinates: Alicization starts
    // at S03, while War of Underworld's two AniDB parts share S04. English
    // aliases and IMDb bounds are controlled here to isolate range semantics.
    fn fixture()->Connection{
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(
                imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
                region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,max_episode INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO titles VALUES
                ('tt2250192','Sword Art Online','swordartonline',2012,
                 'series','',1,0,'tvSeries');
            INSERT INTO anidb_titles VALUES
                ('8692','Sword Art Online','swordartonline','main','en'),
                ('10376','Sword Art Online II','swordartonlineii','main','en'),
                ('13493','Sword Art Online: Alicization',
                 'swordartonlinealicization','main','en'),
                ('14796','Sword Art Online: Alicization - War of Underworld',
                 'swordartonlinealicizationwarofunderworld','main','en'),
                ('15146','Sword Art Online: Alicization - War of Underworld 2nd Season',
                 'swordartonlinealicizationwarofunderworld2ndseason','main','en');
            INSERT INTO anime_series_map VALUES
                ('tt2250192','8692','259640',1),
                ('tt2250192','10376','259640',2),
                ('tt2250192','13493','259640',3),
                ('tt2250192','14796','259640',4),
                ('tt2250192','15146','259640',0);
            INSERT INTO season_episodes VALUES
                ('tt2250192',1,25),('tt2250192',2,24),
                ('tt2250192',3,24),('tt2250192',4,23);
            INSERT INTO episode_map VALUES
                ('8692','259640',1,1,0,0,0,''),
                ('10376','259640',1,2,0,0,0,''),
                ('13493','259640',1,3,0,0,0,''),
                ('14796','259640',1,4,1,12,0,''),
                ('15146','259640',1,4,1,0,12,'');")
            .unwrap();
        db
    }

    #[test]
    fn explicit_base_season_range_matches_individual_global_coordinates(){
        let db=fixture();
        let title="Sword Art Online Alicization";
        let batch=resolve_range(&db,"tt2250192",title,3,3,5).unwrap();
        assert_eq!(batch.len(),3);
        for (number,item) in (3..=5).zip(batch){
            let item=item.expect("verified global Alicization episode");
            assert_eq!((item.season,item.episode,item.anidb_id.as_str(),item.anidb_episode),
                (3,number,"13493",number));
            let local=resolve_local_part(&db,"tt2250192",title,"S",3,number)
                .unwrap().expect("single S3 equality control").mapped;
            assert_eq!(item,local);
        }
    }

    #[test]
    fn explicit_base_season_range_does_not_fill_crosswalk_holes_from_local_parts(){
        let db=fixture();
        db.execute("UPDATE episode_map SET map='3-3;5-5' WHERE anidb_id='13493'",[])
            .unwrap();
        let batch=resolve_range(&db,"tt2250192","Sword Art Online Alicization",3,3,5)
            .unwrap();
        assert_eq!(batch[0].as_ref().unwrap().anidb_id,"13493");
        assert!(batch[1].is_none(),"missing S3E04 cannot become a third-part S4 episode");
        assert_eq!(batch[2].as_ref().unwrap().anidb_id,"13493");
        db.execute("DELETE FROM episode_map WHERE anidb_id='13493'",[]).unwrap();
        assert!(resolve_range(&db,"tt2250192","Sword Art Online Alicization",3,3,5)
            .unwrap().iter().all(Option::is_none));
    }

    #[test]
    fn explicit_base_season_range_enforces_known_bounds(){
        let db=fixture();
        let batch=resolve_range(&db,"tt2250192","Sword Art Online Alicization",3,23,26)
            .unwrap();
        assert_eq!(batch[0].as_ref().unwrap().episode,23);
        assert_eq!(batch[1].as_ref().unwrap().episode,24);
        assert!(batch[2..].iter().all(Option::is_none),
            "episodes after global S3's known maximum cannot become local-part coordinates");
    }

    #[test]
    fn relative_s2_and_explicit_p3_keep_their_verified_local_coordinates(){
        let db=fixture();
        let title="Sword Art Online Alicization";
        let second=resolve_range(&db,"tt2250192",title,2,3,5).unwrap();
        for (number,item) in (3..=5).zip(second){
            let item=item.expect("second named part");
            assert_eq!((item.season,item.episode,item.anidb_id.as_str(),item.anidb_episode),
                (4,number,"14796",number));
        }
        let second=resolve_local_part(&db,"tt2250192",title,"P",2,3)
            .unwrap().unwrap().mapped;
        assert_eq!((second.season,second.episode,second.anidb_id.as_str()),(4,3,"14796"));
        let third=resolve_local_part(&db,"tt2250192",title,"P",3,3)
            .unwrap().unwrap().mapped;
        assert_eq!((third.season,third.episode,third.anidb_id.as_str()),(4,15,"15146"));
        let explicit=resolve(&db,"tt2250192",
            "Sword Art Online Alicization War of Underworld 2nd Season",1,3)
            .unwrap().unwrap();
        assert_eq!((explicit.season,explicit.episode,explicit.anidb_id.as_str()),
            (4,15,"15146"));
    }
}
