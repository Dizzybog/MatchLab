//! Crosswalk between already identified IMDb and AniDB works.
//! AniDB title discovery is handled separately by anidb_matcher; this
//! module converts IMDb season coordinates using verified mapping data.
use rusqlite::{Connection};
use serde::Serialize;
use std::collections::{BTreeSet,HashMap};

#[derive(Clone,Debug)]
struct Link {anidb:String,tvdb:String,trakt:i64,direct:bool}
#[derive(Clone,Debug)]
struct Mapping {anidb:String,tvdb:String,tv_season:i64,start:i64,end:i64,offset:i64,
    // Parsed once per metadata snapshot. A 300-episode range previously
    // parsed the identical semicolon mapping hundreds of times.
    explicit:Vec<(i64,i64)>}
#[derive(Clone,Debug,PartialEq,Eq,Serialize)]
pub struct Episode {pub imdb_id:String,pub season:i64,pub episode:i64,pub anidb_id:String,pub anidb_episode:i64}

// Pure SID/ASID validation belongs to identifiers, not the bridge.
pub(crate) use crate::identifiers::{canonical_asid,valid_asid};

fn links(db:&Connection,imdb:&str)->rusqlite::Result<Vec<Link>>{
    let mut stmt=db.prepare("SELECT anidb_id,tvdb_id,trakt_season FROM anime_series_map WHERE imdb_id=?1")?;
    let mut out=stmt.query_map([imdb],|r|Ok(Link{anidb:r.get(0)?,tvdb:r.get(1)?,trakt:r.get(2)?,direct:true}))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if out.is_empty(){
        let mut stmt=db.prepare("SELECT DISTINCT value FROM anime_ids WHERE group_id=?1 AND type='anidb'")?;
        let ids=stmt.query_map([imdb],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len()!=1{return Ok(vec![]);}
        out.push(Link{anidb:ids[0].clone(),tvdb:String::new(),trakt:0,direct:true});
    }
    let tvdbs:BTreeSet<String>=out.iter().filter(|l|!l.tvdb.is_empty()).map(|l|l.tvdb.clone()).collect();
    // Small families are cheaper to scan inline. Large AniDB families
    // previously checked every discovered sibling against the entire
    // growing link list: O(family²). Build a membership set only after
    // the family becomes large enough, preserving direct duplicates and
    // the original source-row order exactly.
    let mut seen:Option<std::collections::HashSet<(String,String)>>=None;
    for tvdb in tvdbs {
        let mut stmt=db.prepare("SELECT DISTINCT anidb_id FROM episode_map WHERE tvdb_id=?1")?;
        for id in stmt.query_map([&tvdb],|r|r.get::<_,String>(0))? {
            let id=id?;
            let exists=if out.len()<8{
                out.iter().any(|l|l.anidb==id&&l.tvdb==tvdb)
            }else{
                let set=seen.get_or_insert_with(||out.iter()
                    .map(|l|(l.anidb.clone(),l.tvdb.clone())).collect());
                !set.insert((id.clone(),tvdb.clone()))
            };
            if !exists{
                out.push(Link{anidb:id,tvdb:tvdb.clone(),trakt:0,direct:false});
            }
        }
    }
    Ok(out)
}
fn mappings(db:&Connection,links:&[Link],include_absolute:bool)->rusqlite::Result<Vec<Mapping>>{
    let mut out=vec![];
    let ids:BTreeSet<_>=links.iter().map(|l|l.anidb.as_str()).collect();
    let mut stmt=db.prepare("SELECT anidb_id,tvdb_id,anidb_season,tvdb_season,start,end,offset,map
        FROM episode_map WHERE anidb_id=?1 AND anidb_season>0")?;
    for id in ids {
        for row in stmt.query_map([id],|r|{
            let map:String=r.get(7)?;
            Ok(Mapping{anidb:r.get(0)?,tvdb:r.get(1)?,
                tv_season:r.get(3)?,start:r.get(4)?,end:r.get(5)?,
                offset:r.get(6)?,explicit:explicit_pairs(&map)})
        })?{
            let m=row?;
            if include_absolute||m.tv_season>0{out.push(m);}
        }
    }
    Ok(out)
}
// One compact read-only index for the complete crosswalk snapshot.
// Every episode in a range reuses the same grouped mapping references and
// split-cour partners. Grouping does not reorder rows within an AniDB ID,
// which matters when several absolute-season rows overlap.
struct LinkedMappings<'a>{
    link:&'a Link,
    // Preserve the order of mapping rows and every duplicate/overlap.
    mappings:Vec<&'a Mapping>,
    // Split status depends only on the immutable family/season metadata,
    // NOT on the episode number. Evaluate once per map row, then reuse.
    split:Vec<bool>,
    // This is identical to the old per-episode first-absolute-row check.
    absolute_offset:i64,
    first_absolute_end:Option<i64>,
}
struct SplitIndex<'a>{
    // Keep the reference partner graph only for parity tests. Production
    // resolves it into one Boolean per mapping row in split_index().
    #[cfg(test)]
    partners:HashMap<(&'a str,i64),Vec<(&'a str,i64)>>,
    links:Vec<LinkedMappings<'a>>,
}
fn split_index<'a>(links:&'a[Link],maps:&'a[Mapping])->SplitIndex<'a>{
    let mut by_anidb:HashMap<&str,Vec<&Mapping>>=HashMap::new();
    for map in maps{
        by_anidb.entry(map.anidb.as_str()).or_default().push(map);
    }
    let mut partners=HashMap::new();
    for link in links.iter().filter(|link|link.trakt>0){
        for map in by_anidb.get(link.anidb.as_str()).into_iter().flatten(){
            partners.entry((map.tvdb.as_str(),map.tv_season))
                .or_insert_with(Vec::new)
                .push((link.anidb.as_str(),link.trakt));
        }
    }
    // Resolve once per immutable snapshot, never for each episode in a
    // season or absolute range. Preserve the old source-row order, including
    // duplicate mappings. Even a link without mappings remains represented.
    let indexed_links=links.iter().map(|link|{
        let mappings=by_anidb.get(link.anidb.as_str()).into_iter()
            .flatten().copied().filter(|m|
                link.tvdb.is_empty()||link.tvdb==m.tvdb).collect::<Vec<_>>();
        let split=mappings.iter().map(|m|link.trakt>0
            && partners.get(&(m.tvdb.as_str(),m.tv_season)).is_some_and(|others|
                others.iter().any(|(id,trakt)|
                    *id!=link.anidb.as_str() && *trakt!=link.trakt)))
            .collect::<Vec<_>>();
        let mut absolute_count=0usize;
        let mut first_absolute=None;
        for &m in &mappings{
            if m.tv_season==-1{
                absolute_count+=1;
                if first_absolute.is_none(){first_absolute=Some(m);}
            }
        }
        let absolute_offset=if absolute_count==1{
            first_absolute.map_or(0,|m:&Mapping|m.offset)
        }else{0};
        LinkedMappings{link,mappings,split,absolute_offset,
            first_absolute_end:first_absolute.map(|m|m.end)}
    }).collect();
    // The temporary AniDB lookup map is intentionally dropped here.
    // LinkedMappings has already captured only the relevant references.
    SplitIndex{
        #[cfg(test)]
        partners,
        links:indexed_links,
    }
}
#[cfg(test)]
fn split(link:&Link,m:&Mapping,index:&SplitIndex<'_>)->bool{
    link.trakt>0&&index.partners.get(&(m.tvdb.as_str(),m.tv_season))
        .is_some_and(|other|other.iter().any(|(id,trakt)|
            *id!=link.anidb.as_str() && *trakt!=link.trakt))
}
fn explicit_pairs(map:&str)->Vec<(i64,i64)>{
    let mut pairs=vec![];
    for part in map.trim_matches(';').split(';'){
        let Some((left,right))=part.trim().split_once('-')else{continue};
        let Ok(ani)=left.parse::<i64>()else{continue};
        if right.split('+').all(|n|!n.is_empty()&&n.bytes().all(|b|b.is_ascii_digit())){
            for tv in right.split('+').filter_map(|n|n.parse::<i64>().ok()){pairs.push((ani,tv));}
        }
    }
    pairs
}
pub fn resolve_season(db:&Connection,imdb:&str,season:i64,episode:i64)->rusqlite::Result<Option<Episode>>{
    if !(1..=9_999).contains(&season)||!(1..=99_999).contains(&episode){
        return Ok(None);
    }
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,false)?;
    let splits=split_index(&links,&maps);
    Ok(resolve_season_preloaded(imdb,season,episode,&links,&maps,&splits))
}

