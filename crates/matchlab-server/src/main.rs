//! Native Rust matching service backed by validated SQLite metadata.
mod imdb_v4;
mod metadata_import;
mod metadata_download;
mod anime_resolver;
mod anime_title;
mod country_aliases;
mod dashboard_history;
use axum::{extract::{Query, State}, http::{StatusCode,header}, routing::{get,post}, Json, Router};
use rusqlite::{Connection, OpenFlags};
#[cfg(test)]
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, path::PathBuf, sync::{Arc,Mutex,OnceLock}, time::{Instant,Duration}, collections::HashMap};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone)]
struct AppState { db: Arc<PathBuf>, history: Arc<PathBuf> }

#[derive(Debug, Deserialize)]
struct MatchRequest {
    #[serde(alias = "t_title")]
    title: String,
    year: Option<i32>,
    series: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct Candidate {
    imdb_id: String,
    title: String,
    year: i32,
    media_type: String,
    matched_title: String,
    anime_ids: std::collections::BTreeMap<String,String>,
    source: &'static str,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    parsed: Value,
    normalized_title: String,
    imdb_candidates: Vec<Candidate>,
    anime_candidate: Option<Value>,
    selected: Option<Value>,
    reason: String,
}

// Shared, bounded 30-second parser cache for /parse and /match.
const PARSE_TTL:Duration=Duration::from_secs(30);
static PARSE_CACHE:OnceLock<Mutex<HashMap<String,(Instant,Value)>>>=OnceLock::new();
fn parse_cached(title:&str)->Result<Value,String>{
    if title.trim().is_empty()||title.len()>4096 {
        return Err("title must be 1–4096 bytes".into());
    }
    let cache=PARSE_CACHE.get_or_init(||Mutex::new(HashMap::new()));
    if let Ok(guard)=cache.lock(){
        if let Some((at,value))=guard.get(title){
            if at.elapsed()<PARSE_TTL{return Ok(value.clone());}
        }
    }
    let value=Value::Object(ptt_core::parse_title(title,false).map_err(|e|e.to_string())?);
    if let Ok(mut guard)=cache.lock(){
        guard.retain(|_,(at,_)|at.elapsed()<PARSE_TTL);
        if guard.len()>=1024{guard.clear();}
        guard.insert(title.to_string(),(Instant::now(),value.clone()));
    }
    Ok(value)
}

fn normalize(input: &str) -> String {
    let lowered = input.to_lowercase()
        .replace('æ', "ae").replace('ø', "o").replace('œ', "oe").replace('ß', "ss");
    let ascii: String = lowered.nfkd().filter(|c| c.is_ascii()).collect();
    let words = ascii.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty() && *part != "and");
    words.collect()
}

fn numbers(v: Option<&Value>) -> Vec<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().into_iter().collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_i64).collect(),
        _ => vec![],
    }
}

fn explicit_season_range(raw:&str)->Option<(i64,i64)>{
    let leaf=raw.rsplit(&['/','\\'][..]).next().unwrap_or(raw);
    let words:Vec<_>=leaf.split(|c:char|!c.is_ascii_alphanumeric()&&c!='-')
        .collect();
    for word in words {
        let lower=word.to_ascii_lowercase();
        if !lower.starts_with('s'){continue;}
        let Some((left,right))=lower.split_once("-s") else {continue};
        if left.len()<2||left.len()>4||right.is_empty()||right.len()>3{continue;}
        let (Some(start),Ok(end))=(left[1..].parse::<i64>().ok(),right.parse::<i64>()) else {continue};
        if start>0&&end>=start&&end-start<=100{return Some((start,end));}
    }
    None
}

/// Explicit absolute episode notation used by the Python matcher.
fn explicit_absolute_episode(raw:&str)->Option<i64>{
    let words:Vec<_>=raw.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|x|!x.is_empty()).collect();
    for pair in words.windows(2) {
        if (pair[0].eq_ignore_ascii_case("episode")||pair[0].eq_ignore_ascii_case("ep"))
            && pair[1].len()<=5 && pair[1].bytes().all(|b|b.is_ascii_digit()) {
            let n=pair[1].parse::<i64>().ok()?;
            if n>0{return Some(n);}
        }
    }
    None
}

fn open_db(path: &PathBuf) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.create_scalar_function("reverse", 1, rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC | rusqlite::functions::FunctionFlags::SQLITE_UTF8, |ctx| {
        let value: String = ctx.get(0)?;
        Ok(value.chars().rev().collect::<String>())
    })?;
    Ok(conn)
}

#[cfg(test)]
fn candidates(db: &Connection, key: &str, year: Option<i32>, series: bool) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = db.prepare(
        "SELECT imdb_id,title,year,type,clean_title FROM titles
         WHERE (clean_title=?1 OR (?1 LIKE clean_title || '%' AND length(clean_title)>=6))
         AND year>0 AND type IN ('movie','tvMovie','short','tvShort','tvSpecial','tvSeries','tvMiniSeries')
         ORDER BY CASE WHEN clean_title=?1 THEN 0 ELSE 1 END,
                  length(clean_title) DESC, abs(year-coalesce(?2,year)),
                  CASE type WHEN 'movie' THEN 0 WHEN 'tvMovie' THEN 1 WHEN 'short' THEN 2 ELSE 3 END,
                  ordering ASC LIMIT 60"
    )?;
    let rows = stmt.query_map(params![key,year], |r| {
        Ok(Candidate {imdb_id:r.get(0)?,title:r.get(1)?,year:r.get(2)?,
            media_type:r.get(3)?,matched_title:r.get(4)?,anime_ids:Default::default(),source:"imdb"})
    })?;
    let mut result = Vec::new();
    for row in rows {
        let c = row?;
        let is_series = c.media_type == "tvSeries" || c.media_type == "tvMiniSeries";
        if is_series == series && !result.iter().any(|x: &Candidate| x.imdb_id == c.imdb_id) {
            result.push(c);
        }
        if result.len() >= 20 { break; }
    }
    Ok(result)
}

