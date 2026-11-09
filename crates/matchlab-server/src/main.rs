//! MatchLab's native Rust HTTP API.
//! Tantivy searches titles; SQLite snapshots hold relational metadata.
mod airdate;
mod alias;
mod episode_conversion;
mod imdb_matcher;
mod anidb_matcher;
mod title_normalization;
mod tantivy_search;
mod metadata_import;
mod metadata_download;
mod episode_crosswalk;
mod identifiers;
mod anime_title;
mod country_aliases;
mod dashboard_history;
mod debug_feed;
mod query_cache;
use axum::{extract::{Query, State}, http::{StatusCode,header}, routing::{get,post}, Json, Router};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
#[cfg(test)]
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, path::PathBuf, sync::{Arc,Mutex,OnceLock}, time::{Instant,Duration}, collections::HashMap};
use tokio::sync::Semaphore;


#[derive(Clone)]
struct AppState {
    db:Arc<PathBuf>,
    history:Arc<PathBuf>,
    match_slots:Arc<Semaphore>,
    index_phase:Arc<Mutex<IndexPhase>>,
}

/// Index creation can take minutes on a populated volume. It must never
/// prevent the HTTP listener, dashboard or liveness endpoint from starting.
#[derive(Debug,Clone,PartialEq,Eq)]
enum IndexPhase {
    WaitingForMetadata,
    Indexing,
    Ready,
    Failed(String),
}
fn index_phase(state:&Arc<Mutex<IndexPhase>>)->IndexPhase {
    state.lock().unwrap_or_else(|poison|poison.into_inner()).clone()
}
fn set_index_phase(state:&Arc<Mutex<IndexPhase>>,phase:IndexPhase){
    *state.lock().unwrap_or_else(|poison|poison.into_inner())=phase;
}
fn matching_available(state:&AppState)->bool{
    index_phase(&state.index_phase)==IndexPhase::Ready
}
fn indexing_response()->(StatusCode,Json<Value>){
    (StatusCode::SERVICE_UNAVAILABLE,Json(json!({
        "status":"indexing",
        "error":"Metadata indexing is in progress; retry when /health/ready is ready"
    })))
}


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
    anidb_candidate: Option<Value>,
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
    let value=Value::Object(matchlab_parser::parse_title(title,false).map_err(|e|e.to_string())?);
    if let Ok(mut guard)=cache.lock(){
        // Avoid O(cache size) cleanup on every previously unseen title.
        // Under pressure, remove expired entries before evicting only the
        // oldest live entry instead of wiping the entire warm cache.
        const CAPACITY:usize=1024;
        if guard.len()>=CAPACITY {
            guard.retain(|_,(at,_)|at.elapsed()<PARSE_TTL);
            if guard.len()>=CAPACITY {
                if let Some(oldest)=guard.iter()
                    .min_by_key(|(_, (at,_))|*at).map(|(key,_)|key.clone()){
                    guard.remove(&oldest);
                }
            }
        }
        guard.insert(title.to_string(),(Instant::now(),value.clone()));
    }
    Ok(value)
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

/// Explicit absolute episode notation used by the Rust release matcher.
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

fn anime_candidate(db: &Connection, key: &str, season: Option<i64>, episode: i64) -> rusqlite::Result<Option<Value>> {
    if key.len() < 4 || episode <= 0 { return Ok(None); }
    let matches:Vec<_>=tantivy_search::anime_aliases(db,key,6)
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?
        .into_iter().map(|alias|(alias.anidb_id,alias.clean_title,alias.imdb_id)).collect();
    if matches.is_empty() { return Ok(None); }
    let exact = matches[0].1 == key;
    let best_len = matches[0].1.len();
    let best: std::collections::BTreeSet<String> = matches.iter()
        .filter(|(_, alias, _)| (alias == key) == exact && alias.len() == best_len)
        .map(|(_,_,imdb)|imdb.clone()).collect();
    // Shared aliases are frequent (TV, movies, OVAs). The alias alone cannot
    // select an IMDb identity, but a unique verified episode crosswalk can.
    // Reject only when more than one *mapped* identity survives. Do not let
    // a colliding film with no episode 338 block Bleach's TV episode 338.
    let mut verified:Option<episode_crosswalk::Episode>=None;
    for imdb in best {
        let mapped = match season {
            Some(season) if season > 0 => {
                let titled=anime_title::resolve(db,&imdb,key,season,episode)?;
                if titled.is_some() { titled }
                else if anime_title::has_specific_alias(db,&imdb,key)? { None }
                else { episode_crosswalk::resolve_season(db,&imdb,season,episode)? }
            },
            None => {
                // Named anime sequel parts restart local episode numbering.
                let named=anime_title::resolve_episode_from_exact_part_title(
                    db,&imdb,key,episode)?;
                if named.is_some(){named}
                else{episode_crosswalk::resolve_absolute(db,&imdb,episode)?}
            },
            _ => None,
        };
        if let Some(mapped)=mapped {
            if verified.replace(mapped).is_some(){
                // Two independently mapped identities: never guess a winner.
                return Ok(None);
            }
        }
    }
    Ok(verified.map(|m|json!({"imdb_id":m.imdb_id,"season":m.season,
        "episode":m.episode,"anidb_id":m.anidb_id,"anidb_episode":m.anidb_episode})))
}

/// Resolve a title-only anime identity from anime tables, without inventing
/// season or episode coordinates. Exact aliases precede longest prefixes.
fn anime_identity(db:&Connection,key:&str)->rusqlite::Result<Option<String>>{
    if key.len()<4{return Ok(None);}
    let rows:Vec<_>=tantivy_search::anime_aliases(db,key,6)
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?
        .into_iter().map(|alias|(alias.clean_title,alias.imdb_id)).collect();
    let Some((best_alias,best_imdb))=rows.first()else{return Ok(None)};
    let exact=best_alias==key;
    let size=best_alias.len();
    if rows.iter().any(|(alias,id)|(alias==key)==exact&&alias.len()==size&&id!=best_imdb){
        return Ok(None);
    }
    Ok(Some(best_imdb.clone()))
}

/// Return an IMDb identity only when the release title itself is an exact,
/// unique AniDB alias. This is stricter than the prefix-based fallback used
/// to rank title candidates and safe for season-pack inference.
fn exact_anime_identity(db:&Connection,key:&str)->Result<Option<String>,String>{
    if key.len()<4{return Ok(None);}
    let ids=tantivy_search::anime_aliases(db,key,4)?.into_iter()
        .filter(|alias|alias.clean_title==key)
        .map(|alias|alias.imdb_id)
        .collect::<std::collections::BTreeSet<_>>();
    if ids.len()==1{Ok(ids.into_iter().next())}else{Ok(None)}
}