/// Resolve a season-specific batch with one snapshot of the crosswalk. This
/// is used for S01E01-E12 and avoids repeated SQLite reads on one-core hosts.
pub fn resolve_season_range(
    db:&Connection,imdb:&str,season:i64,start:i64,end:i64,
)->rusqlite::Result<Vec<Option<Episode>>>{
    if !(1..=9_999).contains(&season)||start<=0||end<start
        ||end>99_999||end-start>=1_000{return Ok(Vec::new());}
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,false)?;
    let splits=split_index(&links,&maps);
    Ok((start..=end).map(|episode|
        resolve_season_preloaded(imdb,season,episode,&links,&maps,&splits)).collect())
}

// Retain one strongest coordinate and whether another distinct coordinate
// has the same (offset,start) evidence. Duplicate links or map rows must not
// hide a conflicting third candidate behind an identical runner-up. This
// stays constant-size without allocating or cloning candidates per episode.
#[derive(Clone,Copy)]
struct SeasonWinner<'a> {
    offset:i64,start:i64,anidb:&'a str,anime_episode:i64,
}
fn consider_season<'a>(
    best:&mut Option<SeasonWinner<'a>>,ambiguous:&mut bool,
    offset:i64,start:i64,anidb:&'a str,anime_episode:i64,
){
    if canonical_asid(anidb,anime_episode).is_none(){return;}
    let candidate=SeasonWinner{offset,start,anidb,anime_episode};
    match *best {
        None=>{
            *best=Some(candidate);
            *ambiguous=false;
        },
        Some(previous) if (offset,start)>(previous.offset,previous.start)=>{
            *best=Some(candidate);
            *ambiguous=false;
        },
        Some(previous) if (offset,start)==(previous.offset,previous.start)=>{
            if (anidb,anime_episode)!=(previous.anidb,previous.anime_episode){
                *ambiguous=true;
            }
        },
        _=>{},
    }
}
fn resolve_season_preloaded(
    imdb:&str,season:i64,episode:i64,_links:&[Link],_maps:&[Mapping],
    splits:&SplitIndex<'_>,
)->Option<Episode>{
    if !(1..=9_999).contains(&season)||!(1..=99_999).contains(&episode){return None;}
    let mut best:Option<SeasonWinner<'_>>=None;
    let mut ambiguous=false;
    for indexed in &splits.links {
        let link=indexed.link;
        for (&m,&is_split) in indexed.mappings.iter().zip(&indexed.split) {
            if is_split&&link.trakt!=season{continue;}
            if !is_split&&m.tv_season!=season{continue;}
            // An AniDB work discovered only as a TVDB-family sibling
            // cannot claim arbitrary episodes with an open-ended default.
            // Bounded/explicit sibling mappings remain eligible, and a
            // directly linked work keeps its existing mapping policy.
            // Mirror resolve_absolute_preloaded's existing source guard.
            if !link.direct && m.explicit.is_empty()
                && (m.start<=0 || m.end<=0) {continue;}
            if !is_split&&!m.explicit.is_empty(){
                for &(ani,tv) in &m.explicit{
                    if tv==episode{
                        consider_season(&mut best,&mut ambiguous,
                            m.offset,m.start,&link.anidb,ani);
                    }
                }
                continue;
            }
            let ani=if is_split{episode.checked_add(m.start.max(1)-1)}
                else{episode.checked_sub(m.offset)};
            let Some(ani)=ani else{continue};
            if ani<m.start.max(1)||(m.end>0&&ani>m.end){continue;}
            consider_season(&mut best,&mut ambiguous,
                m.offset,m.start,&link.anidb,ani);
        }
    }
    if ambiguous{return None;}
    let best=best?;
    Some(Episode{imdb_id:imdb.into(),season,episode,
        anidb_id:best.anidb.to_owned(),anidb_episode:best.anime_episode})
}
pub fn resolve_absolute(db:&Connection,imdb:&str,episode:i64)->rusqlite::Result<Option<Episode>>{
    if !(1..=99_999).contains(&episode){return Ok(None);}
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,true)?;
    let splits=split_index(&links,&maps);
    Ok(resolve_absolute_preloaded(imdb,episode,&links,&maps,&splits))
}