fn anime_candidate(db: &Connection, key: &str, season: Option<i64>, episode: i64) -> rusqlite::Result<Option<Value>> {
    if key.len() < 4 || episode <= 0 { return Ok(None); }
    let mut stmt = db.prepare(
        "SELECT DISTINCT a.anidb_id,a.clean_title,l.imdb_id
         FROM anidb_titles a JOIN anime_series_map l ON l.anidb_id=a.anidb_id
         WHERE a.clean_title=?1 OR (length(a.clean_title)>=6 AND ?1 LIKE a.clean_title||'%')
         ORDER BY CASE WHEN a.clean_title=?1 THEN 0 ELSE 1 END,
                  length(a.clean_title) DESC LIMIT 40"
    )?;
    let rows = stmt.query_map([key], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?;
    let mut matches = Vec::new();
    for row in rows { matches.push(row?); }
    if matches.is_empty() { return Ok(None); }
    let exact = matches[0].1 == key;
    let best_len = matches[0].1.len();
    let best: Vec<_> = matches.iter().filter(|(_, alias, _)| (alias == key) == exact && alias.len() == best_len).collect();
    if best.iter().any(|(_,_,imdb)| imdb != &best[0].2) { return Ok(None); }
    let imdb = &best[0].2;
    // Resolve only through verified anime crosswalk tables, including explicit
    // mappings, split seasons and absolute numbering.
    let mapped = match season {
        Some(season) if season > 0 => {
            let titled=anime_title::resolve(db,imdb,key,season,episode)?;
            if titled.is_some() { titled }
            else if anime_title::has_specific_alias(db,imdb,key)? { None }
            else { anime_resolver::resolve_season(db,imdb,season,episode)? }
        },
        None => anime_resolver::resolve_absolute(db,imdb,episode)?,
        _ => None,
    };
    Ok(mapped.map(|m|json!({"imdb_id":m.imdb_id,"season":m.season,
        "episode":m.episode,"anidb_id":m.anidb_id,"anidb_episode":m.anidb_episode})))
}

/// Resolve a title-only anime identity from anime tables, without inventing
/// season or episode coordinates. Exact aliases precede longest prefixes.
fn anime_identity(db:&Connection,key:&str)->rusqlite::Result<Option<String>>{
    if key.len()<4{return Ok(None);}
    let mut stmt=db.prepare("SELECT DISTINCT a.clean_title,l.imdb_id
        FROM anidb_titles a JOIN anime_series_map l ON l.anidb_id=a.anidb_id
        WHERE a.clean_title=?1 OR (length(a.clean_title)>=6 AND ?1 LIKE a.clean_title||'%')
        ORDER BY CASE WHEN a.clean_title=?1 THEN 0 ELSE 1 END,
                 length(a.clean_title) DESC,l.imdb_id LIMIT 100")?;
    let rows=stmt.query_map([key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some((best_alias,best_imdb))=rows.first()else{return Ok(None)};
    let exact=best_alias==key;
    let size=best_alias.len();
    if rows.iter().any(|(alias,id)|(alias==key)==exact&&alias.len()==size&&id!=best_imdb){
        return Ok(None);
    }
    Ok(Some(best_imdb.clone()))
}

/// Resolve every absolute episode independently: an anime range may span
/// several IMDb seasons. Never interpolate across missing crosswalk entries.
fn mapped_absolute_range(db:&Connection,imdb:&str,start:i64,end:i64)->Result<Value,String>{
    let mut segments:Vec<Value>=Vec::new();
    let mut missing=0i64;
    let mut first:Option<anime_resolver::Episode>=None;
    let mut last:Option<anime_resolver::Episode>=None;
    for absolute in start..=end {
        let mapped=anime_resolver::resolve_absolute(db,imdb,absolute)
            .map_err(|e|e.to_string())?;
        if let Some(ep)=mapped {
            if absolute==start{first=Some(ep.clone());}
            if absolute==end{last=Some(ep.clone());}
            let continues=segments.last().is_some_and(|seg|
                seg["season"].as_i64()==Some(ep.season)
                &&seg["end_absolute"].as_i64()==Some(absolute-1)
                &&seg["end_episode"].as_i64()==Some(ep.episode-1)
                &&seg["anidb_id"].as_str()==Some(ep.anidb_id.as_str())
                &&seg["end_anime_episode"].as_i64()==Some(ep.anidb_episode-1));
            if continues {
                let tail=segments.last_mut().expect("checked above");
                tail["end_absolute"]=json!(absolute);
                tail["end_episode"]=json!(ep.episode);
                tail["end_anime_episode"]=json!(ep.anidb_episode);
            }else{
                segments.push(json!({"season":ep.season,"start_absolute":absolute,
                    "end_absolute":absolute,"start_episode":ep.episode,
                    "end_episode":ep.episode,"anidb_id":ep.anidb_id,
                    "start_anime_episode":ep.anidb_episode,
                    "end_anime_episode":ep.anidb_episode}));
            }
        }else{missing+=1;}
    }
    Ok(json!({"absolute_start":start,"absolute_end":end,
        "start_season":first.as_ref().map(|e|e.season),
        "start_episode":first.as_ref().map(|e|e.episode),
        "end_season":last.as_ref().map(|e|e.season),
        "end_episode":last.as_ref().map(|e|e.episode),
        "segments":segments,"unmapped_count":missing,
        "complete":missing==0}))
}

/// Use the highest observed episode number, not the count: IMDb
/// episode numbers can contain gaps. Only applies to season-only releases.
/// Reject candidates whose imported IMDb season cannot contain the
/// explicitly requested episode. Missing season metadata is not evidence
/// that an episode exists.
fn candidate_has_episode(db:&Connection,imdb:&str,season:i64,episode:i64)->Result<bool,String>{
    if season<=0||episode<=0{return Ok(true);}
    // Unknown bounds cannot prove the episode is invalid. Only reject when
    // IMDb metadata explicitly establishes a lower maximum episode number.
    Ok(season_episode_end(db,imdb,season)?.is_none_or(|max|episode<=max))
}

fn season_episode_end(db:&Connection,imdb:&str,season:i64)->Result<Option<i64>,String>{
    match db.query_row("SELECT max_episode FROM season_episodes
        WHERE imdb_id=?1 AND season=?2",rusqlite::params![imdb,season],
        |row|row.get::<_,i64>(0)){
        Ok(n) if n>0=>Ok(Some(n)),
        Ok(_)|Err(rusqlite::Error::QueryReturnedNoRows)=>Ok(None),
        Err(e)=>Err(e.to_string())
    }
}

fn diagnose(db_path: &PathBuf, req: &MatchRequest) -> Result<Diagnostic,String> {
    if req.title.trim().is_empty() || req.title.len() > 4096 { return Err("title must be 1–4096 bytes".into()); }
    let parsed_value=parse_cached(&req.title)?;
    let parsed=parsed_value.as_object().ok_or("parser returned no object")?;
    let parsed_title = parsed.get("title").and_then(Value::as_str).unwrap_or("");
    let title_owned = if explicit_absolute_episode(&req.title).is_some() && numbers(parsed.get("seasons")).is_empty() {
        let parts:Vec<_>=parsed_title.split(|c:char|!c.is_ascii_alphanumeric()).filter(|x|!x.is_empty()).collect();
        let mut filtered=Vec::new();
        let mut i=0;
        while i<parts.len(){
            if i+1<parts.len()&&(parts[i].eq_ignore_ascii_case("episode")||parts[i].eq_ignore_ascii_case("ep"))
                &&parts[i+1].len()<=5&&parts[i+1].bytes().all(|b|b.is_ascii_digit()) {i+=2;}
            else {filtered.push(parts[i]);i+=1;}
        }
        filtered.join(" ")
    }else{parsed_title.to_string()};
    let title=title_owned.as_str();
    let key = normalize(title);
    let season_range=explicit_season_range(&req.title);
    let seasons = season_range.map(|(start,end)|vec![start,end])
        .unwrap_or_else(||numbers(parsed.get("seasons")));
    let mut episodes = numbers(parsed.get("episodes"));
    if episodes.is_empty() {
        if let Some(ep)=explicit_absolute_episode(&req.title){episodes.push(ep);}
    }
    let year = req.year.or_else(||parsed.get("year").and_then(Value::as_i64).map(|n|n as i32));
    let db = open_db(db_path).map_err(|e|e.to_string())?;
    // An exact anime alias can identify a series even without SxxExx tokens.
    // Respect an explicitly supplied series=false override.
    let series = match req.series {
        Some(value) => value,
        None if !seasons.is_empty() || !episodes.is_empty() => true,
        // A release with an explicit production year and no episode/season
        // coordinates is a movie candidate, even if an anime alias exists.
        None if year.is_some() => false,
        None => anime_title::season_from_any_exact_title(&db,&key)
            .map_err(|e|e.to_string())?.is_some(),
    };
    let imdb = imdb_v4::search(&db,title,year,series,seasons.iter().copied().max().unwrap_or(0),&req.title,!episodes.is_empty(),20)
        .map_err(|e|e.to_string())?;
    // Require every candidate for a multi-season pack to contain the final season.
    let imdb=if let Some((_,end))=season_range {
        imdb.into_iter().filter(|candidate|{
            db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
                [&candidate.imdb_id],|row|row.get::<_,i64>(0))
                .is_ok_and(|max|max>=end)
        }).collect::<Vec<_>>()
    }else{imdb};
    let imdb=if seasons.len()==1 && episodes.len()==1 {
        let season=seasons[0];
        let episode=episodes[0];
        imdb.into_iter().filter_map(|candidate|
            match candidate_has_episode(&db,&candidate.imdb_id,season,episode) {
                Ok(true)=>Some(candidate),
                Ok(false)=>None,
                Err(error)=>{
                    tracing::warn!(imdb_id=%candidate.imdb_id,%error,
                        "season episode metadata unavailable for candidate");
                    None
                }
            }
        ).collect::<Vec<_>>()
    }else{imdb};
    let imdb: Vec<Candidate> = imdb.into_iter().map(|m|Candidate{
        imdb_id:m.imdb_id,title:m.title,year:m.year,media_type:m.media_type,
        matched_title:m.matched_title,anime_ids:m.anime_ids,source:"imdb"}).collect();
    let anime = if series {
        if let Some(ep)=episodes.first(){
            anime_candidate(&db,&key,seasons.first().copied(),*ep)
                .map_err(|e|e.to_string())?
        }else{
            anime_identity(&db,&key).map_err(|e|e.to_string())?
                .map(|id|{
                    let inferred=if seasons.is_empty() {
                        anime_title::season_from_title(&db,&id,title).ok().flatten()
                    } else {None};
                    json!({"imdb_id":id,"season":seasons.first().copied().or(inferred),
                        "episode":null,"anidb_id":null,"anidb_episode":null})
                })
        }
    } else { None };
    // An unrelated anime with the same alias must not override an exact
    // IMDb TV-series title. Only a matching IMDb identity can take priority.
    // A different anime identity is still eligible when IMDb has no exact
    // series-title candidate (e.g. a distinct anime part title).
    let anime=anime.filter(|a|{
        season_range.is_none_or(|(_,end)|{
            a["imdb_id"].as_str().is_some_and(|id|
                db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
                    [id],|row|row.get::<_,i64>(0))
                    .is_ok_and(|max|max>=end))
        })
    });
    let anime = anime.filter(|a| {
        let anime_id=a["imdb_id"].as_str().unwrap_or_default();
        !imdb.iter().any(|c| {
            (c.media_type=="tvSeries" || c.media_type=="tvMiniSeries")
                && normalize(&c.matched_title)==key && c.imdb_id!=anime_id
        })
    });
    let mut selected = if let Some(a) = &anime {
        let imdb_id = a["imdb_id"].as_str().unwrap_or_default();
        let season = a["season"].as_i64();
        let episode = a["episode"].as_i64();
        let anidb_id = a["anidb_id"].as_str();
        let ani_episode = a["anidb_episode"].as_i64();
        let anime_details=if let (Some(aid),Some(aep))=(anidb_id,ani_episode){
            json!({"anidb_id":aid,"episode":aep,"asid":format!("{aid}:{aep}")})
        }else{Value::Null};
        let sid=if let (Some(se),Some(ep))=(season,episode){
            format!("{imdb_id}:{se}:{ep}")
        }else{imdb_id.to_string()};
        let anime_ids=if let Some(aid)=anidb_id{
            json!({"anidb":aid})
        }else{json!({})};
        Some(json!({"imdb_id":imdb_id,"season":season,"episode":episode,
            "sid":sid,"anime":anime_details,
            "match_source":"anime_priority","title":title,"year":year.unwrap_or(0),
            "category":"series","matched_title":title,
            "anime_ids":anime_ids,"parsed":parsed_value.clone()}))
    } else {
        // Python match_title refuses ambiguous IMDb identities rather than
        // silently returning the first equally plausible candidate.
        // Always select a candidate. A verified anime crosswalk takes priority;
        // otherwise IMDb V4 ranking (including stable IMDb ID tie-break) wins.
        let mut chosen_index=0usize;
        if series {
            if let Some(ep)=episodes.first().copied() {
                for (index,candidate) in imdb.iter().enumerate() {
                    let mapped=match seasons.first().copied() {
                        Some(season) => {
                            let titled=anime_title::resolve(&db,&candidate.imdb_id,title,season,ep)
                                .map_err(|e|e.to_string())?;
                            if titled.is_some(){titled}
                            else if anime_title::has_specific_alias(&db,&candidate.imdb_id,title)
                                .map_err(|e|e.to_string())? {None}
                            else {anime_resolver::resolve_season(&db,&candidate.imdb_id,season,ep)
                                .map_err(|e|e.to_string())?}
                        }
                        None => anime_resolver::resolve_absolute(&db,&candidate.imdb_id,ep)
                            .map_err(|e|e.to_string())?,
                    };
                    if mapped.is_some() {
                        chosen_index=index;
                        break;
                    }
                }
            }
        }
        let chosen=imdb.get(chosen_index);
        if let Some(c)=chosen {
            let mut selected_season=seasons.first().copied();
            if series && selected_season.is_none() && episodes.is_empty() {
                selected_season=anime_title::season_from_title(&db,&c.imdb_id,title)
                    .map_err(|e|e.to_string())?;
            }
            let mut selected_episode=episodes.first().copied();
            let mut anime_result=Value::Null;

            if let Some(ep)=selected_episode {
                let mapped=if let Some(season)=selected_season {
                    let titled=anime_title::resolve(&db,&c.imdb_id,title,season,ep)
                        .map_err(|e|e.to_string())?;
                    if titled.is_some(){titled}
                    else if anime_title::has_specific_alias(&db,&c.imdb_id,title)
                        .map_err(|e|e.to_string())? {None}
                    else {anime_resolver::resolve_season(&db,&c.imdb_id,season,ep)
                        .map_err(|e|e.to_string())?}
                }else{
                    anime_resolver::resolve_absolute(&db,&c.imdb_id,ep)
                        .map_err(|e|e.to_string())?
                };
                if let Some(mapped)=mapped{
                    selected_season=Some(mapped.season);
                    selected_episode=Some(mapped.episode);
                    anime_result=json!({"anidb_id":mapped.anidb_id,
                        "episode":mapped.anidb_episode,
                        "asid":format!("{}:{}",mapped.anidb_id,mapped.anidb_episode)});
                } else if !c.anime_ids.is_empty() ||
                    anime_title::has_specific_alias(&db,&c.imdb_id,title)
                        .map_err(|e|e.to_string())? {
                    // Anime identity can be valid without a verified episode
                    // crosswalk. Do not publish the raw release S/E as canonical.
                    selected_season=None;
                    selected_episode=None;
                }
            }
            Some(json!({"imdb_id":c.imdb_id,"title":c.title,"year":c.year,
                    "category":if series {"series"} else {"movie"},
                    "season":selected_season,"episode":selected_episode,
                    "sid":if let (Some(se),Some(ep))=(selected_season,selected_episode){
                        format!("{}:{se}:{ep}",c.imdb_id)
                    }else{c.imdb_id.clone()},
                    "anime":anime_result,"match_source":"imdb",
                    "matched_title":c.matched_title,"anime_ids":c.anime_ids,
                    "parsed":parsed_value.clone()}))
        }else{None}
    };
    // A season pack such as Supernatural.S02 covers the season's episodes.
    // A specific S02E01 must never be expanded to the entire season.
    if let (Some((start,end)),Some(winner))=(season_range,selected.as_mut()){
        winner["season"]=json!(start);
        winner["season_end"]=json!(end);
        winner["episode"]=json!(1);
        winner["episode_end"]=winner["imdb_id"].as_str()
            .and_then(|id|season_episode_end(&db,id,end).ok().flatten())
            .map_or(Value::Null,|n|json!(n));
        winner["episode_range"]=json!({"start_season":start,"end_season":end,
            "source":"imdb_season_pack"});
    }
    if episodes.is_empty() && seasons.len()==1 {
        if let Some(winner)=selected.as_mut(){
            if let Some(imdb_id)=winner["imdb_id"].as_str().map(str::to_owned){
                let season=seasons[0];
                if let Some(end)=season_episode_end(&db,&imdb_id,season)?{
                    winner["season"]=json!(season);
                    winner["episode"]=json!(1);
                    winner["season_end"]=json!(season);
                    winner["episode_end"]=json!(end);
                    winner["episode_range"]=json!({"start_season":season,"start_episode":1,
                        "end_season":season,"end_episode":end,
                        "source":"imdb_season_episodes"});
                    winner["sid"]=json!(format!("{imdb_id}:{season}:1"));
                }
            }
        }
    }
    if let (Some(start),Some(end),Some(winner))=(
        parsed.get("episode_start").and_then(Value::as_i64),
        parsed.get("episode_end").and_then(Value::as_i64),
        selected.as_mut()
    ){
        if let Some(imdb_id)=winner["imdb_id"].as_str().map(str::to_owned){
            let range=mapped_absolute_range(&db,&imdb_id,start,end)?;
            // Keep the raw absolute endpoints even when the metadata mapping
            // is incomplete; never claim inferred IMDb season coordinates.
            winner["episode_range"]=range.clone();
            winner["season"]=range["start_season"].clone();
            winner["episode"]=range["start_episode"].clone();
            winner["season_end"]=range["end_season"].clone();
            winner["episode_end"]=range["end_episode"].clone();
            winner["sid"]=if let (Some(se),Some(ep))=(
                range["start_season"].as_i64(),range["start_episode"].as_i64()){
                json!(format!("{imdb_id}:{se}:{ep}"))
            }else{json!(imdb_id)};
            // An absolute range can span seasons; a single ASID is not an
            // adequate representation of the entire release.
            // Absolute episode numbers are meaningful to anime viewers even
            // when the AniDB/TVDB mapping crosses parts or seasons.
            winner["anime_episode_start"]=json!(start);
            winner["anime_episode_end"]=json!(end);
            winner["anime"]=Value::Null;
        }
    }
    let reason = if anime.is_some() {"Verified anime crosswalk"} else if selected.is_some() {
        "IMDb title candidate (experimental matching rules)"
    } else {"No eligible candidate"};
    Ok(Diagnostic{parsed: parsed_value,normalized_title:key,imdb_candidates:imdb,anime_candidate:anime,
        selected,reason:reason.into()})
}

fn diagnose_logged(state:&AppState,req:&MatchRequest)->Result<Diagnostic,String>{
    let start=Instant::now();
    let result=diagnose(&state.db,req);
    let elapsed=start.elapsed().as_millis() as u64;
    match &result {
        Ok(d)=>{
            let status=if d.selected.is_some(){"matched"}else{"unmatched"};
            let details=serde_json::to_value(d).unwrap_or(Value::Null);
            dashboard_history::record(&state.history,&req.title,status,d.selected.as_ref(),&details,elapsed);
        }
        Err(error)=>dashboard_history::record(&state.history,&req.title,"error",None,&json!({"error":error}),elapsed),
    }
    result
}

async fn dashboard()->([(header::HeaderName,&'static str);1],&'static str){
    ([(header::CONTENT_TYPE,"text/html; charset=utf-8")],include_str!("dashboard.html"))
}

#[derive(Deserialize)]
struct HistoryQuery {limit:Option<usize>,offset:Option<usize>}

async fn dashboard_history_request(State(state):State<AppState>,Query(q):Query<HistoryQuery>)
    -> (StatusCode,Json<Value>){
    let result=tokio::task::spawn_blocking(move||
        dashboard_history::history(&state.history,q.limit.unwrap_or(25),q.offset.unwrap_or(0))).await;
    match result {
        Ok(Ok(v))=>(StatusCode::OK,Json(v)),
        Ok(Err(e))=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e}))),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))),
    }
}
async fn dashboard_stats_request(State(state):State<AppState>)
    -> (StatusCode,Json<Value>){
    let result=tokio::task::spawn_blocking(move||dashboard_history::stats(&state.history)).await;
    match result {
        Ok(Ok(v))=>(StatusCode::OK,Json(v)),
        Ok(Err(e))=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e}))),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))),
    }
}

