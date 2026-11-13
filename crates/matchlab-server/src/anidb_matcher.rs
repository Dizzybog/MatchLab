//! Verify AniDB identities and episodes without consulting IMDb metadata.
//! Exact AniDB title aliases come from Tantivy. The relational episode_map
//! provides episode evidence; an IMDb crosswalk is optional and later.
use std::collections::BTreeSet;
use rusqlite::Connection;
use crate::{identifiers,tantivy_search,title_normalization};

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct VerifiedEpisode {
    pub anidb_id:String,
    pub episode:i64,
}
impl VerifiedEpisode {
    pub fn asid(&self)->String {
        format!("{}:{}",self.anidb_id,self.episode)
    }
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct VerifiedRange {
    pub anidb_id:String,
    pub start:i64,
    pub end:i64,
}

#[derive(Debug,Clone)]
struct Mapping {
    season:i64,
    start:i64,
    end:i64,
    offset:i64,
    pairs:Vec<(i64,i64)>,
}

fn pairs(text:&str)->Vec<(i64,i64)> {
    let mut out=Vec::new();
    for part in text.trim_matches(';').split(';'){
        let Some((left,right))=part.trim().split_once('-')else{continue};
        let Ok(ani)=left.parse::<i64>()else{continue};
        if right.split('+').all(|n|!n.is_empty() && n.bytes().all(|b|b.is_ascii_digit())){
            for tv in right.split('+').filter_map(|n|n.parse::<i64>().ok()){
                out.push((ani,tv));
            }
        }
    }
    out
}

/// No IMDb IDs are requested or joined. SQLite is used solely to retrieve
/// a small set of crosswalk rows by AniDB identity, never to search titles.
fn mappings(db:&Connection,anidb_id:&str)->Result<Vec<Mapping>,String>{
    let mut stmt=db.prepare("SELECT tvdb_season,start,end,offset,map FROM episode_map
        WHERE anidb_id=?1 AND anidb_season>0").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([anidb_id],|r|{
        let map=r.get::<_,String>(4)?;
        Ok(Mapping{season:r.get(0)?,start:r.get(1)?,end:r.get(2)?,
            offset:r.get(3)?,pairs:pairs(&map)})
    }).map_err(|e|e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())
}

fn exact_anidb_ids(db:&Connection,title:&str)->Result<BTreeSet<String>,String>{
    let key=title_normalization::clean_title(title);
    if key.is_empty() || (!crate::strict_match_mode()&&key.len()<4){
        return Ok(BTreeSet::new());
    }
    let ids=tantivy_search::anidb_ids_exact(db,&key)?;
    Ok(ids.into_iter()
        .filter(|id|identifiers::canonical_asid(id,1).is_some()).collect())
}

/// An exact named anime part can establish its TVDB season even when the
/// default episode_map row is open-ended. This is *season evidence only*:
/// it does not prove a concrete episode or authorize an independent ASID.
/// Examples include Stone Ocean (season 5) and other named anime cours.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct NamedPart {
    pub anidb_id:String,
    pub season:i64,
}
pub fn named_part_season(db:&Connection,title:&str)->Result<Option<NamedPart>,String>{
    let mut selected=None;
    for id in exact_anidb_ids(db,title)?{
        let seasons=mappings(db,&id)?.into_iter()
            .filter_map(|m|(m.season>1 && m.season<=9_999).then_some(m.season))
            .collect::<BTreeSet<_>>();
        if seasons.len()!=1 {continue;}
        let season=*seasons.iter().next().unwrap();
        if selected.is_some(){return Ok(None);}
        selected=Some(NamedPart{anidb_id:id,season});
    }
    Ok(selected)
}

/// A finite Anime-Lists episode range or exact episode pair is evidence.
/// Never treat an open-ended TVDB season default (end=0) as proof that
/// arbitrary episode numbers exist. Absolute markers with finite limits
/// also qualify, even for an AniDB work not linked to any IMDb series.
fn proves_absolute(rows:&[Mapping],episode:i64)->bool{
    rows.iter().any(|m| {
        // A sparse explicit episode list overrides the broad range.
        // Treating an unlisted episode inside start/end as verified would
        // invent ASIDs that the canonical crosswalk intentionally rejects.
        if !m.pairs.is_empty(){
            m.pairs.iter().any(|(ani,tv)|*ani==episode && *tv>0)
        }else{
            m.start>0 && m.end>=m.start
                && (m.season>0||m.season==-1)
                && (m.start..=m.end).contains(&episode)
        }
    })
}

fn season_episode(rows:&[Mapping],season:i64,episode:i64)->Option<i64>{
    let mut found=BTreeSet::new();
    for m in rows {
        if m.season!=season || m.season<=0 {continue;}
        if !m.pairs.is_empty(){
            found.extend(m.pairs.iter().filter_map(|(ani,tv)|
                (*tv==episode).then_some(*ani)));
        }else if m.start>0 && m.end>=m.start {
            let ani=episode-m.offset;
            if (m.start..=m.end).contains(&ani){found.insert(ani);}
        }
    }
    (found.len()==1).then(||*found.iter().next().unwrap())
}

/// Exact title + independently proven absolute AniDB episode.
/// Multiple distinct AniDB works matching the same alias and episode are
/// ambiguous; many IMDb links to a single AniDB work are not.
pub fn match_absolute(
    db:&Connection,title:&str,episode:i64,
)->Result<Option<VerifiedEpisode>,String>{
    if !(1..=99_999).contains(&episode){return Ok(None);}
    let mut winner=None;
    for id in exact_anidb_ids(db,title)?{
        if identifiers::canonical_asid(&id,episode).is_none(){continue;}
        if !proves_absolute(&mappings(db,&id)?,episode){continue;}
        if winner.is_some(){return Ok(None);}
        winner=Some(VerifiedEpisode{anidb_id:id,episode});
    }
    Ok(winner)
}

