//! Port of the episode-map rules in MediaMatch/anime_resolver.py.
//! Only mapping tables establish episode identity; never infer it from IMDb AKAs.
use rusqlite::{params,Connection,OptionalExtension};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone,Debug)]
struct Link {anidb:String,tvdb:String,trakt:i64}
#[derive(Clone,Debug)]
struct Mapping {anidb:String,tvdb:String,ani_season:i64,tv_season:i64,start:i64,end:i64,offset:i64,map:String}
#[derive(Clone,Debug,PartialEq,Eq,Serialize)]
pub struct Episode {pub imdb_id:String,pub season:i64,pub episode:i64,pub anidb_id:String,pub anidb_episode:i64}
fn links(db:&Connection,imdb:&str)->rusqlite::Result<Vec<Link>>{
    let mut stmt=db.prepare("SELECT anidb_id,tvdb_id,trakt_season FROM anime_series_map WHERE imdb_id=?1")?;
    let mut out=stmt.query_map([imdb],|r|Ok(Link{anidb:r.get(0)?,tvdb:r.get(1)?,trakt:r.get(2)?}))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if out.is_empty(){
        let mut stmt=db.prepare("SELECT DISTINCT value FROM anime_ids WHERE group_id=?1 AND type='anidb'")?;
        let ids=stmt.query_map([imdb],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len()!=1{return Ok(vec![]);}
        out.push(Link{anidb:ids[0].clone(),tvdb:String::new(),trakt:0});
    }
    let tvdbs:BTreeSet<String>=out.iter().filter(|l|!l.tvdb.is_empty()).map(|l|l.tvdb.clone()).collect();
    for tvdb in tvdbs {
        let mut stmt=db.prepare("SELECT DISTINCT anidb_id FROM episode_map WHERE tvdb_id=?1")?;
        for id in stmt.query_map([&tvdb],|r|r.get::<_,String>(0))? {
            let id=id?;
            if !out.iter().any(|l|l.anidb==id&&l.tvdb==tvdb){
                out.push(Link{anidb:id,tvdb:tvdb.clone(),trakt:0});
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
        for row in stmt.query_map([id],|r|Ok(Mapping{anidb:r.get(0)?,tvdb:r.get(1)?,
            ani_season:r.get(2)?,tv_season:r.get(3)?,start:r.get(4)?,end:r.get(5)?,
            offset:r.get(6)?,map:r.get(7)?}))?{
            let m=row?;
            if include_absolute||m.tv_season>0{out.push(m);}
        }
    }
    Ok(out)
}
fn split(link:&Link,m:&Mapping,links:&[Link],maps:&[Mapping])->bool{
    link.trakt>0&&links.iter().any(|other|other.anidb!=link.anidb&&other.trakt>0&&other.trakt!=link.trakt
        &&maps.iter().any(|x|x.anidb==other.anidb&&x.tvdb==m.tvdb&&x.tv_season==m.tv_season))
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
    if season<=0||episode<=0{return Ok(None);}
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,false)?;
    let mut candidates=vec![];
    for link in &links {for m in &maps{
        if m.anidb!=link.anidb||(!link.tvdb.is_empty()&&link.tvdb!=m.tvdb){continue;}
        let is_split=split(link,m,&links,&maps);
        if is_split&&link.trakt!=season{continue;}
        if !is_split&&m.tv_season!=season{continue;}
        if !is_split{
            let explicit=explicit_pairs(&m.map);
            if !explicit.is_empty(){
                for (ani,tv) in explicit{
                    if tv==episode{candidates.push((m.offset,m.start,link.anidb.clone(),ani));}
                }
                continue;
            }
        }
        let ani=if is_split{episode+m.start.max(1)-1}else{episode-m.offset};
        if ani<m.start.max(1)||(m.end>0&&ani>m.end){continue;}
        candidates.push((m.offset,m.start,link.anidb.clone(),ani));
    }}
    candidates.sort_by(|a,b|b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let Some(best)=candidates.first()else{return Ok(None)};
    if candidates.get(1).is_some_and(|second|second.0==best.0&&second.1==best.1&&second.2..!=best.2..){
        return Ok(None);
    }
    Ok(Some(Episode{imdb_id:imdb.into(),season,episode,anidb_id:best.2.clone(),anidb_episode:best.3}))
}
pub fn resolve_absolute(db:&Connection,imdb:&str,episode:i64)->rusqlite::Result<Option<Episode>>{
    if episode<=0{return Ok(None);}
    let links=links(db,imdb)?;
    let maps=mappings(db,&links,true)?;
    let mut candidates=BTreeSet::new();
    for link in &links{
        let associated:Vec<_>=maps.iter().filter(|m|m.anidb==link.anidb&&(link.tvdb.is_empty()||m.tvdb==link.tvdb)).collect();
        let absolutes:Vec<_>=associated.iter().filter(|m|m.tv_season==-1).collect();
        let offset=if absolutes.len()==1{absolutes[0].offset}else{0};
        let local=episode-offset;
        if local<=0||absolutes.first().is_some_and(|m|m.end>0&&local>m.end){continue;}
        for m in &associated{
            if m.tv_season<=0||local<m.start.max(1)||(m.end>0&&local>m.end){continue;}
            let is_split=split(link,m,&links,&maps);
            let season=if is_split{link.trakt}else{m.tv_season};
            if is_split{
                let tv=local-m.start.max(1)+1;
                if season>0&&tv>0{candidates.insert((link.anidb.clone(),local,season,tv));}
            }else{
                let explicit=explicit_pairs(&m.map);
                if !explicit.is_empty(){
                    for (ani,tv) in explicit{
                        if ani==local&&season>0&&tv>0{candidates.insert((link.anidb.clone(),local,season,tv));}
                    }
                }else{
                    let tv=local+m.offset;
                    if season>0&&tv>0{candidates.insert((link.anidb.clone(),local,season,tv));}
                }
            }
        }
    }
    if candidates.len()!=1{return Ok(None);}
    let (anidb_id,anidb_episode,season,episode)=candidates.into_iter().next().unwrap();
    Ok(Some(Episode{imdb_id:imdb.into(),season,episode,anidb_id,anidb_episode}))
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn explicit(){assert_eq!(explicit_pairs(";1-2+3;4-7;"),vec![(1,2),(1,3),(4,7)]);}
}