async fn liveness() -> (StatusCode,Json<Value>) {
    (StatusCode::OK,Json(json!({"status":"alive"})))
}

async fn readiness(State(state):State<AppState>) -> (StatusCode,Json<Value>) {
    let ready = open_db(&state.db).and_then(|db|db.query_row(
        "SELECT count(*) FROM metadata_state WHERE source='full_refresh'",[],|r|r.get::<_,i64>(0)))
        .map(|n|n>0).unwrap_or(false);
    (if ready {StatusCode::OK} else {StatusCode::SERVICE_UNAVAILABLE},
        Json(json!({"status":if ready {"ready"} else {"initializing"}})))
}

/// Public response: winner coordinates plus the complete parser output.
/// Keep full candidates and ranking diagnostics exclusive to /debug.
fn public_match_response(selected: Option<Value>) -> Value {
    let Some(winner)=selected else {
        return json!({"sid_start":null,"sid_end":null,"asid_start":null,"asid_end":null});
    };
    let imdb=winner["imdb_id"].as_str().unwrap_or_default();
    let start_season=winner["season"].as_i64();
    let start_episode=winner["episode"].as_i64();
    let end_season=winner["season_end"].as_i64().or(start_season);
    let end_episode=winner["episode_end"].as_i64().or(start_episode);
    let sid_start=if imdb.is_empty(){Value::Null}
        else if let (Some(se),Some(ep))=(start_season,start_episode){
            json!(format!("{imdb}:{se}:{ep}"))
        }else{json!(imdb)};
    let sid_end=if imdb.is_empty(){Value::Null}
        else if let (Some(se),Some(ep))=(end_season,end_episode){
            json!(format!("{imdb}:{se}:{ep}"))
        }else{json!(imdb)};
    let anime=winner.get("anime").filter(|v|v.is_object());
    let direct_asid=anime.and_then(|a|a.get("asid")).cloned().unwrap_or(Value::Null);
    let segments=winner.pointer("/episode_range/segments").and_then(Value::as_array);
    let complete=winner.pointer("/episode_range/complete")
        .and_then(Value::as_bool).unwrap_or(true);
    let asid_start=segments.filter(|_|complete).and_then(|v|v.first()).and_then(|v|{
        Some(json!(format!("{}:{}",v["anidb_id"].as_str()?,
            v["start_anime_episode"].as_i64()?)))
    }).unwrap_or_else(||direct_asid.clone());
    let asid_end=segments.filter(|_|complete).and_then(|v|v.last()).and_then(|v|{
        Some(json!(format!("{}:{}",v["anidb_id"].as_str()?,
            v["end_anime_episode"].as_i64()?)))
    }).unwrap_or(direct_asid);
    json!({"sid_start":sid_start,"sid_end":sid_end,
        "asid_start":asid_start,"asid_end":asid_end})
}