/// Check two absolute episode endpoints against one crosswalk snapshot.
/// This is a cheap eligibility check when multiple unrelated AniDB works
/// share an alias (for example One Piece films vs. the long-running series).
/// The caller must still validate the entire requested range before publishing.
pub fn resolve_absolute_endpoints(
    db:&Connection,imdb:&str,start:i64,end:i64,
)->rusqlite::Result<(Option<Episode>,Option<Episode>)>{
    if start<=0 || end<start || end>99_999 || end-start>=10_000 {
        return Ok((None,None));
    }
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,true)?;
    let splits=split_index(&links,&maps);
    Ok((
        resolve_absolute_preloaded(imdb,start,&links,&maps,&splits),
        resolve_absolute_preloaded(imdb,end,&links,&maps,&splits),
    ))
}

/// Load the AniDB/TVDB crosswalk once per batch, not once per episode.
/// This prevents a 300-episode torrent from issuing hundreds of SQLite reads
/// while still checking every episode independently for ambiguous mappings.
pub fn resolve_absolute_range(
    db:&Connection,imdb:&str,start:i64,end:i64,
)->rusqlite::Result<Vec<Option<Episode>>>{
    if start<=0||end<start||end>99_999||end-start>9_999{return Ok(Vec::new());}
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,true)?;
    let splits=split_index(&links,&maps);
    Ok((start..=end).map(|episode|resolve_absolute_preloaded(
        imdb,episode,&links,&maps,&splits)).collect())
}