/// Resolve every absolute episode independently: an anime range may span
/// several IMDb seasons. Never interpolate across missing crosswalk entries.
fn mapped_absolute_range(db:&Connection,imdb:&str,start:i64,end:i64)->Result<Value,String>{
    let mut segments:Vec<Value>=Vec::new();
    let mut missing=0i64;
    let mut first:Option<episode_crosswalk::Episode>=None;
    let mut last:Option<episode_crosswalk::Episode>=None;
    let mapped_episodes=episode_crosswalk::resolve_absolute_range(db,imdb,start,end)
        .map_err(|e|e.to_string())?;
    for (absolute,mapped) in (start..=end).zip(mapped_episodes) {
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

/// Validate each episode in a season-labeled anime batch against the
/// AniDB/TVDB crosswalk. A partial mapping never produces guessed endpoints.
fn mapped_season_episode_range(
    db:&Connection,imdb:&str,title:&str,season:i64,start:i64,end:i64,
    canonical_title:bool,
)->Result<Value,String>{
    let mut segments:Vec<Value>=Vec::new();
    let mut first:Option<episode_crosswalk::Episode>=None;
    let mut last:Option<episode_crosswalk::Episode>=None;
    let mut missing=0i64;
    // The canonical IMDb series title identifies a fixed IMDb season.
    // Never reinterpret it as a named AniDB part just because an OVA/movie
    // shares its alias (e.g. the base series Bleach).
    let specific=if canonical_title {false} else {
        anime_title::has_specific_alias(db,imdb,title)
            .map_err(|e|e.to_string())?
    };
    let resolved=if specific {
        (start..=end).map(|episode|anime_title::resolve(db,imdb,title,season,episode)
            .map_err(|e|e.to_string())).collect::<Result<Vec<_>,_>>()?
    }else{
        episode_crosswalk::resolve_season_range(db,imdb,season,start,end)
            .map_err(|e|e.to_string())?
    };
    for (episode,mapped) in (start..=end).zip(resolved) {
        if let Some(ep)=mapped {
            if episode==start{first=Some(ep.clone());}
            if episode==end{last=Some(ep.clone());}
            let continues=segments.last().is_some_and(|seg|
                seg["season"].as_i64()==Some(ep.season)
                &&seg["end_episode"].as_i64()==Some(ep.episode-1)
                &&seg["anidb_id"].as_str()==Some(ep.anidb_id.as_str())
                &&seg["end_anime_episode"].as_i64()==Some(ep.anidb_episode-1));
            if continues {
                let tail=segments.last_mut().expect("contiguous segment");
                tail["end_episode"]=json!(ep.episode);
                tail["end_anime_episode"]=json!(ep.anidb_episode);
            }else{
                segments.push(json!({"season":ep.season,
                    "start_episode":ep.episode,"end_episode":ep.episode,
                    "anidb_id":ep.anidb_id,
                    "start_anime_episode":ep.anidb_episode,
                    "end_anime_episode":ep.anidb_episode}));
            }
        }else{missing+=1;}
    }
    Ok(json!({"requested_season":season,
        "requested_episode_start":start,"requested_episode_end":end,
        "start_season":first.as_ref().map(|e|e.season),
        "start_episode":first.as_ref().map(|e|e.episode),
        "end_season":last.as_ref().map(|e|e.season),
        "end_episode":last.as_ref().map(|e|e.episode),
        "segments":segments,"unmapped_count":missing,
        "complete":missing==0,"source":"anime_season_crosswalk"}))
}

/// Use the highest observed episode number, not the count: IMDb
/// episode numbers can contain gaps. Only applies to season-only releases.
/// Reject candidates whose imported IMDb season cannot contain the
/// explicitly requested episode. Missing season metadata is not evidence
/// that an episode exists.
/// If the torrent has no production year or it differs from this IMDb
/// candidate's production year, require an independent identity signal:
/// parsed season, indexed anime identity, or a parsed quality label.

/// Year matching is EXACT; there is no +/-1 production-year tolerance.
/// Episode numbers alone are too weak (the common "Title - 04" case).
fn year_signal_eligible(
    release_year:Option<i32>,candidate_year:i32,
    has_parsed_season:bool,verified_anime:bool,has_parsed_quality:bool,
)->bool{
    let matching_year=release_year.is_some_and(|release|
        candidate_year>0 && candidate_year==release);
    matching_year||has_parsed_season||verified_anime||has_parsed_quality
}
fn parsed_quality_signal(parsed:&serde_json::Map<String,Value>)->bool{
    // Resolution alone (1080p/2160p) is common in unrelated video files;
    // only the parser's explicit release quality classification qualifies.
    parsed.get("quality").and_then(Value::as_str)
        .is_some_and(|quality|!quality.trim().is_empty())
}
/// Only anime confirmed by imported identity data may receive a fallback
/// season=1 for a bare local episode. An episode beyond IMDb's known first
/// season bounds can be an absolute episode from a later season.
fn anime_local_episode_season_one(
    db:&Connection,imdb:&str,episode:i64,has_anime_identity:bool,
    proven_later_part:bool,
)->Result<bool,String>{
    if !has_anime_identity||proven_later_part||
        !valid_sid_coordinate(1,episode)||episode==0{return Ok(false);}
    // If season-one bounds exist they are authoritative. Otherwise, do not
    // relabel a large absolute episode number as a first-season episode.
    Ok(season_episode_end(db,imdb,1)?
        .map_or(episode<=150,|max|episode<=max))
}

fn candidate_has_episode(db:&Connection,imdb:&str,season:i64,episode:i64)->Result<bool,String>{
    if season<=0||episode<=0{return Ok(true);}
    // Unknown bounds cannot prove the episode is invalid. Only reject when
    // IMDb metadata explicitly establishes a lower maximum episode number.
    Ok(season_episode_end(db,imdb,season)?.is_none_or(|max|episode<=max))
}

/// The MatchLab SID consumer accepts seasons 0..=9999 and episodes 0..=99999.
/// Never promote an outlier from IMDb's user-contributed episode numbering into
/// an entire season pack's endpoint.
fn valid_sid_coordinate(season:i64,episode:i64)->bool{
    (0..=9999).contains(&season)&&(0..=99999).contains(&episode)
}
fn season_episode_end(db:&Connection,imdb:&str,season:i64)->Result<Option<i64>,String>{
    if !(1..=9999).contains(&season){return Ok(None);}
    match db.query_row("SELECT max_episode FROM season_episodes
        WHERE imdb_id=?1 AND season=?2",rusqlite::params![imdb,season],
        |row|row.get::<_,i64>(0)){
        Ok(n) if valid_sid_coordinate(season,n)&&n>0=>Ok(Some(n)),
        Ok(_)|Err(rusqlite::Error::QueryReturnedNoRows)=>Ok(None),
        Err(e)=>Err(e.to_string())
    }
}

// Some Chinese releases label the *absolute* episode in a parent directory
// ("第41集"), but encode that number as S02E41 in the file name.
// Never guess a season offset. Only reinterpret the file coordinate when
// IMDb explicitly rules it out and a unique AniDB/TVDB crosswalk verifies
// that the absolute episode belongs to the SAME advertised season.
fn verified_chinese_absolute_fallback(
    db:&Connection,imdb:&str,season:i64,episode:i64,absolute_hint:Option<i64>,
)->Result<Option<episode_crosswalk::Episode>,String>{
    if absolute_hint!=Some(episode)||season<=0||episode<=0{
        return Ok(None);
    }
    if candidate_has_episode(db,imdb,season,episode)?{
        return Ok(None);
    }
    let Some(mapped)=episode_crosswalk::resolve_absolute(db,imdb,episode)
        .map_err(|error|error.to_string())? else {return Ok(None)};
    if mapped.season!=season||mapped.episode==episode
        ||!candidate_has_episode(db,imdb,mapped.season,mapped.episode)?{
        return Ok(None);
    }
    Ok(Some(mapped))
}

/// A child directory can identify an anime cour/part more precisely than the
/// release filename. Title selection here is intentionally *anime-only*:
/// resolve the nearest parent that is an exact, unique AniDB title alias for
/// one of the already ranked IMDb identities. Non-anime shows keep their
/// leaf series title and regular Tantivy ordering. No SQLite text search.
fn verified_anime_subfolder_title(
    db:&Connection,parsed:&serde_json::Map<String,Value>,
    imdb:&[Candidate],
)->Result<Option<String>,String>{
    let Some(parents)=parsed.get("path_title_candidates").and_then(Value::as_array)
        else{return Ok(None)};
    if imdb.is_empty(){return Ok(None);}
    for name in parents.iter().rev().filter_map(Value::as_str){
        let key=imdb_matcher::clean_title(name);
        if key.len()<4 {continue;}
        let aliases=tantivy_search::anime_aliases(db,&key,4)?;
        let exact_ids:std::collections::BTreeSet<String>=aliases.into_iter()
            .filter(|alias|alias.clean_title==key)
            .map(|alias|alias.imdb_id).collect();
        if exact_ids.len()==1 && imdb.iter().any(|candidate|
            exact_ids.contains(&candidate.imdb_id)) {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

/// Verify an unmarked anime interval using indexed exact AniDB aliases.
/// Film and OVA titles can share the long-running TV series' alias. Preserve
/// partial-range diagnostics for an unambiguous single IMDb association; for
/// collisions, require exactly one IMDb identity with an entirely mapped span.
fn verified_unmarked_anime_range_imdb(
    db:&Connection,key:&str,start:i64,end:i64,
)->Result<Option<String>,String>{
    let ids=tantivy_search::anime_aliases(db,key,6)?
        .into_iter().filter(|alias|alias.clean_title==key)
        .map(|alias|alias.imdb_id)
        .collect::<std::collections::BTreeSet<_>>();
    if ids.len()==1{return Ok(ids.into_iter().next());}
    // Bound the cost of checking generic aliases shared by many works.
    if ids.len()>32 || start<=0 || end<start || end-start>=1_000 {
        return Ok(None);
    }
    let mut verified=None;
    for imdb in ids {
        // Two endpoints are cheap to validate using one preloaded crosswalk.
        // Most colliding movie/OVA identities cannot map both endpoints.
        let (first,last)=episode_crosswalk::resolve_absolute_endpoints(
            db,&imdb,start,end).map_err(|error|error.to_string())?;
        if first.is_none() || last.is_none(){continue;}
        let range=mapped_absolute_range(db,&imdb,start,end)?;
        if range["complete"]!=true || range["segments"]
            .as_array().is_none_or(Vec::is_empty){continue;}
        if verified.replace(imdb).is_some(){
            // Distinct works fully cover the range: never choose one by rank.
            return Ok(None);
        }
    }
    Ok(verified)
}

/// An unmarked final number is untrusted until the exact AniDB alias and
/// an episode-specific crosswalk identify one and only one IMDb work.
/// Do not require optional anime_ids decoration or a production year when
/// the actual episode relationship has already been proved.
/// Verify the *ranked IMDb winner*, not every Anime-Lists ID associated
/// with the same AniDB title. One AniDB work may have many related IMDb
/// identities (films, specials, season entries); they must not veto the
/// already-chosen canonical series. Preserve the normal IMDb ranking.
fn verified_unmarked_single_anime_imdb(
    db:&Connection,candidate_title:&str,raw_title:&str,
    year:Option<i32>,episode:i64,
)->Result<Option<String>,String>{
    let key=imdb_matcher::clean_title(candidate_title);
    if key.len()<4 || !(1..=99_999).contains(&episode){return Ok(None);}
    let exact_ids=tantivy_search::anime_aliases(db,&key,4)?
        .into_iter().filter(|alias|alias.clean_title==key)
        .map(|alias|alias.imdb_id)
        .collect::<std::collections::BTreeSet<_>>();
    if exact_ids.is_empty(){return Ok(None);}
    // Use the same deterministic matcher that /match uses. Resolve only
    // its highest-ranked IMDb title. Never count other IMDb IDs linked to
    // this AniDB work as competing proofs of an episode's identity.
    let ranked=imdb_matcher::search(db,candidate_title,year,true,0,raw_title,true,20)
        .map_err(|e|e.to_string())?;
    let Some(winner)=ranked.first()else{return Ok(None)};
    if !exact_ids.contains(&winner.imdb_id){return Ok(None);}
    let Some(mapped)=episode_crosswalk::resolve_absolute(db,&winner.imdb_id,episode)
        .map_err(|e|e.to_string())? else{return Ok(None)};
    if !candidate_has_episode(db,&winner.imdb_id,mapped.season,mapped.episode)?{
        return Ok(None);
    }
    Ok(Some(winner.imdb_id.clone()))
}

/// An independent AniDB result can stand alone without IMDb. However,
/// when *both* IDs are returned together, require a documented link; the
/// same normalized release title is not itself evidence that two different
/// works share an identity. This function only guards *new* AniDB enrichment
/// and never invalidates a pre-existing verified IMDb crosswalk.
fn anidb_related_to_selected_imdb(
    db:&Connection,winner:&Value,anidb_id:&str,
)->Result<bool,String>{
    let Some(imdb)=winner["imdb_id"].as_str()else{return Ok(false)};
    if !metadata_import::valid_imdb_id(imdb){return Ok(false);}
    let link:i64=db.query_row("SELECT
        EXISTS(SELECT 1 FROM anime_series_map
               WHERE imdb_id=?1 AND anidb_id=?2)
        OR EXISTS(SELECT 1 FROM anime_ids
               WHERE group_id=?1 AND type='anidb' AND value=?2)",
        rusqlite::params![imdb,anidb_id],|r|r.get(0))
        .map_err(|e|e.to_string())?;
    Ok(link!=0)
}

/// Public requests apply the release identity gate; dashboard diagnostics
/// deliberately allow a bare title so its candidates can be inspected.
#[derive(Clone,Copy)]
enum MatchPolicy { PublicApi, DashboardDiagnostic }

fn diagnose(db_path: &PathBuf, req: &MatchRequest) -> Result<Diagnostic,String> {
    diagnose_with_policy(db_path,req,MatchPolicy::PublicApi)
}
fn diagnose_with_policy(
    db_path:&PathBuf,req:&MatchRequest,policy:MatchPolicy
)->Result<Diagnostic,String>{
    if req.title.trim().is_empty() || req.title.len() > 4096 { return Err("title must be 1–4096 bytes".into()); }
    let mut parsed_value=parse_cached(&req.title)?;
    let db=open_db(db_path).map_err(|e|e.to_string())?;

    // An unmarked range (Title 01-07, (01-25), 01~12) is not authoritative.
    // The parser exposes a tentative title and endpoints, without changing
    // regular release parsing. Use it only when Tantivy finds an exact,
    // unambiguous AniDB-linked title. No SQLite title discovery is performed.
    let mut verified_anidb_range:Option<anidb_matcher::VerifiedRange>=None;
    // Compatibility lane: long-standing crosswalk rules support full ranges
    // spanning multiple AniDB works, and partially mapped IMDb ranges. The
    // independent AniDB matcher must not silently remove that behavior.
    let mut verified_anime_range_imdb:Option<String>=None;
    if req.series!=Some(false) {
        let hint=parsed_value.get("anime_episode_range_candidate");
        if let Some((Some(candidate_title),Some(start),Some(end)))=hint.map(|v|
            (v.get("title").and_then(Value::as_str),
             v.get("start").and_then(Value::as_i64),
             v.get("end").and_then(Value::as_i64))) {
            let candidate_title=candidate_title.to_owned();
            // Existing IMDb crosswalk validation wins when it can prove
            // this range. Only fall back to the independent AniDB lookup
            // if the legacy path cannot establish a candidate.
            let key=imdb_matcher::clean_title(&candidate_title);
            verified_anime_range_imdb=verified_unmarked_anime_range_imdb(
                &db,&key,start,end)?;
            if verified_anime_range_imdb.is_none(){
                verified_anidb_range=anidb_matcher::match_absolute_range(
                    &db,&candidate_title,start,end)?;
            }
            if verified_anidb_range.is_some() || verified_anime_range_imdb.is_some(){
                if let Some(parsed)=parsed_value.as_object_mut(){
                    parsed.insert("title".into(),json!(candidate_title));
                    parsed.insert("episodes".into(),json!([]));
                    parsed.insert("seasons".into(),json!([]));
                    parsed.insert("episode_start".into(),json!(start));
                    parsed.insert("episode_end".into(),json!(end));
                    parsed.insert("episode_count".into(),json!(end-start+1));
                    parsed.insert("episode_range_numbering".into(),json!("absolute"));
                    parsed.insert("anime_title_verified".into(),json!(true));
                }
            }
        }
    }
    // The generic release parser may have interpreted 01-07 as a single
    // episode even if no anime identity was verified. For an untrusted batch
    // candidate, suppress that inference rather than publishing guessed
    // coordinates for unrelated movies or ordinary TV titles.
    if verified_anidb_range.is_none() && verified_anime_range_imdb.is_none()
        && parsed_value.get("anime_episode_range_candidate").is_some()
        && let Some(parsed)=parsed_value.as_object_mut(){
        parsed.insert("episodes".into(),json!([]));
        parsed.insert("seasons".into(),json!([]));
    }
    // Bracketed anime fansub notation "<title> 3 - 08" may mean S03E08
    // rather than absolute episodes 3..8. Promote only after a unique exact
    // AniDB alias and known IMDb season bounds or a verified crosswalk
    // confirm that the advertised season contains the requested episode.
    // Never publish the same marker as an unmarked episode batch.
    if let Some((Some(hint_title),Some(season),Some(episode)))=
        parsed_value.get("anime_season_episode_candidate").map(|candidate|(
            candidate.get("title").and_then(Value::as_str),
            candidate.get("season").and_then(Value::as_i64),
            candidate.get("episode").and_then(Value::as_i64),
        ))
    {
        let hint_title=hint_title.to_owned();
        // Anime-Lists may index "Jihanki 3" as a season-specific synonym
        // without ever indexing bare "Jihanki". Prefer the more specific
        // exact part alias when it proves an IMDb season-episode coordinate.
        // No substring search and no guessing a short alias.
        let mut verified_title=None;
        if req.series!=Some(false) && (1..=12).contains(&season)
            && (1..=99).contains(&episode) {
            for candidate_title in [format!("{hint_title} {season}"),hint_title.clone()] {
                let key=imdb_matcher::clean_title(&candidate_title);
                if let Some(imdb)=exact_anime_identity(&db,&key)? {
                    let valid=season_episode_end(&db,&imdb,season)?
                        .is_some_and(|end|episode<=end)
                        || episode_crosswalk::resolve_season(&db,&imdb,season,episode)
                            .map_err(|error|error.to_string())?.is_some();
                    if valid {verified_title=Some(candidate_title);break;}
                }
            }
        }
        if let Some(parsed)=parsed_value.as_object_mut(){
            // Generic regex handlers may mistake "3 - 08" for a batch.
            // Preserve the candidate for diagnostics but do not expose
            // unverified coordinates to downstream matching consumers.
            parsed.insert("episodes".into(),json!([]));
            parsed.insert("seasons".into(),json!([]));
            if let Some(verified_title)=verified_title {
                parsed.insert("title".into(),json!(verified_title));
                parsed.insert("seasons".into(),json!([season]));
                parsed.insert("episodes".into(),json!([episode]));
                parsed.insert("anime_season_episode_base_title".into(),json!(hint_title));
                parsed.insert("anime_title_verified".into(),json!(true));
            }
        }
    }
    // A bare final number ("Stone Ocean 27") is not a trusted episode:
    // require an exact and unambiguous anime alias in Tantivy before
    // changing the parsed show title or promoting the episode coordinate.
    // This shares the same crosswalk verification as "Stone Ocean - 27".
    let mut verified_anidb_absolute:Option<anidb_matcher::VerifiedEpisode>=None;
    let mut verified_absolute_episode_imdb:Option<String>=None;
    if req.series != Some(false)
        && parsed_value.get("anime_season_episode_candidate").is_none()
        && parsed_value.get("anime_episode_range_candidate").is_none()
        && let Some(candidate) = parsed_value.get("anime_episode_candidate")
        && let (Some(title),Some(episode)) = (
            candidate.get("title").and_then(Value::as_str),
            candidate.get("episode").and_then(Value::as_i64),
        )
    {
        let candidate_title=title.to_owned();
        // Preserve the existing deterministic IMDb-verification decision
        // regardless of whether the new independent AniDB check succeeds.
        // That retains all legacy ranking and anime-priority safeguards.
        verified_absolute_episode_imdb=verified_unmarked_single_anime_imdb(
            &db,&candidate_title,&req.title,
            req.year.or_else(||parsed_value.get("year")
                .and_then(Value::as_i64).map(|n|n as i32)),episode)?;
        // An already verified IMDb winner should not incur another AniDB
        // lookup or be invalidated by a competing anime title alias.
        if verified_absolute_episode_imdb.is_none(){
            verified_anidb_absolute=anidb_matcher::match_absolute(
                &db,&candidate_title,episode)?;
        }
        if verified_absolute_episode_imdb.is_some()
            || verified_anidb_absolute.is_some() {
            if let Some(parsed)=parsed_value.as_object_mut(){
                parsed.insert("title".into(),json!(candidate_title));
                parsed.insert("episodes".into(),json!([episode]));
                parsed.insert("seasons".into(),json!([]));
                parsed.insert("anime_title_verified".into(),json!(true));
            }
        }else if let Some(part)=anidb_matcher::named_part_season(
            &db,&candidate_title)?{
            // A named sequel can identify a local season even without
            // finite AniDB episode evidence. Episode and SID still require
            // the established crosswalk to prove the final coordinates.
            if let Some(parsed)=parsed_value.as_object_mut(){
                parsed.insert("title".into(),json!(candidate_title));
                parsed.insert("episodes".into(),json!([episode]));
                parsed.insert("seasons".into(),json!([]));
                parsed.insert("anime_named_part_season_hint".into(),json!(part.season));
                parsed.insert("anime_named_part_anidb_id".into(),json!(part.anidb_id));
            }
        }
    }
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
    let dated=airdate::extract(&req.title);
    let airdate_only=dated.is_some() && numbers(parsed.get("seasons")).is_empty()
        && numbers(parsed.get("episodes")).is_empty();
    let title=if airdate_only{dated.as_ref().map(|(name,_)|name.as_str()).unwrap_or(&title_owned)}
        else{title_owned.as_str()};
    let key = imdb_matcher::clean_title(title);
    let season_range=explicit_season_range(&req.title);
    let seasons = season_range.map(|(start,end)|vec![start,end])
        .unwrap_or_else(||numbers(parsed.get("seasons")));
    let mut episodes = numbers(parsed.get("episodes"));
    if episodes.is_empty() {
        if let Some(ep)=explicit_absolute_episode(&req.title){episodes.push(ep);}
    }
    let year = if airdate_only {req.year} else {req.year.or_else(||parsed.get("year").and_then(Value::as_i64).map(|n|n as i32))};
    let parsed_quality=parsed_quality_signal(parsed);
    // An exact anime alias can identify a series even without SxxExx tokens.
    // Respect an explicitly supplied series=false override.
    let series = match req.series {
        Some(value) => value,
        None if airdate_only || !seasons.is_empty() || !episodes.is_empty()
            || parsed.contains_key("episode_start") => true,
        // A release with an explicit production year and no episode/season
        // coordinates is a movie candidate, even if an anime alias exists.
        None if year.is_some() => false,
        None => anime_title::season_from_any_exact_title(&db,&key)
            .map_err(|e|e.to_string())?.is_some()
            || exact_anime_identity(&db,&key)?.is_some(),
    };
    let imdb = imdb_matcher::search(&db,title,year,series,seasons.iter().copied().max().unwrap_or(0),&req.title,
        !episodes.is_empty()||parsed.contains_key("episode_start"),20)
        .map_err(|e|e.to_string())?;
    // Enforce strict production-year/quality identity only for GET/POST /match.
    // The dashboard's /debug/match must still rank plain titles like "Fight Club".
    // Other season/episode, anime and ranking checks remain shared.
    // Exact, uniquely verified AniDB absolute ranges are an independent
    // anime identity signal even if the optional anime_ids metadata is absent.
    // Unverified bare number suffixes do not qualify for this exception.
    let imdb=if matches!(policy,MatchPolicy::PublicApi){
        imdb.into_iter().filter(|candidate|
            year_signal_eligible(year,candidate.year,series&&!seasons.is_empty(),
                !candidate.anime_ids.is_empty()
                    || verified_anime_range_imdb.as_deref()==Some(candidate.imdb_id.as_str())
                    || verified_absolute_episode_imdb.as_deref()==Some(candidate.imdb_id.as_str()),
                parsed_quality)
        ).collect::<Vec<_>>()
    }else{imdb};
    // Require every candidate for a multi-season pack to contain the final season.
    let imdb=if let Some((_,end))=season_range {
        imdb.into_iter().filter(|candidate|{
            db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
                [&candidate.imdb_id],|row|row.get::<_,i64>(0))
                .is_ok_and(|max|max>=end)
        }).collect::<Vec<_>>()
    }else{imdb};
    let chinese_absolute_hint=parsed.get("absolute_episode_candidate").and_then(Value::as_i64);
    let mut chinese_absolute_by_imdb=HashMap::<String,episode_crosswalk::Episode>::new();
    // Episode maxima in user-contributed IMDb data can lag behind actual
    // broadcasts (e.g. Cops.S36.E41). Apply episode-count pruning only
    // when the season/title filters leave more than two plausible identities.
    // With one or two identities, preserve them and the requested episode,
    // while still allowing independently verified absolute crosswalk fixes.
    let apply_episode_filter=imdb.len()>2;
    let imdb=if seasons.len()==1 && episodes.len()==1 {
        let season=seasons[0];
        let episode=episodes[0];
        imdb.into_iter().filter_map(|candidate|{
            match candidate_has_episode(&db,&candidate.imdb_id,season,episode) {
                Ok(true)=>Some(candidate),
                Ok(false)=>{
                    match verified_chinese_absolute_fallback(
                        &db,&candidate.imdb_id,season,episode,chinese_absolute_hint
                    ){
                        Ok(Some(mapped))=>{
                            chinese_absolute_by_imdb.insert(candidate.imdb_id.clone(),mapped);
                            Some(candidate)
                        }
                        Ok(None)=>(!apply_episode_filter).then_some(candidate),
                        Err(error)=>{
                            tracing::warn!(imdb_id=%candidate.imdb_id,%error,
                                "Chinese absolute episode crosswalk unavailable");
                            (!apply_episode_filter).then_some(candidate)
                        }
                    }
                },
                Err(error)=>{
                    tracing::warn!(imdb_id=%candidate.imdb_id,%error,
                        "season episode metadata unavailable for candidate");
                    (!apply_episode_filter).then_some(candidate)
                }
            }
        }).collect::<Vec<_>>()
    }else{imdb};
    // A verified anime absolute-to-season crosswalk is stronger than a
    // same-named live-action candidate whose episode bounds are unknown.
    // Competing verified identities are ambiguous: never select an arbitrary
    // S02E21 conversion. A known *canonical* S02E41 wins over a conversion.
    let mut imdb=imdb;
    if chinese_absolute_by_imdb.len()>1 {
        imdb.retain(|candidate|!chinese_absolute_by_imdb.contains_key(&candidate.imdb_id));
        chinese_absolute_by_imdb.clear();
    }
    if chinese_absolute_by_imdb.len()==1 && seasons.len()==1 && episodes.len()==1 {
        let confirmed:std::collections::HashSet<String>=imdb.iter()
            .filter(|candidate|season_episode_end(&db,&candidate.imdb_id,seasons[0])
                .ok().flatten().is_some_and(|end|end>=episodes[0]))
            .map(|candidate|candidate.imdb_id.clone()).collect();
        if !confirmed.is_empty(){
            imdb.sort_by_key(|candidate|!confirmed.contains(&candidate.imdb_id));
        }else{
            let verified_id=chinese_absolute_by_imdb.keys().next().unwrap();
            imdb.sort_by_key(|candidate|candidate.imdb_id.as_str()!=verified_id.as_str());
        }
    }
    // Preserve parsed /parse coordinates for auditing. For the ranked IMDb
    // winner only, use a uniquely verified absolute-to-season conversion.
    // No crosswalk means the original S02E41 is still rejected.
    let verified_chinese_override=imdb.first()
        .and_then(|candidate|chinese_absolute_by_imdb.get(&candidate.imdb_id).cloned());
    if let Some(mapped)=&verified_chinese_override {
        if !episodes.is_empty(){episodes[0]=mapped.episode;}
    }
    let imdb: Vec<Candidate> = imdb.into_iter().map(|m|Candidate{
        imdb_id:m.imdb_id,title:m.title,year:m.year,media_type:m.media_type,
        matched_title:m.matched_title,anime_ids:m.anime_ids,source:"imdb"}).collect();
    // Normal titles remain leaf-driven; an exact, verified AniDB alias from
    // the *nearest* anime subfolder may provide more specific part/season
    // context for the crosswalk resolver.
    let anime_subfolder_title=if series {
        verified_anime_subfolder_title(&db,parsed,&imdb)?
    }else {None};
    // A verified numeric season marker already supplies series-relative
    // coordinates. Resolve against the base title rather than treating
    // an exact "Jihanki 3" part alias as a local "season 3 of part 3".
    let anime_resolve_title=parsed.get("anime_season_episode_base_title")
        .and_then(Value::as_str)
        .or(anime_subfolder_title.as_deref()).unwrap_or(title);
    let anime_resolve_key=imdb_matcher::clean_title(anime_resolve_title);
    let anime = if verified_chinese_override.is_some()
        || verified_absolute_episode_imdb.is_some() {
        // A separately verified absolute episode owns its crosswalk.
        // Otherwise retain the established anime-priority path, including
        // its explicit collision and season eligibility protections.
        None
    } else if series {
        if let Some(ep)=episodes.first(){
            anime_candidate(&db,&anime_resolve_key,seasons.first().copied(),*ep)
                .map_err(|e|e.to_string())?
        }else{
            anime_identity(&db,&anime_resolve_key).map_err(|e|e.to_string())?
                .map(|id|{
                    let inferred=if seasons.is_empty() {
                        anime_title::season_from_title(&db,&id,anime_resolve_title).ok().flatten()
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
                && imdb_matcher::clean_title(&c.matched_title)==key && c.imdb_id!=anime_id
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
        // Keep established deterministic IMDb ranking and the proven
        // anime crosswalk priority. This is still required for anime parts
        // whose release title differs from the main IMDb series title.
        let mut chosen_index=0usize;
        // The original anime-aware crosswalk remains authoritative when
        // a season marker or unverified unmarked episode leaves several
        // IMDb candidates. The standalone AniDB verifier is additive.
        if series && verified_absolute_episode_imdb.is_none() {
            if let Some(ep)=episodes.first().copied(){
                for (index,candidate) in imdb.iter().enumerate(){
                    let mapped=match seasons.first().copied(){
                        Some(season)=>{
                            let titled=anime_title::resolve(
                                &db,&candidate.imdb_id,anime_resolve_title,season,ep)
                                .map_err(|e|e.to_string())?;
                            if titled.is_some(){titled}
                            else if anime_title::has_specific_alias(
                                &db,&candidate.imdb_id,anime_resolve_title)
                                .map_err(|e|e.to_string())?{None}
                            else{episode_crosswalk::resolve_season(
                                &db,&candidate.imdb_id,season,ep)
                                .map_err(|e|e.to_string())?}
                        },
                        None=>episode_crosswalk::resolve_absolute(
                            &db,&candidate.imdb_id,ep)
                            .map_err(|e|e.to_string())?,
                    };
                    if mapped.is_some(){chosen_index=index;break;}
                }
            }
        }
        let chosen=imdb.get(chosen_index);
        if let Some(c)=chosen {
            let mut selected_season=seasons.first().copied();
            if series && selected_season.is_none() && episodes.is_empty() {
                selected_season=anime_title::season_from_title(&db,&c.imdb_id,anime_resolve_title)
                    .map_err(|e|e.to_string())?;
            }
            let mut selected_episode=episodes.first().copied();
            let mut anime_result=Value::Null;

            if let Some(ep)=selected_episode {
                let mapped=if let Some(ref verified)=verified_chinese_override {
                    if verified.imdb_id==c.imdb_id {Some(verified.clone())} else {None}
                }else if let Some(season)=selected_season {
                    let titled=anime_title::resolve(&db,&c.imdb_id,anime_resolve_title,season,ep)
                        .map_err(|e|e.to_string())?;
                    if titled.is_some(){titled}
                    else if anime_title::has_specific_alias(&db,&c.imdb_id,anime_resolve_title)
                        .map_err(|e|e.to_string())? {None}
                    else {episode_crosswalk::resolve_season(&db,&c.imdb_id,season,ep)
                        .map_err(|e|e.to_string())?}
                }else{
                    // Without an Sxx marker, a verified exact anime part
                    // title is stronger than a potentially ambiguous
                    // absolute episode number.
                    let named=anime_title::resolve_episode_from_exact_part_title(
                        &db,&c.imdb_id,anime_resolve_title,ep)
                        .map_err(|e|e.to_string())?;
                    if named.is_some(){named}
                    else {
                        let absolute=episode_crosswalk::resolve_absolute(
                            &db,&c.imdb_id,ep).map_err(|e|e.to_string())?;
                        if absolute.is_some(){absolute}
                        else {
                            // "Anime - 04" usually means S01E04. Keep
                            // absolute numbering and named sequel parts
                            // higher priority, and verify the season-one
                            // crosswalk before filling an ASID.
                            let later_part=anime_title::season_from_title(
                                &db,&c.imdb_id,anime_resolve_title)
                                .map_err(|e|e.to_string())?.is_some_and(|s|s>1);
                            if anime_local_episode_season_one(
                                &db,&c.imdb_id,ep,!c.anime_ids.is_empty(),later_part)?{
                                episode_crosswalk::resolve_season(
                                    &db,&c.imdb_id,1,ep).map_err(|e|e.to_string())?
                            }else{None}
                        }
                    }
                };
                if let Some(mapped)=mapped{
                    selected_season=Some(mapped.season);
                    selected_episode=Some(mapped.episode);
                    anime_result=json!({"anidb_id":mapped.anidb_id,
                        "episode":mapped.anidb_episode,
                        "asid":format!("{}:{}",mapped.anidb_id,mapped.anidb_episode)});
                } else {
                    let specific=anime_title::has_specific_alias(
                        &db,&c.imdb_id,anime_resolve_title)
                        .map_err(|e|e.to_string())?;
                    let inferred=if selected_season.is_none(){
                        anime_title::season_from_title(
                            &db,&c.imdb_id,anime_resolve_title)
                            .map_err(|e|e.to_string())?
                    }else{None};
                    if selected_season.is_none() && series &&
                        anime_local_episode_season_one(
                            &db,&c.imdb_id,ep,!c.anime_ids.is_empty(),
                            inferred.is_some_and(|season|season>1))? {
                        // Tentative local anime episode: emit SID S01E04,
                        // but leave ASID null until independently verified.
                        selected_season=Some(1);
                    }else if selected_season.is_none() && series &&
                        !c.anime_ids.is_empty() &&
                        inferred.is_some_and(|season|season>1) &&
                        candidate_has_episode(&db,&c.imdb_id,inferred.unwrap_or(1),ep)? {
                        // Exact title identifies a later anime part even if
                        // the particular episode has no verified ASID.
                        selected_season=inferred;
                    }else if selected_season.is_some() && series &&
                        seasons.len()==1 && episodes.len()==1 &&
                        imdb_matcher::clean_title(&c.title)==key &&
                        season_episode_end(&db,&c.imdb_id,
                            selected_season.unwrap_or(0))?
                            .is_some_and(|last|ep<=last){
                        // A canonical IMDb title with an explicit season and
                        // episode may retain its verified IMDb SID even when
                        // the separate AniDB/TVDB crosswalk lacks an episode.
                        // Only known IMDb season bounds qualify; do not use
                        // this fallback for named sequel/part aliases, which
                        // can map the same local S01E03 into a later season.
                        // No crosswalk: ASID intentionally stays null.
                    }else if !c.anime_ids.is_empty()||specific{
                        // Once IMDb has selected a series, explicit parsed
                        // SxxExx coordinates are not guesses about the anime
                        // identity. An absent or ambiguous AniDB crosswalk
                        // must suppress only ASID, not season/episode or SID.
                        // Bare/part-local episode guesses remain conservative.
                        if seasons.len()!=1 || episodes.len()!=1 {
                            selected_season=None;
                            selected_episode=None;
                        }
                    }
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
    if let (Some(mapped),Some(winner))=(verified_chinese_override.as_ref(),selected.as_mut()){
        if winner["imdb_id"].as_str()==Some(mapped.imdb_id.as_str()) {
            winner["match_source"]=json!("verified_chinese_absolute_crosswalk");
            winner["absolute_episode_input"]=json!(chinese_absolute_hint);
        }
    }
    // Date-only releases use Cinemeta; explicit SxxExx coordinates take precedence.
    if airdate_only {
        if let (Some((_,date)),Some(winner))=(dated.as_ref(),selected.as_mut()){
            if let Some(id)=winner["imdb_id"].as_str().map(str::to_owned){
                match airdate::resolve(db_path,&id,date){
                    Ok(Some((season,episode,actual_air_date)))=>{
                        winner["season"]=json!(season);
                        winner["episode"]=json!(episode);
                        winner["season_end"]=json!(season);
                        winner["episode_end"]=json!(episode);
                        winner["sid"]=json!(format!("{id}:{season}:{episode}"));
                        // Preserve both the release date and actual Cinemeta
                        // date so a +/-1 timezone adjustment is auditable.
                        winner["air_date"]=json!(actual_air_date);
                        winner["release_date"]=json!(date);
                        winner["match_source"]=json!(if actual_air_date==*date {
                            "cinemeta_airdate"
                        }else{
                            "cinemeta_airdate_1day"
                        });
                    }
                    Ok(None)=>{},
                    Err(error)=>tracing::warn!(imdb_id=%id,%error,"airdate lookup failed"),
                }
            }
        }
    }
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
    // A season-only release needs both IMDb AND AniDB endpoints. Prior
    // code stopped at IMDb season_episodes, so /match always returned null
    // ASIDs for "Bleach_S02". Also support an exact, title-only anime pack
    // whose metadata verified season 1 (e.g. [Group][Chaos_Head][BDRip]).
    // A supplied episode or batch is never expanded to the entire season.
    if episodes.is_empty() && !parsed.contains_key("episode_start")
        && !parsed.contains_key("anime_episode_candidate")
        && !parsed.contains_key("anime_episode_range_candidate")
        && !parsed.contains_key("anime_season_episode_candidate")
        && season_range.is_none() {
        if let Some(winner)=selected.as_mut(){
            if let Some(imdb_id)=winner["imdb_id"].as_str().map(str::to_owned){
                let selected_season=seasons.first().copied()
                    .or_else(||winner["season"].as_i64());
                let linked_anime=winner["anime_ids"].as_object()
                    .is_some_and(|ids|!ids.is_empty())
                    || imdb.iter().any(|candidate|candidate.imdb_id==imdb_id
                        && !candidate.anime_ids.is_empty());
                // Avoid an extra Tantivy title lookup for a season pack
                // already linked through the selected IMDb identity.
                let exact_anime=if linked_anime && !seasons.is_empty(){
                    false
                }else{
                    exact_anime_identity(&db,&key)?
                        .is_some_and(|id|id==imdb_id)
                };
                let season=if let Some(season)=selected_season{
                    Some(season)
                } else if exact_anime {
                    // Only infer a missing season if IMDb knows the series
                    // has exactly one season. Never guess season 1 for a
                    // long-running anime or a generic folder title.
                    let max=db.query_row("SELECT max_season FROM seasons
                        WHERE imdb_id=?1",[&imdb_id],|row|row.get::<_,i64>(0))
                        .optional().map_err(|e|e.to_string())?;
                    (max==Some(1)).then_some(1)
                }else{None};
                if let Some(season)=season
                    && let Some(end)=season_episode_end(&db,&imdb_id,season)?
                {
                    winner["season"]=json!(season);
                    winner["episode"]=json!(1);
                    winner["season_end"]=json!(season);
                    winner["episode_end"]=json!(end);
                    winner["episode_range"]=json!({"start_season":season,"start_episode":1,
                        "end_season":season,"end_episode":end,
                        "source":"imdb_season_episodes"});
                    winner["sid"]=json!(format!("{imdb_id}:{season}:1"));
                    // The relational AniDB bridge can be present even if
                    // the optional anime_ids enrichment is absent or stale.
                    // Query by selected IMDb ID, never by ambiguous title;
                    // the actual range still needs fully verified mappings.
                    let has_anime=linked_anime || exact_anime || db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM anime_series_map WHERE imdb_id=?1)",
                        [&imdb_id],|row|row.get::<_,i64>(0))
                        .map_err(|e|e.to_string())? != 0;
                    if has_anime {
                        let canonical_title=imdb.iter().any(|candidate|
                            candidate.imdb_id==imdb_id
                            && imdb_matcher::clean_title(&candidate.title)==anime_resolve_key)
                            && anime_subfolder_title.is_none();
                        let mapped=mapped_season_episode_range(
                            &db,&imdb_id,anime_resolve_title,season,1,end,canonical_title)?;
                        // Keep the exact missing/ambiguous-map diagnostics
                        // without weakening the verified public ASID contract.
                        winner["anime_range_diagnostics"]=mapped.clone();
                        // AniDB endpoints are published only when every
                        // requested IMDb episode has an unambiguous mapping.
                        // Incomplete maps keep the verified IMDb range.
                        if mapped["complete"]==true
                            && mapped["segments"].as_array().is_some_and(|s|!s.is_empty())
                            && mapped["start_season"].as_i64()==Some(season)
                            && mapped["start_episode"].as_i64()==Some(1)
                            && mapped["end_episode"].as_i64()==Some(end)
                        {
                            winner["episode_range"]=mapped;
                        }
                    }
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
            let numbering=parsed.get("episode_range_numbering")
                .and_then(Value::as_str).unwrap_or("absolute");
            if numbering=="season" {
                let season=seasons.first().copied().unwrap_or(0);
                if season>0 {
                    let linked=winner["anime_ids"].as_object()
                        .is_some_and(|ids|!ids.is_empty())
                        || anime_title::has_specific_alias(&db,&imdb_id,anime_resolve_title)
                            .map_err(|e|e.to_string())?;
                    let range=if linked {
                        mapped_season_episode_range(&db,&imdb_id,anime_resolve_title,season,start,end,false)?
                    }else {
                        let known_end=season_episode_end(&db,&imdb_id,season)?;
                        let valid=known_end.is_some_and(|last|last>=end);
                        json!({"start_season":if valid {Some(season)}else{None},
                            "start_episode":if valid {Some(start)}else{None},
                            "end_season":if valid {Some(season)}else{None},
                            "end_episode":if valid {Some(end)}else{None},
                            "complete":valid,"segments":[],"source":"imdb_season_bounds",
                            "requested_season":season,
                            "requested_episode_start":start,"requested_episode_end":end})
                    };
                    winner["episode_range"]=range.clone();
                    winner["season"]=range["start_season"].clone();
                    winner["episode"]=range["start_episode"].clone();
                    winner["season_end"]=range["end_season"].clone();
                    winner["episode_end"]=range["end_episode"].clone();
                    winner["sid"]=if let (Some(se),Some(ep))=(
                        range["start_season"].as_i64(),range["start_episode"].as_i64()){
                        json!(format!("{imdb_id}:{se}:{ep}"))
                    }else{json!(imdb_id)};
                    winner["anime"]=Value::Null;
                }
            }else if verified_anime_range_imdb.as_ref()
                .is_none_or(|verified|verified==&imdb_id) {
                let range=mapped_absolute_range(&db,&imdb_id,start,end)?;
                // Preserve the requested absolute endpoints even when some
                // crosswalk entries are missing, but never invent IMDb or
                // AniDB episode coordinates.
                winner["episode_range"]=range.clone();
                winner["season"]=range["start_season"].clone();
                winner["episode"]=range["start_episode"].clone();
                winner["season_end"]=range["end_season"].clone();
                winner["episode_end"]=range["end_episode"].clone();
                winner["sid"]=if let (Some(se),Some(ep))=(
                    range["start_season"].as_i64(),range["start_episode"].as_i64()){
                    json!(format!("{imdb_id}:{se}:{ep}"))
                }else{json!(imdb_id)};
                winner["anime_episode_start"]=json!(start);
                winner["anime_episode_end"]=json!(end);
                winner["anime"]=Value::Null;
            }
        }
    }
    if let Some(winner)=selected.as_mut(){
        enrich_selected_anime_episode(&db,winner)?;
    }
    // AniDB verification is a separate identity lane. An IMDb match is
    // useful for season coordinates but never a precondition for a proven
    // AniDB identity/episode; equally, ASID is not inferred from IMDb alone.
    // Keep the extra Tantivy lookup off the ordinary TV fast path.
    // An independently matched AniDB episode is needed only when the
    // original crosswalk has not already supplied an ASID and the release
    // either has an anime signal or no valid IMDb identity at all.
    let independent_season_needed=selected.as_ref().is_none_or(|winner|{
        winner["category"]=="series"
            && winner.pointer("/anime/asid").and_then(Value::as_str).is_none()
            && (winner["anime_ids"].as_object()
                    .is_some_and(|ids|!ids.is_empty())
                || parsed.get("anime_title_verified").and_then(Value::as_bool)==Some(true)
                || parsed.get("anime_season_episode_candidate").is_some())
    });
    let independent_season=if independent_season_needed
        && req.series!=Some(false)
        && verified_anidb_absolute.is_none()
        && verified_anidb_range.is_none()
        && seasons.len()==1 && episodes.len()==1
        && parsed.get("anime_episode_range_candidate").is_none()
    {
        anidb_matcher::match_season(&db,anime_resolve_title,seasons[0],episodes[0])?
    }else{None};
    let independently_verified=verified_anidb_absolute.as_ref()
        .or(independent_season.as_ref());
    let anidb_candidate=if let Some(item)=independently_verified {
        Some(json!({"anidb_id":item.anidb_id,"episode":item.episode,
            "asid":item.asid(),"verified":true,"source":"anidb_tantivy"}))
    }else{
        verified_anidb_range.as_ref().map(|item|json!({
            "anidb_id":item.anidb_id,"start":item.start,"end":item.end,
            "verified":true,"source":"anidb_tantivy_range"
        }))
    };
    if let Some(item)=independently_verified {
        let independent_asid=item.asid();
        if selected.is_none(){
            selected=Some(json!({"imdb_id":null,"title":title,
                "category":"series","season":null,"episode":null,
                "anime":{"anidb_id":item.anidb_id,"episode":item.episode,
                    "asid":independent_asid},
                "anidb_independent_verified":true,
                "match_source":"anidb_only","parsed":parsed_value.clone()}));
        }else if let Some(winner)=selected.as_mut(){
            if winner.get("episode_range").is_none()
                && winner["category"]=="series" {
                match winner.pointer("/anime/asid").and_then(Value::as_str) {
                    Some(existing) if existing!=independent_asid => {
                        // The existing IMDb-aware resolver has already
                        // verified a complete SID/ASID crosswalk. The new
                        // AniDB-only lane is supplementary and must not
                        // erase a previously valid match. Record conflicts
                        // for diagnostics instead of publishing its guess.
                        winner["anidb_conflict"]=json!({
                            "independent":independent_asid,"imdb_crosswalk":existing});
                    },
                    _ => {
                        if anidb_related_to_selected_imdb(&db,winner,&item.anidb_id)?{
                            winner["anime"]=json!({"anidb_id":item.anidb_id,
                                "episode":item.episode,"asid":independent_asid});
                            winner["anidb_independent_verified"]=json!(true);
                        }else{
                            winner["anidb_conflict"]=json!({
                                "independent":independent_asid,
                                "reason":"no IMDb-to-AniDB relationship"});
                        }
                    }
                }
            }
        }
    }
    if let Some(item)=verified_anidb_range.as_ref(){
        if selected.is_none(){
            selected=Some(json!({"imdb_id":null,"title":title,"category":"series",
                "anime":null,"match_source":"anidb_only",
                "parsed":parsed_value.clone()}));
        }
        if let Some(winner)=selected.as_mut(){
            if winner["imdb_id"].is_null()
                || anidb_related_to_selected_imdb(&db,winner,&item.anidb_id)?{
                winner["anidb_independent_verified"]=json!(true);
                winner["anidb_range"]=json!({"anidb_id":item.anidb_id,
                    "start":item.start,"end":item.end});
            }else{
                winner["anidb_conflict"]=json!({
                    "anidb_id":item.anidb_id,
                    "reason":"no IMDb-to-AniDB relationship"});
            }
        }
    }
    let reason = if anidb_candidate.is_some() && selected.as_ref()
        .is_some_and(|winner|winner["imdb_id"].is_null()) {
        "Independently verified AniDB episode without IMDb identity"
    }else if anidb_candidate.is_some() {
        "Independent AniDB and IMDb match results"
    }else if anime.is_some() {
        "Verified anime crosswalk"
    }else if selected.is_some() {
        "IMDb title candidate (experimental matching rules)"
    }else{"No eligible candidate"};
    Ok(Diagnostic{parsed:parsed_value,normalized_title:key,imdb_candidates:imdb,
        anime_candidate:anime,anidb_candidate,selected,reason:reason.into()})
}

/// A canonical single-episode SID and a verified AniDB crosswalk should
/// always produce both identifiers in /match. Some episode coordinates are
/// assigned *after* candidate ranking (for example Cinemeta airdate), so
/// ensure crosswalk enrichment runs after those steps too. Do not invent an
/// ASID for non-anime, incomplete ranges, or unverified coordinates.
fn enrich_selected_anime_episode(
    db:&Connection,winner:&mut Value,
)->Result<(),String>{
    if winner["category"]!="series" || winner.get("episode_range").is_some()
        || winner.pointer("/anime/asid").and_then(Value::as_str).is_some(){
        return Ok(());
    }
    let Some(imdb)=winner["imdb_id"].as_str().map(str::to_owned)
        else{return Ok(())};
    let (Some(season),Some(episode))=(
        winner["season"].as_i64(),winner["episode"].as_i64())
        else{return Ok(())};
    if !metadata_import::valid_imdb_id(&imdb)
        || !valid_sid_coordinate(season,episode)
        || season<=0||episode<=0{return Ok(());}
    let Some(mapped)=episode_crosswalk::resolve_season(
        db,&imdb,season,episode).map_err(|e|e.to_string())?
        else{return Ok(())};
    if mapped.imdb_id!=imdb||mapped.season!=season||mapped.episode!=episode{
        return Ok(());
    }
    winner["anime"]=json!({
        "anidb_id":mapped.anidb_id,
        "episode":mapped.anidb_episode,
        "asid":format!("{}:{}",mapped.anidb_id,mapped.anidb_episode),
    });
    // Preserve the already chosen IMDb identity, episode coordinates and
    // source. The canonical title may contain several third-party IDs.
    if let Some(identities)=winner["anime_ids"].as_object_mut(){
        identities.insert("anidb".into(),json!(mapped.anidb_id));
    }else{
        winner["anime_ids"]=json!({"anidb":mapped.anidb_id});
    }
    Ok(())
}

fn diagnose_logged(state:&AppState,req:&MatchRequest,policy:MatchPolicy)->Result<Diagnostic,String>{
    let start=Instant::now();
    let result=if matches!(policy,MatchPolicy::PublicApi){
        diagnose(&state.db,req)
    }else{
        diagnose_with_policy(&state.db,req,policy)
    };
    let elapsed=start.elapsed().as_millis() as u64;
    match &result {
        Ok(d)=>{
            let status=if d.selected.is_some(){"matched"}else{"unmatched"};
            let details=serde_json::to_value(d).unwrap_or(Value::Null);
            dashboard_history::record_async(&state.history,&req.title,status,d.selected.as_ref(),&details,elapsed);
        }
        Err(error)=>dashboard_history::record_async(&state.history,&req.title,"error",None,&json!({"error":error}),elapsed),
    }
    result
}

async fn dashboard()->([(header::HeaderName,&'static str);1],&'static str){
    ([(header::CONTENT_TYPE,"text/html; charset=utf-8")],include_str!("dashboard.html"))
}

#[derive(Deserialize)]
struct HistoryQuery {limit:Option<usize>,offset:Option<usize>,status:Option<String>}

async fn dashboard_history_request(State(state):State<AppState>,Query(q):Query<HistoryQuery>)
    -> (StatusCode,Json<Value>){
    if q.status.as_deref().is_some_and(|s|!matches!(s,"matched"|"unmatched"|"error")){
        return (StatusCode::BAD_REQUEST,Json(json!({"error":"invalid status filter"})));
    }
    let result=tokio::task::spawn_blocking(move||
        dashboard_history::history(&state.history,q.limit.unwrap_or(25),q.offset.unwrap_or(0),q.status.as_deref())).await;
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
    let phase=index_phase(&state.index_phase);
    let path=state.db.clone();
    // Even the lightweight SQLite readiness probe runs off Tokio threads.
    let exists=tokio::task::spawn_blocking(move || {
        open_db(&path).ok().is_some_and(|db|{
            let metadata=db.query_row(
                "SELECT count(*) FROM metadata_state WHERE source='full_refresh'",
                [],|row|row.get::<_,i64>(0)).map(|count|count>0).unwrap_or(false);
            metadata&&tantivy_search::index_ready(&db)
        })
    }).await.unwrap_or(false);
    let ready=exists&&phase==IndexPhase::Ready;
    let (status,message)=if ready {
        ("ready","Metadata and search index ready".to_owned())
    }else{
        match phase {
            IndexPhase::Failed(error)=>("error",format!("Metadata indexing failed: {error}")),
            IndexPhase::Indexing|IndexPhase::Ready=>
                ("indexing","Metadata indexing in progress".to_owned()),
            IndexPhase::WaitingForMetadata=>
                ("initializing","Waiting for metadata import".to_owned()),
        }
    };
    (if ready{StatusCode::OK}else{StatusCode::SERVICE_UNAVAILABLE},
        Json(json!({"status":status,"message":message})))
}

/// Public response: winner coordinates plus the complete parser output.
/// Keep full candidates and ranking diagnostics exclusive to /debug.
fn public_match_response(selected: Option<Value>) -> Value {
    let Some(winner)=selected else {
        return json!({"sid_start":null,"sid_end":null,"asid_start":null,"asid_end":null});
    };
    let imdb=winner["imdb_id"].as_str().unwrap_or_default();
    // Refuse to publish a malformed SID even when an older metadata snapshot
    // still contains a comma-separated Anime-Lists IMDb field. Callers must
    // receive an unambiguous single title ID or an explicit no-match.
    if !metadata_import::valid_imdb_id(imdb){
        // An ASID verified by Tantivy's exact AniDB alias and bounded
        // episode evidence can stand without a valid IMDb identity.
        // Never grant this exception to malformed IMDb metadata alone.
        if winner["anidb_independent_verified"]!=true
            || winner.pointer("/parsed/half_episode").and_then(Value::as_bool)==Some(true) {
            return json!({"sid_start":null,"sid_end":null,
                "asid_start":null,"asid_end":null});
        }
        let independent=winner.get("anidb_range").and_then(|r|{
            let id=r["anidb_id"].as_str()?;
            let start=r["start"].as_i64()?;
            let end=r["end"].as_i64()?;
            if end<start{return None;}
            Some((episode_crosswalk::canonical_asid(id,start)?,
                episode_crosswalk::canonical_asid(id,end)?))
        });
        let single=winner.pointer("/anime/asid").and_then(Value::as_str)
            .filter(|asid|episode_crosswalk::valid_asid(asid)).map(str::to_owned);
        let (asid_start,asid_end)=if let Some((start,end))=independent {
            (json!(start),json!(end))
        }else{(json!(single),json!(single))};
        return json!({"sid_start":null,"sid_end":null,
            "asid_start":asid_start,"asid_end":asid_end});
    }
    // A release labelled E12.5 is not necessarily IMDb episode E12.
    // Preserve the verified show identity, but never publish a guessed
    // SxxExx/ASID for a fractional recap or special without a dedicated
    // metadata crosswalk for the fraction itself.
    if winner.pointer("/parsed/half_episode").and_then(Value::as_bool)==Some(true) {
        return json!({"sid_start":imdb,"sid_end":imdb,
            "asid_start":null,"asid_end":null});
    }
    let has_range=winner.get("episode_range").is_some();
    let start_season=winner["season"].as_i64();
    let start_episode=winner["episode"].as_i64();
    let end_season=winner["season_end"].as_i64()
        .or(if has_range {None}else{start_season});
    let end_episode=winner["episode_end"].as_i64()
        .or(if has_range {None}else{start_episode});
    // Retain the valid IMDb identity while discarding impossible episode
    // coordinates. A stale season_episodes snapshot or a bad parse must not
    // make the public /match API rejectable by StremThru.
    let invalid_coordinates=[(start_season,start_episode),(end_season,end_episode)]
        .into_iter().any(|(season,episode)|{
            matches!((season,episode),(Some(se),Some(ep)) if !valid_sid_coordinate(se,ep))
        });
    let sid_start=if invalid_coordinates{json!(imdb)}
        else if imdb.is_empty(){Value::Null}
        else if let (Some(se),Some(ep))=(start_season,start_episode){
            json!(format!("{imdb}:{se}:{ep}"))
        }else if has_range{Value::Null}else{json!(imdb)};
    let sid_end=if invalid_coordinates{json!(imdb)}
        else if imdb.is_empty(){Value::Null}
        else if let (Some(se),Some(ep))=(end_season,end_episode){
            json!(format!("{imdb}:{se}:{ep}"))
        }else if has_range{Value::Null}else{json!(imdb)};
    // An explicit second season without a corresponding last episode is
    // *not* a complete multi-season range. Previously the start episode
    // was silently reused as the last episode, or the end degraded to a
    // bare IMDb ID, creating invalid or misleading SID endpoints. Keep
    // only the proven start; no range-wide ASID can be inferred.
    let missing_cross_season_end=matches!((start_season,end_season),
        (Some(start),Some(end)) if end!=start)
        && winner["episode_end"].as_i64().is_none();
    if missing_cross_season_end {
        return json!({"sid_start":sid_start,"sid_end":null,
            "asid_start":null,"asid_end":null});
    }
    // StremThru accepts a pair only when both sides have the same ID,
    // both are either bare titles or episode coordinates, and coordinate
    // end is not before coordinate start. A malformed range must never
    // poison an entire torrent's deferred mapping batch.
    let sid_components=|raw:&str|->Option<(i64,i64)>{
        let mut parts=raw.split(':');
        if parts.next()!=Some(imdb){return None;}
        let season=parts.next()?.parse::<i64>().ok()?;
        let episode=parts.next()?.parse::<i64>().ok()?;
        if parts.next().is_some() || !valid_sid_coordinate(season,episode){
            return None;
        }
        Some((season,episode))
    };
    if let (Some(start),Some(end))=(sid_start.as_str(),sid_end.as_str()){
        let start_coords=sid_components(start);
        let end_coords=sid_components(end);
        let inconsistent=if start==imdb || end==imdb{
            start!=end
        }else{
            match (start_coords,end_coords){
                (Some(a),Some(b))=>b<a,
                _=>true,
            }
        };
        if inconsistent{
            // Retain a verified range start if the end is missing/bad.
            // Do not infer an end episode or attach a potentially
            // mismatched ASID to this partial range.
            let (safe_start,safe_end)=if has_range && start_coords.is_some(){
                (sid_start,Value::Null)
            }else{(json!(imdb),json!(imdb))};
            return json!({"sid_start":safe_start,"sid_end":safe_end,
                "asid_start":null,"asid_end":null});
        }
    }
    // Public API is a trust boundary: even a stale or malformed crosswalk
    // must never publish out-of-range ASIDs that make consumers retry a
    // whole batch. A valid IMDb SID survives without the invalid ASID.
    let direct_asid=winner.pointer("/anime/asid").and_then(Value::as_str)
        .filter(|value|episode_crosswalk::valid_asid(value))
        .map(str::to_owned);
    let segments=winner.pointer("/episode_range/segments").and_then(Value::as_array);
    let complete=winner.pointer("/episode_range/complete")
        .and_then(Value::as_bool).unwrap_or(true);
    let range_asids=segments.filter(|_|complete).and_then(|entries|{
        if entries.is_empty(){return None;}
        let mapped=entries.iter().map(|segment|{
            let id=segment["anidb_id"].as_str()?;
            Some((
                episode_crosswalk::canonical_asid(id,segment["start_anime_episode"].as_i64()?)?,
                episode_crosswalk::canonical_asid(id,segment["end_anime_episode"].as_i64()?)?,
            ))
        }).collect::<Option<Vec<_>>>()?;
        Some((mapped.first()?.0.clone(),mapped.last()?.1.clone()))
    });
    // Preserve the existing crosswalk as the authority on IMDb-linked
    // ranges. Independent AniDB-only proof may fill an ASID where the
    // legacy matcher has no range relationship at all, but may not
    // contradict a fully verified multi-cour IMDb mapping or turn an
    // incomplete legacy range into a guessed complete one.
    let independent_range=if winner["anidb_independent_verified"]==true
        && segments.is_none() && !has_range {
        winner.get("anidb_range").and_then(|r|{
            let id=r["anidb_id"].as_str()?;
            let start=r["start"].as_i64()?;
            let end=r["end"].as_i64()?;
            if end<start{return None;}
            Some((episode_crosswalk::canonical_asid(id,start)?,
                episode_crosswalk::canonical_asid(id,end)?))
        })
    }else{None};
    let (asid_start,asid_end)=if segments.is_some() {
        // Existing mapped ranges and incomplete-range safeguards win.
        range_asids.map_or((Value::Null,Value::Null),|(start,end)|
            (json!(start),json!(end)))
    }else if let Some((start,end))=independent_range {
        (json!(start),json!(end))
    }else{
        (json!(direct_asid),json!(direct_asid))
    };
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

/// Bound CPU-heavy matching before it enters Tokio's blocking thread pool.
/// This preserves full candidate ranking while protecting the process against
/// request bursts during metadata imports and index-generation changes.
async fn run_match(state: AppState, req: MatchRequest, policy:MatchPolicy)
    -> Result<Result<Diagnostic,String>,String> {
    let permit=state.match_slots.clone().acquire_owned().await
        .map_err(|e|e.to_string())?;
    tokio::task::spawn_blocking(move ||{
        let _permit=permit;
        diagnose_logged(&state,&req,policy)
    }).await.map_err(|e|e.to_string())
}

// The dashboard intentionally permits loose title-only diagnostics, but a
// background service must be able to inspect the *same* production decision
// as /match. A separate strict endpoint avoids changing dashboard behavior.
fn request_policy(debug:bool,strict:bool)->MatchPolicy{
    if debug && !strict{MatchPolicy::DashboardDiagnostic}
    else{MatchPolicy::PublicApi}
}

async fn match_response(state:AppState,req:MatchRequest,debug:bool,strict:bool)
    -> (StatusCode,Json<Value>){
    if !matching_available(&state){return indexing_response();}
    match run_match(state,req,request_policy(debug,strict)).await{
        Ok(Ok(d))=>{
            let value=if debug{
                serde_json::to_value(d).unwrap_or(Value::Null)
            }else{
                public_match_response(d.selected)
            };
            (StatusCode::OK,Json(value))
        },
        Ok(Err(e))=>(StatusCode::BAD_REQUEST,Json(json!({"error":e}))),
        Err(e)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e}))),
    }
}

async fn match_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>){
    match_response(state,req,false,false).await
}

/// GET /match?title=...&year=...
async fn get_match_request(State(state):State<AppState>,Query(req):Query<MatchRequest>)
    -> (StatusCode,Json<Value>){
    match_response(state,req,false,false).await
}

// Integration diagnostics and public /match MUST share the same identity
// selection policy. The dashboard's intentionally permissive search has a
// separate endpoint so it cannot corrupt StremThru crosswalk validation.
async fn debug_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>){
    match_response(state,req,true,true).await
}

async fn dashboard_diagnose_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>){
    match_response(state,req,true,false).await
}

/// The integration-safe debug endpoint uses exactly the same selection
/// policy as /match, but returns the selected range segments as diagnostics.
/// Do not use the permissive dashboard policy for cross-request verification.
async fn strict_debug_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>){
    match_response(state,req,true,true).await
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
    // Bind the HTTP listener before potentially expensive startup indexing.
    // Keep the generation's completion marker authoritative for readiness.
    let index_state=Arc::new(Mutex::new(
        if std::path::Path::new(&db).is_file(){
            IndexPhase::Indexing
        }else{IndexPhase::WaitingForMetadata}
    ));

    // Built-in HTTPS metadata feeds initialize a fresh Docker volume automatically.
    // MATCHLAB_METADATA_URLS can override all seven feeds (or six legacy URLs).
    {
        let urls=metadata_download::configured_urls(env::var("MATCHLAB_METADATA_URLS").ok().as_deref())?;
        let hours=env::var("MATCHLAB_METADATA_REFRESH_HOURS")
            .unwrap_or_else(|_|"168".into()).parse::<u64>()?;
        if !(1..=24*30).contains(&hours) {
            return Err("MATCHLAB_METADATA_REFRESH_HOURS must be between 1 and 720".into());
        }
        let target=PathBuf::from(&db);
        let refresh_index_state=index_state.clone();
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
                    Ok(Ok((imdb,links,mappings,aliases))) => {
                        tracing::info!(imdb,links,mappings,aliases,"scheduled metadata refresh completed");
                        // Pre-open the new on-disk index off the async executor.
                        // This avoids leaving the first request after refresh to
                        // perform a full Tantivy build.
                        set_index_phase(&refresh_index_state,IndexPhase::Indexing);
                        let warm_target=target.clone();
                        match tokio::task::spawn_blocking(move || -> Result<(),String> {
                            let conn=Connection::open_with_flags(
                                &warm_target,OpenFlags::SQLITE_OPEN_READ_ONLY)
                                .map_err(|e|e.to_string())?;
                            tantivy_search::warmup(&conn)
                        }).await {
                            Ok(Ok(())) => {
                                set_index_phase(&refresh_index_state,IndexPhase::Ready);
                                tracing::info!("refreshed Tantivy index ready");
                            },
                            Ok(Err(error)) => {
                                set_index_phase(&refresh_index_state,
                                    IndexPhase::Failed(error.clone()));
                                tracing::error!(%error,"Tantivy refresh warmup failed");
                            },
                            Err(error) => {
                                set_index_phase(&refresh_index_state,
                                    IndexPhase::Failed(error.to_string()));
                                tracing::error!(%error,"Tantivy warmup task failed");
                            },
                        }
                    },
                    Ok(Err(error)) => tracing::error!(%error,"scheduled metadata refresh failed; existing database preserved"),
                    Err(error) => tracing::error!(%error,"scheduled metadata refresh task failed"),
                }
                // On failure retry after one hour; successful imports persist
                // their completion timestamp in metadata_state/full_refresh.
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }
    // Bound blocking match operations independently of the async HTTP executor.
    let max_matches=env::var("MATCHLAB_MAX_CONCURRENT_MATCHES")
        .unwrap_or_else(|_|"4".into()).parse::<usize>()?;
    if !(1..=64).contains(&max_matches) {
        return Err("MATCHLAB_MAX_CONCURRENT_MATCHES must be between 1 and 64".into());
    }
    tracing::info!(max_matches,"MatchLab match concurrency configured");
    let history_db=env::var("MATCHLAB_HISTORY_DB").unwrap_or_else(|_|"/data/history.sqlite3".into());
    // Backfill existing 14-day statistics before the retention worker drops raw rows.
    let history_ready=match dashboard_history::initialize(std::path::Path::new(&history_db)) {
        Ok(())=>true,
        Err(error)=>{
            // Keep matching available, but never delete un-migrated history:
            // those rows may be needed to rebuild the 14-day aggregates later.
            tracing::error!(%error,"history/statistics initialization failed; retention disabled");
            false
        }
    };
    // Only prune after statistics were safely backfilled. On failure the
    // history is left untouched so the migration can be retried at restart.
    if history_ready {
        let history_path=PathBuf::from(&history_db);
        tokio::spawn(async move {
            loop {
                let path=history_path.clone();
                match tokio::task::spawn_blocking(move||dashboard_history::prune(&path)).await {
                    Ok(Ok(count)) if count>0 =>
                        tracing::info!(deleted=count,"expired request history removed"),
                    Ok(Ok(_)) => (),
                    Ok(Err(error)) =>
                        tracing::warn!(%error,"request history cleanup failed"),
                    Err(error) =>
                        tracing::warn!(%error,"history cleanup task failed"),
                }
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        });
    }
    // A pre-existing volume may need a fresh Tantivy schema generation.
    // Start this in the blocking pool only after all service configuration is
    // validated. /health/live and the dashboard remain reachable throughout.
    let listener = tokio::net::TcpListener::bind(&host).await?;
    if std::path::Path::new(&db).is_file(){
        let startup_target=PathBuf::from(&db);
        let startup_index_state=index_state.clone();
        tokio::spawn(async move{
            let warmup=tokio::task::spawn_blocking(move||->Result<(),String>{
                let conn=Connection::open_with_flags(
                    &startup_target,OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e|e.to_string())?;
                tantivy_search::warmup(&conn)
            }).await;
            match warmup{
                Ok(Ok(()))=>{
                    set_index_phase(&startup_index_state,IndexPhase::Ready);
                    tracing::info!("startup Tantivy indexing completed");
                },
                Ok(Err(error))=>{
                    set_index_phase(&startup_index_state,IndexPhase::Failed(error.clone()));
                    tracing::error!(%error,"startup Tantivy indexing failed");
                },
                Err(error)=>{
                    set_index_phase(&startup_index_state,
                        IndexPhase::Failed(error.to_string()));
                    tracing::error!(%error,"startup Tantivy worker failed");
                }
            }
        });
    }
    let app = Router::new().route("/",get(dashboard))
        .route("/api/dashboard/history",get(dashboard_history_request))
        .route("/api/dashboard/stats",get(dashboard_stats_request))
        .route("/api/debug/parsing",get(debug_feed::list))
        .route("/api/debug/parsing/status",get(debug_feed::status))
        .route("/api/debug/parsing/{id}",get(debug_feed::detail))
        .route("/health",get(readiness))
        .route("/health/ready",get(readiness))
        .route("/health/live",get(liveness))
        .route("/match",get(get_match_request).post(match_request))
        .route("/alias",get(alias::get_alias).post(alias::post_alias))
        .route("/episode/resolve",get(episode_conversion::get_resolve).post(episode_conversion::post_resolve))
        .route("/parse",get(get_parse_request).post(parse_request))
        .route("/debug/match",post(debug_request))
        .route("/debug/match/strict",post(strict_debug_request))
        .route("/api/dashboard/diagnose",post(dashboard_diagnose_request))
        .with_state(AppState{db:Arc::new(PathBuf::from(db)),history:Arc::new(PathBuf::from(history_db)),
            match_slots:Arc::new(Semaphore::new(max_matches)),
            index_phase:index_state});
    tracing::info!("Rust API listening on {host}");
    axum::serve(listener,app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn season_only_anime_packs_expose_complete_imdb_and_anidb_ranges() {
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-anime-packs-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
                imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES
                ('tt0000101','Bleach','bleach',2004,'series','',1,0,'tvSeries'),
                ('tt0000202','The Duke of Death and His Maid',
                  'thedukeofdeathandhismaid',2021,'series','',1,0,'tvSeries'),
                ('tt0000303','Chaos;Head','chaoshead',2008,'series','',1,0,'tvSeries'),
                ('tt0000404','Reborn as a Vending Machine',
                 'rebornasavendingmachine',2023,'series','',1,0,'tvSeries'),
                ('tt0000505','Bleach side story','bleachsidestory',2005,
                 'movie','',1,0,'movie');
            INSERT INTO seasons VALUES ('tt0000101',16),('tt0000202',3),
                ('tt0000303',1),('tt0000404',3);
            INSERT INTO season_episodes VALUES
                ('tt0000101',1,20,20),('tt0000101',2,21,21),
                ('tt0000202',2,12,12),
                ('tt0000303',1,12,12),('tt0000404',3,12,12);
            INSERT INTO anime_series_map VALUES
                ('tt0000101','100','bleachtv',0),
                ('tt0000101','101','otherbleachtv',0),
                ('tt0000505','500','sidebleachtv',0),
                ('tt0000202','200','duketv',0),
                ('tt0000303','300','chaostv',0),
                ('tt0000404','400','jihankitv',0);
            INSERT INTO anime_ids VALUES
                ('tt0000202','anidb','200'),
                ('tt0000303','anidb','300'),
                ('tt0000404','anidb','400');
            INSERT INTO anidb_titles VALUES
                ('100','Bleach','bleach','main','en'),
                ('101','Bleach','bleach','synonym','en'),
                ('500','Bleach','bleach','synonym','en'),
                ('200','Shinigami Bocchan to Kuro Maid',
                 'shinigamibocchantokuromaid','synonym','ja'),
                ('300','Chaos Head','chaoshead','main','en'),
                ('400','Jihanki 3','jihanki3','synonym','ja');
            INSERT INTO episode_map VALUES
                ('100','bleachtv',1,-1,1,366,0,''),
                ('100','bleachtv',1,1,1,20,0,''),
                ('101','otherbleachtv',1,99,1,12,0,''),
                ('500','sidebleachtv',1,1,1,2,0,''),
                ('100','bleachtv',1,2,22,42,-21,''),
                ('100','bleachtv',1,14,300,319,-299,''),
                ('100','bleachtv',1,15,320,340,-319,''),
                ('100','bleachtv',1,16,341,366,-340,''),
                -- A newer Bleach cour belongs to the same TVDB family but
                -- must not compete with the explicitly mapped series.
                ('901','bleachtv',1,17,0,0,0,''),
                ('200','duketv',1,-1,13,24,0,''),
                ('200','duketv',1,2,13,24,-12,''),
                ('300','chaostv',1,-1,1,12,0,''),
                ('300','chaostv',1,1,1,12,0,''),
                ('400','jihankitv',1,-1,1,36,0,''),
                ('400','jihankitv',1,3,1,12,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        // Deliberately omit Bleach from anime_ids while retaining its real
        // AniDB series/mapping bridge: season lookup must not depend on it.
        // IMDb already knows the complete first season. Equal Bleach
        // aliases for an unrelated film and a separate AniDB part must not
        // suppress the one fully verified season mapping.
        let d=diagnose(&path,&MatchRequest{
            title:"Bleach S01".into(),year:None,series:None}).unwrap();
        let selected=d.selected.expect("Bleach S01 IMDb match");
        assert_eq!(selected["episode_range"]["complete"],true,"{selected}");
        assert_eq!(selected["anime_range_diagnostics"]["unmapped_count"],0);
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000101:1:1","sid_end":"tt0000101:1:20",
            "asid_start":"100:1","asid_end":"100:20"
        }));
        // An absolute episode matches only when one IMDb identity has
        // a verified crosswalk for it, despite shared AniDB title aliases.
        // Fixture season 14 maps absolute episode 300 -> IMDb S14E01.
        // These are synthetic boundaries, NOT a claim about live IMDb.
        // All three release shapes must survive the strict public filter
        // even without optional anime_ids, year or release-quality hints.
        for title in ["Bleach_300","Bleach.300","Bleach - 300"] {
            let d=diagnose(&path,&MatchRequest{
                title:title.into(),year:None,series:None}).unwrap();
            let selected=d.selected.expect("verified Bleach absolute episode");
            assert_eq!(selected["imdb_id"],"tt0000101","{title}: {selected}");
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":"tt0000101:14:1",
                "sid_end":"tt0000101:14:1",
                "asid_start":"100:300","asid_end":"100:300"
            }),"{title}");
        }
        // Regression for production's 'Bleach 320': shared TVDB sibling
        // defaults must not suppress a uniquely bounded episode mapping.
        // Fixture coordinates are synthetic: absolute 320 -> S15E01.
        for title in ["Bleach 320","Bleach_320","Bleach.320"] {
            let d=diagnose(&path,&MatchRequest{
                title:title.into(),year:None,series:None}).unwrap();
            let selected=d.selected.expect("bounded Bleach 320 match");
            assert_eq!(selected["match_source"],"imdb","{title}: {selected}");
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":"tt0000101:15:1",
                "sid_end":"tt0000101:15:1",
                "asid_start":"100:320","asid_end":"100:320"
            }),"{title}");
        }
        let d=diagnose(&path,&MatchRequest{
            title:"Bleach_338".into(),year:None,series:None}).unwrap();
        let selected=d.selected.expect("Bleach absolute episode 338 match");
        assert_eq!(selected["season"],15,"{selected}");
        assert_eq!(selected["episode"],19,"{selected}");
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000101:15:19","sid_end":"tt0000101:15:19",
            "asid_start":"100:338","asid_end":"100:338"
        }));
        let d=diagnose(&path,&MatchRequest{
            title:"Bleach_S02".into(),year:None,series:None}).unwrap();
        let selected=d.selected.expect("Bleach season pack IMDb");
        assert_eq!(selected["episode_range"]["complete"],true,"{selected}");
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000101:2:1","sid_end":"tt0000101:2:21",
            "asid_start":"100:22","asid_end":"100:42"
        }));
        // Absolute episode batches can cross IMDb seasons without becoming
        // a season-local episode range or requiring a separate "E" marker.
        // Bleach S17 is eligible via the existing season guard even if
        // IMDb has imported only 16 seasons. Adding E01 must not erase the
        // explicit coordinates just because AniDB has no provable map.
        for release in ["bleach.S17E01","bleach.S17.E01"] {
            let parsed=matchlab_parser::parse_title(release,false).unwrap();
            assert_eq!(parsed["seasons"],json!([17]),"{release}: {parsed:?}");
            assert_eq!(parsed["episodes"],json!([1]),"{release}: {parsed:?}");
            let d=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let selected=d.selected.expect("selected Bleach IMDb series");
            assert_eq!(selected["imdb_id"],"tt0000101","{release}: {selected}");
            assert_eq!(selected["season"],17,"{release}: {selected}");
            assert_eq!(selected["episode"],1,"{release}: {selected}");
            let response=public_match_response(Some(selected));
            assert_eq!(response["sid_start"],"tt0000101:17:1","{release}: {response}");
            assert_eq!(response["sid_end"],"tt0000101:17:1","{release}: {response}");
        }
        let d=diagnose(&path,&MatchRequest{
            title:"Bleach_320-360".into(),year:None,series:None}).unwrap();
        assert_eq!(d.parsed["anime_title_verified"],true);
        assert_eq!(d.parsed["episode_range_numbering"],"absolute");
        let selected=d.selected.expect("Bleach absolute batch identity");
        assert_eq!(selected["episode_range"]["complete"],true,"{selected}");
        assert_eq!(selected["episode_range"]["segments"]
            .as_array().expect("range segments").len(),2);
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000101:15:1","sid_end":"tt0000101:16:20",
            "asid_start":"100:320","asid_end":"100:360"
        }));
        // No English IMDb alias appears anywhere in the romanized release.
        let d=diagnose(&path,&MatchRequest{
            title:"[orion origin] Shinigami Bocchan to Kuro Maid S2 [13-24]  [WebRip] [1080p] [H265 AAC] [CHS＆JPN]".into(),
            year:None,series:None}).unwrap();
        let selected=d.selected.expect("romanized AniDB alias matched to English IMDb title");
        assert_eq!(selected["episode_range"]["complete"],true,"{selected}");
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000202:2:1","sid_end":"tt0000202:2:12",
            "asid_start":"200:13","asid_end":"200:24"
        }));
        let d=diagnose(&path,&MatchRequest{
            title:"[HKG&SumiSora&CASO][Chaos_Head][BDRip]".into(),
            year:None,series:None}).unwrap();
        let selected=d.selected.expect("exact anime pack identity");
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000303:1:1","sid_end":"tt0000303:1:12",
            "asid_start":"300:1","asid_end":"300:12"
        }));
        let d=diagnose(&path,&MatchRequest{
            title:"[Erai-raws] Jihanki 3 - 08 [1080p CR WEBRip HEVC AAC][MultiSub][C869EEDC].mkv".into(),
            year:None,series:None}).unwrap();
        assert_eq!(d.parsed["anime_title_verified"],true,"{:?}",d.parsed);
        assert_eq!(d.parsed["seasons"],json!([3]));
        assert_eq!(d.parsed["episodes"],json!([8]));
        assert_eq!(d.parsed["anime_season_episode_base_title"],"Jihanki");
        assert_eq!(d.parsed["title"],"Jihanki 3");
        assert!(d.parsed.get("episode_start").is_none());
        let selected=d.selected.expect("metadata verifies season 3 episode 08");
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0000404:3:8","sid_end":"tt0000404:3:8",
            "asid_start":"400:8","asid_end":"400:8"
        }));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn bleach_absolute_episode_uses_ranked_winner_despite_five_anime_list_imdb_ids(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-bleach-five-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
                imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES
                ('tt0434665','Bleach','bleach',2004,'series','',1,0,'tvSeries'),
                ('tt1105263','Bleach OVA A','bleachovaa',2008,'movie','',1,0,'movie'),
                ('tt1148261','Bleach Film B','bleachfilmb',2009,'movie','',1,0,'movie'),
                ('tt1339302','Bleach Film C','bleachfilmc',2010,'movie','',1,0,'movie'),
                ('tt1785394','Bleach Special D','bleachspeciald',2011,'movie','',1,0,'tvMovie');
            INSERT INTO seasons VALUES ('tt0434665',16);
            INSERT INTO season_episodes VALUES ('tt0434665',15,26,26);
            INSERT INTO anime_series_map VALUES
                ('tt0434665','2369','74796',0),
                ('tt1105263','2369','74796',0),
                ('tt1148261','2369','74796',0),
                ('tt1339302','2369','74796',0),
                ('tt1785394','2369','74796',0);
            INSERT INTO anidb_titles VALUES('2369','Bleach','bleach','main','en');
            INSERT INTO episode_map VALUES
                ('2369','74796',1,-1,1,366,0,''),
                ('2369','74796',1,15,317,342,-316,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        // The Anime-Lists crosswalk really resolves all five IMDb IDs;
        // reverse lookup must remain ambiguous, but /match must use the
        // already-ranked original TV-series winner.
        for id in ["tt0434665","tt1105263","tt1148261",
            "tt1339302","tt1785394"] {
            let mapped=episode_crosswalk::resolve_absolute(&db,id,320).unwrap()
                .expect("crosswalk must map episode 320");
            assert_eq!((mapped.season,mapped.episode),(15,4));
        }
        drop(db);
        for title in ["Bleach 320","Bleach_320","Bleach.320"] {
            let d=diagnose(&path,&MatchRequest{
                title:title.into(),year:None,series:None}).unwrap();
            let selected=d.selected.expect("ranked Bleach series winner");
            assert_eq!(selected["imdb_id"],"tt0434665");
            assert_eq!(selected["match_source"],"imdb");
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":"tt0434665:15:4",
                "sid_end":"tt0434665:15:4",
                "asid_start":"2369:320","asid_end":"2369:320"
            }),"{title}");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn independent_anidb_match_publishes_asid_without_any_imdb_identity(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-anidb-only-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,
                ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO anidb_titles VALUES
                ('700','Independent Anime','independentanime','main','en'),
                ('701','Ambiguous Show','ambiguousshow','main','en'),
                ('702','Ambiguous Show','ambiguousshow','main','en');
            INSERT INTO episode_map VALUES
                ('700','notimdb',1,1,1,10,0,''),
                ('701','other',1,1,1,12,0,''),
                ('702','other',1,1,1,12,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let d=diagnose(&path,&MatchRequest{
            title:"Independent.Anime.S01E07".into(),
            year:None,series:None}).unwrap();
        let winner=d.selected.expect("independent AniDB identity");
        assert!(winner["imdb_id"].is_null(),"{winner}");
        assert_eq!(winner["match_source"],"anidb_only");
        assert_eq!(winner["anidb_independent_verified"],true);
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":null,"sid_end":null,
            "asid_start":"700:7","asid_end":"700:7"}));
        let ambiguous=diagnose(&path,&MatchRequest{
            title:"Ambiguous.Show.S01E07".into(),
            year:None,series:None}).unwrap();
        assert!(ambiguous.anidb_candidate.is_none());
        assert_eq!(public_match_response(ambiguous.selected),json!({
            "sid_start":null,"sid_end":null,
            "asid_start":null,"asid_end":null}));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn public_match_rejects_invalid_anidb_asids_without_losing_imdb_identity(){
        for malformed in ["732:190315","436:180525","6424:431078",
            "732:0","732:foo","bad:3","732:3:4"]{
            let selected=json!({"imdb_id":"tt1234567","category":"series",
                "season":1,"episode":3,"anime":{"asid":malformed}});
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":"tt1234567:1:3","sid_end":"tt1234567:1:3",
                "asid_start":null,"asid_end":null
            }),"{malformed}");
        }
        let valid=json!({"imdb_id":"tt1234567","category":"series",
            "season":1,"episode":3,"anime":{"asid":"732:99999"}});
        let valid_response=public_match_response(Some(valid));
        assert_eq!(valid_response["asid_start"],"732:99999");
        assert_eq!(valid_response["asid_end"],"732:99999");

        // Incomplete or out-of-bounds ranges must not leak just one
        // endpoint. Keep the valid IMDb coordinates independent.
        for (complete,episode) in [(true,190315),(false,3)] {
            let winner=json!({"imdb_id":"tt1234567","category":"series",
                "season":1,"episode":3,"season_end":1,"episode_end":7,
                "anime":{"asid":"732:3"},
                "episode_range":{"complete":complete,"segments":[{
                    "anidb_id":"732","start_anime_episode":3,
                    "end_anime_episode":episode
                }]}});
            let response=public_match_response(Some(winner));
            assert_eq!(response["sid_start"],"tt1234567:1:3");
            assert_eq!(response["sid_end"],"tt1234567:1:7");
            assert!(response["asid_start"].is_null());
            assert!(response["asid_end"].is_null());
        }
    }

    #[test]
    fn independently_verified_anidb_id_cannot_be_combined_with_unrelated_imdb(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_series_map VALUES
                ('tt2359704','16187','262954',0),
                ('tt0434665','2369','74796',0);
            INSERT INTO anime_ids VALUES('tt2359704','anidb','16187');").unwrap();
        let jojo=json!({"imdb_id":"tt2359704","category":"series"});
        let bleach=json!({"imdb_id":"tt0434665","category":"series"});
        assert!(anidb_related_to_selected_imdb(&db,&jojo,"16187").unwrap());
        assert!(anidb_related_to_selected_imdb(&db,&bleach,"2369").unwrap());
        assert!(!anidb_related_to_selected_imdb(&db,&bleach,"16187").unwrap());
        assert!(!anidb_related_to_selected_imdb(&db,&jojo,"2369").unwrap());
        assert!(!anidb_related_to_selected_imdb(
            &db,&json!({"imdb_id":null}),"16187").unwrap());
    }

    #[test]
    fn independent_anidb_evidence_never_weakens_existing_valid_imdb_match(){
        let baseline=json!({"imdb_id":"tt0434665","category":"series",
            "season":15,"episode":4,"anime":{"anidb_id":"2369",
                "episode":320,"asid":"2369:320"}});
        // An independent title alias could point at another AniDB work.
        // The canonical IMDb-selected crosswalk remains the trusted result.
        let mut candidate=baseline.clone();
        candidate["anidb_conflict"]=json!({
            "independent":"17765:4","imdb_crosswalk":"2369:320"});
        assert_eq!(public_match_response(Some(candidate)),
            public_match_response(Some(baseline)));
    }

    #[test]
    fn independent_anidb_range_conflict_preserves_existing_verified_imdb_range(){
        let winner=json!({"imdb_id":"tt1234567","category":"series",
            "season":1,"episode":1,"season_end":1,"episode_end":2,
            "anidb_independent_verified":true,
            "anidb_range":{"anidb_id":"2369","start":1,"end":2},
            "episode_range":{"complete":true,"segments":[{
                "anidb_id":"17765","start_anime_episode":1,
                "end_anime_episode":2
            }]}});
        let response=public_match_response(Some(winner));
        assert_eq!(response["sid_start"],"tt1234567:1:1");
        assert_eq!(response["sid_end"],"tt1234567:1:2");
        assert_eq!(response["asid_start"],"17765:1");
        assert_eq!(response["asid_end"],"17765:2");
        // Incomplete legacy crosswalks still publish no ASID, even when
        // independent evidence suggests a range is complete.
        let incomplete=json!({"imdb_id":"tt1234567","category":"series",
            "season":1,"episode":1,"season_end":null,"episode_end":null,
            "anidb_independent_verified":true,
            "anidb_range":{"anidb_id":"2369","start":1,"end":2},
            "episode_range":{"complete":false,"segments":[{
                "anidb_id":"17765","start_anime_episode":1,
                "end_anime_episode":1
            }]}});
        let response=public_match_response(Some(incomplete));
        assert!(response["asid_start"].is_null());
        assert!(response["asid_end"].is_null());
    }

    #[test]
    fn fractional_anime_episode_does_not_publish_base_episode_sid_or_asid(){
        let fractional=json!({"imdb_id":"tt1234567","category":"series",
            "season":1,"episode":12,"anime":{"asid":"123:12"},
            "parsed":{"half_episode":true,"episodes":[12]}});
        assert_eq!(public_match_response(Some(fractional)),json!({
            "sid_start":"tt1234567","sid_end":"tt1234567",
            "asid_start":null,"asid_end":null
        }));
        let ordinary=json!({"imdb_id":"tt1234567","category":"series",
            "season":1,"episode":12,"anime":{"asid":"123:12"},
            "parsed":{"episodes":[12]}});
        let normal=public_match_response(Some(ordinary));
        assert_eq!(normal["sid_start"],"tt1234567:1:12");
        assert_eq!(normal["asid_start"],"123:12");
    }

    #[test]
    fn bare_jojo_episode_uses_exact_anime_alias_before_mapping(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-jojo-bare-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt1234567',
                'Jojo''s bizarre adventure stone ocean',
                'jojosbizarreadventurestoneocean',2021,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt0133093','District 9','district9',2009,
                'movie','',1,0,'movie');
            INSERT INTO seasons VALUES('tt1234567',1);
            INSERT INTO season_episodes VALUES('tt1234567',1,39,39);
            INSERT INTO anime_series_map VALUES('tt1234567','123','98765',1);
            INSERT INTO anime_ids VALUES('tt1234567','anidb','123');
            INSERT INTO episode_map VALUES('123','98765',1,1,1,39,0,'');
            INSERT INTO anidb_titles VALUES('123',
                'Jojo''s bizarre adventure stone ocean',
                'jojosbizarreadventurestoneocean','main','en');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for name in [
            "Jojo's bizarre adventure stone ocean - 27",
            "Jojo's bizarre adventure stone ocean 27",
            "Jojo's bizarre adventure stone ocean 27.mkv",
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:name.into(),year:None,series:None}).unwrap();
            assert_eq!(result.parsed["episodes"],json!([27]),
                "{name}: {:?}",result.parsed);
            let selected=result.selected.expect("metadata-verified JoJo episode");
            let public=public_match_response(Some(selected));
            assert_eq!(public["sid_start"],json!("tt1234567:1:27"),
                "{name}: {public}");
            assert_eq!(public["asid_start"],json!("123:27"),
                "{name}: {public}");
        }
        let ordinary=diagnose(&path,&MatchRequest{
            title:"District 9".into(),year:None,series:None}).unwrap();
        assert!(ordinary.parsed.get("anime_title_verified").is_none());
        let ordinary_public=public_match_response(ordinary.selected);
        assert!(ordinary_public["sid_start"].is_null(),
            "a bare movie number without anime verification must not publish: {ordinary_public}");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn stone_ocean_named_part_uses_canonical_jojo_imdb_season_five(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-jojo-stone-ocean-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
            category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO titles VALUES
                ('tt2359704','JoJo''s Bizarre Adventure',
                 'jojosbizarreadventure',2012,'series','',1,0,'tvSeries'),
                ('tt9990001','Stone Ocean: The Movie',
                 'stoneoceanthemovie',2022,'movie','',1,0,'movie');
            INSERT INTO seasons VALUES ('tt2359704',5);
            INSERT INTO season_episodes VALUES ('tt2359704',1,26,26),
                ('tt2359704',5,38,38);
            INSERT INTO anime_series_map VALUES
                ('tt2359704','16187','262954',0),
                ('tt2359704','17286','262954',0),
                ('tt2359704','17664','262954',0);
            INSERT INTO anime_ids VALUES('tt2359704','anidb','16187');
            INSERT INTO anidb_titles VALUES
                ('16187','JoJo''s Bizarre Adventure Stone Ocean',
                 'jojosbizarreadventurestoneocean','main','en'),
                ('17286','JoJo''s Bizarre Adventure Stone Ocean Part 2',
                 'jojosbizarreadventurestoneoceanpart2','main','en'),
                ('17664','JoJo''s Bizarre Adventure Stone Ocean Part 3',
                 'jojosbizarreadventurestoneoceanpart3','main','en');
            -- Anime-Lists maps Stone Ocean's three AniDB cours into the
            -- same TVDB/IMDb season five, offset by 0, 12, and 24.
            -- Unbounded defaults are season evidence, not independent
            -- proof that AniDB work 16187 owns episode 27.
            INSERT INTO episode_map VALUES
                ('16187','262954',1,5,1,0,0,''),
                ('17286','262954',1,5,1,0,12,''),
                ('17664','262954',1,5,1,0,24,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        assert_eq!(anidb_matcher::named_part_season(
            &db,"JoJo's Bizarre Adventure Stone Ocean").unwrap(),
            Some(anidb_matcher::NamedPart{anidb_id:"16187".into(),season:5}));
        assert_eq!(anidb_matcher::match_absolute(
            &db,"JoJo's Bizarre Adventure Stone Ocean",27).unwrap(),None);
        drop(db);
        for raw in [
            "JoJo's Bizarre Adventure Stone Ocean - 27",
            "JoJo's Bizarre Adventure Stone Ocean 27",
            "JoJo's Bizarre Adventure Stone Ocean 27.mkv",
            "JoJo's Bizarre Adventure Stone Ocean S01E27.mkv",
        ]{
            let d=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:None}).unwrap();
            let winner=d.selected.expect("named anime part must resolve");
            assert_eq!(winner["imdb_id"],"tt2359704","{raw}: {winner}");
            assert_eq!(winner["season"],5,"{raw}: {winner}");
            assert_eq!(winner["episode"],27,"{raw}: {winner}");
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":"tt2359704:5:27","sid_end":"tt2359704:5:27",
                "asid_start":"17664:3","asid_end":"17664:3"
            }),"{raw}");
        }
        // Earlier episodes remain in the first AniDB work; part-specific
        // releases use local episode numbering while preserving S05.
        let early=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean - 03".into(),
            year:None,series:None}).unwrap();
        let winner=early.selected.expect("first Stone Ocean part");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt2359704:5:3","sid_end":"tt2359704:5:3",
            "asid_start":"16187:3","asid_end":"16187:3"}));
        // An explicit Part 3 title uses its own AniDB-local episode three,
        // corresponding to the parent show's IMDb/TVDB S05E27.
        let third=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean Part 3 S01E03".into(),
            year:None,series:None}).unwrap();
        let winner=third.selected.expect("Stone Ocean third cour");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt2359704:5:27","sid_end":"tt2359704:5:27",
            "asid_start":"17664:3","asid_end":"17664:3"}));
        let title_only=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean".into(),
            year:None,series:None}).unwrap();
        let winner=title_only.selected.expect("named part title");
        assert_eq!(winner["imdb_id"],"tt2359704","{winner}");
        assert_eq!(winner["season"],5,"{winner}");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn one_piece_batch_with_seven_anidb_aliases_uses_unique_complete_crosswalk(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-one-piece-batch-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt0388629','One Piece','onepiece',1999,
                'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0388629',23);
            INSERT INTO season_episodes VALUES('tt0388629',21,194,194);
            INSERT INTO season_episodes VALUES('tt0388629',22,70,70);
            INSERT INTO anime_series_map VALUES('tt0388629','69','81797',1);
            INSERT INTO anime_ids VALUES('tt0388629','anidb','69');
            INSERT INTO episode_map VALUES('69','81797',1,-1,1,0,0,'');
            INSERT INTO episode_map VALUES('69','81797',1,21,892,1085,-891,'');
            INSERT INTO episode_map VALUES('69','81797',1,22,1086,1155,-1085,'');").unwrap();
        // Real-world data contains several One Piece films/OVAs sharing the
        // exact normalized alias; only the main series has a crosswalk
        // that spans absolute episodes 892..1089.
        for (aid,imdb) in [
            ("1253","tt0997084"),("2036","tt1010435"),("2644","tt1018764"),
            ("2736","tt1012788"),("4851","tt1037116"),("893","tt0832449"),
        ]{
            db.execute("INSERT INTO anime_series_map VALUES(?1,?2,'',0)",
                rusqlite::params![imdb,aid]).unwrap();
            db.execute("INSERT INTO anime_ids VALUES(?1,'anidb',?2)",
                rusqlite::params![imdb,aid]).unwrap();
            db.execute("INSERT INTO anidb_titles VALUES(?1,'One Piece','onepiece','main','en')",
                [aid]).unwrap();
        }
        db.execute("INSERT INTO anidb_titles VALUES('69','One Piece','onepiece','main','en')",[])
            .unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let title="[Erai-raws] One Piece - 0892 ~ 1089 [720p][Multiple Subtitle]";
        let match_title=||diagnose(&path,&MatchRequest{
            title:title.into(),year:None,series:None}).unwrap();
        let result=match_title();
        assert_eq!(result.parsed["anime_title_verified"],true);
        assert_eq!(result.parsed["episode_start"],892);
        assert_eq!(result.parsed["episode_end"],1089);
        let selected=result.selected.expect("verified full-range One Piece match");
        assert_eq!(selected["imdb_id"],"tt0388629");
        assert_eq!(selected["episode_range"]["complete"],true);
        assert_eq!(selected["episode_range"]["unmapped_count"],0);
        assert_eq!(selected["episode_range"]["segments"].as_array().unwrap().len(),2);
        assert_eq!(selected["season"],21);
        assert_eq!(selected["episode"],1);
        assert_eq!(selected["season_end"],22);
        assert_eq!(selected["episode_end"],4);
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt0388629:21:1","sid_end":"tt0388629:22:4",
            "asid_start":"69:892","asid_end":"69:1089"
        }));
        // If another AniDB work also maps the complete span, the same
        // exact title becomes ambiguous and no coordinates are promoted.
        // Rebuild Tantivy on a separate immutable metadata snapshot; changing
        // episode_map in place after warmup would correctly invalidate
        // MatchLab's SQLite/Tantivy generation checksum.
        let ambiguous_path=std::env::temp_dir().join(format!(
            "matchlab-one-piece-ambiguous-{}-{stamp}.sqlite3",std::process::id()));
        std::fs::copy(&path,&ambiguous_path).unwrap();
        let db=Connection::open(&ambiguous_path).unwrap();
        db.execute_batch("UPDATE anime_series_map SET tvdb_id='81797' WHERE anidb_id='1253';
            INSERT INTO episode_map VALUES('1253','81797',1,-1,1,0,0,'');
            INSERT INTO episode_map VALUES('1253','81797',1,21,892,1085,-891,'');
            INSERT INTO episode_map VALUES('1253','81797',1,22,1086,1155,-1085,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        assert!(anidb_matcher::match_absolute_range(&db,"One Piece",892,1089)
            .unwrap().is_none());
        drop(db);
        let ambiguous=diagnose(&ambiguous_path,&MatchRequest{
            title:title.into(),year:None,series:None}).unwrap();
        assert!(ambiguous.parsed.get("anime_title_verified").is_none());
        assert!(ambiguous.parsed.get("episode_start").is_none());
        for database in [&path,&ambiguous_path] {
            std::fs::remove_file(database).unwrap();
            std::fs::remove_dir_all(format!("{}.tantivy",database.display())).unwrap();
        }
    }

    #[test]
    fn anime_batches_verify_unmarked_ranges_and_keep_incomplete_endpoints_null(){
        let suffix=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-anime-range-{}-{suffix}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt1234567','Spy x Family','spyxfamily',2022,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt7654321','Example Show','exampleshow',2020,
                'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt1234567',1);
            INSERT INTO seasons VALUES('tt7654321',1);
            INSERT INTO season_episodes VALUES('tt1234567',1,7,7);
            INSERT INTO season_episodes VALUES('tt7654321',1,12,12);
            INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
            INSERT INTO anime_ids VALUES('tt1234567','anidb','123');
            INSERT INTO anidb_titles VALUES('123','Spy x Family','spyxfamily','main','en');
            INSERT INTO episode_map VALUES('123','456',1,1,1,7,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);

        for raw in [
            "[SubsPlease] Spy x Family (01-07) (1080p) [Batch]",
            "Spy x Family 1-7",
            "Spy x Family 01~07",
        ] {
            let d=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:None}).unwrap();
            assert_eq!(d.parsed["anime_title_verified"],true,"{raw}");
            let winner=d.selected.expect("verified anime title must match");
            assert_eq!(winner["imdb_id"],"tt1234567","{raw}");
            assert_eq!(winner["anime_episode_start"],1,"{raw}");
            assert_eq!(winner["anime_episode_end"],7,"{raw}");
            assert_eq!(winner["episode_range"]["complete"],true,"{raw}");
            let response=public_match_response(Some(winner));
            assert_eq!(response["sid_start"],"tt1234567:1:1","{raw}");
            assert_eq!(response["sid_end"],"tt1234567:1:7","{raw}");
            assert_eq!(response["asid_start"],"123:1","{raw}");
            assert_eq!(response["asid_end"],"123:7","{raw}");
        }

        let incomplete=diagnose(&path,&MatchRequest{
            title:"Spy x Family 01-09 Batch".into(),year:None,series:None}).unwrap();
        let winner=incomplete.selected.unwrap();
        assert_eq!(winner["episode_range"]["complete"],false);
        assert_eq!(winner["episode_range"]["unmapped_count"],2);
        let response=public_match_response(Some(winner));
        assert_eq!(response["sid_start"],"tt1234567:1:1");
        assert!(response["sid_end"].is_null());
        assert!(response["asid_start"].is_null());
        assert!(response["asid_end"].is_null());

        let generic=diagnose(&path,&MatchRequest{
            title:"Example Show S01E01-E07".into(),year:None,series:None}).unwrap();
        let public=public_match_response(generic.selected);
        assert_eq!(public["sid_start"],"tt7654321:1:1");
        assert_eq!(public["sid_end"],"tt7654321:1:7");
        assert!(public["asid_start"].is_null());

        let season_anime=diagnose(&path,&MatchRequest{
            title:"Spy x Family S01E01-E07".into(),year:None,series:None}).unwrap();
        let public=public_match_response(season_anime.selected);
        assert_eq!(public["sid_start"],"tt1234567:1:1");
        assert_eq!(public["sid_end"],"tt1234567:1:7");
        assert_eq!(public["asid_start"],"123:1");
        assert_eq!(public["asid_end"],"123:7");

        let forced_movie=diagnose(&path,&MatchRequest{
            title:"Spy x Family 01-07".into(),year:None,series:Some(false)}).unwrap();
        assert!(forced_movie.parsed.get("anime_title_verified").is_none());
        assert!(forced_movie.parsed.get("episode_start").is_none());

        let unrelated=diagnose(&path,&MatchRequest{
            title:"Example Show 01-07".into(),year:None,series:None}).unwrap();
        assert!(unrelated.parsed.get("anime_title_verified").is_none());
        assert!(unrelated.parsed.get("episode_start").is_none());
        assert_eq!(unrelated.parsed["episodes"],json!([]));
        if let Some(selected)=unrelated.selected {
            assert!(selected["episode"].is_null(),
                "unverified unmarked range must not invent an ordinary TV episode");
        }

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn production_year_requires_exact_match_or_independent_signal(){
        // Missing years and mismatches -- including +/-1 year -- require
        // independent evidence. An episode number alone is insufficient.
        for (year,imdb) in [
            (None,2024),(Some(2023),2024),(Some(2025),2024),
            (Some(2010),2024),(Some(2024),0)
        ]{
            assert!(!year_signal_eligible(year,imdb,false,false,false));
            assert!(year_signal_eligible(year,imdb,true,false,false));
            assert!(year_signal_eligible(year,imdb,false,true,false));
            assert!(year_signal_eligible(year,imdb,false,false,true));
        }
        assert!(year_signal_eligible(Some(2024),2024,false,false,false));
        assert!(!year_signal_eligible(Some(2023),2024,false,false,false),
            "a one-year offset must not be a special exception");
        assert!(!year_signal_eligible(Some(2025),2024,false,false,false));
        let object=|value:Value|value.as_object().unwrap().clone();
        assert!(parsed_quality_signal(&object(json!({"quality":"WEB-DL"}))));
        assert!(parsed_quality_signal(&object(json!({"quality":"WEB-DL"}))));
        assert!(parsed_quality_signal(&object(json!({"quality":"BluRay"}))));
        assert!(!parsed_quality_signal(&object(json!({"resolution":"1080p"}))));
        assert!(!parsed_quality_signal(&object(json!({"resolution":"2160p"}))));
        assert!(!parsed_quality_signal(&object(json!({"resolution":"1080p","codec":"x265"}))));
        assert!(!parsed_quality_signal(&object(json!({"codec":"x265"}))));
        assert!(!parsed_quality_signal(&object(json!({"quality":""}))));
        assert!(!parsed_quality_signal(&object(json!({"quality":"   "}))));
        assert!(!parsed_quality_signal(&object(json!({"quality":null}))));
        assert!(!parsed_quality_signal(&object(json!({"quality":["WEB-DL"]}))));
    }


    #[tokio::test]
    async fn dashboard_finds_bare_fight_club_but_public_api_enforces_year_policy(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-dashboard-year-policy-{}-{stamp}.sqlite3",std::process::id()));
        let history=std::env::temp_dir().join(format!(
            "matchlab-dashboard-year-policy-history-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
                imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,
                updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
            INSERT INTO titles VALUES('tt0137523','Fight Club','fightclub',
                1999,'movie','US',1,0,'movie');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        dashboard_history::initialize(&history).unwrap();
        let state=AppState{
            db:Arc::new(path.clone()),history:Arc::new(history.clone()),
            match_slots:Arc::new(Semaphore::new(2)),
            index_phase:Arc::new(Mutex::new(IndexPhase::Ready)),
        };
        // Dashboard diagnosis keeps all candidate rankings, even without
        // release metadata, without weakening the public /match contract.
        for title in ["Fight Club","Fight.Club.1080p.x265.mkv",
            "Fight.Club.2000.mkv"]{
            let request=||MatchRequest{
                title:title.into(),year:None,series:None};
            let (status,Json(debug))=match_response(
                state.clone(),request(),true,false).await;
            assert_eq!(status,StatusCode::OK,"{title}");
            assert_eq!(debug["selected"]["imdb_id"],"tt0137523","{title}");
            let (status,Json(api))=match_response(
                state.clone(),request(),false,false).await;
            assert_eq!(status,StatusCode::OK,"{title}");
            assert!(api["sid_start"].is_null(),
                "public API must enforce production-year signal for {title}");
            // Regression: /debug/match must apply exactly the same
            // production eligibility filter as /match. The separate
            // dashboard route above remains intentionally permissive.
            let (debug_status,Json(integration_debug))=debug_request(
                State(state.clone()),Json(request())).await;
            assert_eq!(debug_status,StatusCode::OK,"{title}");
            assert!(integration_debug["selected"].is_null(),
                "debug published a candidate excluded by /match for {title}: {integration_debug}");
        }
        let (status,Json(api))=match_response(state.clone(),
            MatchRequest{title:"Fight.Club.1999.mkv".into(),
                year:None,series:None},false,false).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(api["sid_start"],"tt0137523");
        let (status,Json(debug))=debug_request(State(state),Json(MatchRequest{
            title:"Fight.Club.1999.mkv".into(),year:None,series:None,
        })).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(debug["selected"]["imdb_id"],"tt0137523");
        assert_eq!(api["sid_start"],debug["selected"]["imdb_id"]);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
        std::fs::remove_file(&history).unwrap();
    }

    #[test]
    fn indexing_and_matching_readiness_states_are_independent(){
        let phase=Arc::new(Mutex::new(IndexPhase::Indexing));
        assert_eq!(index_phase(&phase),IndexPhase::Indexing);
        set_index_phase(&phase,IndexPhase::Ready);
        assert_eq!(index_phase(&phase),IndexPhase::Ready);
        set_index_phase(&phase,IndexPhase::Failed("bad metadata".into()));
        assert!(matches!(index_phase(&phase),IndexPhase::Failed(_)));
    }

    #[tokio::test]
    async fn server_health_and_json_errors_work_while_metadata_is_indexing(){
        let state=AppState{
            db:Arc::new(PathBuf::from("/nonexistent/matchlab-metadata-test.sqlite3")),
            history:Arc::new(PathBuf::from("/nonexistent/matchlab-history-test.sqlite3")),
            match_slots:Arc::new(Semaphore::new(2)),
            index_phase:Arc::new(Mutex::new(IndexPhase::Indexing)),
        };
        let (status,Json(live))=liveness().await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(live["status"],"alive");
        let (status,Json(ready))=readiness(State(state.clone())).await;
        assert_eq!(status,StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(ready["status"],"indexing");
        let (status,Json(matched))=match_response(state,MatchRequest{
            title:"One Piece S21E03".into(),year:None,series:None
        },false,false).await;
        assert_eq!(status,StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(matched["status"],"indexing");
        assert!(matched["error"].as_str().unwrap().contains("indexing"));
    }

    #[test]
    fn match_single_anime_season_episode_publishes_sid_and_asid(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-onepiece-season-asid-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES
                ('tt0388629','One Piece','onepiece',1999,
                    'series','',1,0,'tvSeries'),
                ('tt2379308','Psycho-Pass','psychopass',2012,
                    'series','',1,0,'tvSeries'),
                ('tt2222222','Example Show','exampleshow',2020,
                    'series','',1,0,'tvSeries'),
                ('tt4644488','Dragon Ball Super','dragonballsuper',2015,
                    'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0388629',23);
            INSERT INTO seasons VALUES('tt2379308',1);
            INSERT INTO seasons VALUES('tt2222222',1);
            INSERT INTO seasons VALUES('tt4644488',1);
            INSERT INTO season_episodes VALUES('tt4644488',1,131,131);
            INSERT INTO season_episodes VALUES('tt0388629',21,194,194);
            INSERT INTO season_episodes VALUES('tt2379308',1,22,22);
            INSERT INTO season_episodes VALUES('tt2222222',1,12,12);
            INSERT INTO anime_series_map VALUES('tt0388629','69','81797',1);
            INSERT INTO anime_ids VALUES('tt0388629','anidb','69');
            INSERT INTO anime_ids VALUES('tt2379308','anidb','777');
            INSERT INTO anime_ids VALUES('tt4644488','anidb','11210');
            INSERT INTO anime_series_map VALUES('tt4644488','11210','295068',0);
            INSERT INTO anidb_titles VALUES('11210','Dragon Ball Super',
                'dragonballsuper','main','en');
            INSERT INTO anidb_titles VALUES('69','One Piece',
                'onepiece','main','en');
            INSERT INTO anidb_titles VALUES('777','Psycho-Pass',
                'psychopass','main','en');
            INSERT INTO episode_map VALUES('69','81797',1,-1,1,0,0,'');
            INSERT INTO episode_map VALUES('69','81797',1,21,892,1085,-891,'');
            INSERT INTO episode_map VALUES('69','81797',1,22,1086,1155,-1085,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        let result=diagnose(&path,&MatchRequest{
            title:"One Piece S21E03".into(),year:None,series:None}).unwrap();
        assert_eq!(result.parsed["title"],"One Piece");
        assert_eq!(result.parsed["seasons"],json!([21]));
        assert_eq!(result.parsed["episodes"],json!([3]));
        let winner=result.selected.expect("One Piece must be matched");
        assert_eq!(winner["imdb_id"],"tt0388629");
        assert_eq!(winner["season"],21);
        assert_eq!(winner["episode"],3);
        assert_eq!(winner["anime"]["asid"],"69:894");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt0388629:21:3","sid_end":"tt0388629:21:3",
            "asid_start":"69:894","asid_end":"69:894"}));
        // Real-world regression: the parser and IMDb match are correct,
        // but the AniDB episode crosswalk is incomplete. Explicit S01E03
        // must still give a valid IMDb SID and never invent an ASID.
        let dragon=diagnose(&path,&MatchRequest{
            title:"Dragon.Ball.Super.S01E03.DUBBED".into(),
            year:None,series:None}).unwrap();
        assert_eq!(dragon.parsed["title"],"Dragon Ball Super");
        assert_eq!(dragon.parsed["seasons"],json!([1]));
        assert_eq!(dragon.parsed["episodes"],json!([3]));
        assert_eq!(dragon.parsed["dubbed"],true);
        let winner=dragon.selected.expect("Dragon Ball Super must match");
        assert_eq!(winner["imdb_id"],"tt4644488");
        assert_eq!(winner["season"],1);
        assert_eq!(winner["episode"],3);
        assert!(winner["anime"].is_null());
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt4644488:1:3","sid_end":"tt4644488:1:3",
            "asid_start":null,"asid_end":null
        }));
        let unavailable=diagnose(&path,&MatchRequest{
            title:"Dragon.Ball.Super.S01E999.DUBBED".into(),
            year:None,series:None}).unwrap();
        // With only one title/season identity, a low IMDb episode maximum
        // must not discard that identity. The separate anime crosswalk
        // safety guard still withholds an unverified S01E999 and ASID.
        let unmatched=unavailable.selected.expect("retain unique anime identity");
        assert_eq!(unmatched["imdb_id"],"tt4644488");
        assert!(unmatched["episode"].is_null(),
            "do not publish an unverified anime episode");
        assert_eq!(public_match_response(Some(unmatched)),json!({
            "sid_start":"tt4644488","sid_end":"tt4644488",
            "asid_start":null,"asid_end":null
        }));
        // No Sxx token must not overwrite an absolute episode that can
        // be verified to belong to a later season of One Piece.
        let absolute=diagnose(&path,&MatchRequest{
            title:"One Piece - 894".into(),year:None,series:None}).unwrap();
        let abs_response=public_match_response(absolute.selected);
        assert_eq!(abs_response["sid_start"],"tt0388629:21:3");
        assert_eq!(abs_response["asid_start"],"69:894");
        // A local anime episode without a mapped AniDB episode retains a
        // tentative S01 episode SID; never invent its ASID.
        let local=diagnose(&path,&MatchRequest{
            title:"[Commie] Psycho-Pass - 04 [F79C11D9].mkv".into(),
            year:None,series:None}).unwrap();
        assert_eq!(local.parsed["episodes"],json!([4]));
        assert_eq!(local.parsed["seasons"],json!([]));
        let local_api=public_match_response(local.selected);
        assert_eq!(local_api["sid_start"],"tt2379308:1:4");
        assert_eq!(local_api["sid_end"],"tt2379308:1:4");
        assert!(local_api["asid_start"].is_null());
        // A 900+ absolute episode that does not have a crosswalk must never
        // be relabeled as a small local first-season episode.
        let mut extreme=json!({"imdb_id":"tt2379308","category":"series",
            "season":null,"episode":894,"anime":null});
        assert!(!anime_local_episode_season_one(
            &db,"tt2379308",894,true,false).unwrap());
        enrich_selected_anime_episode(&db,&mut extreme).unwrap();
        assert!(extreme["anime"].is_null());
        // The final enrichment also handles coordinates assigned after
        // candidate matching (such as dates) without relying on titles.
        let mut late=json!({"imdb_id":"tt0388629","category":"series",
            "season":21,"episode":3,"anime":null,
            "match_source":"cinemeta_airdate","anime_ids":{"kitsu":"9"}});
        enrich_selected_anime_episode(&db,&mut late).unwrap();
        assert_eq!(late["anime"]["asid"],"69:894");
        assert_eq!(late["match_source"],"cinemeta_airdate");
        assert_eq!(late["anime_ids"]["kitsu"],"9");
        assert_eq!(late["anime_ids"]["anidb"],"69");
        let mut ordinary=json!({"imdb_id":"tt2222222","category":"series",
            "season":1,"episode":3,"anime":null});
        enrich_selected_anime_episode(&db,&mut ordinary).unwrap();
        assert!(ordinary["anime"].is_null());
        let mut range=json!({"imdb_id":"tt0388629","category":"series",
            "season":21,"episode":3,"anime":null,
            "episode_range":{"complete":false}});
        enrich_selected_anime_episode(&db,&mut range).unwrap();
        assert!(range["anime"].is_null());
        drop(db);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn exact_anime_sequel_title_maps_unmarked_local_episode_to_its_season(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-amanchu-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt5871330','Amanchu!','amanchu',2016,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt5871330','Amanchu! Advance','amanchuadvance',2018,
                'series','',0,1,'tvSeries');
            INSERT INTO seasons VALUES('tt5871330',2);
            INSERT INTO season_episodes VALUES('tt5871330',1,12,12);
            INSERT INTO season_episodes VALUES('tt5871330',2,12,12);
            INSERT INTO anime_series_map VALUES('tt5871330','201','304900',0);
            INSERT INTO anime_series_map VALUES('tt5871330','202','304900',0);
            INSERT INTO anime_ids VALUES('tt5871330','anidb','201');
            INSERT INTO anidb_titles VALUES('201','Amanchu!','amanchu','main','en');
            INSERT INTO anidb_titles VALUES('202','Amanchu! Advance',
                'amanchuadvance','main','en');
            INSERT INTO episode_map VALUES('201','304900',1,1,1,12,0,'');
            INSERT INTO episode_map VALUES('202','304900',1,2,1,12,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let find=|title:&str|diagnose(&path,&MatchRequest{
            title:title.into(),year:None,series:None}).unwrap();
        let advance=find("[Erai-raws] Amanchu! Advance - 01 [1080p][Multiple Subtitle].mkv");
        assert_eq!(advance.parsed["title"],"Amanchu! Advance");
        assert_eq!(advance.parsed["episodes"],json!([1]));
        assert_eq!(advance.parsed["seasons"],json!([]));
        let mapped=advance.selected.expect("must match sequel");
        assert_eq!(mapped["imdb_id"],"tt5871330");
        assert_eq!(mapped["season"],2);
        assert_eq!(mapped["episode"],1);
        assert_eq!(mapped["anime"]["asid"],"202:1");
        assert_eq!(public_match_response(Some(mapped)),json!({
            "sid_start":"tt5871330:2:1","sid_end":"tt5871330:2:1",
            "asid_start":"202:1","asid_end":"202:1"}));
        // An exact base alias with its own verified episode numbering
        // continues to map to season 1.
        let base=find("[Erai-raws] Amanchu! - 01 [1080p].mkv")
            .selected.unwrap();
        assert_eq!(base["season"],1);
        assert_eq!(base["episode"],1);
        assert_eq!(base["anime"]["asid"],"201:1");
        // Explicit SxxEyy still has priority over title-only inference.
        let explicit=find("Amanchu! Advance S01E01.mkv").selected.unwrap();
        assert_eq!(explicit["season"],2);
        assert_eq!(explicit["episode"],1);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn anime_part_subfolder_overrides_leaf_only_for_verified_anime_identity() {
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-anime-path-title-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt0300001','Sword Art Online','swordartonline',2012,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt0300002','Ordinary Show','ordinaryshow',2010,
                'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0300001',3);
            INSERT INTO seasons VALUES('tt0300002',3);
            INSERT INTO season_episodes VALUES('tt0300001',1,12,12);
            INSERT INTO season_episodes VALUES('tt0300002',1,12,12);
            INSERT INTO anime_series_map VALUES('tt0300001','100','456',0);
            INSERT INTO anime_series_map VALUES('tt0300001','101','456',0);
            INSERT INTO anime_ids VALUES('tt0300001','anidb','100');
            INSERT INTO anidb_titles VALUES('100','Sword Art Online','swordartonline','main','en');
            INSERT INTO anidb_titles VALUES('101','Sword Art Online Alicization',
                'swordartonlinealicization','main','en');
            INSERT INTO episode_map VALUES('100','456',1,1,1,12,0,'');
            INSERT INTO episode_map VALUES('101','456',1,3,1,12,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let find=|raw:&str|{
            diagnose(&path,&MatchRequest{title:raw.into(),year:None,series:None})
                .unwrap_or_else(|e|panic!("{raw}: {e}"))
        };
        let standard=find("Sword.Art.Online.S01E01.mkv");
        let base=standard.selected.unwrap();
        assert_eq!(base["imdb_id"],"tt0300001");
        assert_eq!(base["season"],1);
        assert_eq!(base["episode"],1);
        assert_eq!(base["anime"]["asid"],"100:1");
        let nested=find("Sword Art Online/Sword Art Online Alicization/Sword.Art.Online.S01E01.mkv");
        assert_eq!(nested.parsed["path_title_candidates"],
            json!(["Sword Art Online","Sword Art Online Alicization"]));
        let part=nested.selected.unwrap();
        assert_eq!(part["imdb_id"],"tt0300001");
        assert_eq!(part["season"],3);
        assert_eq!(part["episode"],1);
        assert_eq!(part["anime"]["asid"],"101:1");
        assert_eq!(public_match_response(Some(part))["sid_start"],"tt0300001:3:1");

        // The word "Alicization" is not blindly inherited for non-anime.
        let ordinary=find("Anime Library/Ordinary.Show.S01E01.mkv");
        let ordinary=ordinary.selected.unwrap();
        assert_eq!(ordinary["imdb_id"],"tt0300002");
        assert_eq!(ordinary["season"],1);
        assert_eq!(ordinary["episode"],1);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn matching_parent_year_selects_2005_doctor_who_not_1963() {
        // The same IMDb title is shared by two series. The folder's
        // production year disambiguates the actual episode filename.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-parent-year-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt0056751','Doctor Who','doctorwho',1963,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt0436992','Doctor Who','doctorwho',2005,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt9999900','Doctor Who','doctorwho',2006,
                'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0056751',26);
            INSERT INTO seasons VALUES('tt0436992',14);
            INSERT INTO seasons VALUES('tt9999900',14);
            INSERT INTO season_episodes VALUES('tt0056751',1,42,42);
            INSERT INTO season_episodes VALUES('tt0436992',1,13,13);").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);

        let folder="Doctor.Who.2005.S01.REPACK.1080p.AMZN.WEB-DL.DDP2.0.H.264-KHEZU";
        let leaf="Doctor.Who.S01E06.Dalek.1080p.AMZN.WEB-DL.DDP2.0.H.264-KHEZU.mkv";
        let full=format!("{folder}/{leaf}");
        let diagnose_title=|title:String,year:Option<i32>|{
            diagnose(&path,&MatchRequest{title,year,series:None}).unwrap()
        };
        let selected=diagnose_title(full.clone(),None).selected.unwrap();
        assert_eq!(selected["imdb_id"],"tt0436992");
        assert_eq!(selected["season"],1);
        assert_eq!(selected["episode"],6);
        let response=public_match_response(Some(selected));
        assert_eq!(response["sid_start"],"tt0436992:1:6");
        assert_eq!(response["sid_end"],"tt0436992:1:6");

        let forced=diagnose_title(full,Some(1963)).selected.unwrap();
        assert_eq!(forced["imdb_id"],"tt0056751",
            "explicit API year must override inherited folder year");
        let archived=diagnose_title(
            format!("Archive.1963/{leaf}"),None).selected.unwrap();
        assert_eq!(archived["imdb_id"],"tt0056751",
            "parent year is inherited even when the folder is an archive");
        let child_override=diagnose_title(
            format!("{folder}/Doctor.Who.2006.S01E06.Dalek.mkv"),None)
            .selected.unwrap();
        assert_eq!(child_override["imdb_id"],"tt9999900",
            "the leaf's 2006 production year must override its 2005 parent");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn files_with_a_shared_torrent_title_reuse_candidates_but_not_episode_coordinates(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-shared-torrent-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,
                updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-shared-torrent',0);
            INSERT INTO titles VALUES('tt0056751','Doctor Who','doctorwho',1963,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt0436992','Doctor Who','doctorwho',2005,
                'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0056751',26);
            INSERT INTO seasons VALUES('tt0436992',14);
            INSERT INTO season_episodes VALUES('tt0056751',1,42,42);
            INSERT INTO season_episodes VALUES('tt0436992',1,13,13);").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for episode in [1,2,6] {
            let raw=format!("Doctor.Who.2005.S01.REPACK/Doctor.Who.2005.S01E{episode:02}.mkv");
            let result=diagnose(&path,&MatchRequest{
                title:raw.clone(),year:None,series:None}).unwrap();
            let winner=result.selected.expect("series must match");
            assert_eq!(winner["imdb_id"],"tt0436992","{raw}");
            let response=public_match_response(Some(winner));
            assert_eq!(response["sid_start"],
                format!("tt0436992:1:{episode}"),"{raw}");
            assert_eq!(response["sid_end"],
                format!("tt0436992:1:{episode}"),"{raw}");
        }
        // A request-specific override must not be polluted by the title cache.
        let override_result=diagnose(&path,&MatchRequest{
            title:"Doctor.Who.2005.S01.REPACK/Doctor.Who.2005.S01E01.mkv".into(),
            year:Some(1963),series:None}).unwrap().selected.unwrap();
        assert_eq!(override_result["imdb_id"],"tt0056751");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

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
        let snapshot=Connection::open(&path).unwrap();
        tantivy_search::warmup(&snapshot).unwrap();
        drop(snapshot);
        let result=diagnose(&path,&MatchRequest{
            title:"Supernatural.S01-s12.hdr".into(),year:None,series:None
        }).unwrap();
        let winner=result.selected.unwrap();
        assert_eq!(winner["imdb_id"],"tt0460681");
        assert_eq!(winner["match_source"],"imdb");
        assert_eq!(winner["category"],"series");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }
    #[test]
    fn imdb_episode_bounds_only_filter_when_more_than_two_season_candidates_remain() {
        // IMDb can list fewer episodes than aired: Cops S36E41 must match
        // when the title+season search has 1 or 2 plausible identities.
        // Three identities are disambiguated by the episode maximum.
        // The final case starts with three title IDs but only two pass the
        // season filter: the episode filter must remain inactive.
        for (tag,records,expected_ids) in [
            ("single",vec![(36,22)],vec!["tt0000001"]),
            ("two",vec![(36,22),(36,30)],
                vec!["tt0000001","tt0000002"]),
            ("three",vec![(36,22),(36,30),(36,55)],
                vec!["tt0000003"]),
            ("season_filter_first",vec![(36,22),(36,30),(1,55)],
                vec!["tt0000001","tt0000002"]),
        ] {
            let stamp=std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path=std::env::temp_dir().join(format!(
                "matchlab-cops-{}-{tag}-{stamp}.sqlite3",std::process::id()));
            let db=Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE titles(
                    imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                    category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
                CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
                CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                    episode_count INTEGER,max_episode INTEGER);
                CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
                CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                    tvdb_id TEXT,trakt_season INTEGER);
                CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                    anidb_season INTEGER,tvdb_season INTEGER,
                    start INTEGER,end INTEGER,offset INTEGER,map TEXT);
                CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);").unwrap();
            for (index,(max_season,max_episode)) in records.into_iter().enumerate() {
                let id=format!("tt{:07}",index+1);
                db.execute("INSERT INTO titles VALUES(?1,'Cops','cops',1989,
                    'series','US',1,0,'tvSeries')",[&id]).unwrap();
                db.execute("INSERT INTO seasons VALUES(?1,?2)",
                    rusqlite::params![id,max_season]).unwrap();
                db.execute("INSERT INTO season_episodes VALUES(?1,36,?2,?2)",
                    rusqlite::params![id,max_episode]).unwrap();
            }
            tantivy_search::warmup(&db).unwrap();
            drop(db);
            for release in ["Cops.S36.E41","Cops.S36E41"] {
                let result=diagnose(&path,&MatchRequest{
                    title:release.into(),year:None,series:None}).unwrap();
                assert_eq!(result.parsed["seasons"],json!([36]),
                    "{tag} {release}: {:?}",result.parsed);
                assert_eq!(result.parsed["episodes"],json!([41]),
                    "{tag} {release}: {:?}",result.parsed);
                let ids=result.imdb_candidates.iter()
                    .map(|candidate|candidate.imdb_id.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(ids,expected_ids,"{tag} {release}: {ids:?}");
                let selected=result.selected.expect("Cops title+season candidate retained");
                let response=public_match_response(Some(selected));
                assert_eq!(response["sid_start"],format!("{}:36:41",expected_ids[0]),
                    "{tag} {release}");
                assert_eq!(response["sid_end"],response["sid_start"]);
            }
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
        }
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
    fn imported_imdb_episode_outliers_do_not_poison_season_packs_or_public_sids(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
            episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt29171927',1,12,121247);
            INSERT INTO season_episodes VALUES('tt1561921',1,52,301085);
            INSERT INTO season_episodes VALUES('tt1234567',1,12,12);").unwrap();
        assert_eq!(season_episode_end(&db,"tt29171927",1).unwrap(),None);
        assert_eq!(season_episode_end(&db,"tt1561921",1).unwrap(),None);
        assert_eq!(season_episode_end(&db,"tt1234567",1).unwrap(),Some(12));
        for (id,bad) in [("tt29171927",121247),("tt1561921",301085)]{
            let selected=json!({"imdb_id":id,"season":1,"episode":1,
                "season_end":1,"episode_end":bad});
            let response=public_match_response(Some(selected));
            assert_eq!(response["sid_start"],id);
            assert_eq!(response["sid_end"],id);
            assert_eq!(response["asid_start"],Value::Null);
            assert_eq!(response["asid_end"],Value::Null);
            let direct=public_match_response(Some(json!({"imdb_id":id,
                "season":1,"episode":bad})));
            assert_eq!(direct["sid_start"],id);
            assert_eq!(direct["sid_end"],id);
        }
        let ordinary=public_match_response(Some(json!({
            "imdb_id":"tt1234567","season":1,"episode":1,
            "season_end":1,"episode_end":12})));
        assert_eq!(ordinary["sid_start"],"tt1234567:1:1");
        assert_eq!(ordinary["sid_end"],"tt1234567:1:12");
    }
    #[test]
    fn public_sid_endpoints_never_publish_reversed_or_mixed_shapes(){
        let reversed=public_match_response(Some(json!({
            "imdb_id":"tt1234567","category":"series","season":3,
            "episode":8,"season_end":2,"episode_end":9,
            "episode_range":{"complete":true,"segments":[]}
        })));
        assert_eq!(reversed["sid_start"],"tt1234567:3:8");
        assert!(reversed["sid_end"].is_null());
        assert!(reversed["asid_start"].is_null());
        let mixed=public_match_response(Some(json!({
            "imdb_id":"tt1234567","category":"series","season":2,
            "episode":8,"season_end":3,"anime":{"asid":"2369:8"}
        })));
        assert_eq!(mixed["sid_start"],"tt1234567:2:8");
        assert!(mixed["sid_end"].is_null());
        assert!(mixed["asid_start"].is_null());
        assert!(mixed["asid_end"].is_null());
        let valid=public_match_response(Some(json!({
            "imdb_id":"tt1234567","category":"series","season":2,
            "episode":8,"season_end":3,"episode_end":2
        })));
        assert_eq!(valid["sid_start"],"tt1234567:2:8");
        assert_eq!(valid["sid_end"],"tt1234567:3:2");
    }

    #[test]
    fn stremthru_strict_debug_matches_production_policy(){
        assert!(matches!(request_policy(false,false),MatchPolicy::PublicApi));
        assert!(matches!(request_policy(true,true),MatchPolicy::PublicApi));
        assert!(matches!(request_policy(true,false),MatchPolicy::DashboardDiagnostic));
        // /debug/match calls with (debug=true,strict=true), while the
        // dashboard's independent interactive route uses (true,false).
    }

    #[test]
    fn match_response_has_only_four_fields() {
        let movie=public_match_response(Some(json!({"imdb_id":"tt1234567",
            "category":"movie","season":null,"episode":null})));
        assert_eq!(movie,json!({"sid_start":"tt1234567","sid_end":"tt1234567",
            "asid_start":null,"asid_end":null}));
        let season=public_match_response(Some(json!({"imdb_id":"tt7654321",
            "season":1,"season_end":2,"episode":1,"episode_end":22})));
        assert_eq!(season["sid_start"],"tt7654321:1:1");
        assert_eq!(season["sid_end"],"tt7654321:2:22");
        let anime=public_match_response(Some(json!({"imdb_id":"tt9999999",
            "season":1,"episode":4,"anime":{"asid":"123:4"}})));
        assert_eq!(anime["asid_start"],"123:4");
        assert_eq!(anime["asid_end"],"123:4");
        let range=public_match_response(Some(json!({"imdb_id":"tt0388629",
            "episode_range":{"complete":true,"segments":[
                {"anidb_id":"69","start_anime_episode":700,"end_anime_episode":750},
                {"anidb_id":"70","start_anime_episode":1,"end_anime_episode":50}
            ]}})));
        assert_eq!(range["asid_start"],"69:700");
        assert_eq!(range["asid_end"],"70:50");
        assert_eq!(public_match_response(None),
            json!({"sid_start":null,"sid_end":null,"asid_start":null,"asid_end":null}));
    }
    #[test]
    fn ambiguous_anime_collection_does_not_choose_the_first_imdb_movie(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(
                anidb_id TEXT,imdb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anidb_titles VALUES('123','multiple specials');
            INSERT INTO episode_map VALUES('123','shared',1,1,1,12,0,'');
            INSERT INTO anime_series_map VALUES('123','tt1663145','shared',0);
            INSERT INTO anime_series_map VALUES('123','tt1679583','shared',0);
            INSERT INTO anime_series_map VALUES('123','tt1726641','shared',0);
            INSERT INTO anime_series_map VALUES('123','tt1753855','shared',0);
            INSERT INTO anime_series_map VALUES('123','tt1809274','shared',0);
            INSERT INTO anime_series_map VALUES('123','tt1865375','shared',0);").unwrap();
        assert!(anime_identity(&db,"multiple specials").unwrap().is_none(),
            "ambiguous AniDB alias must not select the first IMDb movie");
        assert!(anime_candidate(&db,"multiple specials",Some(1),1).unwrap().is_none(),
            "ambiguous AniDB alias must not guess a film for an episode");
    }
    #[test]
    fn public_api_never_exposes_comma_joined_imdb_id(){
        let response=public_match_response(Some(json!({
            "imdb_id":"tt1663145,tt1679583,tt1726641,tt1753855,tt1809274,tt1865375",
            "season":1,"episode":1,
            "anime":{"asid":"123:1"}
        })));
        assert_eq!(response,json!({"sid_start":null,"sid_end":null,
            "asid_start":null,"asid_end":null}));
        let valid=public_match_response(Some(json!({
            "imdb_id":"tt1663145","season":1,"episode":1
        })));
        assert_eq!(valid["sid_start"],"tt1663145:1:1");
        assert_eq!(valid["sid_end"],"tt1663145:1:1");
    }
    #[test]
    fn parse_cache_returns_full_parser_fields() {
        let first=parse_cached("Supernatural.S02E01.1080p.WEB-DL").unwrap();
        let second=parse_cached("Supernatural.S02E01.1080p.WEB-DL").unwrap();
        assert_eq!(first,second);
        assert_eq!(first["title"],"Supernatural");
        assert!(first.get("resolution").is_some());
    }
    #[test]
    fn anime_identity_without_episode_is_unambiguous(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
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
                    params![id,name,imdb_matcher::clean_title(name),category,kind]).unwrap();
            }
            for id in [&sid,&aid] {
                db.execute("INSERT INTO seasons VALUES(?1,10)",[id]).unwrap();
            }
            db.execute("INSERT INTO anime_series_map VALUES(?1,?2,?3,0)",
                params![aid,ani_id,format!("tvdb{i}")]).unwrap();
            db.execute("INSERT INTO anidb_titles VALUES(?1,?2,?3,'main','en')",
                params![ani_id,anime,imdb_matcher::clean_title(&anime)]).unwrap();
            db.execute("INSERT INTO episode_map VALUES(?1,?2,1,1,1,24,0,'')",
                params![ani_id,format!("tvdb{i}")]).unwrap();
        }
        drop(db);
        // A published request must use a warmed index; building here would
        // conceal an expensive request-time reindexing regression.
        let snapshot=Connection::open(&path).unwrap();
        tantivy_search::warmup(&snapshot).unwrap();
        drop(snapshot);
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
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }
    #[test]
    fn normalization_handles_accents_and_and_word() {
        assert_eq!(imdb_matcher::clean_title("Bjørn & Ægir"),"bjorn aegir".replace(' ',""));
        assert_eq!(imdb_matcher::clean_title("Rock and Roll"),"rockroll");
        assert_eq!(imdb_matcher::clean_title("Pokémon"),"pokemon");
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
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            INSERT INTO titles VALUES('tt1','Example','example',2020,'movie','US',1,1,'movie');
            INSERT INTO titles VALUES('tt2','Example','example',2021,'series','US',1,1,'tvSeries');").unwrap();
        let movies=imdb_matcher::search(&db,"Example",Some(2020),false,0,"Example",false,20).unwrap();
        let shows=imdb_matcher::search(&db,"Example",Some(2021),true,0,"Example",false,20).unwrap();
        assert_eq!(movies.len(),1);
        assert_eq!(movies[0].imdb_id,"tt1");
        assert_eq!(shows.len(),1);
        assert_eq!(shows[0].imdb_id,"tt2");
    }
    #[test]
    fn anime_requires_unique_verified_crosswalk() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT,tvdb_id TEXT DEFAULT '',trakt_season INTEGER DEFAULT 0);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT DEFAULT '',tvdb_season INTEGER,
                anidb_season INTEGER,map TEXT,offset INTEGER,start INTEGER,end INTEGER);
            INSERT INTO anidb_titles VALUES('123','exampleanime');
            INSERT INTO anime_series_map(anidb_id,imdb_id) VALUES('123','tt0000999');
            INSERT INTO episode_map(anidb_id,tvdb_season,anidb_season,map,offset,start,end) VALUES('123',1,1,'',0,1,12);").unwrap();
        let found = anime_candidate(&db,"exampleanime",Some(1),4).unwrap().unwrap();
        assert_eq!(found["imdb_id"],"tt0000999");
        assert_eq!(found["anidb_episode"],4);
        assert!(anime_candidate(&db,"exampleanime",Some(2),4).unwrap().is_none());
        db.execute("INSERT INTO episode_map(anidb_id,tvdb_season,anidb_season,map,offset,start,end) VALUES('123',1,1,'',1,1,12)",[]).unwrap();
        assert_eq!(anime_candidate(&db,"exampleanime",Some(1),4).unwrap().unwrap()["anidb_episode"],3);
    }
    #[test]
    fn chinese_episode_41_uses_only_verified_absolute_anime_crosswalk(){
        // The 2024 live-action drama shares an English alias with the 2022
        // donghua. The former is a bad year-ranked candidate; only the
        // anime has a verified absolute episode 41 -> season 2 episode 21.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-chinese-episode-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
                imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES
                ('tt0001111','The Young Brewmasters Adventure',
                 'theyoungbrewmastersadventure',2022,'series','',1,0,'tvSeries'),
                ('tt0002222','The Young Brewmasters Adventure',
                 'theyoungbrewmastersadventure',2024,'series','',1,1,'tvSeries');
            INSERT INTO seasons VALUES ('tt0001111',2),('tt0002222',1);
            INSERT INTO season_episodes VALUES
                ('tt0001111',1,20,20),('tt0001111',2,26,26),
                ('tt0002222',1,40,40);
            INSERT INTO anime_series_map VALUES('tt0001111','1234','99999',0);
            INSERT INTO anime_ids VALUES('tt0001111','anidb','1234');
            INSERT INTO anidb_titles VALUES('1234','The Young Brewmasters Adventure',
                 'theyoungbrewmastersadventure','main','en');
            INSERT INTO episode_map VALUES
                ('1234','99999',1,-1,1,46,0,''),
                ('1234','99999',1,1,1,20,0,''),
                ('1234','99999',1,2,21,46,-20,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let path_title="【高清剧集网发布 www.BPHDTV.com】少年白马醉春风2[第41集][国语配音+中文字幕].The.Young.Brewmasters.Adventure.S02.2024.1080p.WEB-DL.H264.AAC-ZeroTV/少年白马醉春风.The.Young.Brewmasters.Adventure.S02E41.2024.1080p.WEB-DL.H264.AAC-ZeroTV.mp4";
        let result=diagnose(&path,&MatchRequest{
            title:path_title.into(),year:None,series:None}).unwrap();
        assert_eq!(result.parsed["title"],"The Young Brewmasters Adventure");
        assert_eq!(result.parsed["seasons"],json!([2]));
        assert_eq!(result.parsed["episodes"],json!([41]));
        assert_eq!(result.parsed["absolute_episode_candidate"],json!(41));
        let winner=result.selected.expect("verified absolute-episode anime match");
        assert_eq!(winner["imdb_id"],"tt0001111");
        assert_eq!(winner["season"],2);
        assert_eq!(winner["episode"],21);
        assert_eq!(winner["anime"]["asid"],"1234:41");
        assert_eq!(winner["match_source"],"verified_chinese_absolute_crosswalk");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt0001111:2:21","sid_end":"tt0001111:2:21",
            "asid_start":"1234:41","asid_end":"1234:41"
        }));
        // Without a Chinese absolute marker, nothing authorizes converting
        // S02E41 into S02E21, even with the same title and year.
        let plain=diagnose(&path,&MatchRequest{
            title:"The.Young.Brewmasters.Adventure.S02E41.2024.WEB-DL".into(),
            year:None,series:None}).unwrap();
        assert!(plain.parsed.get("absolute_episode_candidate").is_none());
        if let Some(selected)=plain.selected {
            assert_ne!(selected["imdb_id"],"tt0001111");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

}