async fn parse_request(Json(req):Json<MatchRequest>)->(StatusCode,Json<Value>){
    parse_response(req).await
}
async fn get_parse_request(Query(req):Query<MatchRequest>)->(StatusCode,Json<Value>){
    parse_response(req).await
}
async fn parse_response(req:MatchRequest)->(StatusCode,Json<Value>){
    match tokio::task::spawn_blocking(move||parse_cached(&req.title)).await{
        Ok(Ok(value))=>(StatusCode::OK,Json(value)),
        Ok(Err(error))=>(StatusCode::BAD_REQUEST,Json(json!({"error":error}))),
        Err(error)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":error.to_string()}))),
    }
}

async fn match_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>) {
    match tokio::task::spawn_blocking(move ||diagnose_logged(&state,&req)).await {
        Ok(Ok(d)) => (StatusCode::OK,Json(public_match_response(d.selected))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST,Json(json!({"error":e}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))),
    }
}

/// GET /match?title=...&year=...
async fn get_match_request(State(state):State<AppState>,Query(req):Query<MatchRequest>)
    -> (StatusCode,Json<Value>) {
    match tokio::task::spawn_blocking(move ||diagnose_logged(&state,&req)).await {
        Ok(Ok(d))=>(StatusCode::OK,Json(public_match_response(d.selected))),
        Ok(Err(e))=>(StatusCode::BAD_REQUEST,Json(json!({"error":e}))),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))),
    }
}