// A full BTreeMap of candidates is unnecessary when the only possible
// answer is a unique strongest coordinate. Deduplicate identical tuples
// as they stream in, preserve the strongest evidence priority, and retain
// ambiguity across different tuples at that same evidence strength.
#[derive(Clone,Copy,PartialEq,Eq)]
struct AbsoluteWinner<'a>{
    anidb:&'a str,anime_episode:i64,season:i64,episode:i64,strength:u8,
}
fn consider_absolute<'a>(
    best:&mut Option<AbsoluteWinner<'a>>,ambiguous:&mut bool,
    anidb:&'a str,anime_episode:i64,season:i64,episode:i64,strength:u8,
){
    if canonical_asid(anidb,anime_episode).is_none()
        ||!(1..=9_999).contains(&season)||!(1..=99_999).contains(&episode){
        return;
    }
    let candidate=AbsoluteWinner{anidb,anime_episode,season,episode,strength};
    match *best{
        None=>{
            *best=Some(candidate);
            *ambiguous=false;
        },
        Some(previous) if strength>previous.strength=>{
            *best=Some(candidate);
            *ambiguous=false;
        },
        Some(previous) if strength==previous.strength=>{
            if (previous.anidb,previous.anime_episode,
                previous.season,previous.episode)!=(anidb,anime_episode,season,episode){
                *ambiguous=true;
            }
        },
        _=>{},
    }
}
fn resolve_absolute_preloaded(
    imdb:&str,episode:i64,_links:&[Link],_maps:&[Mapping],
    splits:&SplitIndex<'_>,
)->Option<Episode>{
    if !(1..=99_999).contains(&episode){return None;}
    let mut best:Option<AbsoluteWinner<'_>>=None;
    let mut ambiguous=false;
    for indexed in &splits.links{
        let link=indexed.link;
        // Computed once per crosswalk snapshot: preserve first absolute
        // mapping row, duplicate links and source-row traversal order.
        let Some(local)=episode.checked_sub(indexed.absolute_offset) else{continue};
        if local<=0||indexed.first_absolute_end
            .is_some_and(|end|end>0&&local>end){continue;}
        for (&m,&is_split) in indexed.mappings.iter().zip(&indexed.split){
            if m.tv_season<=0{continue;}
            if !m.explicit.is_empty(){
                for &(ani,tv) in &m.explicit{
                    if ani==local&&tv>0{
                        let season=if is_split{link.trakt}else{m.tv_season};
                        if season>0{
                            consider_absolute(&mut best,&mut ambiguous,
                                &link.anidb,local,season,tv,3);
                        }
                    }
                }
                continue;
            }
            if local<m.start.max(1)||(m.end>0&&local>m.end){continue;}
            // Indirect links never make unbounded default rows proof of
            // ownership. The old evidence tiers (3/2/1) stay unchanged.
            if !link.direct && (m.start<=0||m.end<=0){continue;}
            let season=if is_split{link.trakt}else{m.tv_season};
            let tv=if is_split{Some(local-m.start.max(1)+1)}
                else{local.checked_add(m.offset)};
            let Some(tv)=tv else{continue};
            if season>0&&tv>0{
                let strength=if m.start>0&&m.end>0{2}else{1};
                consider_absolute(&mut best,&mut ambiguous,
                    &link.anidb,local,season,tv,strength);
            }
        }
    }
    if ambiguous{return None;}
    let best=best?;
    Some(Episode{imdb_id:imdb.into(),season:best.season,episode:best.episode,
        anidb_id:best.anidb.to_owned(),anidb_episode:best.anime_episode})
}