/// Match a season-relative TVDB episode to AniDB without an IMDb identity.
/// Only bounded rows or explicit episode pairs can establish the ASID.
pub fn match_season(
    db:&Connection,title:&str,season:i64,episode:i64,
)->Result<Option<VerifiedEpisode>,String>{
    if !(1..=9_999).contains(&season)||!(1..=99_999).contains(&episode){
        return Ok(None);
    }
    let mut winner=None;
    for id in exact_anidb_ids(db,title)?{
        let Some(ani)=season_episode(&mappings(db,&id)?,season,episode)else{continue};
        if identifiers::canonical_asid(&id,ani).is_none(){continue;}
        if winner.is_some(){return Ok(None);}
        winner=Some(VerifiedEpisode{anidb_id:id,episode:ani});
    }
    Ok(winner)
}

/// All episodes of an inferred absolute batch must be verified against the
/// *same* AniDB work. Load each work's mappings once, not once per episode.
pub fn match_absolute_range(
    db:&Connection,title:&str,start:i64,end:i64,
)->Result<Option<VerifiedRange>,String>{
    if start<1 || end<start || end-start>=1_000
        || end>99_999 {return Ok(None);}
    let mut winner=None;
    for id in exact_anidb_ids(db,title)?{
        if identifiers::canonical_asid(&id,end).is_none(){continue;}
        let maps=mappings(db,&id)?;
        if !(start..=end).all(|ep|proves_absolute(&maps,ep)){continue;}
        if winner.is_some(){return Ok(None);}
        winner=Some(VerifiedRange{anidb_id:id,start,end});
    }
    Ok(winner)
}

#[cfg(test)]
mod tests{
    use super::*;
    fn setup()->Connection{
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO anidb_titles VALUES
                ('2369','Bleach','bleach','main','en'),
                ('700','Independent Anime','independentanime','main','en'),
                ('701','Ambiguous Show','ambiguousshow','main','en'),
                ('702','Ambiguous Show','ambiguousshow','main','en'),
                ('800','Unbounded','unbounded','main','en'),
                ('900','Sparse OVA','sparseova','main','en'),
                ('16187','Jojo Stone Ocean','jojostoneocean','main','en');
            INSERT INTO episode_map VALUES
                ('2369','74796',1,-1,1,366,0,''),
                ('2369','74796',1,15,317,342,-316,''),
                ('700','custom',1,-1,1,10,0,''),
                ('700','custom',1,1,1,10,0,''),
                ('701','other',1,1,1,12,0,''),
                ('702','other',1,1,1,12,0,''),
                ('800','other',1,1,1,0,0,''),
                ('900','other',1,1,1,10,0,'1-1;3-3;'),
                -- Real Anime-Lists season evidence: an open-ended default
                -- for JoJo Stone Ocean, TVDB season 5.
                ('16187','262954',1,5,1,0,0,'');
            INSERT INTO anime_series_map VALUES
                ('tt0434665','2369','74796',0),
                ('tt1105263','2369','74796',0),
                ('tt1148261','2369','74796',0),
                ('tt1339302','2369','74796',0),
                ('tt1785394','2369','74796',0);").unwrap();
        db
    }
    #[test]
    fn identifies_bleach_despite_five_imdb_links(){
        let db=setup();
        assert_eq!(match_absolute(&db,"Bleach",320).unwrap(),
            Some(VerifiedEpisode{anidb_id:"2369".into(),episode:320}));
        assert_eq!(match_absolute_range(&db,"Bleach",320,342).unwrap(),
            Some(VerifiedRange{anidb_id:"2369".into(),start:320,end:342}));
        assert_eq!(match_absolute_range(&db,"Bleach",320,400).unwrap(),None);
    }
    #[test]
    fn anidb_without_imdb_can_resolve_and_ambiguous_titles_cannot(){
        let db=setup();
        assert_eq!(match_absolute(&db,"Independent Anime",7).unwrap(),
            Some(VerifiedEpisode{anidb_id:"700".into(),episode:7}));
        assert_eq!(match_season(&db,"Independent Anime",1,7).unwrap(),
            Some(VerifiedEpisode{anidb_id:"700".into(),episode:7}));
        assert_eq!(match_absolute(&db,"Ambiguous Show",7).unwrap(),None);
        assert_eq!(match_season(&db,"Ambiguous Show",1,7).unwrap(),None);
        assert_eq!(match_absolute(&db,"Unbounded",320).unwrap(),None);
        assert_eq!(match_absolute(&db,"Sparse OVA",3).unwrap(),
            Some(VerifiedEpisode{anidb_id:"900".into(),episode:3}));
        assert_eq!(match_absolute(&db,"Sparse OVA",2).unwrap(),None);
        assert_eq!(match_absolute_range(&db,"Sparse OVA",1,3).unwrap(),None);
        assert_eq!(match_absolute(&db,"Independent Anime",11).unwrap(),None);
        assert_eq!(match_absolute(&db,"Independent Anime",0).unwrap(),None);
        assert_eq!(named_part_season(&db,"Jojo Stone Ocean").unwrap(),
            Some(NamedPart{anidb_id:"16187".into(),season:5}));
        // Season knowledge is not sufficient proof of episode 27:
        // no bounded AniDB episode range exists for this part.
        assert_eq!(match_absolute(&db,"Jojo Stone Ocean",27).unwrap(),None);
    }
}