async fn debug_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>) {
    match tokio::task::spawn_blocking(move ||diagnose_logged(&state,&req)).await {
        Ok(Ok(d)) => (StatusCode::OK,Json(serde_json::to_value(d).unwrap_or(Value::Null))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST,Json(json!({"error":e}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))),
    }
}

#[tokio::main]
async fn main() -> Result<(),Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_|"matchlab_server=info".into())).init();
    let args:Vec<String>=env::args().collect();
    if args.get(1).is_some_and(|arg|arg=="import-imdb") {
        if args.len()!=6 {
            return Err("usage: matchlab-server import-imdb <database.sqlite3> <title.basics.tsv.gz> <title.akas.tsv.gz> <title.episode.tsv.gz>".into());
        }
        let count=metadata_import::import_imdb(
            std::path::Path::new(&args[2]),std::path::Path::new(&args[3]),
            std::path::Path::new(&args[4]),std::path::Path::new(&args[5]))?;
        tracing::info!("IMDb metadata imported: {count} supported titles");
        return Ok(());
    }
    if args.get(1).is_some_and(|arg|arg=="import-animeapi") {
        if args.len()!=4 {
            return Err("usage: matchlab-server import-animeapi <database.sqlite3> <animeapi.tsv>".into());
        }
        let count=metadata_import::import_animeapi(
            std::path::Path::new(&args[2]),std::path::Path::new(&args[3]))?;
        tracing::info!("animeApi imported: {count} valid IMDb/AniDB links");
        return Ok(());
    }
    if args.get(1).is_some_and(|arg|arg=="import-anidb-titles") {
        if args.len()!=4 {
            return Err("usage: matchlab-server import-anidb-titles <database.sqlite3> <anime-titles.xml.gz>".into());
        }
        let count=metadata_import::import_anidb_titles(
            std::path::Path::new(&args[2]),std::path::Path::new(&args[3]))?;
        tracing::info!("AniDB aliases imported: {count}");
        return Ok(());
    }
    if args.get(1).is_some_and(|arg|arg=="import-animelists") {
        if args.len()!=4 {
            return Err("usage: matchlab-server import-animelists <database.sqlite3> <anime-list.xml>".into());
        }
        let count=metadata_import::import_animelists(
            std::path::Path::new(&args[2]),std::path::Path::new(&args[3]))?;
        tracing::info!("Anime-Lists mappings imported: {count}");
        return Ok(());
    }
    if args.get(1).is_some_and(|arg|arg=="refresh-metadata") {
        if args.len()!=9 {
            return Err("usage: matchlab-server refresh-metadata <database.sqlite3> <title.basics.tsv.gz> <title.akas.tsv.gz> <title.episode.tsv.gz> <animeapi.tsv> <anime-list.xml> <anime-titles.xml.gz>".into());
        }
        let (imdb,links,mappings,aliases)=metadata_import::refresh_from_files(
            std::path::Path::new(&args[2]),std::path::Path::new(&args[3]),
            std::path::Path::new(&args[4]),std::path::Path::new(&args[5]),
            std::path::Path::new(&args[6]),std::path::Path::new(&args[7]),
            std::path::Path::new(&args[8]))?;
        tracing::info!("metadata refresh completed: imdb={imdb} anime_links={links} episode_maps={mappings} aliases={aliases}");
        return Ok(());
    }
    if args.get(1).is_some_and(|arg|arg=="refresh-metadata-urls") {
        if args.len()!=9 {
            return Err("usage: matchlab-server refresh-metadata-urls <database.sqlite3> <basics_url> <akas_url> <episodes_url> <animeapi_url> <animelists_url> <anidb_titles_url>".into());
        }
        let (imdb,links,mappings,aliases)=metadata_download::refresh_from_urls(
            std::path::Path::new(&args[2]),&args[3..])?;
        tracing::info!("downloaded metadata refresh completed: imdb={imdb} anime_links={links} episode_maps={mappings} aliases={aliases}");
        return Ok(());
    }
    let db = env::var("MATCHLAB_DB").unwrap_or_else(|_|"/data/metadata.sqlite3".into());
    let host = env::var("MATCHLAB_HOST").unwrap_or_else(|_|"127.0.0.1:8766".into());
    // Built-in HTTPS metadata feeds initialize a fresh Docker volume automatically.
    // MATCHLAB_METADATA_URLS can override all six feeds.
    {
        let urls=metadata_download::configured_urls(env::var("MATCHLAB_METADATA_URLS").ok().as_deref())?;
        let hours=env::var("MATCHLAB_METADATA_REFRESH_HOURS")
            .unwrap_or_else(|_|"168".into()).parse::<u64>()?;
        if !(1..=24*30).contains(&hours) {
            return Err("MATCHLAB_METADATA_REFRESH_HOURS must be between 1 and 720".into());
        }
        let target=PathBuf::from(&db);
        tokio::spawn(async move {
            let refresh_seconds=hours*3600;
            loop {
                let check_target=target.clone();
                let last_refresh=tokio::task::spawn_blocking(move || {
                    let db=rusqlite::Connection::open_with_flags(
                        &check_target,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
                    let value:String=db.query_row(
                        "SELECT updated_at FROM metadata_state WHERE source='full_refresh'",
                        [],|row|row.get(0)).ok()?;
                    value.parse::<u64>().ok()
                }).await.ok().flatten();
                let now=std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default().as_secs();
                if let Some(last)=last_refresh {
                    let remaining=last.saturating_add(refresh_seconds).saturating_sub(now);
                    if remaining>0 {
                        tracing::info!(remaining_seconds=remaining,
                            "metadata refresh not due; keeping existing database");
                        tokio::time::sleep(std::time::Duration::from_secs(remaining.min(3600))).await;
                        continue;
                    }
                }
                let refresh_target=target.clone();
                let refresh_urls=urls.clone();
                match tokio::task::spawn_blocking(move ||
                    metadata_download::refresh_from_urls(&refresh_target,&refresh_urls)
                        .map_err(|error|error.to_string())).await {
                    Ok(Ok((imdb,links,mappings,aliases))) =>
                        tracing::info!(imdb,links,mappings,aliases,"scheduled metadata refresh completed"),
                    Ok(Err(error)) => tracing::error!(%error,"scheduled metadata refresh failed; existing database preserved"),
                    Err(error) => tracing::error!(%error,"scheduled metadata refresh task failed"),
                }
                // On failure retry after one hour; successful imports persist
                // their completion timestamp in metadata_state/full_refresh.
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }
    let history_db=env::var("MATCHLAB_HISTORY_DB").unwrap_or_else(|_|"/data/history.sqlite3".into());
    let app = Router::new().route("/",get(dashboard))
        .route("/api/dashboard/history",get(dashboard_history_request))
        .route("/api/dashboard/stats",get(dashboard_stats_request))
        .route("/health",get(readiness))
        .route("/health/ready",get(readiness))
        .route("/health/live",get(liveness))
        .route("/match",get(get_match_request).post(match_request))
        .route("/parse",get(get_parse_request).post(parse_request))
        .route("/debug/match",post(debug_request))
        .with_state(AppState{db:Arc::new(PathBuf::from(db)),history:Arc::new(PathBuf::from(history_db))});
    let listener = tokio::net::TcpListener::bind(&host).await?;
    tracing::info!("Rust API listening on {host}");
    axum::serve(listener,app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_supernatural_series_beats_unrelated_anime_alias() {
        let path=std::env::temp_dir().join(format!(
            "matchlab_supernatural_{}.sqlite3",std::process::id()));
        let _=std::fs::remove_file(&path);
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt0460681','Supernatural','supernatural',2005,
                'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0460681',15);
            INSERT INTO anime_series_map VALUES('tt1679482','999','999',0);
            INSERT INTO anidb_titles VALUES('999','Supernatural','supernatural','main','en');
            INSERT INTO episode_map VALUES('999','999',1,1,1,12,0,'');").unwrap();
        drop(db);
        let result=diagnose(&path,&MatchRequest{
            title:"Supernatural.S01-s12.hdr".into(),year:None,series:None
        }).unwrap();
        let winner=result.selected.unwrap();
        assert_eq!(winner["imdb_id"],"tt0460681");
        assert_eq!(winner["match_source"],"imdb");
        assert_eq!(winner["category"],"series");
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn requested_episode_must_exist_within_known_season_bounds() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
            episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt0460681',1,22,22);
            INSERT INTO season_episodes VALUES('ttshort',1,20,20);").unwrap();
        assert!(candidate_has_episode(&db,"tt0460681",1,22).unwrap());
        assert!(!candidate_has_episode(&db,"ttshort",1,22).unwrap());
        assert!(candidate_has_episode(&db,"ttmissing",1,22).unwrap());
    }

    #[test]
    fn season_episode_bounds_use_maximum_episode_not_row_count() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
            episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt0460681',2,21,22);").unwrap();
        assert_eq!(season_episode_end(&db,"tt0460681",2).unwrap(),Some(22));
        assert_eq!(season_episode_end(&db,"tt0460681",3).unwrap(),None);
    }

    #[test]
    fn absolute_episode_range_crosses_seasons_and_keeps_gaps() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES('ttonepiece','123','456',0);
            INSERT INTO episode_map VALUES('123','456',1,-1,1,4,0,'');
            INSERT INTO episode_map VALUES('123','456',1,10,1,2,0,'');
            INSERT INTO episode_map VALUES('123','456',1,11,3,4,-2,'');").unwrap();
        let range=mapped_absolute_range(&db,"ttonepiece",1,4).unwrap();
        assert_eq!(range["start_season"],10);
        assert_eq!(range["end_season"],11);
        assert_eq!(range["start_episode"],1);
        assert_eq!(range["end_episode"],2);
        assert_eq!(range["unmapped_count"],0);
        assert_eq!(range["segments"].as_array().unwrap().len(),2);
        let gap=mapped_absolute_range(&db,"ttonepiece",1,5).unwrap();
        assert_eq!(gap["unmapped_count"],1);
        assert_eq!(gap["complete"],false);
        assert!(gap["end_season"].is_null());
    }
    #[test]
    fn explicit_multi_season_packs_keep_both_bounds() {
        assert_eq!(explicit_season_range("Gary.Unmarried.S01-S02.1080p"),Some((1,2)));
        assert_eq!(explicit_season_range("Supernatural S01-S02"),Some((1,2)));
        assert_eq!(explicit_season_range("Supernatural.S01E22"),None);
        assert_eq!(explicit_season_range("Supernatural S01-S12/S02E01"),None);
    }
    #[test]
    fn public_response_movie_series_and_anime() {
        let parsed=json!({"title":"Example","resolution":"1080p","languages":["Norwegian"],"year":2024});
        let movie=public_match_response(Some(json!({
            "imdb_id":"tt1234567","category":"movie","sid":"tt1234567",
            "season":null,"episode":null,"anime":null,"anime_ids":{},"parsed":parsed
        })));
        assert_eq!(movie["type"],"movie");
        assert_eq!(movie["imdbid"],"tt1234567");
        assert_eq!(movie["parsed"]["resolution"],"1080p");
        assert!(movie.get("imdb_candidates").is_none());
        assert!(movie.get("matched_title").is_none());
        let series=public_match_response(Some(json!({
            "imdb_id":"tt7654321","category":"series","sid":"tt7654321:2:5",
            "season":2,"episode":5,"anime":null,"anime_ids":{},"parsed":parsed
        })));
        assert_eq!(series["type"],"series");
        assert_eq!(series["sid"],"tt7654321:2:5");
        assert_eq!(series["season"],2);
        assert_eq!(series["episode"],5);
        let anime=public_match_response(Some(json!({
            "imdb_id":"tt9999999","category":"series","sid":"tt9999999:1:4",
            "season":1,"episode":4,
            "anime":{"anidb_id":"123","episode":4,"asid":"123:4"},
            "anime_ids":{"anidb":"123"},"parsed":parsed
        })));
        assert_eq!(anime["type"],"anime");
        assert_eq!(anime["anime_id"],"123");
        assert_eq!(anime["asid"],"123:4");
        assert_eq!(anime["parsed"]["languages"][0],"Norwegian");
        assert!(public_match_response(None).is_null());
    }
    #[test]
    fn public_response_exposes_absolute_anime_range_without_inventing_asid() {
        let result=public_match_response(Some(json!({
            "imdb_id":"tt0388629","category":"series",
            "anime_ids":{"anidb":"69"},
            "anime":null,
            "anime_episode_start":700,"anime_episode_end":800,
            "episode_range":{"absolute_start":700,"absolute_end":800},
            "season":null,"episode":null
        })));
        assert_eq!(result["type"],"anime");
        assert_eq!(result["anime_episode_start"],700);
        assert_eq!(result["anime_episode_end"],800);
        assert!(result["asid"].is_null());
    }
    #[test]
    fn anime_without_verified_episode_does_not_invent_coordinates() {
        let result=public_match_response(Some(json!({
            "imdb_id":"tt1111111","category":"series","sid":"tt1111111",
            "season":null,"episode":null,"anime":null,
            "anime_ids":{"anidb":"456"},"parsed":{"title":"Example Anime"}
        })));
        assert_eq!(result["type"],"anime");
        assert!(result["season"].is_null());
        assert!(result["episode"].is_null());
        assert!(result["asid"].is_null());
    }
    #[test]
    fn anime_identity_without_episode_is_unambiguous(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO anidb_titles VALUES('100','frieren');
            INSERT INTO anime_series_map VALUES('100','tt1234567');").unwrap();
        assert_eq!(anime_identity(&db,"frieren").unwrap().as_deref(),Some("tt1234567"));
        db.execute_batch("INSERT INTO anidb_titles VALUES('200','frieren');
            INSERT INTO anime_series_map VALUES('200','tt7654321');").unwrap();
        assert!(anime_identity(&db,"frieren").unwrap().is_none());
    }
    #[test]
    fn three_hundred_end_to_end_matches() {
        // These exercise the actual /match decision path, not just the parser.
        // Synthetic metadata makes expected IDs and episodes deterministic.
        let path=std::env::temp_dir().join(format!(
            "matchlab_rust_300_{}_{}.sqlite3",std::process::id(),
            std::thread::current().name().unwrap_or("test")));
        let _=std::fs::remove_file(&path);
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);").unwrap();
        for i in 1..=100 {
            let movie=format!("Cinemafilm{i}");
            let series=format!("Televisionseries{i}");
            let anime=format!("Animationseries{i}");
            let mid=format!("tt{:07}",i);
            let sid=format!("tt{:07}",i+100);
            let aid=format!("tt{:07}",i+200);
            let ani_id=format!("{}",i+1000);
            for (id,name,category,kind) in [
                (&mid,&movie,"movie","movie"),
                (&sid,&series,"series","tvSeries"),
                (&aid,&anime,"series","tvSeries")] {
                db.execute("INSERT INTO titles VALUES(?1,?2,?3,2020,?4,'US',1,0,?5)",
                    params![id,name,normalize(name),category,kind]).unwrap();
            }
            for id in [&sid,&aid] {
                db.execute("INSERT INTO seasons VALUES(?1,10)",[id]).unwrap();
            }
            db.execute("INSERT INTO anime_series_map VALUES(?1,?2,?3,0)",
                params![aid,ani_id,format!("tvdb{i}")]).unwrap();
            db.execute("INSERT INTO anidb_titles VALUES(?1,?2,?3,'main','en')",
                params![ani_id,anime,normalize(&anime)]).unwrap();
            db.execute("INSERT INTO episode_map VALUES(?1,?2,1,1,1,24,0,'')",
                params![ani_id,format!("tvdb{i}")]).unwrap();
        }
        drop(db);
        let mut checked=0;
        for i in 1..=100 {
            for (raw,id,season,episode) in [
                (format!("Cinemafilm{i}.2020.1080p.WEB-DL"),
                    format!("tt{:07}",i),None,None),
                (format!("Televisionseries{i}.S01E02.1080p.WEB-DL"),
                    format!("tt{:07}",i+100),Some(1),Some(2)),
                (format!("Animationseries{i}.S01E02.1080p.WEB-DL"),
                    format!("tt{:07}",i+200),Some(1),Some(2))] {
                let result=diagnose(&path,&MatchRequest{title:raw.clone(),year:None,
                    series:None}).unwrap_or_else(|e|panic!("{raw}: {e}"));
                let selected=result.selected.unwrap_or_else(||panic!("no match: {raw}"));
                assert_eq!(selected["imdb_id"],id,"{raw}");
                if let Some(season)=season {assert_eq!(selected["season"],season,"{raw}");}
                if let Some(episode)=episode {assert_eq!(selected["episode"],episode,"{raw}");}
                if raw.starts_with("Animationseries") {
                    assert!(selected["anime"]["anidb_id"].is_string(),"{raw}");
                }
                checked+=1;
            }
        }
        assert_eq!(checked,300);
        // Episode 99 is outside the only verified AniDB interval (1..24).
        // Preserve the series identity, never publish S01E99 as verified.
        let unknown=diagnose(&path,&MatchRequest{
            title:"Animationseries1.S01E99.1080p.WEB-DL".into(),
            year:None,series:None}).unwrap().selected.unwrap();
        assert_eq!(unknown["imdb_id"],"tt0000201");
        assert!(unknown["season"].is_null());
        assert!(unknown["episode"].is_null());
        assert_eq!(unknown["sid"],"tt0000201");
        assert!(unknown["anime"].is_null());
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn normalization_handles_accents_and_and_word() {
        assert_eq!(normalize("Bjørn & Ægir"),"bjorn aegir".replace(' ',""));
        assert_eq!(normalize("Rock and Roll"),"rockroll");
        assert_eq!(normalize("Pokémon"),"pokemon");
    }
    #[test]
    fn absolute_episode_notation(){
        assert_eq!(explicit_absolute_episode("One Piece Episode 1067 1080p"),Some(1067));
        assert_eq!(explicit_absolute_episode("One.Piece.EP.1067.WEB-DL"),Some(1067));
        assert_eq!(explicit_absolute_episode("One Piece S01E02"),None);
    }
    #[test]
    fn empty_number_array() { assert!(numbers(None).is_empty()); }
    #[test]
    fn imdb_candidates_are_filtered_by_media_type() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles (
            imdb_id TEXT, title TEXT, year INTEGER, type TEXT,
            clean_title TEXT, ordering INTEGER
        );").unwrap();
        db.execute("INSERT INTO titles VALUES ('tt1','Example',2020,'movie','example',1)",[]).unwrap();
        db.execute("INSERT INTO titles VALUES ('tt2','Example',2021,'tvSeries','example',1)",[]).unwrap();
        let movies = candidates(&db,"example",Some(2020),false).unwrap();
        let shows = candidates(&db,"example",Some(2021),true).unwrap();
        assert_eq!(movies.len(),1);
        assert_eq!(movies[0].imdb_id,"tt1");
        assert_eq!(shows.len(),1);
        assert_eq!(shows[0].imdb_id,"tt2");
    }
    #[test]
    fn anime_requires_unique_verified_crosswalk() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT,tvdb_id TEXT DEFAULT '',trakt_season INTEGER DEFAULT 0);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT DEFAULT '',tvdb_season INTEGER,
                anidb_season INTEGER,map TEXT,offset INTEGER,start INTEGER,end INTEGER);
            INSERT INTO anidb_titles VALUES('123','exampleanime');
            INSERT INTO anime_series_map(anidb_id,imdb_id) VALUES('123','tt999');
            INSERT INTO episode_map(anidb_id,tvdb_season,anidb_season,map,offset,start,end) VALUES('123',1,1,'',0,1,12);").unwrap();
        let found = anime_candidate(&db,"exampleanime",Some(1),4).unwrap().unwrap();
        assert_eq!(found["imdb_id"],"tt999");
        assert_eq!(found["anidb_episode"],4);
        assert!(anime_candidate(&db,"exampleanime",Some(2),4).unwrap().is_none());
        db.execute("INSERT INTO episode_map(anidb_id,tvdb_season,anidb_season,map,offset,start,end) VALUES('123',1,1,'',1,1,12)",[]).unwrap();
        assert_eq!(anime_candidate(&db,"exampleanime",Some(1),4).unwrap().unwrap()["anidb_episode"],3);
    }
}