/// Reverse lookup: AniDB work + AniDB-local (often absolute) episode number
/// -> all independently verifiable IMDb season/episode coordinates.
///
/// A TVDB family may share several AniDB parts. First derive candidates from
/// that part's real episode_map rows, then run the *existing* forward resolver
/// against each coordinate. This ensures both directions agree on explicit
/// pairs, offsets, split cours, and ambiguity rules. Never guess season 1.
pub fn resolve_anidb(
    db:&Connection,anidb_id:&str,episode:i64,
)->rusqlite::Result<Vec<Episode>>{
    if canonical_asid(anidb_id,episode).is_none(){return Ok(Vec::new());}
    let mut stmt=db.prepare(
        "SELECT DISTINCT imdb_id FROM anime_series_map
           WHERE anidb_id=?1
         UNION
         SELECT DISTINCT l.imdb_id
           FROM episode_map e
           JOIN anime_series_map l ON l.tvdb_id=e.tvdb_id
          WHERE e.anidb_id=?1 AND e.tvdb_id<>'' AND l.tvdb_id<>''")?;
    let imdbs=stmt.query_map([anidb_id],|r|r.get::<_,String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let mut results=BTreeSet::new();
    for imdb in imdbs {
        if !crate::metadata_import::valid_imdb_id(&imdb){continue;}
        let linked=links(db,&imdb)?;
        let maps=mappings(db,&linked,false)?;
        let splits=split_index(&linked,&maps);
        for indexed in splits.links.iter()
            .filter(|indexed|indexed.link.anidb==anidb_id){
            let link=indexed.link;
            for (&m,&split_part) in indexed.mappings.iter().zip(&indexed.split){
                if m.tv_season<=0{continue;}
                let season=if split_part{link.trakt}else{m.tv_season};
                if season<=0||season>9_999{continue;}
                let ep_candidates:Vec<i64>=if split_part{
                    if episode<m.start.max(1) || (m.end>0&&episode>m.end){
                        Vec::new()
                    }else{vec![episode-m.start.max(1)+1]}
                }else{
                    if !m.explicit.is_empty(){
                        // Non-bijective maps may associate one AniDB episode
                        // with several TVDB episodes: preserve every result.
                        m.explicit.iter().copied().filter_map(|(ani,tv)|
                            (ani==episode).then_some(tv)).collect()
                    }else if episode>=m.start.max(1) &&
                        (m.end<=0||episode<=m.end){
                        episode.checked_add(m.offset).into_iter().collect()
                    }else{Vec::new()}
                };
                for tv_episode in ep_candidates{
                    if tv_episode<=0||tv_episode>99_999{continue;}
                    // Re-run the verified forward conversion with this
                    // *preloaded* snapshot. Reject aliases/overlap where it
                    // chooses a different AniDB episode or returns None.
                    if let Some(forward)=resolve_season_preloaded(
                        &imdb,season,tv_episode,&linked,&maps,&splits)
                        && forward.anidb_id==anidb_id
                        && forward.anidb_episode==episode {
                        results.insert((imdb.clone(),season,tv_episode));
                    }
                }
            }
        }
    }
    Ok(results.into_iter().map(|(imdb_id,season,tv_episode)|Episode{
        imdb_id,season,episode:tv_episode,anidb_id:anidb_id.to_owned(),
        anidb_episode:episode,
    }).collect())
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn streaming_winners_match_full_candidate_reference_for_1000_fixtures(){
        let ids=["100","101","102","bad","0","123"];
        let mut state=0x18f3a11c_u64;
        let mut pick=|n:u64|{
            state=state.wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state%n
        };
        for _case in 0..1000 {
            let mut season_reference:Vec<(i64,i64,String,i64)>=Vec::new();
            let mut best_season=None;
            let mut ambiguous_season=false;
            let mut old_absolute=std::collections::BTreeMap::<(String,i64,i64,i64),u8>::new();
            let mut best_absolute=None;
            let mut ambiguous_absolute=false;
            for _ in 0..38 {
                let id=ids[pick(ids.len() as u64) as usize];
                let offset=pick(8) as i64-3;
                let start=pick(7) as i64;
                let ani=pick(105_000) as i64;
                let season=pick(10_100) as i64;
                let episode=pick(105_000) as i64;
                let strength=(pick(3)+1) as u8;
                season_reference.push((offset,start,id.into(),ani));
                consider_season(&mut best_season,&mut ambiguous_season,
                    offset,start,id,ani);
                old_absolute.entry((id.into(),ani,season,episode))
                    .and_modify(|seen|*seen=(*seen).max(strength))
                    .or_insert(strength);
                consider_absolute(&mut best_absolute,&mut ambiguous_absolute,
                    id,ani,season,episode,strength);
            }
            // Compare streaming ambiguity detection to an independent full
            // candidate set, including duplicates at the strongest rank.
            season_reference.retain(|(_,_,id,ani)|canonical_asid(id,*ani).is_some());
            let max=season_reference.iter().map(|r|(r.0,r.1)).max();
            let top=season_reference.into_iter().filter(|r|Some((r.0,r.1))==max)
                .map(|r|(r.2,r.3)).collect::<BTreeSet<_>>();
            let expected_season=if top.len()==1{top.into_iter().next()}else{None};
            let actual_season=if ambiguous_season{None}else{
                best_season.map(|best:SeasonWinner<'_>|
                    (best.anidb.to_owned(),best.anime_episode))
            };
            assert_eq!(actual_season,expected_season);

            // The old absolute resolver merged identical keys using their
            // maximum evidence strength and required one strongest key.
            old_absolute.retain(|(id,ani,season,episode),_|
                canonical_asid(id,*ani).is_some()
                &&(1..=9_999).contains(season)
                &&(1..=99_999).contains(episode));
            let max=old_absolute.values().copied().max();
            let mut top=old_absolute.into_iter().filter(|(_,strength)|
                Some(*strength)==max);
            let expected_absolute=match (top.next(),top.next()){
                (Some((key,_)),None)=>Some(key),
                _=>None,
            };
            let actual_absolute=if ambiguous_absolute {None}else{
                best_absolute.map(|best:AbsoluteWinner<'_>|(
                    best.anidb.to_owned(),best.anime_episode,
                    best.season,best.episode))
            };
            assert_eq!(actual_absolute,expected_absolute);
        }
    }

    #[test]
    fn duplicate_mapping_rows_cannot_hide_ambiguous_season_identity(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','100','700',0);
            INSERT INTO episode_map VALUES('100','700',1,1,1,12,0,'');
            INSERT INTO episode_map VALUES('100','700',1,1,1,12,0,'');").unwrap();
        assert_eq!(resolve_season(&db,"tt1234567",1,3).unwrap()
            .unwrap().anidb_id,"100","identical duplicates are harmless");
        db.execute("INSERT INTO episode_map VALUES('200','700',1,1,1,12,0,'')",[])
            .unwrap();
        assert!(resolve_season(&db,"tt1234567",1,3).unwrap().is_none(),
            "a distinct tied identity remains ambiguous behind duplicate map rows");
        assert!(resolve_season_range(&db,"tt1234567",1,1,3).unwrap()
            .iter().all(Option::is_none));
        assert!(resolve_anidb(&db,"100",3).unwrap().is_empty(),
            "reverse resolution must agree with ambiguous forward resolution");
    }

    #[test]
    fn duplicate_explicit_pairs_cannot_hide_a_different_anidb_episode(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','100','700',0);
            INSERT INTO episode_map VALUES('100','700',1,1,1,12,0,'1-1;1-1;2-1');")
            .unwrap();
        assert!(resolve_season(&db,"tt1234567",1,1).unwrap().is_none());
    }

    #[test]
    fn stronger_season_evidence_clears_only_weaker_ambiguity(){
        let mut best=None;
        let mut ambiguous=false;
        for (offset,id) in [(0,"100"),(0,"200"),(1,"300"),(0,"100")]{
            consider_season(&mut best,&mut ambiguous,offset,1,id,3);
        }
        assert!(!ambiguous);
        assert_eq!(best.unwrap().anidb,"300");
        consider_season(&mut best,&mut ambiguous,1,1,"400",3);
        assert!(ambiguous);
        consider_season(&mut best,&mut ambiguous,1,1,"300",3);
        assert!(ambiguous,"a duplicate never clears equal-strength ambiguity");
    }

    #[test]
    fn malformed_offsets_are_rejected_without_arithmetic_overflow(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','100','700',0);
            INSERT INTO episode_map VALUES
                ('100','700',1,-1,1,12,-9223372036854775808,''),
                ('100','700',1,1,1,12,-9223372036854775808,'');")
            .unwrap();
        assert!(resolve_season(&db,"tt1234567",1,3).unwrap().is_none());
        assert!(resolve_absolute(&db,"tt1234567",3).unwrap().is_none());
        db.execute("DELETE FROM episode_map WHERE tvdb_season=-1",[]).unwrap();
        db.execute("UPDATE episode_map SET offset=9223372036854775807",[]).unwrap();
        assert!(resolve_absolute(&db,"tt1234567",3).unwrap().is_none());
        assert!(resolve_anidb(&db,"100",3).unwrap().is_empty());
    }

    #[test]
    fn invalid_public_coordinates_skip_crosswalk_reads(){
        let db=Connection::open_in_memory().unwrap();
        for (season,episode) in [(10_000,1),(1,100_000),(i64::MAX,i64::MAX)]{
            assert!(resolve_season(&db,"tt1234567",season,episode).unwrap().is_none());
        }
        assert!(resolve_absolute(&db,"tt1234567",100_000).unwrap().is_none());
        assert!(resolve_season_range(&db,"tt1234567",1,99_999,100_000)
            .unwrap().is_empty());
        assert!(resolve_absolute_range(&db,"tt1234567",99_999,100_000)
            .unwrap().is_empty());
        assert_eq!(resolve_absolute_endpoints(&db,"tt1234567",99_999,100_000)
            .unwrap(),(None,None));
    }

    #[test]
    fn large_ani_family_link_lookup_preserves_direct_duplicates_and_order(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','100','700',0);
            INSERT INTO anime_series_map VALUES('tt1234567','100','700',0);")
            .unwrap();
        for id in 100..125{
            db.execute("INSERT INTO episode_map VALUES (?1,'700')",
                [id.to_string()]).unwrap();
        }
        let actual=links(&db,"tt1234567").unwrap();
        assert_eq!(actual.iter().filter(|l|l.direct).count(),2);
        let mut expected=vec![
            ("100".to_owned(),"700".to_owned(),true),
            ("100".to_owned(),"700".to_owned(),true),
        ];
        let mut stmt=db.prepare("SELECT DISTINCT anidb_id FROM episode_map
            WHERE tvdb_id='700'").unwrap();
        let ids=stmt.query_map([],|r|r.get::<_,String>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        for id in ids{
            if !expected.iter().any(|(seen,tvdb,_)|*seen==id&&tvdb=="700"){
                expected.push((id,"700".into(),false));
            }
        }
        let got=actual.iter().map(|l|
            (l.anidb.clone(),l.tvdb.clone(),l.direct)).collect::<Vec<_>>();
        assert_eq!(got,expected);
        assert_eq!(got.len(),26);
    }

    #[test]
    fn bleach_absolute_320_prefers_bounded_episode_over_shared_tvdb_open_sequels(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            -- Bleach (2004) has an absolute-season marker plus explicit
            -- TVDB season 15 mapping for absolute episodes 317..342.
            INSERT INTO anime_series_map VALUES('tt0434665','2369','74796',0);
            INSERT INTO episode_map VALUES
                ('2369','74796',1,-1,1,0,0,''),
                ('2369','74796',1,15,317,342,-316,''),
                -- Later Bleach parts share TVDB 74796 and have open defaults.
                ('15449','74796',1,17,0,0,0,''),
                ('17765','74796',1,17,0,0,13,'');").unwrap();
        let mapped=resolve_absolute(&db,"tt0434665",320).unwrap().unwrap();
        assert_eq!((mapped.season,mapped.episode),(15,4));
        assert_eq!((mapped.anidb_id.as_str(),mapped.anidb_episode),("2369",320));
        let batch=resolve_absolute_range(&db,"tt0434665",319,321).unwrap();
        assert_eq!(batch.len(),3);
        for (number,episode) in (319..=321).zip(batch){
            let ep=episode.expect("bounded Bleach episode");
            assert_eq!((ep.season,ep.episode,ep.anidb_episode),(15,number-316,number));
            assert_eq!(ep.anidb_id,"2369");
        }
        // No bounded mapping covers a later sequel episode. Related
        // unbounded default rows cannot claim it for the 2004 IMDb series.
        assert!(resolve_absolute(&db,"tt0434665",400).unwrap().is_none());
        // Two independently bounded works remain ambiguous even when they
        // share the same TVDB family; never pick one by name or ID ordering.
        db.execute("INSERT INTO episode_map VALUES
            ('8888','74796',1,15,317,342,-316,'')",[]).unwrap();
        assert!(resolve_absolute(&db,"tt0434665",320).unwrap().is_none());
    }

    #[test]fn explicit(){assert_eq!(explicit_pairs(";1-2+3;4-7;"),vec![(1,2),(1,3),(4,7)]);}
    #[test]
    fn explicit_season_mapping_uses_exact_pairs(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
            INSERT INTO episode_map VALUES('123','456',1,2,1,5,0,'10-3;11-4');").unwrap();
        let mapped=resolve_season(&db,"tt1234567",2,3).unwrap().unwrap();
        assert_eq!(mapped.anidb_episode,10);
        assert!(resolve_season(&db,"tt1234567",2,5).unwrap().is_none());
    }

    #[test]
    fn batch_season_mapping_matches_each_individual_coordinate(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
            INSERT INTO episode_map VALUES('123','456',1,2,1,7,0,'');").unwrap();
        let batch=resolve_season_range(&db,"tt1234567",2,1,8).unwrap();
        let singles=(1..=8).map(|ep|resolve_season(&db,"tt1234567",2,ep).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(batch,singles);
        assert!(batch[7].is_none());
    }

    #[test]
    fn batch_absolute_mapping_is_identical_to_individual_resolution(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
            imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
            tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
            INSERT INTO episode_map VALUES('123','456',1,1,1,2,0,'');
            INSERT INTO episode_map VALUES('123','456',1,2,3,5,-2,'');").unwrap();
        let batch=resolve_absolute_range(&db,"tt1234567",1,6).unwrap();
        let individually=(1..=6).map(|n|resolve_absolute(&db,"tt1234567",n).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(batch,individually);
        assert!(batch[5].is_none(),"unmapped episodes must remain unverified");
        assert_eq!(batch[0].as_ref().unwrap().season,1);
        assert_eq!(batch[2].as_ref().unwrap().season,2);
    }

    #[test]
    fn invalid_six_digit_anidb_episodes_are_never_resolved(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES
                ('tt1234567','732','123',0),
                ('tt1234567','436','124',0),
                ('tt1234567','6424','125',0);
            INSERT INTO episode_map VALUES
                ('732','123',1,1,1,0,0,''),
                ('436','124',1,2,1,0,0,'180525-3'),
                ('6424','125',1,3,1,0,-431075,'');").unwrap();

        // Open-ended rows and explicit maps must not leak invalid ASIDs.
        assert!(resolve_season(&db,"tt1234567",1,190315).unwrap().is_none());
        assert!(resolve_season(&db,"tt1234567",2,3).unwrap().is_none());
        assert!(resolve_season(&db,"tt1234567",3,3).unwrap().is_none());
        assert!(resolve_absolute(&db,"tt1234567",190315).unwrap().is_none());
        assert!(resolve_anidb(&db,"732",190315).unwrap().is_empty());

        // Valid episodes still resolve and preserve their AniDB identity.
        let valid=resolve_season(&db,"tt1234567",1,3).unwrap().unwrap();
        assert_eq!(canonical_asid(&valid.anidb_id,valid.anidb_episode).as_deref(),
            Some("732:3"));
        assert!(valid_asid("732:99999"));
        for bad in ["732:190315","436:180525","6424:431078",
                    "732:0","732:-1","732:abc","bad:3","0:3","732:3:4"]{
            assert!(!valid_asid(bad),"{bad}");
        }
    }
    #[test]
    fn explicit_absolute_pairs_override_inferred_bounds(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
            INSERT INTO episode_map VALUES('123','456',1,2,1,5,0,'10-3;11-4');").unwrap();
        let resolved=resolve_absolute(&db,"tt1234567",10).unwrap().unwrap();
        assert_eq!((resolved.season,resolved.episode,resolved.anidb_episode),(2,3,10));
        assert!(resolve_absolute(&db,"tt1234567",12).unwrap().is_none());
    }

    #[test]
    fn split_index_matches_nested_reference_across_tvdb_families(){
        // The precomputed index must preserve the previous O(links*maps)
        // split-cour membership rule, including duplicate and indirect
        // links and unrelated TVDB families.
        let links=vec![
            Link{anidb:"101".into(),tvdb:"700".into(),trakt:1,direct:true},
            Link{anidb:"202".into(),tvdb:"700".into(),trakt:2,direct:true},
            Link{anidb:"303".into(),tvdb:"800".into(),trakt:2,direct:true},
            Link{anidb:"404".into(),tvdb:"700".into(),trakt:0,direct:false},
            Link{anidb:"101".into(),tvdb:"700".into(),trakt:1,direct:true},
        ];
        let maps=vec![
            Mapping{anidb:"101".into(),tvdb:"700".into(),
                tv_season:1,start:1,end:12,offset:0,explicit:vec![]},
            Mapping{anidb:"202".into(),tvdb:"700".into(),
                tv_season:1,start:1,end:12,offset:0,explicit:vec![]},
            Mapping{anidb:"303".into(),tvdb:"800".into(),
                tv_season:1,start:1,end:12,offset:0,explicit:vec![]},
            Mapping{anidb:"404".into(),tvdb:"700".into(),
                tv_season:3,start:1,end:12,offset:0,explicit:vec![]},
        ];
        let index=split_index(&links,&maps);
        for indexed in &index.links{
            assert_eq!(indexed.mappings.len(),indexed.split.len());
            for (&map,&cached) in indexed.mappings.iter().zip(&indexed.split){
                assert_eq!(cached,split(indexed.link,map,&index),
                    "the cached split flag must equal the historical rule");
            }
        }
        for link in &links{
            for map in &maps{
                let previous=link.trakt>0&&links.iter().any(|other|
                    other.anidb!=link.anidb
                    &&other.trakt>0
                    &&other.trakt!=link.trakt
                    &&maps.iter().any(|x|x.anidb==other.anidb
                        &&x.tvdb==map.tvdb&&x.tv_season==map.tv_season));
                assert_eq!(split(link,map,&index),previous,
                    "link={} map={}",link.anidb,map.anidb);
            }
        }
    }

    #[test]
    fn grouped_crosswalk_preserves_original_mapping_order_and_tvdb_filters(){
        // The original O(links*maps) implementation selected mapping rows
        // in database traversal order. An indexed batch must be identical
        // for both direct and indirect TVDB-family links, even when an
        // AniDB identity occurs with multiple TVDB families.
        let links=vec![
            Link{anidb:"101".into(),tvdb:"700".into(),trakt:2,direct:true},
            Link{anidb:"101".into(),tvdb:"800".into(),trakt:2,direct:true},
            Link{anidb:"202".into(),tvdb:"700".into(),trakt:0,direct:false},
            Link{anidb:"303".into(),tvdb:String::new(),trakt:0,direct:true},
            Link{anidb:"999".into(),tvdb:"900".into(),trakt:1,direct:true},
        ];
        let mk=|id:&str,tvdb:&str,season:i64| Mapping{
            anidb:id.into(),tvdb:tvdb.into(),tv_season:season,
            start:1,end:100,offset:0,explicit:Vec::new(),
        };
        let maps=vec![
            mk("101","800",-1),mk("202","700",1),mk("101","700",-1),
            mk("303","100",3),mk("101","700",2),mk("303","200",4),
            mk("101","800",2),mk("202","700",5),mk("101","700",2),
        ];
        let index=split_index(&links,&maps);
        for link in &links {
            let expected=maps.iter()
                .filter(|m|m.anidb==link.anidb
                    &&(link.tvdb.is_empty()||m.tvdb==link.tvdb))
                .map(|m|(m.anidb.as_str(),m.tvdb.as_str(),m.tv_season))
                .collect::<Vec<_>>();
            let indexed=index.links.iter().find(|indexed|
                std::ptr::eq(indexed.link,link)).expect("link retained");
            let actual=indexed.mappings.iter()
                .map(|m|(m.anidb.as_str(),m.tvdb.as_str(),m.tv_season))
                .collect::<Vec<_>>();
            assert_eq!(actual,expected);
        }
        assert!(index.links.iter().find(|x|x.link.anidb=="999")
            .is_some_and(|entry|entry.mappings.is_empty()),
            "a link with no mappings must not borrow another anime's row");
        for link in &links {
            for mapping in &maps {
                let previous=link.trakt>0&&links.iter().any(|other|
                    other.anidb!=link.anidb &&other.trakt>0
                    &&other.trakt!=link.trakt
                    &&maps.iter().any(|m|m.anidb==other.anidb
                        &&m.tvdb==mapping.tvdb
                        &&m.tv_season==mapping.tv_season));
                assert_eq!(split(link,mapping,&index),previous);
            }
        }
    }

    #[test]
    fn hundred_episode_batch_preserves_explicit_pairs_and_ambiguity(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('tt1234567','123','789',0);
            INSERT INTO episode_map VALUES('123','789',1,-1,1,120,0,'');")
            .unwrap();
        let pairs=(1..=120)
            .map(|n|if n==17 {"17-17+18".to_owned()}
                else{format!("{n}-{n}")})
            .collect::<Vec<_>>().join(";");
        db.execute("INSERT INTO episode_map VALUES
            ('123','789',1,2,1,120,0,?1)",rusqlite::params![pairs]).unwrap();

        let season=resolve_season_range(&db,"tt1234567",2,1,120).unwrap();
        let absolute=resolve_absolute_range(&db,"tt1234567",1,120).unwrap();
        assert_eq!(season.len(),120);
        assert_eq!(absolute.len(),120);
        for number in 1..=120 {
            let slot=(number-1) as usize;
            assert_eq!(season[slot],
                resolve_season(&db,"tt1234567",2,number).unwrap(),
                "season episode {number}");
            assert_eq!(absolute[slot],
                resolve_absolute(&db,"tt1234567",number).unwrap(),
                "absolute episode {number}");
        }
        // One AniDB-local episode mapping to two TVDB numbers must never
        // become an arbitrary, first-result-wins SID/ASID selection.
        assert!(season[17].is_none());
        assert!(absolute[16].is_none());
        let mapped=season[24].as_ref().unwrap();
        assert_eq!((mapped.season,mapped.episode,mapped.anidb_episode),(2,25,25));
    }
}
