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
mod field_audit;
mod field_learning;
mod corrective_learning;
mod general_learning;
mod tv_learning;
mod community_learning;
mod community_rule_archive;
mod learning_rule_log;
mod local_learning;
mod online_model;
mod candidate_discovery;
mod title_learning;
mod learning_cache;
mod ml_config;
mod learned_regex;
mod query_cache;
use axum::{extract::{Query, State}, http::{StatusCode,header}, response::Redirect, routing::{get,post}, Json, Router};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
#[cfg(test)]
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, path::PathBuf, sync::{Arc,Mutex,OnceLock,atomic::{AtomicBool,Ordering}}, time::{Instant,Duration}, collections::HashMap};
use tokio::sync::Semaphore;


static STRICT_MATCH_MODE:OnceLock<bool>=OnceLock::new();
fn strict_match_mode()->bool{
    // Runtime sets this before accepting requests. Production fails closed
    // with exact-only matching if accessed before initialization. Standalone
    // unit fixtures retain legacy discovery until they explicitly test strict.
    STRICT_MATCH_MODE.get().copied().unwrap_or(!cfg!(test))
}
fn parse_strict_match_mode(raw:Option<&str>)->Result<bool,String>{
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(){
        None|Some("true"|"1"|"on"|"yes")=>Ok(true),
        Some("false"|"0"|"off"|"no")=>Ok(false),
        Some(other)=>Err(format!(
            "MATCHLAB_STRICT_MODE must be true or false (got {other:?})")),
    }
}

#[derive(Clone)]
struct AppState {
    db:Arc<PathBuf>,
    history:Arc<PathBuf>,
    match_slots:Arc<Semaphore>,
    index_phase:Arc<Mutex<IndexPhase>>,
    history_ready:Arc<AtomicBool>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Candidate {
    imdb_id: String,
    title: String,
    year: i32,
    media_type: String,
    matched_title: String,
    anime_ids: std::collections::BTreeMap<String,String>,
    source: String,
}

fn complete_candidate_alias(candidate:&Candidate,title:&str)->bool{
    imdb_matcher::clean_title(&candidate.matched_title)==imdb_matcher::clean_title(title)
        &&imdb_matcher::semantic_words(&candidate.matched_title)==imdb_matcher::semantic_words(title)
        &&title_normalization::title_word_count(&candidate.matched_title)
            ==title_normalization::title_word_count(title)
}

fn unmarked_crosswalk_identity_eligible(
    top:Option<&Candidate>,candidate:&Candidate,title:&str,
    exact_anime_ids:&std::collections::BTreeSet<String>,
)->bool{
    // Anchor to the existing top title fit. A lower exact short alias must
    // not outrank a stronger full-release recovery. An independently exact
    // AniDB title may still name a translated series or a specific part.
    top.is_none_or(|top|!complete_candidate_alias(top,title))
        ||complete_candidate_alias(candidate,title)
        ||exact_anime_ids.contains(&candidate.imdb_id)
}

fn unresolved_date_predates_identity(winner:&Value,date:&str,requested_year:Option<i32>)->bool{
    if requested_year.is_some()||winner["category"]!="series"
        ||winner["air_date"].as_str().is_some_and(|date|!date.is_empty()){
        return false;
    }
    let release_year=date.split('-').next()
        .filter(|year|year.len()==4&&year.bytes().all(|c|c.is_ascii_digit()))
        .and_then(|year|year.parse::<i64>().ok());
    match (release_year,winner["year"].as_i64()){
        (Some(release),Some(start))=>release>0&&start>release,
        _=>false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Diagnostic {
    parsed: Value,
    normalized_title: String,
    imdb_candidates: Vec<Candidate>,
    anime_candidate: Option<Value>,
    anidb_candidate: Option<Value>,
    selected: Option<Value>,
    reason: String,
}

// Shared, bounded 30-second parser cache for /parse and /match. Each entry
// belongs to an exact active regex generation; obsolete entries are NEVER
// returned. When rules change, a request performs a fresh base parse.
const PARSE_TTL:Duration=Duration::from_secs(30);
// Ready parses are immutable; large JSON clones happen outside the global lock.
static PARSE_CACHE:OnceLock<Mutex<HashMap<String,(Instant,u64,Arc<Value>)>>>=OnceLock::new();
fn cache_generation_current(saved:u64,at:Instant,current:u64)->bool{
    saved==current && at.elapsed()<PARSE_TTL
}
fn parse_rules_revision()->u64{
    learned_regex::revision().rotate_left(1) ^ field_learning::revision().rotate_left(17)
        ^ tv_learning::revision().rotate_left(33)
        ^ general_learning::revision().rotate_left(7)
        ^ matchlab_parser::handler_patch_revision().rotate_left(49)
}

fn stable_parse_snapshot(
    revision:impl Fn()->u64,
    mut load:impl FnMut()->Result<(Value,Option<String>),String>,
)->Result<(Value,u64,Option<String>),String>{
    // One retry handles publication during a parse without an unbounded loop
    // when rules are being updated repeatedly. Always retain the generation
    // captured BEFORE loading: an old parse must never acquire a newer label.
    for _ in 0..2 {
        let generation=revision();
        let (value,persistent_generation)=load()?;
        if revision()==generation {
            return Ok((value,generation,persistent_generation));
        }
    }
    Err("parser rules changed during request; retry".into())
}

fn parse_cached(title:&str)->Result<Value,String>{
    if title.trim().is_empty()||title.len()>4096 {
        return Err("title must be 1–4096 bytes".into());
    }
    let generation=parse_rules_revision();
    let cache=PARSE_CACHE.get_or_init(||Mutex::new(HashMap::new()));
    // Arc clone under the mutex, expensive JSON clone after unlocking.
    // A rule update between the lookup and return still forces a fresh parse.
    let cached=cache.lock().ok().and_then(|guard|{
        guard.get(title).and_then(|(at,saved_generation,value)|
            cache_generation_current(*saved_generation,*at,generation)
                .then(||Arc::clone(value)))
    });
    if let Some(value)=cached {
        let value=(*value).clone();
        if parse_rules_revision()==generation {return Ok(value);}
        // A concurrent source-regex update requires a fresh parser run.
    }
    // Persisted hits are likewise strict current-namespace only. On a
    // parser/image/regex change, an old result can remain as historical
    // learning evidence but cannot become a live parse result.
    let (value,generation,persistent_generation)=stable_parse_snapshot(
        parse_rules_revision,||{
            let persistent_generation=learning_cache::parser_generation();
            let value=if let Some(cached)=learning_cache::get_parser(title){cached}
                else{Value::Object(matchlab_parser::parse_title(title,false)
                    .map_err(|e|e.to_string())?)};
            Ok((value,persistent_generation))
        })?;
    // The base parser output is stored, not an ML-decorated response. This
    // ensures retired learned patterns never survive as cached coordinates.
    if let Some(saved)=persistent_generation {
        learning_cache::put_parser(title,&value,&saved);
    }
    let cache_value=Arc::new(value.clone());
    if let Ok(mut guard)=cache.lock(){
        const CAPACITY:usize=1024;
        if guard.len()>=CAPACITY {
            let latest=parse_rules_revision();
            guard.retain(|_,(at,old,_)|cache_generation_current(*old,*at,latest));
            if guard.len()>=CAPACITY {
                if let Some(oldest)=guard.iter()
                    .min_by_key(|(_, (at,_,_))|*at).map(|(key,_)|key.clone()){
                    guard.remove(&oldest);
                }
            }
        }
        guard.insert(title.to_string(),(Instant::now(),generation,cache_value));
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

/// The published metadata file, identified by everything an atomic
/// refresh changes. A connection is reused only for the same file.
#[derive(Clone,PartialEq,Eq,Debug)]
struct DbFile {path:PathBuf,dev:u64,ino:u64,len:u64,modified:Option<std::time::SystemTime>}
fn db_file(path:&std::path::Path)->Option<DbFile>{
    let metadata=std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let (dev,ino)=(std::os::unix::fs::MetadataExt::dev(&metadata),
        std::os::unix::fs::MetadataExt::ino(&metadata));
    #[cfg(not(unix))]
    let (dev,ino)=(0,0);
    Some(DbFile{path:path.to_owned(),dev,ino,len:metadata.len(),
        modified:metadata.modified().ok()})
}
thread_local! {
    // Opening a connection makes SQLite parse the whole schema on its first
    // statement: about an eighth of a match request's CPU. Tokio reuses its
    // blocking threads, so each keeps its last read-only connection.
    static DB_POOL:std::cell::RefCell<Option<(DbFile,Connection)>>=
        const { std::cell::RefCell::new(None) };
}
/// A read-only metadata connection that returns to its thread on drop.
struct PooledDb {conn:Option<Connection>,file:Option<DbFile>}
impl std::ops::Deref for PooledDb {
    type Target=Connection;
    fn deref(&self)->&Connection{self.conn.as_ref().expect("connection until drop")}
}
impl Drop for PooledDb {
    fn drop(&mut self){
        if let (Some(conn),Some(file))=(self.conn.take(),self.file.take()){
            let _=DB_POOL.try_with(|pool|*pool.borrow_mut()=Some((file,conn)));
        }
    }
}
fn open_db(path: &PathBuf) -> rusqlite::Result<PooledDb> {
    let file=db_file(path);
    if let Some(file)=&file
        && let Some(conn)=DB_POOL.with(|pool|match pool.borrow_mut().take(){
            Some((cached,conn)) if cached==*file=>Some(conn),
            _=>None,
        })
    {
        return Ok(PooledDb{conn:Some(conn),file:Some(file.clone())});
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.create_scalar_function("reverse", 1, rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC | rusqlite::functions::FunctionFlags::SQLITE_UTF8, |ctx| {
        let value: String = ctx.get(0)?;
        Ok(value.chars().rev().collect::<String>())
    })?;
    // A file that changed while it was being opened is not reused.
    let file=file.filter(|before|db_file(path).as_ref()==Some(before));
    Ok(PooledDb{conn:Some(conn),file})
}

#[derive(Default)]
struct AnimeEpisodeEvidence {
    candidate:Option<Value>,
    rejected_exact_nonseries:bool,
    eligible_exact_ids:std::collections::BTreeSet<String>,
}

#[cfg(test)]
fn anime_candidate(db:&Connection,title:&str,season:Option<i64>,episode:i64)
    ->rusqlite::Result<Option<Value>>{
    anime_candidate_evidence(db,title,season,episode).map(|found|found.candidate)
}

fn anime_candidate_evidence(db:&Connection,title:&str,season:Option<i64>,episode:i64)
    ->rusqlite::Result<AnimeEpisodeEvidence>{
    let key=imdb_matcher::clean_title(title);
    if key.is_empty()||(!strict_match_mode()&&key.len()<4)||episode<=0{
        return Ok(AnimeEpisodeEvidence::default());
    }
    let aliases=if strict_match_mode(){
        tantivy_search::anime_aliases_exact(db,title)
    }else{tantivy_search::anime_aliases_for_title(db,title,6)};
    let matches:Vec<_>=aliases
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?
        .into_iter().map(|alias|(alias.anidb_id,alias.clean_title,alias.imdb_id))
        .collect();
    // A film may share a TVDB family with the main anime series. That
    // relationship does not make the film's IMDb ID a TV episode identity.
    // Inspect only IDs already discovered through the anime alias index;
    // missing IMDb metadata remains supported by the independent anime lane.
    let discovered_ids=matches.iter().map(|(_,_,id)|id.clone())
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    let cataloged=tantivy_search::titles_by_ids(db,&discovered_ids)
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?;
    let nonseries_ids=cataloged.iter()
        .filter(|row|matches!(row.media_type.as_str(),"movie"|"tvMovie"|"short"
            |"tvShort"|"tvSpecial"|"video"|"videoGame"|"tvEpisode"))
        .map(|row|row.imdb_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let series_ids=cataloged.iter()
        .filter(|row|matches!(row.media_type.as_str(),"tvSeries"|"tvMiniSeries"))
        .map(|row|row.imdb_id.as_str()).collect::<std::collections::HashSet<_>>();
    // A complete film alias cannot authorize a weaker, shorter TV alias
    // merely because the film is ineligible for episode coordinates.
    // Equally exact series identities remain eligible; an alias with no
    // positively known type retains the existing verified fallback order.
    let rejected_exact=matches.iter().any(|(_,alias,id)|alias==&key
        &&nonseries_ids.contains(id.as_str())&&!series_ids.contains(id.as_str()));
    let mut evidence=AnimeEpisodeEvidence{
        candidate:None,rejected_exact_nonseries:rejected_exact,
        eligible_exact_ids:matches.iter().filter(|(_,alias,id)|alias==&key
            &&(!nonseries_ids.contains(id.as_str())||series_ids.contains(id.as_str())))
            .map(|(_,_,id)|id.clone()).collect(),
    };
    let matches=matches.into_iter().filter(|(_,alias,id)|
        (!nonseries_ids.contains(id.as_str())||series_ids.contains(id.as_str()))
            &&(!rejected_exact||alias==&key))
        .collect::<Vec<_>>();
    // AniDB alias hits are sorted by length, with leading edge ahead of
    // trailing edge only for equal lengths. A longer alias with NO verified
    // episode must not suppress a shorter alias that has real crosswalk
    // evidence (e.g. a current JoJo TV series versus an older OVA).
    let mut cursor=0;
    while cursor<matches.len(){
        let len=matches[cursor].1.len();
        let leading=key.starts_with(&matches[cursor].1);
        let mut imdb_ids=std::collections::BTreeSet::new();
        while cursor<matches.len() && matches[cursor].1.len()==len
            && key.starts_with(&matches[cursor].1)==leading {
            imdb_ids.insert(matches[cursor].2.clone());
            cursor+=1;
        }
        let mut verified:Option<episode_crosswalk::Episode>=None;
        for imdb in imdb_ids {
            let mapped=match season{
                Some(season) if season>0=>{
                    let titled=anime_title::resolve(db,&imdb,title,season,episode)?;
                    if titled.is_some(){titled}
                    else if anime_title::has_specific_alias(db,&imdb,title)?{None}
                    else{episode_crosswalk::resolve_season(db,&imdb,season,episode)?}
                },
                None=>{
                    let named=anime_title::resolve_episode_from_exact_part_title(
                        db,&imdb,title,episode)?;
                    if named.is_some(){named}
                    else{episode_crosswalk::resolve_absolute(db,&imdb,episode)?}
                },
                _=>None,
            };
            if let Some(mapped)=mapped{
                if verified.is_some(){
                    // More than one independently mapped identity with the
                    // same lexical fit: do not invent a winner.
                    return Ok(evidence);
                }
                verified=Some(mapped);
            }
        }
        if let Some(m)=verified{
            evidence.candidate=Some(json!({"imdb_id":m.imdb_id,"season":m.season,
                "episode":m.episode,"anidb_id":m.anidb_id,
                "anidb_episode":m.anidb_episode}));
            return Ok(evidence);
        }
    }
    Ok(evidence)
}

/// Resolve a title-only anime identity from anime tables, without inventing
/// season or episode coordinates. Exact aliases precede longest prefixes.
fn anime_identity(db:&Connection,title:&str)->rusqlite::Result<Option<String>>{
    let key=imdb_matcher::clean_title(title);
    if key.is_empty()||(!strict_match_mode()&&key.len()<4){return Ok(None);}
    let aliases=if strict_match_mode(){
        tantivy_search::anime_aliases_exact(db,title)
    }else{tantivy_search::anime_aliases_for_title(db,title,6)};
    let rows:Vec<_>=aliases
        .map_err(|error|rusqlite::Error::ToSqlConversionFailure(
            Box::new(std::io::Error::other(error))))?
        .into_iter().map(|alias|(alias.clean_title,alias.imdb_id)).collect();
    let Some((best_alias,best_imdb))=rows.first()else{return Ok(None)};
    let exact=best_alias==&key;
    let size=best_alias.len();
    if rows.iter().any(|(alias,id)|(alias==&key)==exact&&alias.len()==size&&id!=best_imdb){
        return Ok(None);
    }
    Ok(Some(best_imdb.clone()))
}

/// "Mob Psycho 100" and "Digimon Adventure 02" are complete titles. When
/// the numbered form is an exact anime alias or an exact IMDb series title,
/// its trailing number is part of the name, not an episode.
fn numbered_title_is_work(db:&Connection,full:&str)->Result<bool,String>{
    if exact_anime_identity(db,full)?.is_some(){return Ok(true);}
    let key=imdb_matcher::clean_title(full);
    if key.len()<4{return Ok(false);}
    let words=imdb_matcher::semantic_words(full);
    Ok(tantivy_search::title_aliases_exact(db,&key)?.into_iter()
        .any(|alias|alias.clean==key
            && matches!(alias.media_type.as_str(),"tvSeries"|"tvMiniSeries")
            && imdb_matcher::semantic_words(&alias.title)==words))
}

/// Split "Title Part 2" / "Title (part 2)" / "Title 2nd Cour" style names
/// into the first part's title and the part number.
fn numbered_part_title(title:&str)->Option<(&str,i64)>{
    let trimmed=title.trim().trim_end_matches([')',']']).trim_end();
    let digit=trimmed.chars().next_back()?.to_digit(10)? as i64;
    if !(2..=9).contains(&digit){return None;}
    let head=trimmed[..trimmed.len()-1].trim_end_matches([' ','.','_','-']);
    let lower=head.to_ascii_lowercase();
    let word=["part","cour"].into_iter().find(|word|lower.ends_with(word))?;
    let base=head[..head.len()-word.len()]
        .trim_end_matches([' ','.','_','-','(','[',':']);
    // The marker must be its own word and the base a real title.
    if base.len()==head.len()-word.len()
        || base.chars().filter(|c|c.is_alphabetic()).count()<4{return None;}
    Some((base,digit))
}

/// Return an IMDb identity only when the release title itself is an exact,
/// unique AniDB alias. This is stricter than the prefix-based fallback used
/// to rank title candidates and safe for season-pack inference.
fn exact_anime_identity(db:&Connection,title:&str)->Result<Option<String>,String>{
    let key=imdb_matcher::clean_title(title);
    if key.is_empty()||(!strict_match_mode()&&key.len()<4){return Ok(None);}
    let aliases=if strict_match_mode(){
        tantivy_search::anime_aliases_exact(db,title)?
    }else{tantivy_search::anime_aliases(db,&key,4)?};
    let ids=aliases.into_iter()
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

/// Labeled EP01-24 endpoints can describe an ordinary IMDb season.
/// Honor an independently parsed season, or infer S01 only for a known
/// single-season show. Never replace absolute anime numbering when its
/// AniDB crosswalk is incomplete. Exact identity and imported episode bounds
/// must independently establish the complete ordinary-TV interval.
/// A missing or stale IMDb season snapshot is not permission to invent one.
fn verified_ordinary_imdb_episode_range(
    db:&Connection,imdb:&str,release_key:&str,matched_title:&str,
    start:i64,end:i64,season_hint:Option<i64>,
)->Result<Option<Value>,String>{
    if start<=0 || end<start || end-start>=1_000
        || !valid_sid_coordinate(1,start) || !valid_sid_coordinate(1,end)
        || imdb_matcher::clean_title(matched_title)!=release_key {
        return Ok(None);
    }
    let max_season:Option<i64>=db.query_row(
        "SELECT max_season FROM seasons WHERE imdb_id=?1",
        [imdb],|row|row.get(0)
    ).optional().map_err(|e|e.to_string())?;
    let season=match season_hint {
        Some(season) if season>0 && valid_sid_coordinate(season,end)
            &&max_season.is_some_and(|max|max>=season)=>season,
        None if max_season==Some(1)=>1,
        _=>return Ok(None),
    };
    if !season_episode_end(db,imdb,season)?.is_some_and(|limit|limit>=end) {
        return Ok(None);
    }
    // A linked AniDB identity indicates a potentially absolute episode
    // numbering system. Preserve the crosswalk's authority, even if its
    // mapping is incomplete or temporarily missing.
    let anime_link:i64=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM anime_series_map WHERE imdb_id=?1)
            OR EXISTS(SELECT 1 FROM anime_ids
                WHERE group_id=?1 AND type='anidb')",
        [imdb],|row|row.get(0)
    ).map_err(|e|e.to_string())?;
    if anime_link!=0{return Ok(None);}
    Ok(Some(json!({
        "start_season":season,"start_episode":start,
        "end_season":season,"end_episode":end,
        "requested_episode_start":start,"requested_episode_end":end,
        "complete":true,"segments":[],"unmapped_count":0,
        "source":if season_hint.is_some(){"verified_imdb_explicit_season_episode_range"}
            else{"verified_imdb_single_season_episode_range"}
    })))
}

/// A release year equal to a show's *first* year, coupled with an exact
/// full-season episode batch, identifies its first IMDb season even if a
/// historical AniDB snapshot lacks any episode crosswalk. This is a strictly
/// IMDb-only fallback: it never publishes guessed AniDB identifiers.
fn verified_first_year_anime_imdb_batch(
    db:&Connection,winner:&Value,parsed:&serde_json::Map<String,Value>,
    candidates:&[Candidate],
)->Result<Option<Value>,String>{
    if winner["category"]!="series" || winner.get("episode_range").is_some()
        || winner["episode"].as_i64().is_some()
        || winner.pointer("/anime/asid").and_then(Value::as_str).is_some()
        || parsed.get("special_episode").is_some()
        || parsed.get("anime_local_part_unresolved").is_some()
        || parsed.get("anime_named_part_season_hint").is_some()
    {return Ok(None);}
    let Some(hint)=parsed.get("anime_episode_range_candidate") else {
        return Ok(None);
    };
    let (Some(title),Some(start),Some(end),Some(year),Some(imdb))=(
        hint["title"].as_str(),hint["start"].as_i64(),hint["end"].as_i64(),
        parsed.get("year").and_then(Value::as_i64),
        winner["imdb_id"].as_str()
    )else{return Ok(None)};
    if hint["numbering"]!="absolute" || hint["season_hint"].as_i64().is_some()
        || start!=1 || !(2..=99).contains(&end)
        || !(1900..=2100).contains(&year)
        || winner["year"].as_i64()!=Some(year)
        || !metadata_import::valid_imdb_id(imdb)
        || imdb_matcher::clean_title(title)
            !=imdb_matcher::clean_title(winner["matched_title"].as_str().unwrap_or(""))
    {return Ok(None);}
    // A same-name 1997 series must never acquire a 2016 batch merely
    // because both titles normalize to "Berserk".
    let unique_year_id=candidates.iter().filter(|c|
        c.year as i64==year && c.media_type=="tvSeries"
        && imdb_matcher::clean_title(&c.matched_title)
            ==imdb_matcher::clean_title(title)
    ).map(|c|c.imdb_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if unique_year_id.len()!=1 || !unique_year_id.contains(imdb) {
        return Ok(None);
    }
    // Only full S01 packs are eligible. Crosswalk fragments must remain
    // unresolved instead of being overwritten by an unrelated IMDb count.
    let exact_first_season:Option<(i64,i64)>=db.query_row(
        "SELECT episode_count,max_episode FROM season_episodes
          WHERE imdb_id=?1 AND season=1",
        [imdb],|row|Ok((row.get(0)?,row.get(1)?))
    ).optional().map_err(|e|e.to_string())?;
    if exact_first_season!=Some((end,end)){return Ok(None);}
    let linked:i64=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM anime_series_map WHERE imdb_id=?1)
          OR EXISTS(SELECT 1 FROM anime_ids
                    WHERE group_id=?1 AND type='anidb')",
        [imdb],|row|row.get(0)
    ).map_err(|e|e.to_string())?;
    if linked==0{return Ok(None);}
    let crosswalk=mapped_absolute_range(db,imdb,start,end)?;
    if crosswalk["unmapped_count"].as_i64()!=Some(end)
        ||crosswalk["segments"].as_array().is_none_or(|rows|!rows.is_empty())
    {return Ok(None);}
    Ok(Some(json!({
        "start_season":1,"start_episode":1,
        "end_season":1,"end_episode":end,
        "requested_episode_start":1,"requested_episode_end":end,
        "complete":true,"segments":[],"unmapped_count":0,
        "source":"verified_imdb_first_year_complete_anime_batch"
    })))
}

/// Validate each episode in a season-labeled anime batch against the
/// AniDB/TVDB crosswalk. A partial mapping never produces guessed endpoints.
fn mapped_season_episode_range(
    db:&Connection,imdb:&str,title:&str,season:i64,start:i64,end:i64,
    canonical_title:bool,force_named_part:bool,
)->Result<Value,String>{
    let mut segments:Vec<Value>=Vec::new();
    let mut first:Option<episode_crosswalk::Episode>=None;
    let mut last:Option<episode_crosswalk::Episode>=None;
    let mut missing=0i64;
    // The canonical IMDb series title identifies a fixed IMDb season.
    // Never reinterpret it as a named AniDB part just because an OVA/movie
    // shares its alias (e.g. the base series Bleach).
    let specific=if force_named_part {true}else if canonical_title {false}else{
        anime_title::has_specific_alias(db,imdb,title)
            .map_err(|e|e.to_string())?
    };
    let resolved=if force_named_part {
        anime_title::resolve_local_part_range(db,imdb,title,season,start,end)
            .map_err(|e|e.to_string())?
    }else if specific {
        anime_title::resolve_range(db,imdb,title,season,start,end)
            .map_err(|e|e.to_string())?
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

/// A conservative SID-only fallback for anime with incomplete AniDB crosswalks.
/// Every preceding IMDb season must contain episodes numbered 1..N with no
/// gaps, so the absolute index has a unique, bounded season coordinate.
fn complete_imdb_absolute_episode(
    db:&Connection,imdb:&str,absolute:i64,
)->Result<Option<(i64,i64)>,String>{
    if !(2..=99_999).contains(&absolute){return Ok(None);}
    let mut stmt=db.prepare("SELECT season,episode_count,max_episode
        FROM season_episodes WHERE imdb_id=?1 AND season>0 ORDER BY season")
        .map_err(|e|e.to_string())?;
    let rows=stmt.query_map([imdb],|row|Ok((
        row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?
    ))).map_err(|e|e.to_string())?;
    let (mut next_season,mut remaining)=(1i64,absolute);
    for row in rows {
        let (season,count,max)=row.map_err(|e|e.to_string())?;
        if season!=next_season || count!=max || max<=0
            || !valid_sid_coordinate(season,max){return Ok(None);}
        if remaining<=max {
            // Never guess within S01: this helper is only for converting
            // whole-series absolute coordinates into a later IMDb season.
            // Source and title evidence is checked by the caller.
            return Ok((season>1).then_some((season,remaining)));
        }
        remaining-=max;
        next_season+=1;
    }
    Ok(None)
}

/// Do not turn ordinary "Title - 33" releases or named sequel-local episode
/// numbers into IMDb coordinates. Require an explicit absolute-anime hint,
/// a unique exact anime/IMDb title identity, complete season metadata and
/// no contradictory AniDB crosswalk evidence.
fn verified_unmapped_anime_absolute_episode(
    db:&Connection,winner:&Value,parsed:&serde_json::Map<String,Value>,
    allow_invalid_absolute_crosswalk:bool,
)->Result<Option<(i64,i64)>,String>{
    if winner["episode"].as_i64().is_some()
        || winner["season"].as_i64().is_some_and(|season|season>1)
        || winner.pointer("/anime/asid").and_then(Value::as_str).is_some()
        || parsed.get("seasons").and_then(Value::as_array).is_some_and(|s|!s.is_empty())
        || parsed.contains_key("episode_start")
        || parsed.contains_key("anime_local_part_unresolved")
        || parsed.contains_key("anime_local_part_range_unresolved")
        || parsed.contains_key("anime_named_part_season_hint")
        || parsed.contains_key("special_episode")
        // A fansub release group is an independent structural signal that
        // the number is an absolute anime episode, not an unnamed local part.
        // Do not reinterpret bare "Dragon Ball Super - 80 WEB-DL".
        || !(parsed.get("group").and_then(Value::as_str)
                .is_some_and(|group|!group.trim().is_empty())
            // "[Group][Title][14]" is the same fansub structure even when
            // the generic parser does not name the group.
            || parsed.get("anime_episode_candidate")
                .and_then(|hint|hint.get("bracketed"))
                .and_then(Value::as_bool)==Some(true))
    {return Ok(None);}
    let Some(hint)=parsed.get("anime_episode_candidate") else{return Ok(None)};
    if hint["numbering"]!="absolute" || hint["path_part"]==true {
        return Ok(None);
    }
    let (Some(title),Some(absolute),Some(imdb))=(
        hint["title"].as_str(),hint["episode"].as_i64(),winner["imdb_id"].as_str()
    )else{return Ok(None)};
    if !(1..=99_999).contains(&absolute)
        || parsed.get("episodes").and_then(Value::as_array)
            .is_some_and(|episodes|episodes.len()>1
                || episodes.first().is_some_and(|ep|ep.as_i64()!=Some(absolute)))
    {return Ok(None);}
    let exact_title=imdb_matcher::clean_title(title)
        ==imdb_matcher::clean_title(winner["matched_title"].as_str().unwrap_or(""))
        && imdb_matcher::semantic_words(title)
            ==imdb_matcher::semantic_words(winner["matched_title"].as_str().unwrap_or(""))
        && title_normalization::title_word_count(title)
            ==title_normalization::title_word_count(
                winner["matched_title"].as_str().unwrap_or(""));
    let linked_anime=winner["anime_ids"].as_object()
        .is_some_and(|ids|!ids.is_empty());
    if !(exact_title && linked_anime)
        && exact_anime_identity(db,title)?.as_deref()!=Some(imdb)
    {return Ok(None);}
    // An exact translated anime title must be proven to name the first
    // AniDB part of this IMDb series. Unknown named parts are not safe to
    // reinterpret as whole-series absolute numbering (Alicization etc.).
    // The canonical IMDb title already identifies the overall series.
    if imdb_matcher::clean_title(winner["title"].as_str().unwrap_or(""))
            !=imdb_matcher::clean_title(title)
        && anime_title::season_from_title(db,imdb,title)
            .map_err(|e|e.to_string())?!=Some(1)
    {return Ok(None);}
    if let Some(existing)=episode_crosswalk::resolve_absolute(db,imdb,absolute)
        .map_err(|e|e.to_string())? {
        // Only repair a crosswalk already proven impossible by IMDb season
        // bounds. Preserve every valid AniDB/TVDB absolute mapping.
        // The impossible crosswalk can sit in any season: an open-ended
        // later part also accepts an absolute number it cannot contain.
        if !allow_invalid_absolute_crosswalk
            || existing.anidb_episode!=absolute
            || season_episode_end(db,imdb,existing.season)?
                .is_none_or(|max|existing.episode<=max)
            || winner["anime_ids"]["anidb"].as_str()!=Some(existing.anidb_id.as_str())
        {return Ok(None);}
        // Anime-Lists' open-ended defaulttvdbseason=1 is a weak, unbounded
        // ownership hint, not proof that all absolute episodes belong to
        // IMDb S01. A finite or explicit S01 map is higher-quality evidence
        // and must never be overridden by arithmetic.
        let weak_default_only:i64=db.query_row(
            "SELECT EXISTS(SELECT 1 FROM anime_series_map l
                 JOIN episode_map m ON m.anidb_id=l.anidb_id
                   AND (l.tvdb_id='' OR m.tvdb_id=l.tvdb_id)
                 WHERE l.imdb_id=?1 AND m.anidb_id=?2
                   AND m.anidb_season>0 AND m.tvdb_season=?3
                   AND m.start<=1 AND m.end<=0
                   AND TRIM(m.map)='')
              AND NOT EXISTS(SELECT 1 FROM episode_map m
                 WHERE m.anidb_id=?2 AND m.tvdb_season=?3
                   AND (m.end>0 OR TRIM(m.map)<>''))",
            rusqlite::params![imdb,&existing.anidb_id,existing.season],|row|row.get(0)
        ).map_err(|e|e.to_string())?;
        if weak_default_only==0{return Ok(None);}
    }
    let Some((season,episode))=complete_imdb_absolute_episode(db,imdb,absolute)?
        else{return Ok(None)};
    // If the target IMDb season has an AniDB map, it must positively resolve
    // this episode. A partial, ambiguous or contradictory map is not a
    // license to infer an unverified coordinate.
    let mapped_season_exists:i64=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM anime_series_map l
            JOIN episode_map m ON m.anidb_id=l.anidb_id
                OR (l.tvdb_id<>'' AND m.tvdb_id=l.tvdb_id)
            WHERE l.imdb_id=?1 AND m.anidb_season>0 AND m.tvdb_season=?2)",
        rusqlite::params![imdb,season],|row|row.get(0)
    ).map_err(|e|e.to_string())?;
    let mapped_target=episode_crosswalk::resolve_season(db,imdb,season,episode)
        .map_err(|e|e.to_string())?;
    if let Some(ref target)=mapped_target {
        // The release names a work through its title. The IMDb record's
        // single stored AniDB ID can be any linked work (often the latest
        // season) and is only the fallback when the alias is not unique.
        // Optional evidence: an unavailable alias index leaves the
        // stored ID as the only identification.
        let named_work=anime_title::exact_alias_work(db,imdb,title)
            .ok().flatten();
        let original_anidb=named_work.as_deref()
            .or(winner["anime_ids"]["anidb"].as_str());
        if original_anidb==Some(target.anidb_id.as_str()) {
            // Same AniDB work: never reinterpret its explicit local episode
            // 43 as the release's absolute episode 145.
            if target.anidb_episode!=absolute{return Ok(None);}
        }else {
            // Different AniDB work: a true season-specific, finite source
            // crosswalk may identify the correct local episode. A weak
            // open-ended default or same-TVDB association is not enough.
            let bounded:i64=db.query_row(
                "SELECT EXISTS(SELECT 1 FROM episode_map m
                    JOIN anime_series_map l ON l.tvdb_id=m.tvdb_id
                    WHERE l.imdb_id=?1 AND l.tvdb_id<>'' AND m.tvdb_id<>''
                      AND m.anidb_id=?2 AND m.anidb_season>0
                      AND m.tvdb_season=?3
                      AND ((m.end>=m.start AND m.start>0)
                        OR TRIM(m.map)<>''))",
                rusqlite::params![imdb,&target.anidb_id,season],
                |row|row.get(0)
            ).map_err(|e|e.to_string())?;
            // An open-ended default also identifies the season when this
            // directly linked work is the ONLY one mapped to it and starts
            // it at episode one; IMDb's complete season counts, checked
            // above, bound the episode.
            let sole_season_starter:i64=db.query_row(
                "SELECT EXISTS(SELECT 1 FROM anime_series_map l
                        JOIN episode_map m ON m.anidb_id=l.anidb_id
                        WHERE l.imdb_id=?1 AND l.anidb_id=?2
                          AND m.anidb_season>0 AND m.tvdb_season=?3
                          AND m.start<=1 AND m.offset=0 AND TRIM(m.map)='')
                    AND NOT EXISTS(SELECT 1 FROM anime_series_map l
                        JOIN episode_map m ON m.anidb_id=l.anidb_id
                        WHERE l.imdb_id=?1 AND l.anidb_id<>?2
                          AND m.anidb_season>0 AND m.tvdb_season=?3)",
                rusqlite::params![imdb,&target.anidb_id,season],
                |row|row.get(0)
            ).map_err(|e|e.to_string())?;
            if bounded==0 && sole_season_starter==0{return Ok(None);}
            // Verify the reverse direction too: the local AniDB identity
            // must map back to exactly this IMDb season and episode.
            let reverse=episode_crosswalk::resolve_anidb(
                db,&target.anidb_id,target.anidb_episode)
                .map_err(|e|e.to_string())?;
            if !reverse.iter().any(|row|
                row.imdb_id==imdb && row.season==season
                && row.episode==episode
                && row.anidb_id==target.anidb_id
                && row.anidb_episode==target.anidb_episode)
            {return Ok(None);}
        }
    }else if mapped_season_exists!=0{
        return Ok(None);
    }
    Ok(Some((season,episode)))
}

/// AniDB counts a broadcast "episode 0" prologue as its episode 1, while
/// releases and IMDb number the regular episodes from 1. When Anime-Lists
/// maps exactly that first AniDB episode to a special and the rest with
/// offset -1, and IMDb's season has exactly the remaining episode count, a
/// bare fansub number N is the official episode N (AniDB N+1), not AniDB N.
/// An explicit season, a range, or AniDB's own last number are untouched.
fn official_numbering_after_prologue(
    db:&Connection,winner:&mut Value,parsed:&serde_json::Map<String,Value>,
)->Result<(),String>{
    let parsed_episodes=numbers(parsed.get("episodes"));
    let [number]=parsed_episodes.as_slice() else{return Ok(())};
    let number=*number;
    if !numbers(parsed.get("seasons")).is_empty()
        || parsed.contains_key("episode_start")
        || winner.get("episode_range").is_some_and(|range|!range.is_null())
        || winner["episode"].as_i64()!=Some(number-1)
        || winner.pointer("/anime/episode").and_then(Value::as_i64)!=Some(number)
    {return Ok(());}
    let (Some(imdb),Some(season),Some(anidb))=(
        winner["imdb_id"].as_str().map(str::to_owned),winner["season"].as_i64(),
        winner.pointer("/anime/anidb_id").and_then(Value::as_str).map(str::to_owned),
    )else{return Ok(())};
    let regular_end:Option<i64>=db.query_row(
        "SELECT regular.end FROM episode_map regular
         JOIN episode_map prologue ON prologue.anidb_id=regular.anidb_id
         WHERE regular.anidb_id=?1 AND regular.anidb_season=1
           AND regular.tvdb_season=?2 AND regular.start=2 AND regular.offset=-1
           AND TRIM(regular.map)='' AND regular.end>2
           AND prologue.anidb_season=1 AND prologue.tvdb_season=0
           AND prologue.map LIKE ';1-%' AND prologue.map NOT LIKE ';1-%;%-%'
           AND (SELECT COUNT(*) FROM episode_map other
                WHERE other.anidb_id=regular.anidb_id AND other.anidb_season=1)=2",
        rusqlite::params![anidb,season],|row|row.get(0))
        .optional().map_err(|e|e.to_string())?;
    let Some(regular_end)=regular_end else{return Ok(())};
    let official_count=regular_end-1;
    if number>official_count
        || season_episode_end(db,&imdb,season)?!=Some(official_count)
    {return Ok(());}
    let Some(asid)=episode_crosswalk::canonical_asid(&anidb,number+1) else{return Ok(())};
    winner["episode"]=json!(number);
    winner["sid"]=json!(format!("{imdb}:{season}:{number}"));
    winner["anime"]=json!({"anidb_id":anidb,"episode":number+1,"asid":asid});
    winner["coordinate_fallback"]=json!("official_numbering_after_prologue");
    Ok(())
}

/// Reconcile a selected IMDb series with explicit single-release coordinates.
/// Only verified crosswalks may reinterpret an SxxEyy token. A missing
/// AniDB mapping must not delete an otherwise parsed IMDb episode.
/// Preserve unresolved packs for their dedicated range verifier.
fn reconcile_series_coordinates(
    db:&Connection,winner:&mut Value,parsed:&serde_json::Map<String,Value>,
)->Result<(),String>{
    if winner["category"]!="series"
        || !winner["imdb_id"].as_str().is_some_and(metadata_import::valid_imdb_id)
        || winner.get("episode_range").is_some()
        || parsed.get("half_episode").and_then(Value::as_bool)==Some(true){
        return Ok(());
    }
    let parsed_seasons=numbers(parsed.get("seasons"));
    let parsed_episodes=numbers(parsed.get("episodes"));
    if parsed_seasons.len()>1 || parsed_episodes.len()>1 {return Ok(());}
    let parsed_season=parsed_seasons.first().copied()
        .filter(|s|*s>=0 && valid_sid_coordinate(*s,0));
    let parsed_episode=parsed_episodes.first().copied()
        .filter(|e|*e>0 && valid_sid_coordinate(1,*e));
    let mut season=winner["season"].as_i64();
    let mut episode=winner["episode"].as_i64();
    // An open-ended anime part offset is not proof of an IMDb episode
    // beyond that season's imported maximum. Check the final pair here
    // so neither the anime-priority nor IMDb fallback lane can publish it.
    // Unknown bounds retain verified crosswalk coverage. A contradiction
    // with known bounds must not fall through to a guessed raw coordinate.
    if winner.pointer("/anime/asid").and_then(Value::as_str).is_some()
        && let (Some(mapped_season),Some(mapped_episode))=(season,episode)
        && mapped_season>0
        && (season_episode_end(db,winner["imdb_id"].as_str().unwrap_or_default(),mapped_season)?
            .is_some_and(|last|mapped_episode>last)
            || crosswalk_season_contradicts_imdb(db,winner["imdb_id"].as_str().unwrap_or_default(),
                winner.pointer("/anime/anidb_id").and_then(Value::as_str).unwrap_or_default(),
                mapped_season)?)
    {
        // The AniDB identity and absolute episode may still be valid even
        // when its open-ended crosswalk falsely assigns S01E145. Repair
        // *only* when IMDb supplies complete preceding-season bounds, the
        // source is an exact fansub release and the original ASID matches
        // the parsed absolute coordinate. Never publish that stale ASID
        // as proof of the newly inferred IMDb coordinate.
        // IMDb can list a later cour of one TVDB season as its own next
        // season. When this season ends exactly where the cour begins, the
        // same broadcast episode is the cour-local episode of the next IMDb
        // season, and the verified ASID is unchanged.
        let imdb_id=winner["imdb_id"].as_str().unwrap_or_default().to_owned();
        if let (Some(mapped_season),Some(mapped_episode),Some(anidb),Some(local))=(
                season,episode,
                winner.pointer("/anime/anidb_id").and_then(Value::as_str),
                winner.pointer("/anime/episode").and_then(Value::as_i64))
            && let Some(offset)=anime_title::imdb_split_offset(
                db,&imdb_id,anidb,mapped_season).ok().flatten()
            && mapped_episode-offset==local
            && season_episode_end(db,&imdb_id,mapped_season+1)?
                .is_some_and(|last|local<=last)
        {
            winner["season"]=json!(mapped_season+1);
            winner["episode"]=json!(local);
            winner["sid"]=json!(format!("{imdb_id}:{}:{local}",mapped_season+1));
            winner["coordinate_fallback"]=json!("imdb_season_split_at_cour");
            return Ok(());
        }
        let raw_absolute=parsed.get("anime_episode_candidate")
            .and_then(|candidate|candidate["episode"].as_i64());
        let identified=winner.pointer("/anime/anidb_id").and_then(Value::as_str);
        let expected_asid=identified.and_then(|id|raw_absolute
            .and_then(|absolute|episode_crosswalk::canonical_asid(id,absolute)));
        let original_asid=winner.pointer("/anime/asid").and_then(Value::as_str);
        let repaired=if expected_asid.as_deref()==original_asid
            && identified==winner["anime_ids"]["anidb"].as_str(){
            let mut unverified=winner.clone();
            unverified["season"]=Value::Null;
            unverified["episode"]=Value::Null;
            unverified["anime"]=Value::Null;
            verified_unmapped_anime_absolute_episode(
                db,&unverified,parsed,true)?
        }else{None};
        if let Some((fixed_season,fixed_episode))=repaired {
            season=Some(fixed_season);
            episode=Some(fixed_episode);
            winner["season"]=json!(fixed_season);
            winner["episode"]=json!(fixed_episode);
            winner["anime"]=Value::Null;
            winner["coordinate_fallback"]=json!("repaired_imdb_absolute_anime");
            winner["rejected_crosswalk"]=json!("anime_crosswalk_exceeds_known_imdb_bounds");
        }else{
            // The IMDb coordinate is rejected, but an ASID that is the
            // identified AniDB work plus the parsed absolute episode
            // (Detective Conan - 1108 → 266:1108) does not depend on the
            // crosswalk. Keep it; drop any other ASID.
            let imdb_id=winner["imdb_id"].as_str().unwrap_or_default();
            let sole_anidb:i64=db.query_row(
                "SELECT COUNT(DISTINCT anidb_id) FROM anime_series_map WHERE imdb_id=?1",
                [imdb_id],|row|row.get(0)).unwrap_or(0);
            let independent=expected_asid.is_some()
                && expected_asid.as_deref()==original_asid
                && (winner["anidb_independent_verified"]==true
                    || (sole_anidb==1
                        && identified==winner["anime_ids"]["anidb"].as_str()));
            winner["season"]=Value::Null;
            winner["episode"]=Value::Null;
            winner["season_end"]=Value::Null;
            winner["episode_end"]=Value::Null;
            if !independent{winner["anime"]=Value::Null;}
            winner["sid"]=winner["imdb_id"].clone();
            winner["coordinate_fallback_blocked"]=json!("anime_crosswalk_exceeds_known_imdb_bounds");
            return Ok(());
        }
    }
    // An ASID alone can be independently verified without proving the
    // matching IMDb season/episode. Only an established pair is final.
    let verified_crosswalk=winner.pointer("/anime/asid")
        .and_then(Value::as_str).is_some()
        && season.is_some() && episode.is_some();
    if !verified_crosswalk {
        if let Some((mapped_season,mapped_episode))=
            verified_unmapped_anime_absolute_episode(db,winner,parsed,false)?{
            season=Some(mapped_season);
            episode=Some(mapped_episode);
            winner["coordinate_fallback"]=json!("complete_imdb_absolute_anime");
        }
        if season.is_none(){season=parsed_season;}
        let imdb=winner["imdb_id"].as_str().unwrap_or_default().to_owned();
        let has_anime=winner["anime_ids"].as_object()
            .is_some_and(|ids|!ids.is_empty());
        // Do not restore an episode which known IMDb bounds reject. When
        // IMDb episode coverage is missing but a bounded anime season
        // crosswalk exists, its failure also blocks guessed IMDb episodes.
        // An anime with NO season crosswalk still retains explicit S17E01.
        if episode.is_none() {
            if let Some(ep)=parsed_episode {
                let target_season=season.unwrap_or(1);
                let imdb_max=season_episode_end(db,&imdb,target_season)?;
                let unverified_anime=if has_anime && imdb_max.is_none() {
                    db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM anime_series_map l
                           JOIN episode_map m ON m.anidb_id=l.anidb_id
                           WHERE l.imdb_id=?1 AND m.tvdb_season=?2)",
                        rusqlite::params![&imdb,target_season],
                        |row|row.get::<_,i64>(0)
                    ).map_err(|e|e.to_string())? != 0
                }else{false};
                // "S01E147" on a long-running anime is an absolute number
                // under a nominal season-one marker. Accept it only when
                // the absolute crosswalk maps exactly that AniDB episode to
                // a later season that IMDb knows to contain it.
                let absolute_marker=if parsed_season==Some(1)
                    && imdb_max.is_some_and(|end|ep>end) {
                    episode_crosswalk::resolve_absolute(db,&imdb,ep)
                        .map_err(|e|e.to_string())?
                        .filter(|mapped|mapped.anidb_episode==ep && mapped.season>1)
                }else{None};
                let absolute_marker=match absolute_marker {
                    Some(mapped) if season_episode_end(db,&imdb,mapped.season)?
                        .is_some_and(|end|mapped.episode<=end)=>Some(mapped),
                    _=>None,
                };
                if let Some(mapped)=absolute_marker {
                    season=Some(mapped.season);
                    episode=Some(mapped.episode);
                    winner["anime"]=json!({"anidb_id":mapped.anidb_id,
                        "episode":mapped.anidb_episode,
                        "asid":format!("{}:{}",mapped.anidb_id,mapped.anidb_episode)});
                    winner["coordinate_fallback"]=json!("season_one_marker_is_absolute");
                }else if imdb_max.is_some_and(|end|ep>end)||unverified_anime {
                    winner["coordinate_fallback_blocked"]=json!(
                        "episode_exceeds_known_bounds_or_unresolved_anime_crosswalk");
                }else{
                    episode=Some(ep);
                }
            }
        }
        if season.is_none(){
            if let Some(ep)=episode{
                let max=season_episode_end(db,&imdb,1)?;
                if max.is_some_and(|end|ep>end)
                    ||(has_anime && max.is_none() && ep>150){
                    episode=None;
                    winner["coordinate_fallback_blocked"]=json!("unknown_absolute_episode_season");
                }else{
                    season=Some(1);
                    winner["coordinate_fallback"]=json!("season_one_from_episode");
                }
            }
        }
    }
    winner["season"]=json!(season);
    winner["episode"]=json!(episode);
    let imdb=winner["imdb_id"].as_str().unwrap_or_default();
    winner["sid"]=match (season,episode){
        (Some(s),Some(e))=>json!(format!("{imdb}:{s}:{e}")),
        _=>json!(imdb),
    };
    // Preserve a verified ASID when an IMDb-season crosswalk exists for the
    // final coordinate, whichever lane selected it ("Title S01E21" in a
    // later cour as much as a repaired absolute number). Otherwise keep
    // ASID null; never invent an AniDB episode number.
    let inferred=matches!(winner["coordinate_fallback"].as_str(),
        Some("complete_imdb_absolute_anime"|"repaired_imdb_absolute_anime"));
    let enriched=enrich_selected_anime_episode(db,winner);
    // The enrichment is additive for an already final coordinate: missing
    // crosswalk tables mean "no ASID", not a failed match.
    if inferred {enriched?;}
    Ok(())
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
/// A TVDB season from the Anime-Lists crosswalk is an IMDb coordinate only
/// when IMDb's own season structure can contain it. It is contradicted when
/// - IMDb's known season count is lower (One Piece: IMDb S1 only, TVDB S22);
/// - an absolutely numbered AniDB work (Detective Conan 266: TVDB S32 =
///   1109-1147) has IMDb seasons that are systematically shorter: this and
///   the preceding IMDb season both list fewer episodes than TVDB's
///   (S31: 16 vs 41, S32: 15 vs 39), so IMDb groups episodes differently;
/// - IMDb already lists more episodes before the season than AniDB's
///   absolute numbering leaves room for (Hunter x Hunter 2011: IMDb S1 has
///   148 episodes, TVDB S2 starts at 59).
/// IMDb listing fewer episodes is not a contradiction: listings are often
/// incomplete. Missing data is never a contradiction.
fn crosswalk_season_contradicts_imdb(db:&Connection,imdb:&str,anidb:&str,season:i64)
    ->Result<bool,String>{
    if !(1..=9999).contains(&season){return Ok(false);}
    let tolerate=|result:rusqlite::Result<Option<i64>>|match result {
        Ok(value)=>Ok(value),
        Err(rusqlite::Error::QueryReturnedNoRows|rusqlite::Error::SqliteFailure(..))=>Ok(None),
        Err(error)=>Err(error.to_string()),
    };
    let imdb_seasons=tolerate(db.query_row(
        "SELECT MAX(max_season) FROM seasons WHERE imdb_id=?1",[imdb],|row|row.get(0)))?;
    if imdb_seasons.is_some_and(|known|known>0&&known<season){return Ok(true);}
    if anidb.is_empty()||season<2{return Ok(false);}
    let row=db.prepare("SELECT start,offset FROM episode_map
            WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season=?2")
        .and_then(|mut stmt|stmt.query_map(rusqlite::params![anidb,season],
            |row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>());
    let rows=match row {
        Ok(rows)=>rows,
        Err(rusqlite::Error::SqliteFailure(..))=>return Ok(false),
        Err(error)=>return Err(error.to_string()),
    };
    // Exactly one absolute → season-local row: offset = -(start-1).
    let [(start,offset)]=rows.as_slice() else{return Ok(false)};
    if *start<=1||*offset!=1-*start{return Ok(false);}
    // Systematically shorter IMDb seasons: both this and the preceding
    // season list fewer episodes than their finite TVDB absolute ranges.
    let shorter=|tv_season:i64|->Result<bool,String>{
        let tvdb=tolerate(db.query_row(
            "SELECT CASE WHEN COUNT(*)=1 AND MIN(end)>0 THEN MIN(end)-MIN(start)+1 END
                FROM episode_map WHERE anidb_id=?1 AND anidb_season>0 AND tvdb_season=?2",
            rusqlite::params![anidb,tv_season],|row|row.get(0)))?;
        let listed=tolerate(db.query_row(
            "SELECT max_episode FROM season_episodes WHERE imdb_id=?1 AND season=?2",
            rusqlite::params![imdb,tv_season],|row|row.get(0)))?;
        Ok(matches!((tvdb,listed),(Some(tvdb),Some(listed)) if listed<tvdb))
    };
    if shorter(season)? && shorter(season-1)? {return Ok(true);}
    let before=tolerate(db.query_row(
        "SELECT CASE WHEN COUNT(*)=?2-1 THEN SUM(max_episode) END FROM season_episodes
            WHERE imdb_id=?1 AND season BETWEEN 1 AND ?2-1",
        rusqlite::params![imdb,season],|row|row.get(0)))?;
    Ok(before.is_some_and(|listed|listed>*start-1))
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
        if key.is_empty()||(!strict_match_mode()&&key.len()<4){continue;}
        let aliases=if strict_match_mode(){
            tantivy_search::anime_aliases_exact(db,name)?
        }else{tantivy_search::anime_aliases(db,&key,4)?};
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

/// A collection window belongs to a parent directory and is not a single
/// film production year. Use the closest matching ancestor for a yearless
/// movie release while leaving its actual production year unspecified.
fn movie_collection_year_range(parsed:&serde_json::Map<String,Value>)->Option<(i32,i32)>{
    let layers=parsed.get("path_layers")?.as_array()?;
    layers.iter().take(layers.len().saturating_sub(1)).rev()
        .filter_map(|layer|layer.get("raw").and_then(Value::as_str))
        .find_map(matchlab_parser::collection_year_span)
}

/// Use identical production gates for both a filename and each parent directory.
/// A candidate rejected later must not prevent the search from climbing.
#[derive(Clone,Copy)]
struct ImdbIdentityGate<'a>{
    policy:MatchPolicy,
    strict:bool,
    year:Option<i32>,
    has_season:bool,
    has_quality:bool,
    collection_year_range:Option<(i32,i32)>,
    verified_bluray_stream:bool,
    season_range_end:Option<i64>,
    verified_anime_range:Option<&'a str>,
    verified_absolute_episode:Option<&'a str>,
}
// The AniDB episode resolver can promote an IMDb identity independently of
// imdb_matcher::search_validated. It must obey the SAME strict production-year
// rule, not just the ranked IMDb shortlist. The narrow primary-key lookup is
// necessary ONLY when a requested year exists and the resolver produced a
// separate anime identity; yearless and non-strict requests incur no SQL.
fn strict_anime_imdb_year_eligible(
    db:&Connection,anime:&Value,year:Option<i32>,strict:bool,
)->bool{
    if !strict {return true;}
    let Some(year)=year else{return true;};
    let Some(imdb)=anime.get("imdb_id").and_then(Value::as_str)else{
        return false;
    };
    db.query_row("SELECT EXISTS(SELECT 1 FROM titles WHERE imdb_id=?1 AND year=?2)",
        rusqlite::params![imdb,year],|row|row.get::<_,bool>(0))
        .unwrap_or(false)
}

fn filter_request_imdb_candidates(
    db:&Connection,hits:Vec<imdb_matcher::Match>,gate:ImdbIdentityGate<'_>,
)->Vec<imdb_matcher::Match>{
    // In strict mode, a yearless complete film title in a real Blu-ray
    // STREAM path is independent identity evidence. Require exactly ONE
    // IMDb identity before treating a numbered transport stream as a release
    // signal, so remakes sharing an exact title remain ambiguous without
    // year metadata. Ordinary bare movie names keep their old public gate.
    let unique_bluray_identity=gate.strict && gate.year.is_none()
        &&!gate.has_season && gate.verified_bluray_stream
        &&hits.iter().map(|row|row.imdb_id.as_str())
            .collect::<std::collections::HashSet<_>>().len()==1;
    let mut filtered=hits.into_iter().filter(|candidate|{
        // A box-set year window is contextual evidence for a yearless
        // individual film, not the production year of every film in the pack.
        // Strict has already verified the full title/alias before this gate.
        let in_collection_range=gate.strict && gate.year.is_none()
            && gate.collection_year_range.is_some_and(|(start,end)|
                (start..=end).contains(&candidate.year));
        // Apply the same hard constraint after any supplementary candidate
        // promotion, including verified anime episode recovery. This also
        // covers dashboard diagnostics, not only the public API gate.
        imdb_matcher::year_eligible(gate.strict,gate.year,candidate.year)
            && (gate.collection_year_range.is_none() ||
                !matches!(candidate.media_type.as_str(),"tvSeries"|"tvMiniSeries"))
            && (!matches!(gate.policy,MatchPolicy::PublicApi)
            ||year_signal_eligible(
                gate.year,candidate.year,gate.has_season,
                !candidate.anime_ids.is_empty()
                    ||gate.verified_anime_range==Some(candidate.imdb_id.as_str())
                    ||gate.verified_absolute_episode==Some(candidate.imdb_id.as_str()),
                gate.has_quality,
            ) || in_collection_range || unique_bluray_identity)
    }).collect::<Vec<_>>();
    // An explicit season range is stricter than the season-1 tolerance
    // used for provisional title discovery. No range -> no SQL at all.
    let Some(end)=gate.season_range_end else{return filtered;};
    // Keep the low-overhead scalar path for the usual tiny shortlist.
    if filtered.len()<=4 {
        filtered.retain(|candidate|
            db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
                [&candidate.imdb_id],|row|row.get::<_,i64>(0))
                .is_ok_and(|max|max>=end));
        return filtered;
    }
    // Only IDs that passed the public year gate need season validation.
    // Preserve input order and duplicate IDs while replacing N lookups
    // with bounded relational reads. Missing/corrupt bounds still reject.
    let ids=filtered.iter().map(|candidate|candidate.imdb_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter().collect::<Vec<_>>();
    let mut allowed=std::collections::HashSet::<String>::new();
    for batch in ids.chunks(200){
        let markers=vec!["?";batch.len()].join(",");
        let sql=format!("SELECT imdb_id,max_season FROM seasons
            WHERE imdb_id IN ({markers})");
        if let Ok(mut stmt)=db.prepare(&sql){
            if let Ok(rows)=stmt.query_map(
                rusqlite::params_from_iter(batch.iter().copied()),|row|
                Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?))){
                for row in rows.flatten(){
                    if row.1>=end {allowed.insert(row.0);}
                }
            }
        }
    }
    filtered.retain(|candidate|allowed.contains(&candidate.imdb_id));
    filtered
}

/// Match independently parsed levels leaf-first. A level wins only after
/// public identity/season eligibility; otherwise fall back to the parent.
fn bottom_up_imdb_path_match(
    db:&Connection,parsed:&serde_json::Map<String,Value>,
    year:Option<i32>,series:bool,season:i64,has_episode:bool,
    check_episode_bounds:bool,
    eligible:impl Fn(Vec<imdb_matcher::Match>)->Vec<imdb_matcher::Match>,
)->Result<Option<(String,Vec<imdb_matcher::Match>)>,String>{
    let Some(layers)=parsed.get("path_layers").and_then(Value::as_array)else{
        return Ok(None);
    };
    for (depth,layer) in layers.iter().enumerate().rev(){
        let Some(raw)=layer.get("raw").and_then(Value::as_str)else{continue};
        let Some(fields)=layer.get("parsed").and_then(Value::as_object)else{
            continue
        };
        let Some(title)=fields.get("title").and_then(Value::as_str)else{
            continue
        };
        let trimmed=title.trim();
        let lower=trimmed.to_ascii_lowercase();
        let numeric=!lower.is_empty()&&lower.bytes().all(|b|b.is_ascii_digit());
        // Sxx, Exx, extensions and structural season directories cannot
        // supply IMDb identity. A real numeric series (e.g. "24") is
        // eligible only when an exact indexed IMDb alias independently proves it.
        let coordinate_only=matchlab_parser::generic_path_title(trimmed)
            ||(numeric&&depth>0
                &&(lower.len()==1||lower.parse::<i64>().ok()==Some(season)));
        if coordinate_only{continue;}
        // The exact-first phase must fall back to full lexical discovery
        // if these request-specific gates reject all exact candidates.
        let mut found=imdb_matcher::search_validated(
            db,trimmed,year,series,season,raw,has_episode,20,|mut hits|{
                if numeric{
                    let exact=imdb_matcher::clean_title(trimmed);
                    hits.retain(|hit|imdb_matcher::clean_title(&hit.matched_title)==exact);
                }
                eligible(hits)
            }).map_err(|e|e.to_string())?;
        // The later episode-bound gate rejects unsupported candidates only
        // if more than two identities remain. Apply that same condition
        // before accepting a directory, otherwise all candidates can vanish
        // after leaf selection without ever trying a valid parent.
        if found.len()>2 && check_episode_bounds && season>0
            && let Some(episode)=numbers(parsed.get("episodes")).first().copied()
        {
            let absolute_hint=parsed.get("absolute_episode_candidate")
                .and_then(Value::as_i64);
            found.retain(|candidate|
                candidate_has_episode(db,&candidate.imdb_id,season,episode)
                    .unwrap_or(false)
                ||verified_chinese_absolute_fallback(
                    db,&candidate.imdb_id,season,episode,absolute_hint)
                    .ok().flatten().is_some()
                ||imdb_matcher::has_longer_title_evidence_in_release(
                    trimmed,raw,&candidate.matched_title));
        }
        if !found.is_empty(){
            return Ok(Some((trimmed.to_owned(),found)));
        }
    }
    Ok(None)
}

/// Verify an unmarked anime interval using indexed exact AniDB aliases.
/// Film and OVA titles can share the long-running TV series' alias. Preserve
/// partial-range diagnostics for an unambiguous single IMDb association; for
/// collisions, require exactly one IMDb identity with an entirely mapped span.
fn verified_unmarked_anime_range_imdb(
    db:&Connection,title:&str,start:i64,end:i64,
)->Result<Option<String>,String>{
    let key=imdb_matcher::clean_title(title);
    let aliases=if strict_match_mode(){
        tantivy_search::anime_aliases_exact(db,title)?
    }else{tantivy_search::anime_aliases(db,&key,6)?};
    let ids=aliases.into_iter().filter(|alias|alias.clean_title==key)
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
    year:Option<i32>,episode:i64,numbered_title_is_alias:bool,
)->Result<Option<String>,String>{
    let key=imdb_matcher::clean_title(candidate_title);
    if key.is_empty() ||(!strict_match_mode()&&key.len()<4)
        ||!(1..=99_999).contains(&episode){return Ok(None);}
    // A non-Latin title that keeps only digits has lost its identity words.
    if imdb_matcher::numeric_key_lost_title_words(&key,candidate_title){return Ok(None);}
    let aliases=if strict_match_mode(){
        tantivy_search::anime_aliases_exact(db,candidate_title)?
    }else{tantivy_search::anime_aliases(db,&key,4)?};
    let exact_ids=aliases.into_iter().filter(|alias|alias.clean_title==key)
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
    // Several AniDB works of one IMDb series can each claim an absolute
    // number ("Dragon Ball Kai" 2009 and 2014). The exact alias then names
    // the part, exactly as it does for the dashed "Title - 07" form.
    let mapped=match episode_crosswalk::resolve_absolute(db,&winner.imdb_id,episode)
        .map_err(|e|e.to_string())? {
        Some(mapped)=>mapped,
        // "Digimon Adventure 02" is itself a work: its number is not an
        // episode of "Digimon Adventure", so the part fallback stays off.
        None if numbered_title_is_alias=>return Ok(None),
        // A bounded AniDB work that independently proves this episode owns
        // the ambiguous title (the 2020 "Digimon Adventure:" remake).
        None if anidb_matcher::match_absolute(db,candidate_title,episode)?.is_some()
            =>return Ok(None),
        None=>match anime_title::resolve_episode_from_exact_part_title(
            db,&winner.imdb_id,candidate_title,episode).map_err(|e|e.to_string())? {
            Some(mapped)=>mapped,
            None=>return Ok(None),
        },
    };
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

    // An unadorned named anime part may end with a spaced hyphen:
    // "Sword Art Online - Alicization". The general release parser can
    // mistake the final word for a torrent release group and truncate the
    // title to "Sword Art Online". Recover it ONLY when the raw input is
    // exactly the two parsed components, a unique AniDB alias confirms
    // the joined name, and its crosswalk proves an IMDb season. Ordinary
    // release groups (e.g. -GROUP) keep their original parser semantics.
    if req.series!=Some(false)
        && parsed_value.get("seasons").and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        && parsed_value.get("episodes").and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        && parsed_value.get("episode_start").is_none()
        && parsed_value.get("year").is_none()
        && let (Some(base),Some(group))=(
            parsed_value.get("title").and_then(Value::as_str),
            parsed_value.get("group").and_then(Value::as_str))
    {
        let joined=format!("{base} - {group}");
        if req.title.trim().eq_ignore_ascii_case(&joined)
            && let Some(imdb)=exact_anime_identity(&db,&joined)?
            && anime_title::season_from_title(&db,&imdb,&joined)
                .map_err(|e|e.to_string())?.is_some()
            && let Some(parsed)=parsed_value.as_object_mut()
        {
            parsed.insert("title".into(),json!(joined));
            parsed.remove("group");
            parsed.insert("anime_title_verified".into(),json!(true));
            parsed.insert("anime_part_title_recovered".into(),
                json!("verified_misclassified_release_group"));
        }
    }

    // "<first part> Part 2 - 01 ~ 12": a batch of a numbered part. Both
    // endpoints must resolve inside the same AniDB work and IMDb season;
    // the batch then becomes an ordinary season-relative range of the
    // first part's title, verified by the existing season crosswalk.
    if req.series!=Some(false)
        && let Some(hint)=parsed_value.get("anime_episode_range_candidate")
        && let (Some(name),Some(start),Some(end))=(
            hint.get("title").and_then(Value::as_str),
            hint.get("start").and_then(Value::as_i64),
            hint.get("end").and_then(Value::as_i64))
        && end>start
        && let Some((base,part))=numbered_part_title(name)
        && let Some(imdb)=exact_anime_identity(&db,base)?
        && let Some(first)=anime_title::resolve_numbered_part(
            &db,&imdb,base,part,start).map_err(|e|e.to_string())?
        && let Some(last)=anime_title::resolve_numbered_part(
            &db,&imdb,base,part,end).map_err(|e|e.to_string())?
        && first.season==last.season && first.anidb_id==last.anidb_id
        && last.episode-first.episode==end-start
    {
        let base=base.to_owned();
        if let Some(parsed)=parsed_value.as_object_mut(){
            parsed.remove("anime_episode_range_candidate");
            parsed.insert("title".into(),json!(base));
            parsed.insert("seasons".into(),json!([first.season]));
            parsed.insert("episodes".into(),json!([first.episode]));
            parsed.insert("episode_start".into(),json!(first.episode));
            parsed.insert("episode_end".into(),json!(last.episode));
            parsed.insert("episode_count".into(),json!(end-start+1));
            parsed.insert("episode_range_numbering".into(),json!("season"));
            parsed.insert("anime_title_verified".into(),json!(true));
            parsed.insert("anime_numbered_part".into(),json!(part));
        }
    }
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
            let season_hint=hint.and_then(|v|v.get("season_hint"))
                .and_then(Value::as_i64).filter(|s|(1..=120).contains(s));
            // The ordinary unmarked anime batch has no season: the
            // AniDB absolute crosswalk remains authoritative. A literal
            // S2 next to the batch is different evidence. An absolute
            // conversion is valid only if its ENTIRE mapped interval
            // belongs to that explicitly advertised season.
            verified_anime_range_imdb=verified_unmarked_anime_range_imdb(
                &db,&candidate_title,start,end)?;
            let absolute_agrees_with_season=if let (Some(season),Some(imdb))=(
                season_hint,verified_anime_range_imdb.as_deref())
            {
                let mapped=mapped_absolute_range(&db,imdb,start,end)?;
                mapped["complete"]==true
                    && mapped["start_season"].as_i64()==Some(season)
                    && mapped["end_season"].as_i64()==Some(season)
            }else{false};
            // When absolute 01..24 does not agree with the explicitly
            // advertised S2, first test whether S2/P2 is LOCAL to a
            // uniquely identified AniDB work. Only a completely mapped
            // part-local interval can become canonical IMDb endpoints.
            let mut local_promoted=false;
            let mut named_part_unresolved:Option<String>=None;
            if !absolute_agrees_with_season {
                let local_hint=parsed_value.get("anime_local_part_candidate")
                    .and_then(|h|Some((
                        h.get("title")?.as_str()?.to_owned(),
                        h.get("marker")?.as_str()?.to_owned(),
                        h.get("local")?.as_i64()?
                    )));
                if let Some((name,marker,local))=local_hint
                    && (season_hint==Some(local)
                        || (marker=="P" && season_hint.is_none()))
                    && let Some(imdb)=exact_anime_identity(&db,&name)?
                {
                    // A proven named part with an INCOMPLETE local range
                    // must never fall back to an unrelated global S02.
                    // S3==base is already absolute and is exempt.
                    if let Some(base)=anime_title::named_part_base(
                        &db,&imdb,&name).map_err(|e|e.to_string())?
                        && !(marker=="S" && local==base)
                    {
                        named_part_unresolved=Some(name.clone());
                    }
                    if let Some(first)=anime_title::resolve_local_part(
                        &db,&imdb,&name,&marker,local,start)
                            .map_err(|e|e.to_string())?
                    {
                    let effective=if marker=="S" && local==first.base_season {
                        1
                    }else{local};
                    let checked=mapped_season_episode_range(
                        &db,&imdb,&name,effective,start,end,false,true)?;
                    if checked["complete"]==true
                        && checked["start_season"].as_i64()
                            ==Some(first.mapped.season)
                        && checked["end_season"].as_i64()
                            ==Some(first.mapped.season)
                        && let (Some(actual_start),Some(actual_end))=(
                            checked["start_episode"].as_i64(),
                            checked["end_episode"].as_i64())
                    {
                        if let Some(parsed)=parsed_value.as_object_mut(){
                            parsed.insert("title".into(),json!(name));
                            parsed.insert("seasons".into(),
                                json!([first.mapped.season]));
                            parsed.insert("episodes".into(),json!([actual_start]));
                            parsed.insert("episode_start".into(),json!(actual_start));
                            parsed.insert("episode_end".into(),json!(actual_end));
                            parsed.insert("episode_count".into(),json!(end-start+1));
                            parsed.insert("episode_range_numbering".into(),
                                json!("season"));
                            parsed.insert("anime_local_part_verified".into(),json!({
                                "imdb_id":imdb,"base_season":first.base_season,
                                "local_marker":marker,"local_number":local,
                                "resolved_season":first.mapped.season,
                                "mapping_source":"verified_anidb_part_crosswalk"
                            }));
                            parsed.insert("anime_title_verified".into(),
                                json!(true));
                        }
                        verified_anime_range_imdb=None;
                        local_promoted=true;
                    }
                    }
                }
            }
            if !local_promoted {
                if let Some(name)=named_part_unresolved {
                    // Preserve the exact anime identity but no unverified
                    // S02 or absolute episode endpoints. The candidate
                    // remains visible for diagnostics and future imports.
                    verified_anime_range_imdb=None;
                    if let Some(parsed)=parsed_value.as_object_mut(){
                        parsed.insert("title".into(),json!(name));
                        parsed.insert("seasons".into(),json!([]));
                        parsed.insert("episodes".into(),json!([]));
                        parsed.insert("anime_local_part_range_unresolved".into(),
                            json!(true));
                    }
                }else if let Some(season)=season_hint
                    && !absolute_agrees_with_season {

                // A verified absolute anime episode 01..24 could belong
                // to S03, but "S2 (01-24)" must never publish S03. Treat
                // these as a season-relative batch, then let the existing
                // season crosswalk and IMDb episode bounds prove endpoints.
                // Preserve the candidate in /parse for diagnostics; never
                // manufacture ASIDs or a different season as a fallback.
                verified_anime_range_imdb=None;
                if let Some(parsed)=parsed_value.as_object_mut(){
                    parsed.insert("title".into(),json!(candidate_title));
                    parsed.insert("seasons".into(),json!([season]));
                    parsed.insert("episodes".into(),json!([start]));
                    parsed.insert("episode_start".into(),json!(start));
                    parsed.insert("episode_end".into(),json!(end));
                    parsed.insert("episode_count".into(),json!(end-start+1));
                    parsed.insert("episode_range_numbering".into(),json!("season"));
                }
                }else{
                    if season_hint.is_none() && verified_anime_range_imdb.is_none(){
                    verified_anidb_range=anidb_matcher::match_absolute_range(
                        &db,&candidate_title,start,end)?;
                }
                if verified_anidb_range.is_some()
                    || verified_anime_range_imdb.is_some() {
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
        }
    }
    // The generic release parser may have interpreted 01-07 as a single
    // episode even if no anime identity was verified. For an untrusted batch
    // candidate, suppress that inference rather than publishing guessed
    // coordinates for unrelated movies or ordinary TV titles.
    if verified_anidb_range.is_none() && verified_anime_range_imdb.is_none()
        && parsed_value.get("episode_start").is_none()
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
            for (suffixed,candidate_title) in [
                (true,format!("{hint_title} {season}")),(false,hint_title.clone())] {
                if let Some(imdb)=exact_anime_identity(&db,&candidate_title)? {
                    // A numbered alias proves the season only when its own
                    // AniDB work is mapped there. "Title Part 2" can be the
                    // second cour of season one, not season two.
                    if suffixed && !anime_title::numbered_alias_names_season(
                        &db,&imdb,&candidate_title,season)
                        .map_err(|e|e.to_string())? {continue;}
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
    // A video file named only "02.mkv" may inherit a named anime part
    // from its two parents. Promote its unverified parser candidate only
    // when the exact AniDB identity AND named-part season crosswalk agree.
    // Never use whole-series absolute episode 2 as a fallback here: that
    // can silently convert Alicization's local episode 2 to season-one E02.
    let mut verified_path_part:Option<Value>=None;
    if req.series!=Some(false)
        && let Some(hint)=parsed_value.get("anime_episode_candidate")
        && hint.get("path_part").and_then(Value::as_bool)==Some(true)
        && let (Some(name),Some(episode))=(
            hint.get("title").and_then(Value::as_str),
            hint.get("episode").and_then(Value::as_i64))
    {
        let name=name.to_owned();
        if let Some(imdb)=exact_anime_identity(&db,&name)?
            && let Some(mapped)=anime_title::resolve_episode_from_exact_part_title(
                &db,&imdb,&name,episode).map_err(|e|e.to_string())?
        {
            verified_path_part=Some(json!({
                "imdb_id":mapped.imdb_id,"season":mapped.season,
                "episode":mapped.episode,"anidb_id":mapped.anidb_id,
                "anidb_episode":mapped.anidb_episode
            }));
            if let Some(parsed)=parsed_value.as_object_mut(){
                parsed.insert("title".into(),json!(name));
                parsed.insert("episodes".into(),json!([episode]));
                parsed.insert("seasons".into(),json!([]));
                parsed.insert("anime_title_verified".into(),json!(true));
            }
        }
    }
    if let Some((full,short))=parsed_value.get("title").and_then(Value::as_str)
        .zip(parsed_value.pointer("/anime_episode_candidate/title").and_then(Value::as_str))
        && full!=short
        && parsed_value.get("episodes").and_then(Value::as_array).is_none_or(Vec::is_empty)
        && numbered_title_is_work(&db,full)?
        && let Some(parsed)=parsed_value.as_object_mut()
    {
        parsed.remove("anime_episode_candidate");
    }
    // "<first part> Part 2 - 05": the numbered part is a separate AniDB work
    // inside the same IMDb season. Resolve it from the verified base title
    // before the generic paths read "Part 2" as a season or drop it.
    if verified_path_part.is_none() && req.series!=Some(false)
        && parsed_value.get("anime_episode_range_candidate").is_none()
        && parsed_value.get("episode_start").is_none()
        && let Some(hint)=parsed_value.get("anime_episode_candidate")
        && let (Some(name),Some(episode))=(
            hint.get("title").and_then(Value::as_str),
            hint.get("episode").and_then(Value::as_i64))
        && let Some((base,part))=numbered_part_title(name)
        && let Some(imdb)=exact_anime_identity(&db,base)?
        && let Some(mapped)=anime_title::resolve_numbered_part(
            &db,&imdb,base,part,episode).map_err(|e|e.to_string())?
    {
        let name=name.to_owned();
        verified_path_part=Some(json!({
            "imdb_id":mapped.imdb_id,"season":mapped.season,
            "episode":mapped.episode,"anidb_id":mapped.anidb_id,
            "anidb_episode":mapped.anidb_episode
        }));
        if let Some(parsed)=parsed_value.as_object_mut(){
            parsed.insert("title".into(),json!(name));
            parsed.insert("episodes".into(),json!([episode]));
            parsed.insert("seasons".into(),json!([]));
            parsed.insert("anime_title_verified".into(),json!(true));
            parsed.insert("anime_numbered_part".into(),json!(part));
        }
    }
    // A named anime part may interpret S2/P2 *locally*. The old episode
    // crosswalk is authoritative: never promote an arithmetic season guess,
    // and never touch Chinese absolute-episode or explicit range paths.
    // S3 on a part beginning in IMDb S03 is interpreted as absolute S03.
    let mut verified_local_anime_episode:Option<Value>=None;
    if req.series!=Some(false)
        && parsed_value.get("absolute_episode_candidate").is_none()
        && parsed_value.get("anime_episode_range_candidate").is_none()
        && parsed_value.get("episode_start").is_none()
        && let Some(hint)=parsed_value.get("anime_local_part_candidate")
        && let (Some(name),Some(marker),Some(local))=(
            hint.get("title").and_then(Value::as_str),
            hint.get("marker").and_then(Value::as_str),
            hint.get("local").and_then(Value::as_i64))
        && (2..=99).contains(&local)
    {
        let name=name.to_owned();
        let marker=marker.to_owned();
        let original_seasons=parsed_value.get("seasons").cloned();
        let original_episodes=parsed_value.get("episodes").cloned();
        let parsed_ep=original_episodes.as_ref().and_then(Value::as_array)
            .and_then(|a|match a.as_slice(){[one]=>one.as_i64(),_=>None});
        let marked_ep=parsed_value.get("anime_local_part_candidate")
            .and_then(|hint|hint.get("episode")).and_then(Value::as_i64);
        // A bare P2 is a part, not episode 2. P2E03, on the other hand,
        // is an explicit part-local episode, verified by the same crosswalk.
        let explicit_ep=if marker=="P" {marked_ep}
            else{marked_ep.or(parsed_ep)};
        let probe=explicit_ep.unwrap_or(1);
        if let Some(imdb)=exact_anime_identity(&db,&name)? {
            let base=anime_title::named_part_base(&db,&imdb,&name)
                .map_err(|e|e.to_string())?;
            if let Some(verified)=anime_title::resolve_local_part(
                &db,&imdb,&name,&marker,local,probe)
                    .map_err(|e|e.to_string())?
            {
            let mapped=verified.mapped;
            if let Some(parsed)=parsed_value.as_object_mut(){
                parsed.insert("title".into(),json!(name));
                parsed.insert("seasons".into(),json!([mapped.season]));
                parsed.insert("episodes".into(),if explicit_ep.is_some(){
                    json!([mapped.episode])
                }else{json!([])});
                if marker=="P" && mapped.season==verified.base_season
                    && explicit_ep.is_none() {
                    // A second cour within the SAME IMDb season is not
                    // proof that the torrent contains its whole season.
                    parsed.insert("anime_local_part_is_cour".into(),json!(true));
                }
                parsed.insert("anime_local_part_verified".into(),json!({
                    "imdb_id":imdb,"base_season":verified.base_season,
                    "local_marker":marker,"local_number":local,
                    "original_seasons":original_seasons,
                    "original_episodes":original_episodes,
                    "resolved_season":mapped.season,
                    "mapping_source":"verified_anidb_part_crosswalk"
                }));
                parsed.insert("anime_title_verified".into(),json!(true));
            }
            if explicit_ep.is_some(){
                verified_local_anime_episode=Some(json!({
                    "imdb_id":imdb,"season":mapped.season,
                    "episode":mapped.episode,"anidb_id":mapped.anidb_id,
                    "anidb_episode":mapped.anidb_episode
                }));
            }
            } else if base.is_some_and(|base|
                !(marker=="S" && local==base)) {
                // The named part is proven, but this local coordinate is
                // not. Never silently publish S02 or a whole season pack.
                if let Some(parsed)=parsed_value.as_object_mut(){
                    parsed.insert("title".into(),json!(name));
                    parsed.insert("seasons".into(),json!([]));
                    parsed.insert("episodes".into(),json!([]));
                    parsed.insert("anime_local_part_unresolved".into(),
                        json!({"imdb_id":imdb,"local_marker":marker,
                            "local_number":local,"base_season":base}));
                    parsed.insert("anime_title_verified".into(),json!(true));
                }
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
        && candidate.get("path_part").and_then(Value::as_bool)!=Some(true)
        && let (Some(title),Some(episode)) = (
            candidate.get("title").and_then(Value::as_str),
            candidate.get("episode").and_then(Value::as_i64),
        )
    {
        let candidate_title=title.to_owned();
        // Preserve the existing deterministic IMDb-verification decision
        // regardless of whether the new independent AniDB check succeeds.
        // That retains all legacy ranking and anime-priority safeguards.
        let numbered_title_is_alias=match parsed_value.get("title").and_then(Value::as_str){
            Some(full) if full!=candidate_title=>numbered_title_is_work(&db,full)?,
            _=>false,
        };
        verified_absolute_episode_imdb=verified_unmarked_single_anime_imdb(
            &db,&candidate_title,&req.title,
            req.year.or_else(||parsed_value.get("year")
                .and_then(Value::as_i64).map(|n|n as i32)),episode,
            numbered_title_is_alias)?;
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
    // The ML subsystem supplies a compiled regex candidate, never a parsed
    // field value. Only MatchLab's matcher may reinterpret an episode after
    // independently verifying the current AniDB/TVDB crosswalk.
    let learned_candidate=learned_regex::candidate(&req.title,&parsed_value);
    if req.series!=Some(false)
        && parsed_value.get("episode_end").is_none()
        && parsed_value.get("season_end").is_none()
        && parsed_value.get("anime_episode_range_candidate").is_none()
        && let Some(candidate)=learned_candidate.as_ref()
        && let (Some(candidate_title),Some(absolute))=(
            candidate["title"].as_str(),candidate["episode"].as_i64())
        && (1..=99_999).contains(&absolute)
    {
        let proposed_title=candidate_title.to_owned();
        if let Some(verified_imdb)=verified_unmarked_anime_range_imdb(
            &db,&proposed_title,absolute,absolute
        )?
            && let Some(parsed)=parsed_value.as_object_mut()
        {
            let previous=json!({
                "title":parsed.get("title"),
                "seasons":parsed.get("seasons"),
                "episodes":parsed.get("episodes")
            });
            parsed.insert("title".into(),json!(proposed_title));
            parsed.insert("seasons".into(),json!([]));
            parsed.insert("episodes".into(),json!([absolute]));
            parsed.insert("anime_title_verified".into(),json!(true));
            parsed.insert("learned_regex_override_verified".into(),json!({
                "imdb_id":verified_imdb,
                "absolute_episode":absolute,
                "previous_parser":previous
            }));
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
    // The Rust parser owns all date regexes and ISO normalization. Do not
    // maintain a second date recognizer in the IMDb matcher: its grammar
    // would drift from /parse and from ML-enhanced native date handlers.
    let dated=parsed.get("date").and_then(Value::as_str)
        .map(|date| date.to_owned());
    let airdate_only=dated.is_some() && numbers(parsed.get("seasons")).is_empty()
        && numbers(parsed.get("episodes")).is_empty();
    let title=title_owned.as_str();
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
        // coordinates is a movie candidate, even if an anime alias exists,
        // unless that exact year identifies a complete series title and no
        // same-name film of that period ("Trigun (1998)" vs the 2010 film).
        None if parsed.contains_key("collection_year_range") => false,
        None if let Some(requested)=year =>
            imdb_matcher::year_identifies_series_title(&db,title,requested)
                .map_err(|e|e.to_string())?,
        None => anime_title::season_from_any_exact_title(&db,title)
            .map_err(|e|e.to_string())?.is_some()
            || exact_anime_identity(&db,title)?.is_some(),
    };
    let gate=ImdbIdentityGate{
        policy,strict:strict_match_mode(),year,has_season:series&&!seasons.is_empty(),
        has_quality:parsed_quality,
        collection_year_range:if year.is_none()&&!series{
            parsed.get("collection_year_range").and_then(Value::as_array)
                .and_then(|range|match range.as_slice(){
                    [start,end]=>Some((start.as_i64()? as i32,end.as_i64()? as i32)),
                    _=>None,
                })
                .or_else(||movie_collection_year_range(parsed))
        }else{None},
        verified_bluray_stream:!series
            &&matchlab_parser::bluray_movie_stream_path(&req.title),
        season_range_end:season_range.map(|(_,end)|end),
        verified_anime_range:verified_anime_range_imdb.as_deref(),
        verified_absolute_episode:verified_absolute_episode_imdb.as_deref(),
    };
    let has_episode=!episodes.is_empty()||parsed.contains_key("episode_start");
    // A verified anime part identity must not be replaced by an unrelated
    // ordinary-series folder; its original crosswalk resolver stays in charge.
    let chosen_layer=if parsed.get("anime_title_verified")
        .and_then(Value::as_bool)==Some(true){None}else{
        bottom_up_imdb_path_match(
            &db,parsed,year,series,seasons.iter().copied().max().unwrap_or(0),
            has_episode,seasons.len()==1&&episodes.len()==1,
            |hits|filter_request_imdb_candidates(&db,hits,gate))?
    };
    let (layer_title,imdb)=if let Some((name,matches))=chosen_layer{
        (Some(name),matches)
    }else{
        (None,imdb_matcher::search_validated(
            &db,title,year,series,seasons.iter().copied().max().unwrap_or(0),
            &req.title,has_episode,20,
            |hits|filter_request_imdb_candidates(&db,hits,gate))
            .map_err(|e|e.to_string())?)
    };
    // The matcher's verified level becomes the presentation title of its
    // diagnostic, while the standalone /parse result remains unchanged.
    // All later anime/crosswalk checks now use that same selected level.
    if let Some(ref name)=layer_title {
        if let Some(fields)=parsed_value.as_object_mut(){
            fields.insert("title".into(),json!(name));
        }
    }
    let parsed=parsed_value.as_object().ok_or("parser returned no object")?;
    let title=layer_title.as_deref().unwrap_or(title);
    let key=imdb_matcher::clean_title(title);
    // A complete longer title found in the ORIGINAL torrent is stronger
    // identity evidence than a short anime alias created by a truncated
    // parser title. Preserve this across all episode/anime selection paths.
    let full_release_imdb=imdb.first().and_then(|candidate|
        imdb_matcher::has_longer_title_evidence_in_release(
            title,&req.title,&candidate.matched_title)
            .then_some(candidate.imdb_id.clone()));
    // A same-name older OVA may win an exact IMDb alias even though it
    // cannot contain S02E37. Prefer verified episode evidence only when
    // there is no more specific complete IMDb title in the original release.
    let imdb=if full_release_imdb.is_none()
        && series && year.is_none() && seasons.len()==1 && episodes.len()==1{
        // A known release production year is stronger identity evidence
        // than episode presence in another same-name show (Doctor Who).
        // The crosswalk resolver may introduce a newly verified identity
        // that was absent from the lexical result. Apply the same public
        // eligibility gates to that additional candidate before publishing.
        filter_request_imdb_candidates(
            &db,imdb_matcher::prefer_verified_episode_identity(
                &db,imdb,title,seasons[0],episodes[0])
                .map_err(|e|e.to_string())?,gate)
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
                        Ok(None)=>(!apply_episode_filter
                            ||full_release_imdb.as_deref()==Some(candidate.imdb_id.as_str()))
                            .then_some(candidate),
                        Err(error)=>{
                            tracing::debug!(imdb_id=%candidate.imdb_id,%error,
                                "Chinese absolute episode crosswalk unavailable");
                            (!apply_episode_filter
                                ||full_release_imdb.as_deref()==Some(candidate.imdb_id.as_str()))
                                .then_some(candidate)
                        }
                    }
                },
                Err(error)=>{
                    tracing::debug!(imdb_id=%candidate.imdb_id,%error,
                        "season episode metadata unavailable for candidate");
                    (!apply_episode_filter
                        ||full_release_imdb.as_deref()==Some(candidate.imdb_id.as_str()))
                        .then_some(candidate)
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
        matched_title:m.matched_title,anime_ids:m.anime_ids,source:"imdb".into()}).collect();
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
    let mut anime_episode_evidence=AnimeEpisodeEvidence::default();
    let anime = if let Some(verified)=verified_local_anime_episode
        .or(verified_path_part) {
        Some(verified)
    } else if verified_chinese_override.is_some()
        || verified_absolute_episode_imdb.is_some() {
        // A separately verified absolute episode owns its crosswalk.
        // Otherwise retain the established anime-priority path, including
        // its explicit collision and season eligibility protections.
        None
    } else if series {
        if let Some(ep)=episodes.first(){
            anime_episode_evidence=anime_candidate_evidence(
                &db,anime_resolve_title,seasons.first().copied(),*ep)
                .map_err(|e|e.to_string())?;
            anime_episode_evidence.candidate.take()
        }else{
            anime_identity(&db,anime_resolve_title).map_err(|e|e.to_string())?
                .map(|id|{
                    let inferred=if seasons.is_empty()
                        && !parsed.contains_key("anime_local_part_unresolved")
                        && !parsed.contains_key("anime_local_part_range_unresolved") {
                        anime_title::season_from_title(&db,&id,anime_resolve_title).ok().flatten()
                    } else {None};
                    json!({"imdb_id":id,"season":seasons.first().copied().or(inferred),
                        "episode":null,"anidb_id":null,"anidb_episode":null})
                })
        }
    } else { None };
    // Reuse the exact film-alias type proof across both candidate lanes.
    // IMDb's broad discovery must not replace a rejected complete film
    // with a shorter TV title. Preserve equally exact AniDB identities,
    // including those whose canonical IMDb title differs or mapping is sparse.
    let imdb=if anime_episode_evidence.rejected_exact_nonseries {
        let words=imdb_matcher::semantic_words(anime_resolve_title);
        let count=title_normalization::title_word_count(anime_resolve_title);
        imdb.into_iter().filter(|candidate|
            anime_episode_evidence.eligible_exact_ids.contains(&candidate.imdb_id)
                ||(imdb_matcher::clean_title(&candidate.matched_title)==anime_resolve_key
                    &&imdb_matcher::semantic_words(&candidate.matched_title)==words
                    &&title_normalization::title_word_count(&candidate.matched_title)==count))
            .collect::<Vec<_>>()
    }else{imdb};
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
        // An AniDB match on the truncated short title ("Monster") may have
        // an episode map yet still be the WRONG show. A complete longer IMDb
        // title literally present in the source cannot be overridden by it.
        if full_release_imdb.as_deref().is_some_and(|id|id!=anime_id){
            return false;
        }
        !imdb.iter().any(|c| {
            (c.media_type=="tvSeries" || c.media_type=="tvMiniSeries")
                && imdb_matcher::clean_title(&c.matched_title)==key && c.imdb_id!=anime_id
        })
    });
    // The separate AniDB-priority branch below does not pass through
    // filter_request_imdb_candidates: validate its IMDb production year
    // before it can publish an SID in strict mode.
    let anime=anime.filter(|a|strict_anime_imdb_year_eligible(
        &db,a,year,strict_match_mode()));
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
        if series && verified_absolute_episode_imdb.is_none()
            && full_release_imdb.is_none() {
            if let Some(ep)=episodes.first().copied(){
                // Preserve positive IMDb evidence for the entire explicit
                // request before switching to a different identity solely
                // because it has an AniDB crosswalk. Open-ended defaults
                // can otherwise make an old same-name OVA claim S02E37.
                // This chooses only the identity: its named-part resolver
                // still interprets local/global coordinates below.
                let requested_end=episodes.iter().copied().max().unwrap_or(ep)
                    .max(parsed.get("episode_end").and_then(Value::as_i64)
                        .unwrap_or(ep));
                let mut canonical=None;
                // An explicit or inherited production year already ranks
                // same-name identities; episode coverage must not replace it.
                if year.is_none() && imdb.len()>1 && seasons.len()==1 && ep>0
                    && valid_sid_coordinate(seasons[0],requested_end){
                    for (index,candidate) in imdb.iter().enumerate(){
                        if imdb_matcher::verified_named_part_remap(
                            &db,&candidate.imdb_id,anime_resolve_title,
                            seasons[0],ep,requested_end).map_err(|e|e.to_string())?{
                            canonical=Some(index);
                            break;
                        }
                        if season_episode_end(&db,&candidate.imdb_id,seasons[0])?
                            .is_some_and(|max|max>=requested_end)&&canonical.is_none(){
                            canonical=Some(index);
                        }
                    }
                }
                // The ranked winner already agrees with the release year
                // and IMDb knows the requested episode. A same-name anime
                // from another decade must not replace it merely because it
                // has a crosswalk ("Peacemaker (2022) S01E04" vs the 2003 anime).
                let year_protects_top=year.is_some_and(|requested|
                    imdb.first().is_some_and(|top|top.year>0
                        && (i64::from(top.year)-i64::from(requested)).abs()<=1
                        && seasons.first().is_none_or(|season|
                            season_episode_end(&db,&top.imdb_id,*season)
                                .ok().flatten()
                                .is_some_and(|max|max>=requested_end))));
                if let Some(index)=canonical {chosen_index=index;}else{
                    for (index,candidate) in imdb.iter().enumerate(){
                        if year_protects_top && index>0
                            && year.is_some_and(|requested|
                                (i64::from(candidate.year)-i64::from(requested)).abs()>1)
                        {continue;}
                        if seasons.is_empty()&&!unmarked_crosswalk_identity_eligible(
                            imdb.first(),candidate,title,
                            &anime_episode_evidence.eligible_exact_ids){continue;}
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
                        candidate_has_episode(&db,&c.imdb_id,
                            selected_season.unwrap_or(0),ep)?{
                        // An exact canonical IMDb series winner keeps the
                        // explicitly parsed SxxExx when there is no proven
                        // AniDB mapping, unless IMDb metadata definitively
                        // rules that episode out. A missing season_episode row
                        // is NOT evidence that S17E01 is invalid (Bleach's
                        // season 17 is absent from some IMDb snapshots).
                        // Do not use this for named sequel/part aliases:
                        // those can remap local S01E03 to another season.
                        // No crosswalk: ASID intentionally stays null.
                    }else if !c.anime_ids.is_empty()||specific{
                        // Never infer anime part or absolute numbering without
                        // a verified crosswalk and identity: leave SID at title.
                        selected_season=None;
                        selected_episode=None;
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
    // A Chinese/Korean parent can label an *absolute* episode (第62集)
    // while the leaf file is only a download banner. Such a file is NOT a
    // complete season pack, nor proof that episode 62 belongs to S01.
    // The already-selected IMDb identity can receive one episode only
    // through an unambiguous imported AniDB/TVDB absolute crosswalk.
    if episodes.is_empty() && !airdate_only && season_range.is_none()
        && !parsed.contains_key("anime_episode_range_candidate")
        && let Some(absolute)=parsed.get("absolute_episode_candidate")
            .and_then(Value::as_i64)
        && let Some(winner)=selected.as_mut()
        && winner["category"]=="series" && winner["episode"].is_null()
        && let Some(imdb)=winner["imdb_id"].as_str()
            .filter(|id|metadata_import::valid_imdb_id(id))
            .map(str::to_owned)
        && let Some(mapped)=episode_crosswalk::resolve_absolute(&db,&imdb,absolute)
            .map_err(|error|error.to_string())?
        && candidate_has_episode(&db,&imdb,mapped.season,mapped.episode)?
        && valid_sid_coordinate(mapped.season,mapped.episode)
    {
        winner["season"]=json!(mapped.season);
        winner["episode"]=json!(mapped.episode);
        winner["sid"]=json!(format!("{imdb}:{}:{}",mapped.season,mapped.episode));
        winner["anime"]=json!({
            "anidb_id":mapped.anidb_id,
            "episode":mapped.anidb_episode,
            "asid":format!("{}:{}",mapped.anidb_id,mapped.anidb_episode)
        });
        winner["absolute_episode_input"]=json!(absolute);
        winner["match_source"]=json!("verified_parent_absolute_episode");
        if let Some(ids)=winner["anime_ids"].as_object_mut(){
            ids.insert("anidb".into(),json!(mapped.anidb_id));
        }
    }
    // Date-only releases use Cinemeta; explicit SxxExx coordinates take precedence.
    if airdate_only {
        if let (Some(date),Some(winner))=(dated.as_ref(),selected.as_mut()){
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
                    Err(error)=>tracing::debug!(imdb_id=%id,%error,"airdate lookup failed"),
                }
            }
        }
    }
    // A calendar release must not select a series that did not yet exist
    // when no episode airdate independently supports the identity. A
    // resolved pre-premiere special or timezone-adjusted date is proof;
    // unknown production years and explicit identity-year overrides keep
    // their established behavior. Never guess a replacement show here.
    let rejected_future_airdate=airdate_only&&dated.as_deref().is_some_and(|date|
        selected.as_ref().is_some_and(|winner|
            unresolved_date_predates_identity(winner,date,req.year)));
    if rejected_future_airdate {selected=None;}
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
    // Date-only releases have their own single-episode resolution above.
    // Even when Cinemeta could not resolve the date, a dated release is
    // never a season pack and must not expand to IMDb episode 1..N.
    // A season-only release needs both IMDb AND AniDB endpoints. Prior
    // code stopped at IMDb season_episodes, so /match always returned null
    // ASIDs for "Bleach_S02". Also support an exact, title-only anime pack
    // whose metadata verified season 1 (e.g. [Group][Chaos_Head][BDRip]).
    // A supplied episode or batch is never expanded to the entire season.
    if !airdate_only && episodes.is_empty() && !parsed.contains_key("episode_start")
        && !parsed.contains_key("anime_local_part_is_cour")
        && !parsed.contains_key("anime_local_part_unresolved")
        && !parsed.contains_key("absolute_episode_candidate")
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
                    exact_anime_identity(&db,title)?
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
                        // A local S/P marker was converted by an
                        // independently verified crosswalk. This season is
                        // already absolute; do not offset it a second time.
                        let canonical_title=parsed.get("anime_local_part_verified").is_some()
                            || (imdb.iter().any(|candidate|
                                candidate.imdb_id==imdb_id
                                && imdb_matcher::clean_title(&candidate.title)==anime_resolve_key)
                                && anime_subfolder_title.is_none());
                        let mapped=mapped_season_episode_range(
                            &db,&imdb_id,anime_resolve_title,season,1,end,
                            canonical_title,false)?;
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
                        let mapped=mapped_season_episode_range(
                            &db,&imdb_id,anime_resolve_title,season,start,end,
                            parsed.get("anime_local_part_verified").is_some(),false)?;
                        // A partial AniDB mapping is not evidence that the
                        // explicitly labelled IMDb SxxEyy interval is wrong.
                        // Retain IMDb endpoints when the canonical IMDb
                        // series and imported episode bounds independently
                        // confirm them. Do not fabricate missing ASIDs.
                        let exact_canonical=imdb_matcher::clean_title(
                            winner["title"].as_str().unwrap_or(""))==key
                            && anime_subfolder_title.is_none();
                        let imdb_end=season_episode_end(&db,&imdb_id,season)?;
                        if mapped["complete"]!=true && exact_canonical
                            && imdb_end.is_some_and(|max|max>=end)
                        {
                            winner["anime_range_diagnostics"]=mapped;
                            json!({"start_season":season,"start_episode":start,
                                "end_season":season,"end_episode":end,
                                "complete":true,"segments":[],
                                "source":"verified_imdb_range_without_complete_anidb"})
                        }else{mapped}
                    }else {
                        let known_end=season_episode_end(&db,&imdb_id,season)?;
                        let valid=known_end.is_some_and(|last|last>=end);
                        // A missing IMDb episode table does not erase the
                        // explicit season. Do not manufacture episodes for
                        // an unverified release range.
                        json!({"start_season":Some(season),
                            "start_episode":if valid {Some(start)}else{None},
                            "end_season":Some(season),
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
                let mut range=mapped_absolute_range(&db,&imdb_id,start,end)?;
                // Several AniDB works of one IMDb series can each claim the
                // same absolute numbers ("Baki The Grappler 1-3" vs. its
                // second series). The verified exact alias names one part;
                // accept its interval only when every episode is mapped.
                if range["complete"]!=true
                    && verified_anime_range_imdb.as_deref()==Some(imdb_id.as_str())
                    && seasons.is_empty()
                    && let Some(part_season)=anime_title::season_from_title(
                        &db,&imdb_id,title).map_err(|e|e.to_string())?
                {
                    let named=mapped_season_episode_range(
                        &db,&imdb_id,title,part_season,start,end,false,true)?;
                    if named["complete"]==true
                        && named["start_season"].as_i64()==Some(part_season)
                        && named["end_season"].as_i64()==Some(part_season) {
                        range=named;
                    }
                }
                // Only after the authoritative AniDB mapping has no
                // endpoints may imported IMDb metadata resolve a clearly
                // marked EP01-24 range for a non-anime show. An explicit
                // single season is authoritative only with its own bounds;
                // without one, the series must have exactly one season.
                // For ambiguous title matches, partial anime mappings or
                // an explicit multi-season release, fail closed.
                let single_exact_imdb=imdb.iter()
                    .filter(|candidate|imdb_matcher::clean_title(
                        &candidate.matched_title)==key)
                    .map(|candidate|candidate.imdb_id.as_str())
                    .collect::<std::collections::BTreeSet<_>>().len()==1;
                if range["unmapped_count"].as_i64()==Some(end-start+1)
                    && range["segments"].as_array().is_some_and(Vec::is_empty)
                    && seasons.len()<=1
                    && single_exact_imdb
                    && verified_anidb_range.is_none()
                    && verified_anime_range_imdb.is_none()
                    && parsed.get("anime_episode_range_candidate").is_none()
                    && let Some(fallback)=verified_ordinary_imdb_episode_range(
                        &db,&imdb_id,&key,
                        winner["matched_title"].as_str().unwrap_or(""),
                        start,end,seasons.first().copied())?
                {
                    range=fallback;
                }
                // Preserve requested endpoints even when the AniDB
                // crosswalk is incomplete. Never extrapolate a partly
                // verified anime mapping into a fabricated season.
                winner["episode_range"]=range.clone();
                winner["season"]=range["start_season"].clone();
                winner["episode"]=range["start_episode"].clone();
                winner["season_end"]=range["end_season"].clone();
                winner["episode_end"]=range["end_episode"].clone();
                winner["sid"]=if let (Some(se),Some(ep))=(
                    range["start_season"].as_i64(),range["start_episode"].as_i64()){
                    json!(format!("{imdb_id}:{se}:{ep}"))
                }else{json!(imdb_id)};
                // These are the release's explicitly marked range
                // endpoints, not proof that the work is anime.
                winner["anime_episode_start"]=json!(start);
                winner["anime_episode_end"]=json!(end);
                winner["anime"]=Value::Null;
            }
        }
    }
    if let Some(winner)=selected.as_mut()
        && let Some(range)=verified_first_year_anime_imdb_batch(
            &db,winner,parsed,&imdb)?
    {
        let imdb_id=winner["imdb_id"].as_str().unwrap_or_default().to_owned();
        winner["season"]=json!(1);
        winner["episode"]=json!(1);
        winner["season_end"]=json!(1);
        winner["episode_end"]=range["end_episode"].clone();
        winner["sid"]=json!(format!("{imdb_id}:1:1"));
        winner["episode_range"]=range;
        winner["anime"]=Value::Null;
        winner["coordinate_fallback"]=json!("verified_imdb_first_year_full_anime_batch");
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
    if let Some(winner)=selected.as_mut(){
        reconcile_series_coordinates(&db,winner,parsed)?;
        official_numbering_after_prologue(&db,winner,parsed)?;
    }
    let reason = if rejected_future_airdate&&selected.is_none(){
        "Release date predates the series without a verified episode airdate"
    }else if anidb_candidate.is_some() && selected.as_ref()
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
/// The season resolver follows Trakt when it lists a later cour as its own
/// season. IMDb often keeps the cour inside the TVDB season instead
/// ("Spy x Family" S01 has 25 episodes; its second cour starts at offset
/// 12). When IMDb's season is known to contain the episode, the directly
/// linked cour with the greatest positive offset below it owns the episode.
/// Two cours at that offset, or an episode outside the cour, publish nothing.
fn later_cour_in_imdb_season(
    db:&Connection,imdb:&str,season:i64,episode:i64,
)->Result<Option<episode_crosswalk::Episode>,String>{
    if !season_episode_end(db,imdb,season)?.is_some_and(|last|episode<=last){
        return Ok(None);
    }
    let mut stmt=db.prepare("SELECT DISTINCT m.anidb_id,m.offset,m.start,m.end
        FROM anime_series_map l JOIN episode_map m ON m.anidb_id=l.anidb_id
        WHERE l.imdb_id=?1 AND m.anidb_season>0 AND m.tvdb_season=?2
          AND TRIM(m.map)='' AND m.offset>0 AND m.offset<?3
        ORDER BY m.offset DESC LIMIT 2").map_err(|e|e.to_string())?;
    let cours=stmt.query_map(rusqlite::params![imdb,season,episode],|row|Ok((
        row.get::<_,String>(0)?,row.get::<_,i64>(1)?,
        row.get::<_,i64>(2)?,row.get::<_,i64>(3)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    let Some((anidb,offset,start,end))=cours.first() else{return Ok(None)};
    if cours.get(1).is_some_and(|other|other.1==*offset){return Ok(None);}
    let local=episode-offset;
    if local<(*start).max(1) || (*end>0 && local>*end)
        || episode_crosswalk::canonical_asid(anidb,local).is_none(){return Ok(None);}
    Ok(Some(episode_crosswalk::Episode{imdb_id:imdb.to_owned(),season,episode,
        anidb_id:anidb.clone(),anidb_episode:local}))
}

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
    let mapped=match episode_crosswalk::resolve_season(
        db,&imdb,season,episode).map_err(|e|e.to_string())? {
        Some(mapped)=>mapped,
        None=>match later_cour_in_imdb_season(db,&imdb,season,episode)
            .ok().flatten() {
            Some(mapped)=>mapped,
            None=>return Ok(()),
        },
    };
    if mapped.imdb_id!=imdb||mapped.season!=season||mapped.episode!=episode{
        return Ok(());
    }
    // A coordinate inferred from IMDb season lengths is only described by
    // the TVDB-based crosswalk when both agree on where the season begins:
    // the work starts the season (local == episode) or is a later cour with
    // that positive offset. A work spanning several TVDB seasons with a
    // negative offset (Gintama's TVDB S03E10 is its absolute 109, while
    // IMDb S03 is another series) would attach an unrelated ASID.
    if matches!(winner["coordinate_fallback"].as_str(),
        Some("complete_imdb_absolute_anime"|"repaired_imdb_absolute_anime")) {
        let cour_offset=episode-mapped.anidb_episode;
        let agrees=cour_offset==0 || (cour_offset>0 && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM episode_map m
                WHERE m.anidb_id=?1 AND m.anidb_season>0 AND m.tvdb_season=?2
                  AND m.offset=?3 AND TRIM(m.map)='')",
            rusqlite::params![&mapped.anidb_id,season,cour_offset],
            |row|row.get::<_,i64>(0)).map_err(|e|e.to_string())?!=0);
        if !agrees{return Ok(());}
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

// Always-on, bounded, single-flight cache for final public matches keyed by
// the COMPLETE original torrent path plus parsed/matcher/metadata generation.
// Unlike the existing persistent ML cache this is available with ML disabled;
// unlike candidate caches, it never shares a final SID across different files.
static TORRENT_MATCH_CACHE:OnceLock<query_cache::QueryCache<Diagnostic>>=OnceLock::new();
/// Strict and broad candidate discovery must never share a persisted or
/// short-lived final result. Keep the full metadata/regex fingerprint, but
/// append the immutable boot-time search policy.
// A semantic matcher change must invalidate old persistent ML match results,
// even when metadata.sqlite3, learned regex rules and the strict-mode flag
// are unchanged. Bump this revision when altering title/year eligibility.
const MATCHER_SEMANTIC_REVISION:&str="anidb-parts-and-numbered-titles-v7";
fn scoped_match_generation(metadata:&str,strict:bool)->String{
    format!("{metadata}:matcher={MATCHER_SEMANTIC_REVISION}:strict={strict}")
}
// Reuse the generation already checked by diagnose_logged, rather than
// recomputing the filesystem/WAL SHA-256 to enter the same cache.
fn whole_torrent_cached_diagnose(
    db_path:&PathBuf,req:&MatchRequest,metadata:Option<&str>,
)->Result<Diagnostic,String>{
    let Some(metadata)=metadata else{return diagnose(db_path,req);};
    let regex_revision=parse_rules_revision();
    let strict=strict_match_mode();
    let scoped=scoped_match_generation(metadata,strict);
    let key=format!("{scoped}:{regex_revision:016x}:{:?}:{:?}:{}",
        req.year,req.series,req.title);
    TORRENT_MATCH_CACHE.get_or_init(||query_cache::QueryCache::new(
        512,Duration::from_secs(30))).get_or_compute(
        key,||diagnose(db_path,req),
        |_|parse_rules_revision()==regex_revision
            &&strict_match_mode()==strict
            &&learning_cache::match_fingerprint(db_path)
                .as_deref()==Some(metadata))
}

// The optional persistent ML cache and always-on match cache share an
// identical fingerprint. This remains a fresh per-request snapshot: do not
// memoize filesystem/regex generations across requests or SQLite refreshes.
fn match_generations(db_path:&PathBuf)->(Option<String>,Option<String>){
    let fingerprint=learning_cache::match_fingerprint(db_path);
    let namespace=if learning_cache::enabled(){
        fingerprint.as_ref().map(|fp|scoped_match_generation(fp,strict_match_mode()))
    }else{None};
    (namespace,fingerprint)
}

fn history_details_with_request(mut details:Value,req:&MatchRequest,policy:MatchPolicy)->Value{
    if let Some(record)=details.as_object_mut(){
        // Both keys must be present: explicit null means the new event had
        // no override; an absent object on legacy history means unknown.
        record.insert("request_context".into(),json!({
            "year":req.year,"series":req.series
        }));
        // The boot policy and compiled semantics are immutable. Store their
        // actual values with the event, never infer them when reading old
        // history. This decoration still happens after pristine cache storage.
        record.insert("matcher_context".into(),json!({
            "strict_mode":strict_match_mode(),
            "semantic_revision":MATCHER_SEMANTIC_REVISION
        }));
        // Policy is replay provenance even when learning is disabled.
        // Retain the existing key used by the independent ML evidence queue.
        record.insert("ml_source_policy".into(),json!(
            if matches!(policy,MatchPolicy::PublicApi){"public_api"}
            else{"dashboard_diagnostic"}));
    }
    details
}

fn diagnose_logged(state:&AppState,req:&MatchRequest,policy:MatchPolicy)->Result<Diagnostic,String>{
    let start=Instant::now();
    // Dashboard-only exploratory diagnostics deliberately bypass matching
    // caches. /match and strict /debug/match use the identical PublicApi
    // eligibility policy and may share verified cache entries.
    // Cache only public-policy results. Strict and broad searches use
    // distinct versioned namespaces and distinct final-result keys; one
    // can never serve the other, even after an opt-out/restart. Exploratory
    // dashboard diagnostics still bypass these caches entirely.
    let public_policy=matches!(policy,MatchPolicy::PublicApi);
    let cacheable=public_policy;
    // Snapshot generation once: the persistent ML namespace is the same
    // fingerprint when ML is enabled, but the always-on cache also works
    // without ML. Avoid hashing metadata/WAL twice for the same snapshot.
    let (mut generation,mut whole_path_generation)=if cacheable{
        match_generations(&state.db)
    }else{(None,None)};
    let mut result=if cacheable{
        if let Some(snapshot)=generation.as_deref().and_then(|ns|
            learning_cache::get_match(&req.title,req.year,req.series,ns)){
            match serde_json::from_value::<Diagnostic>(snapshot){
                Ok(diagnostic)=>Ok(diagnostic),
                Err(_)=>whole_torrent_cached_diagnose(
                    &state.db,req,whole_path_generation.as_deref()),
            }
        }else{whole_torrent_cached_diagnose(
            &state.db,req,whole_path_generation.as_deref())}
    }else{
        diagnose_with_policy(&state.db,req,policy)
    };
    // Re-evaluate at most once after a metadata or rule generation change.
    // Refresh both related fingerprints together; never accept a stale SID.
    if cacheable {
        let (latest_namespace,latest_fingerprint)=match_generations(&state.db);
        if latest_fingerprint!=whole_path_generation || latest_namespace!=generation {
            generation=latest_namespace;
            whole_path_generation=latest_fingerprint;
            result=whole_torrent_cached_diagnose(
                &state.db,req,whole_path_generation.as_deref());
        }
        if match_generations(&state.db)!=(generation.clone(),whole_path_generation.clone()){
            return Err("matching metadata or learned regex changed during request; retry".into());
        }
    }
    let elapsed=start.elapsed().as_millis() as u64;
    match &result{
        Ok(d)=>{
            // One serialization for the optional ML cache and bounded
            // history queue; ownership is transferred to the writer.
            let details=serde_json::to_value(d).unwrap_or(Value::Null);
            if cacheable && let Some(ref ns)=generation {
                learning_cache::put_match(
                    &req.title,req.year,req.series,ns,&details);
            }
            // Request provenance belongs to history, after the cache has
            // copied the pristine Diagnostic. It never changes API output.
            let details=history_details_with_request(details,req,policy);
            let status=if d.selected.is_some(){"matched"}else{"unmatched"};
            dashboard_history::record_async(
                &state.history,&req.title,status,d.selected.as_ref(),details,elapsed);
        },
        Err(error)=>dashboard_history::record_async(
            &state.history,&req.title,"error",None,
            history_details_with_request(json!({"error":error}),req,policy),elapsed),
    }
    result
}

fn ml_status_snapshot()->Value{
    let mut report=field_learning::status();
    if let Some(object)=report.as_object_mut(){
        object.insert("capacity".into(),ml_config::status());
        object.insert("training_and_cache".into(),learning_cache::status());
        object.insert("training_queue".into(),local_learning::queue_status());
        object.insert("online_model".into(),online_model::OnlineModel::status());
        object.insert("unknown_markers".into(),candidate_discovery::status());
        object.insert("title_boundaries".into(),title_learning::status());
        object.insert("regex_rules".into(),learning_rule_log::status());
        object.insert("corrective_regex".into(),corrective_learning::status());
        object.insert("general_regex".into(),general_learning::status());
        object.insert("compiled_parser_replay".into(),matchlab_parser::replay_cache_status());
    }
    report
}
async fn ml_status()->Json<Value>{
    // Some status providers lazily build parser regex catalogues. Never do
    // that work on Tokio's HTTP executor; a dashboard poll must not stall
    // liveness, /parse or parallel matching requests.
    match tokio::task::spawn_blocking(ml_status_snapshot).await{
        Ok(report)=>Json(report),
        Err(error)=>{
            tracing::debug!(%error,"ML telemetry snapshot unavailable");
            Json(json!({"status":"unavailable","error":"ML telemetry unavailable"}))
        }
    }
}

/// Preserve bookmarks to the original dashboard while reserving / for
/// navigation. Keep APIs rooted at their existing absolute paths.
async fn dashboard_redirect()->Redirect{
    Redirect::permanent("/dash")
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
    if !state.history_ready.load(Ordering::Acquire){
        return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({
            "status":"initializing",
            "error":"History and statistics migration is running in the background"
        })));
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
    if !state.history_ready.load(Ordering::Acquire){
        return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({
            "status":"initializing",
            "error":"History and statistics migration is running in the background"
        })));
    }
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
    // The public four-field SID protocol has no season-only encoding.
    // Keep a bare IMDb ID (not a fictitious :S:0 episode) when an
    // incomplete interval has neither episode endpoint. Season remains
    // visible in /debug/match/strict and the independent /parse output.
    let unscoped_range=has_range && winner["episode"].as_i64().is_none()
        && winner["episode_end"].as_i64().is_none();
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
        }else if has_range && !unscoped_range{Value::Null}else{json!(imdb)};
    let sid_end=if invalid_coordinates{json!(imdb)}
        else if imdb.is_empty(){Value::Null}
        else if let (Some(se),Some(ep))=(end_season,end_episode){
            json!(format!("{imdb}:{se}:{ep}"))
        }else if has_range && !unscoped_range{Value::Null}else{json!(imdb)};
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
    if args.get(1).is_some_and(|arg|arg=="verify-native-ml") {
        local_learning::verify_native_model().map_err(std::io::Error::other)?;
        println!("native Rust FTRL model self-test passed");
        return Ok(());
    }
    if args.get(1).is_some_and(|arg|arg=="benchmark-native-ml") {
        let stats=local_learning::benchmark_native_model()
            .map_err(std::io::Error::other)?;
        println!("{stats}");
        return Ok(());
    }
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
        let overrides=metadata_import::apply_bundled_animelists_overrides(
            std::path::Path::new(&args[2]))?;
        let inferred=metadata_import::infer_implicit_animelists_crosswalks(
            std::path::Path::new(&args[2]))?;
        tracing::info!("Anime-Lists mappings imported: {count}, bundled overrides: {overrides}, generic inferred: {inferred}");
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
        if !matches!(args.len(),9|10) {
            return Err("usage: matchlab-server refresh-metadata-urls <database.sqlite3> <basics_url> <akas_url> <episodes_url> [<ratings_url>] <animeapi_url> <animelists_url> <anidb_titles_url>".into());
        }
        // Share startup validation and the legacy six-source compatibility
        // path, which inserts the official ratings feed in the right slot.
        let urls=metadata_download::configured_urls(Some(&args[3..].join(",")))?;
        let (imdb,links,mappings,aliases)=metadata_download::refresh_from_urls(
            std::path::Path::new(&args[2]),&urls)?;
        tracing::info!("downloaded metadata refresh completed: imdb={imdb} anime_links={links} episode_maps={mappings} aliases={aliases}");
        return Ok(());
    }
    let startup_started=Instant::now();
    let strict_raw=env::var("MATCHLAB_STRICT_MODE").ok();
    let strict=parse_strict_match_mode(strict_raw.as_deref())?;
    let _=STRICT_MATCH_MODE.set(strict);
    tracing::info!(strict_mode=strict,
        "IMDb and AniDB title matching policy configured");
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
        let override_marker=metadata_import::bundled_animelists_override_marker();
        let refresh_index_state=index_state.clone();
        tokio::spawn(async move {
            let refresh_seconds=hours*3600;
            loop {
                let check_target=target.clone();
                let expected_override=override_marker.clone();
                let last_refresh=tokio::task::spawn_blocking(move || {
                    let db=rusqlite::Connection::open_with_flags(
                        &check_target,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
                    let value:String=db.query_row(
                        "SELECT updated_at FROM metadata_state WHERE source='full_refresh'",
                        [],|row|row.get(0)).ok()?;
                    // A previously published database may predate the
                    // embedded mapping exceptions. Rebuild once on startup
                    // when the baked-in exception list has changed, rather
                    // than serving stale episode crosswalks for seven days.
                    let current=db.query_row(
                        "SELECT 1 FROM metadata_state WHERE source=?1",
                        [&expected_override],|row|row.get::<_,i64>(0)).is_ok();
                    if !current {
                        tracing::info!("bundled Anime-Lists mappings changed; scheduling full metadata refresh");
                        return None;
                    }
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
    // Never run historical SQLite migrations (including ML queue counters)
    // before binding HTTP. A large history volume must not delay /parse,
    // /dash or /health/live. The bounded history writer buffers observations
    // while a dedicated worker migrates the schema.
    let history_ready=Arc::new(AtomicBool::new(false));
    dashboard_history::defer_writes_until(history_ready.clone());
    // A pre-existing volume may need a fresh Tantivy schema generation.
    // Start this in the blocking pool only after all service configuration is
    // validated. /health/live and the dashboard remain reachable throughout.
    let listener = tokio::net::TcpListener::bind(&host).await?;
    if std::path::Path::new(&db).is_file(){
        let startup_target=PathBuf::from(&db);
        let startup_index_state=index_state.clone();
        tokio::spawn(async move{
            let started=Instant::now();
            let warmup=tokio::task::spawn_blocking(move||->Result<(),String>{
                let conn=Connection::open_with_flags(
                    &startup_target,OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e|e.to_string())?;
                tracing::info!(existing_index=tantivy_search::index_ready(&conn),
                    "startup Tantivy search generation check");
                tantivy_search::warmup(&conn)
            }).await;
            match warmup{
                Ok(Ok(()))=>{
                    set_index_phase(&startup_index_state,IndexPhase::Ready);
                    tracing::info!(elapsed_ms=started.elapsed().as_millis(),
                        "startup Tantivy indexing completed");
                },
                Ok(Err(error))=>{
                    set_index_phase(&startup_index_state,IndexPhase::Failed(error.clone()));
                    tracing::error!(%error,elapsed_ms=started.elapsed().as_millis(),
                        "startup Tantivy indexing failed");
                },
                Err(error)=>{
                    set_index_phase(&startup_index_state,
                        IndexPhase::Failed(error.to_string()));
                    tracing::error!(%error,elapsed_ms=started.elapsed().as_millis(),
                        "startup Tantivy worker failed");
                }
            }
        });
    }
    // History migration and ML are best-effort background services, NEVER
    // prerequisites for HTTP listening or metadata-backed matching. In
    // particular, do not let an ML SQLite backlog or model replay contend
    // with the first Tantivy search warmup.
    let background_history=PathBuf::from(&history_db);
    let background_metadata=PathBuf::from(&db);
    let background_index=index_state.clone();
    let background_history_ready=history_ready.clone();
    tokio::spawn(async move{
        let started=Instant::now();
        let mut failures=0u64;
        loop {
            let path=background_history.clone();
            match tokio::task::spawn_blocking(move||dashboard_history::initialize(&path)).await {
                Ok(Ok(()))=>break,
                Ok(Err(error))=>{
                    failures+=1;
                    if failures==1||failures%10==0{
                        tracing::warn!(%error,failures,
                            "history migration deferred; API and matching remain available");
                    }
                },
                Err(error)=>{
                    failures+=1;
                    if failures==1||failures%10==0{
                        tracing::warn!(%error,failures,
                            "history migration worker failed; will retry independently");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        // Publish only after SQLite's single write transaction commits. The
        // history writer may now drain its bounded in-memory backlog.
        background_history_ready.store(true,Ordering::Release);
        tracing::info!(elapsed_ms=started.elapsed().as_millis(),
            "history/statistics migration completed in background");
        let prune_path=background_history.clone();
        tokio::spawn(async move{
            loop {
                let path=prune_path.clone();
                match tokio::task::spawn_blocking(move||dashboard_history::prune(&path)).await {
                    Ok(Ok(count)) if count>0 =>
                        tracing::info!(deleted=count,"expired request history removed"),
                    Ok(Ok(_))=>(),
                    Ok(Err(error))=>
                        tracing::debug!(%error,"request history cleanup failed"),
                    Err(error)=>
                        tracing::debug!(%error,"history cleanup task failed"),
                }
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        });
        if local_learning::active(){
            if index_phase(&background_index)!=IndexPhase::Ready{
                tracing::info!("offline ML initialization deferred until search index is ready");
            }
            while index_phase(&background_index)!=IndexPhase::Ready{
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            community_learning::start(background_history.clone(),background_metadata.clone());
            local_learning::start(background_history,background_metadata);
            tracing::info!(elapsed_ms=started.elapsed().as_millis(),
                "offline ML workers started after HTTP, history and search index");
        }
    });
    let app = Router::new()
        .route("/",get(dashboard_redirect))
        .route("/dash",get(dashboard))
        .route("/dash/",get(dashboard_redirect))
        .route("/api/dashboard/history",get(dashboard_history_request))
        .route("/api/dashboard/stats",get(dashboard_stats_request))
        .route("/api/community/status",get(community_learning::status))
        .route("/api/ml/status",get(ml_status))
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
            index_phase:index_state,history_ready});
    tracing::info!(elapsed_ms=startup_started.elapsed().as_millis(),
        "Rust API listening on {host} (ML and history migration deferred)");
    axum::serve(listener,app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_match_cache_namespaces_are_strictly_policy_separated(){
        let key="v3-metadata-123";
        assert_ne!(scoped_match_generation(key,true),
            scoped_match_generation(key,false));
        assert_eq!(scoped_match_generation(key,true),
            scoped_match_generation(key,true));
        assert_ne!(scoped_match_generation("v3-metadata-124",true),
            scoped_match_generation(key,true));
    }
    #[test]
    fn strict_mode_defaults_to_exact_only_and_rejects_invalid_configuration(){
        assert_eq!(parse_strict_match_mode(None),Ok(true));
        for value in ["true","TRUE","1","on","yes"," true "]{
            assert_eq!(parse_strict_match_mode(Some(value)),Ok(true),"{value}");
        }
        for value in ["false","FALSE","0","off","no"," false "]{
            assert_eq!(parse_strict_match_mode(Some(value)),Ok(false),"{value}");
        }
        for value in ["strict","enabled","2",""]{
            assert!(parse_strict_match_mode(Some(value)).is_err(),"{value}");
        }
    }

    #[tokio::test]
    async fn dashboard_is_on_dash_and_root_redirects_without_moving_api_routes(){
        use axum::response::IntoResponse;
        let response=dashboard_redirect().await.into_response();
        assert_eq!(response.status(),StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers().get(header::LOCATION)
            .and_then(|v|v.to_str().ok()),Some("/dash"));
        let (headers,html)=dashboard().await;
        assert_eq!(headers[0].1,"text/html; charset=utf-8");
        assert!(html.contains("MatchLab — Dashboard"));
        // The page uses absolute API URLs, not links resolved relative
        // to the /dash path. Matching endpoints remain unchanged.
        assert!(html.contains("fetch('/health')"));
        assert!(html.contains("json('/api/dashboard/history"));
        assert!(html.contains("json('/api/dashboard/diagnose"));
    }
    #[test]
    fn airdate_releases_never_expand_to_season_packs() {
        // Keep the airdate cache in an isolated directory so these tests
        // never call Cinemeta or share cache state with parallel fixtures.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let folder=std::env::temp_dir().join(format!(
            "matchlab-airdate-pack-{}-{stamp}",std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path=folder.join("metadata.sqlite3");
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
                ('tt0115147','The Daily Show','thedailyshow',1996,
                 'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES ('tt0115147',27);
            INSERT INTO season_episodes VALUES ('tt0115147',27,50,50);").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        // A known date maps to exactly one episode, while two neighboring
        // different episodes must never be chosen for an ambiguous date.
        let cache=Connection::open(folder.join("matchlab_airdates.sqlite3")).unwrap();
        cache.execute_batch("CREATE TABLE episode_airdates(
                imdb_id TEXT NOT NULL,air_date TEXT NOT NULL,
                season INTEGER NOT NULL,episode INTEGER NOT NULL,
                PRIMARY KEY(imdb_id,air_date,season,episode));
            CREATE TABLE episode_airdate_refresh(
                imdb_id TEXT PRIMARY KEY,refreshed_at INTEGER NOT NULL);
            INSERT INTO episode_airdates VALUES
                ('tt0115147','2022-01-20',27,45),
                ('tt0115147','2022-01-22',27,47);
            INSERT INTO episode_airdate_refresh VALUES
                ('tt0115147',CAST(strftime('%s','now') AS INTEGER));").unwrap();
        drop(cache);

        for (release, sid, source) in [
            ("The.Daily.Show.2022.01.20.1080p.WEB-DL",
                "tt0115147:27:45","cinemeta_airdate"),
            ("The_Daily_Show_2022-01-20.1080p.WEB-DL",
                "tt0115147:27:45","cinemeta_airdate"),
            ("The.Daily.Show.20.January.2022.1080p.WEB-DL",
                "tt0115147:27:45","cinemeta_airdate"),
            ("The.Daily.Show.January.20.2022.1080p.WEB-DL",
                "tt0115147:27:45","cinemeta_airdate"),
            ("The.Daily.Show.2022-01-20.1080p.WEB-DL",
                "tt0115147:27:45","cinemeta_airdate"),
            ("The.Daily.Show.2022/01/20.1080p.WEB-DL",
                "tt0115147:27:45","cinemeta_airdate"),
            ("The.Daily.Show.2022.01.23.1080p.WEB-DL",
                "tt0115147:27:47","cinemeta_airdate_1day"),
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let expected_date=if release.contains("2022.01.23"){
                "2022-01-23"
            }else{"2022-01-20"};
            assert_eq!(result.parsed["date"],expected_date,
                "{release}: {:?}",result.parsed);
            let winner=result.selected.expect("dated IMDb series match");
            assert_eq!(winner["imdb_id"],"tt0115147","{release}: {winner}");
            assert_eq!(winner["match_source"],source,"{release}: {winner}");
            assert_eq!(winner["episode_range"],Value::Null,
                "{release} was expanded into a season pack: {winner}");
            assert_eq!(winner["release_date"].as_str().unwrap().len(),10);
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":sid,"sid_end":sid,
                "asid_start":null,"asid_end":null
            }),"{release}");
        }

        // Both a missing date and an ambiguous neighboring-day lookup must
        // keep only the verified IMDb series identity (never S27E01-E50).
        for release in [
            "The.Daily.Show.2022.02.15.1080p.WEB-DL",
            "The.Daily.Show.2022.01.21.1080p.WEB-DL",
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let winner=result.selected.expect("dated series title still matches");
            assert_eq!(winner["episode_range"],Value::Null,"{release}: {winner}");
            assert!(winner["episode"].is_null(),"{release}: {winner}");
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":"tt0115147","sid_end":"tt0115147",
                "asid_start":null,"asid_end":null
            }),"{release}");
        }

        // Real season packs and explicitly labeled episodes are unchanged.
        for (release, start, end) in [
            ("The.Daily.Show.S27.1080p.WEB-DL",
                "tt0115147:27:1","tt0115147:27:50"),
            ("The.Daily.Show.S27E45.1080p.WEB-DL",
                "tt0115147:27:45","tt0115147:27:45"),
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let winner=result.selected.expect("regular series match");
            let response=public_match_response(Some(winner));
            assert_eq!(response["sid_start"],start,"{release}: {response}");
            assert_eq!(response["sid_end"],end,"{release}: {response}");
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn hierarchy_matches_each_independent_level_from_leaf_then_parents(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt2004','Monster','monster',2004,'series','JP',1,0,'tvSeries'),
                ('ttgein','Monster: The Ed Gein Story','monstertheedgeinstory',
                    2025,'series','US',1,0,'tvSeries'),
                ('ttshort','The Ed','theed',2025,'series','US',0,11,'tvSeries');
            INSERT INTO seasons VALUES('tt2004',1),('ttgein',3),('ttshort',1);
            INSERT INTO imdb_ratings VALUES('tt2004',300000),('ttgein',3),
                ('ttshort',100000);")
            .unwrap();
        for path in [
            "monster/the.ed.gein.story/S01/E02.mkv",
            "monster/the.ed.gein.story/S01E02.mkv",
            "monster/the.ed.gein.story/Season.1/E02.mkv",
            "monster\\the.ed.gein.story\\S01\\E02.mkv",
        ]{
            let parsed=matchlab_parser::parse_title(path,false).unwrap();
            let result=bottom_up_imdb_path_match(&db,&parsed,None,true,1,true,true,|hits|hits)
                .unwrap().expect("a named directory must resolve independently");
            assert_eq!(result.0,
                parsed["path_layers"][1]["parsed"]["title"].as_str().unwrap(),
                "matcher must use the independently parsed named component: {path}");
            assert!(!result.0.eq_ignore_ascii_case("Monster"),
                "the parent must not preempt the closer series suffix");
            assert_eq!(result.1[0].imdb_id,"ttgein",
                "long complete-title suffix wins from nearest named directory");
            assert_eq!(parsed["seasons"],json!([1]),"{path}");
            assert_eq!(parsed["episodes"],json!([2]),"{path}");
        }
        // The closest meaningful title level wins. Directory prefix
        // "Monster" must not be concatenated into a synthetic alias.
        let parsed=matchlab_parser::parse_title(
            "Monster/Not.A.Real.Show/S01/E02.mkv",false).unwrap();
        let from_parent=bottom_up_imdb_path_match(&db,&parsed,None,true,1,true,true,|hits|hits)
            .unwrap().expect("climb to a matching parent");
        assert_eq!(from_parent.0,"Monster");
        assert_eq!(from_parent.1[0].imdb_id,"tt2004");

        // Do not stop on the closest lexical hit if the public identity
        // gate rejects it. Without a season or quality signal, explicit
        // release year must validate the identity before the layer wins.
        let parsed=matchlab_parser::parse_title(
            "Monster/The.Ed.Gein.Story/E02.mkv",false).unwrap();
        let gate=ImdbIdentityGate{
            policy:MatchPolicy::PublicApi,strict:false,year:Some(2004),
            has_season:false,has_quality:false,collection_year_range:None,verified_bluray_stream:false,season_range_end:None,
            verified_anime_range:None,verified_absolute_episode:None,
        };
        let recovered=bottom_up_imdb_path_match(
            &db,&parsed,Some(2004),true,0,true,true,
            |hits|filter_request_imdb_candidates(&db,hits,gate)
        ).unwrap().expect("an ineligible leaf must allow parent fallback");
        assert_eq!(recovered.0,"Monster");
        assert_eq!(recovered.1[0].imdb_id,"tt2004");

        // Season discovery tolerates a one-season metadata lag, but a
        // multi-season pack must reject a smaller title before climbing.
        let parsed=matchlab_parser::parse_title(
            "The.Ed.Gein.Story/Monster/S02/E02.mkv",false).unwrap();
        let gate=ImdbIdentityGate{
            policy:MatchPolicy::PublicApi,strict:false,year:None,
            has_season:true,has_quality:false,collection_year_range:None,verified_bluray_stream:false,season_range_end:Some(2),
            verified_anime_range:None,verified_absolute_episode:None,
        };
        let recovered=bottom_up_imdb_path_match(
            &db,&parsed,None,true,2,true,true,
            |hits|filter_request_imdb_candidates(&db,hits,gate)
        ).unwrap().expect("season-pack eligibility must permit fallback");
        assert_eq!(recovered.1[0].imdb_id,"ttgein");

        // When three same-name leaf identities all have a proven lower
        // episode maximum, the final >2-candidate episode filter must not
        // suppress a valid parent identity.
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt2004',1,24,24);
            INSERT INTO titles VALUES
                ('ttbad01','Bad Show','badshow',2010,'series','',1,0,'tvSeries'),
                ('ttbad02','Bad Show','badshow',2011,'series','',1,0,'tvSeries'),
                ('ttbad03','Bad Show','badshow',2012,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttbad01',2),('ttbad02',2),('ttbad03',2);
            INSERT INTO season_episodes VALUES
                ('ttbad01',1,10,10),('ttbad02',1,10,10),('ttbad03',1,10,10);
            INSERT INTO imdb_ratings VALUES
                ('ttbad01',4000),('ttbad02',3000),('ttbad03',2000);")
            .unwrap();
        let bad_candidates=imdb_matcher::search(
            &db,"Bad Show",None,true,1,"Bad.Show.S01E20.mkv",true,20)
            .unwrap();
        assert!(bad_candidates.len()>2,
            "regression must exercise the multi-candidate episode gate");
        let parsed=matchlab_parser::parse_title(
            "Monster/Bad.Show.S01E20.mkv",false).unwrap();
        let fallback=bottom_up_imdb_path_match(
            &db,&parsed,None,true,1,true,true,|hits|hits
        ).unwrap().expect("episode-ineligible leaf must allow parent fallback");
        assert_eq!(fallback.1[0].imdb_id,"tt2004");

        // Generic folder names use the EXACT same metadata-only parser
        // guard as title inheritance, even when a coincidental IMDb
        // title such as "Library" would otherwise pass lexical matching.
        db.execute_batch("INSERT INTO titles VALUES
            ('ttlibrary','Library','library',2000,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttlibrary',20);
            INSERT INTO imdb_ratings VALUES('ttlibrary',200000);").unwrap();
        let generic=matchlab_parser::parse_title("Monster/Library/S01/E02.mkv",false)
            .unwrap();
        let found=bottom_up_imdb_path_match(
            &db,&generic,None,true,1,true,true,|hits|hits
        ).unwrap().expect("library folder must not steal the series");
        assert_eq!(found.1[0].imdb_id,"tt2004");

        // Numeric show names remain valid through exact IMDb evidence,
        // but nested bare season numbers must not hijack their parent.
        db.execute_batch("INSERT INTO titles VALUES
            ('tt24','24','24',2001,'series','US',1,0,'tvSeries'),
            ('tt6','6','6',2006,'series','US',1,0,'tvSeries'),
            ('ttcops','Cops','cops',1989,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt24',8),('tt6',10),('ttcops',36);
            INSERT INTO imdb_ratings VALUES
                ('tt24',50000),('tt6',200000),('ttcops',10);").unwrap();
        let numeric=matchlab_parser::parse_title("24/S01/E02.mkv",false).unwrap();
        let found=bottom_up_imdb_path_match(
            &db,&numeric,None,true,1,true,true,|hits|hits
        ).unwrap().expect("a genuine numeric series must remain matchable");
        assert_eq!(found.1[0].imdb_id,"tt24");
        let structural=matchlab_parser::parse_title("Cops/6/S06E10.mkv",false)
            .unwrap();
        let found=bottom_up_imdb_path_match(
            &db,&structural,None,true,6,true,true,|hits|hits
        ).unwrap().expect("the parent title must win over a season directory");
        assert_eq!(found.1[0].imdb_id,"ttcops");

        // A file leaf with its own recognized show name wins before
        // the parent folder, without mixing the levels.
        let parsed=matchlab_parser::parse_title(
            "Monster/The.Ed.Gein.Story.S01E02.mkv",false).unwrap();
        let leaf=bottom_up_imdb_path_match(&db,&parsed,None,true,1,true,true,|hits|hits)
            .unwrap().expect("leaf named release must win");
        assert_eq!(leaf.1[0].imdb_id,"ttgein");
    }

    #[test]
    fn anidb_parts_prologues_and_numbered_titles_agree_with_parsed_episodes(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-anidb-parts-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt9200001','Orb Saga Z Kai','orbsagazkai',2009,'series','',1,0,'tvSeries'),
                ('tt9200001','Orb Saga Kai','orbsagakai',2009,'series','JP',0,3,'tvSeries'),
                ('tt9200002','Orb Saga','orbsaga',1986,'series','',1,0,'tvSeries'),
                ('tt9200003','Spy Home','spyhome',2022,'series','',1,0,'tvSeries'),
                ('tt9200004','Cour Split','coursplit',2023,'series','',1,0,'tvSeries'),
                ('tt9200005','Blade Works','bladeworks',2014,'series','',1,0,'tvSeries'),
                ('tt9200006','Mob Hero 100','mobhero100',2016,'series','',1,0,'tvSeries'),
                ('tt9200007','Grappler Tale','grapplertale',2001,'series','',1,0,'tvSeries'),
                ('tt9200008','Kaiju Unit-gou','kaijuunitgou',2024,'series','',1,0,'tvSeries'),
                ('tt9200009','Hero School','heroschool',2016,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt9200001',2),('tt9200002',1),('tt9200003',2),
                ('tt9200004',2),('tt9200005',1),('tt9200006',1),('tt9200007',2),
                ('tt9200008',2),('tt9200009',2);
            INSERT INTO anime_ids VALUES('tt9200008','anidb','662');
            INSERT INTO season_episodes VALUES
                ('tt9200001',1,98,98),('tt9200001',2,69,69),('tt9200002',1,153,153),
                ('tt9200003',1,25,25),('tt9200003',2,12,12),
                ('tt9200004',1,12,12),('tt9200004',2,12,12),
                ('tt9200005',1,12,12),('tt9200006',1,12,12),
                ('tt9200007',1,24,24),('tt9200007',2,24,24),
                ('tt9200008',1,12,12),('tt9200008',2,11,11),
                ('tt9200009',1,13,13),('tt9200009',2,25,25);
            INSERT INTO imdb_ratings VALUES('tt9200001',100),('tt9200002',90000),
                ('tt9200003',100),('tt9200004',100),('tt9200005',100),
                ('tt9200006',100),('tt9200007',100);
            INSERT INTO anime_series_map VALUES
                ('tt9200001','601','tvorb',1),('tt9200001','602','tvorb',2),
                ('tt9200002','600','tvorig',1),
                ('tt9200003','611','tvspy',1),('tt9200003','612','tvspy',2),
                ('tt9200003','613','tvspy',3),
                ('tt9200004','621','tvcour',1),('tt9200004','622','tvcour',0),
                ('tt9200005','631','tvblade',1),('tt9200006','641','tvmob',1),
                ('tt9200007','651','tvgrap',1),('tt9200007','652','tvgrap',2),
                ('tt9200008','661','tvkaiju',1),('tt9200008','662','tvkaiju',0),
                ('tt9200009','671','tvhero',1),('tt9200009','672','tvhero',2);
            INSERT INTO anidb_titles VALUES
                ('600','Orb Saga','orbsaga','main','en'),
                ('601','Orb Saga Kai','orbsagakai','main','en'),
                ('602','Orb Saga Kai (2014)','orbsagakai2014','main','en'),
                ('611','Spy Home','spyhome','main','en'),
                ('612','Spy Home Part 2','spyhomepart2','syn','en'),
                ('613','Spy Home Season 2','spyhomeseason2','official','en'),
                ('621','Cour Split','coursplit','main','en'),
                ('622','Cour Split Part 2','coursplitpart2','official','en'),
                ('631','Blade Works','bladeworks','main','en'),
                ('641','Mob Hero 100','mobhero100','main','en'),
                ('651','Grappler Tale','grapplertale','main','en'),
                ('652','Grappler Tale Tournament','grapplertaletournament','main','en'),
                ('661','Kaiju Unit Gou','kaijuunitgou','main','en'),
                ('662','Kaiju Unit Gou (2025)','kaijuunitgou2025','main','en'),
                ('671','Hero School','heroschool','main','en'),
                ('672','Hero School (2017)','heroschool2017','main','en');
            INSERT INTO episode_map VALUES
                ('600','tvorig',1,1,0,0,0,''),
                ('601','tvorb',1,1,0,0,0,''),('602','tvorb',1,2,0,0,0,''),
                ('611','tvspy',1,1,1,12,0,''),('612','tvspy',1,1,1,0,12,''),
                ('613','tvspy',1,2,0,0,0,''),
                ('621','tvcour',1,1,1,12,0,''),('622','tvcour',1,1,1,0,12,''),
                ('631','tvblade',1,0,0,0,0,';1-1;'),('631','tvblade',1,1,2,13,-1,''),
                ('641','tvmob',1,1,1,12,0,''),
                ('651','tvgrap',1,1,0,0,0,''),('652','tvgrap',1,2,0,0,0,''),
                ('661','tvkaiju',1,1,1,12,0,''),('662','tvkaiju',1,2,1,11,0,''),
                ('671','tvhero',1,1,0,0,0,''),('672','tvhero',1,2,0,0,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let public=|release:&str|public_match_response(diagnose(&path,&MatchRequest{
            title:release.into(),year:None,series:None}).unwrap().selected);
        for (release,sid_start,sid_end,asid_start,asid_end) in [
            // An exact alias whose primary IMDb name is longer is not a
            // shortened name of the unrelated prefix "Orb Saga"; two AniDB
            // works of the series make absolute 7 ambiguous, the alias not.
            ("Orb Saga Kai_07_(HD.720p)",
                "tt9200001:1:7","tt9200001:1:7","601:7","601:7"),
            ("[Group] Orb Saga Kai 07 [3EB3C732].mkv",
                "tt9200001:1:7","tt9200001:1:7","601:7","601:7"),
            // "Part 2" is the second AniDB work of season one, with local
            // or season-continuing numbering, never IMDb season two.
            ("[Group] Spy Home Part 2 - 05 [720p][94588851].mkv",
                "tt9200003:1:17","tt9200003:1:17","612:5","612:5"),
            ("[Group] Spy Home Part 2 - 25 [720p][94588851].mkv",
                "tt9200003:1:25","tt9200003:1:25","612:13","612:13"),
            // IMDb ends season one exactly where the second cour begins.
            ("[Group] Cour Split Part 2 - 12 [1080p][3EBCAABA].mkv",
                "tt9200004:2:12","tt9200004:2:12","622:12","622:12"),
            // AniDB episode 1 is the broadcast prologue "episode 0".
            ("[Group] Blade Works - 05 [AF51767D].mkv",
                "tt9200005:1:5","tt9200005:1:5","631:6","631:6"),
            ("[Group] Blade Works - 13 [AF51767D].mkv",
                "tt9200005:1:12","tt9200005:1:12","631:13","631:13"),
            // The exact alias names the first of two works that both
            // accept absolute episodes 1..3.
            ("[Group]Grappler Tale  1-3 Dvd-rip",
                "tt9200007:1:1","tt9200007:1:3","651:1","651:3"),
        ] {
            assert_eq!(public(release),json!({"sid_start":sid_start,"sid_end":sid_end,
                "asid_start":asid_start,"asid_end":asid_end}),"{release}");
        }
        for (release,sid_start,sid_end,asid_start,asid_end) in [
            // A running number continues into the next season, whose own
            // AniDB work restarts at 1. The title names the first work even
            // though the IMDb record stores the second work's AniDB ID.
            ("[Group] Kaiju Unit-gou - 21 [1080p].mkv",
                "tt9200008:2:9","tt9200008:2:9",Some("662:9"),Some("662:9")),
            // The same with open-ended season defaults: the second work is
            // the only one mapped to season two.
            ("[Group] Hero School - 20 [720p].mkv",
                "tt9200009:2:7","tt9200009:2:7",Some("672:7"),Some("672:7")),
            ("【Group】★新番★[Hero School][20][1080p]",
                "tt9200009:2:7","tt9200009:2:7",Some("672:7"),Some("672:7")),
            // A running number across an IMDb season split at the cour.
            ("[Group] Cour Split - 14 [2041EC53].mkv",
                "tt9200004:2:2","tt9200004:2:2",Some("622:2"),Some("622:2")),
            // An explicit episode inside a later cour of the IMDb season.
            ("Spy Home S01E21 1080p WEB",
                "tt9200003:1:21","tt9200003:1:21",Some("612:9"),Some("612:9")),
            // A batch of a numbered part becomes a season-relative range.
            ("[Group] Spy Home Part 2 - 01 ~ 12 [1080p]",
                "tt9200003:1:13","tt9200003:1:24",None,None),
        ] {
            assert_eq!(public(release),json!({"sid_start":sid_start,"sid_end":sid_end,
                "asid_start":asid_start,"asid_end":asid_end}),"{release}");
        }
        // "Mob Hero 100" is the title; 100 is never episode 100.
        let numbered=diagnose(&path,&MatchRequest{
            title:"[Group] Mob Hero 100 (BD 1080P x264 FLAC)".into(),
            year:None,series:None}).unwrap();
        assert!(numbered.parsed.get("anime_episode_candidate").is_none(),
            "{:?}",numbered.parsed);
        let selected=numbered.selected.expect("numbered title");
        assert_eq!(selected["imdb_id"],"tt9200006","{selected}");
        assert_ne!(selected["episode"],100,"{selected}");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn release_year_selects_identity_over_same_name_films_and_anime(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-year-identity-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt9100001','Gunslinger Saga','gunslingersaga',1998,'series','JP',1,0,'tvSeries'),
                ('tt9100002','Gunslinger Saga','gunslingersaga',2010,'movie','JP',1,0,'movie'),
                ('tt9100003','Peace Keeper','peacekeeper',2022,'series','US',1,0,'tvSeries'),
                ('tt9100004','Peace Keeper','peacekeeper',2003,'series','JP',1,0,'tvSeries'),
                ('tt9100005','Raging','raging',2007,'movie','US',1,0,'movie'),
                ('tt9100006','Old School Days','oldschooldays',1995,'movie','US',1,0,'movie'),
                ('tt9100007','Lonely Giants','lonelygiants',2019,'movie','US',1,0,'movie'),
                ('tt9100008','Детектив 11','11',2007,'movie','RU',0,3,'movie'),
                ('tt9100011','拿命.Example','example',2002,'movie','CN',0,3,'movie'),
                ('tt9100012','Пример Example','example',2015,'movie','RU',0,3,'movie'),
                ('tt9100009','Twin Remake','twinremake',1998,'series','US',1,0,'tvSeries'),
                ('tt9100010','Twin Remake','twinremake',1998,'movie','US',1,0,'movie');
            INSERT INTO seasons VALUES('tt9100001',1),('tt9100003',1),('tt9100004',1),
                ('tt9100009',1);
            INSERT INTO season_episodes VALUES('tt9100001',1,26,26),
                ('tt9100003',1,8,8),('tt9100004',1,24,24),('tt9100009',1,10,10);
            INSERT INTO imdb_ratings VALUES('tt9100001',1000),('tt9100002',90000),
                ('tt9100003',1000),('tt9100004',90000),('tt9100009',90000),
                ('tt9100010',10);
            INSERT INTO anime_series_map VALUES('tt9100001','531','gunanidb',0),
                ('tt9100004','532','peaceanidb',0);
            INSERT INTO anidb_titles VALUES
                ('531','Gunslinger Saga','gunslingersaga','main','en'),
                ('532','Peace Keeper','peacekeeper','main','en');
            INSERT INTO episode_map VALUES('531','gunanidb',1,1,1,26,0,''),
                ('532','peaceanidb',1,1,1,24,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let winner=|release:&str|diagnose(&path,&MatchRequest{
            title:release.into(),year:None,series:None}).unwrap().selected;
        // A year without coordinates used to exclude every series, so the
        // 1998 anime lost to an unrelated 2010 film alias.
        for release in ["Gunslinger Saga (1998) [BD 1080p]",
            "[Group] Gunslinger Saga (1998) [BDRip 1080p] Complete"] {
            let selected=winner(release).expect("year-identified series");
            assert_eq!(selected["imdb_id"],"tt9100001","{release}: {selected}");
        }
        let film=winner("Gunslinger.Saga.2010.1080p.BluRay.x264").expect("film year");
        assert_eq!(film["imdb_id"],"tt9100002","{film}");
        // A same-year film with the same complete title keeps film priority.
        let twin=winner("Twin.Remake.1998.1080p.BluRay.x264").expect("same-year film");
        assert_eq!(twin["imdb_id"],"tt9100010","{twin}");
        // The ranked 2022 series has the requested episode; the 2003 anime
        // crosswalk is not a reason to replace it.
        let series=winner("Peace.Keeper.2022.S01E04.720p.WEB.x264").expect("2022 series");
        assert_eq!(series["sid"],"tt9100003:1:4","{series}");
        let anime=winner("Peace.Keeper.2003.S01E04.720p.WEB.x264").expect("2003 anime");
        assert_eq!(anime["imdb_id"],"tt9100004","{anime}");
        // Films from another period and number-only key collisions abstain.
        for release in ["Raging.Sharks.2005.720p.WEB.x264-GRP",
            "Old.School.Days.1958.720p.BluRay.x264-GRP",
            "1.1 Интро (2020) WEBRip 1080p"] {
            assert!(winner(release).is_none(),"{release} must not match");
        }
        // A mixed-script alias needs its non-Latin words in the release.
        assert!(winner("Example.2002.720p.WEB.x264-GRP").is_none(),
            "a Latin fragment is not the mixed-script alias");
        let mixed=winner("Пример Example (2015) WEBRip 1080p").expect("written alias");
        assert_eq!(mixed["imdb_id"],"tt9100012","{mixed}");
        // One year of tolerance, and two for a unique complete title.
        for release in ["Raging.2008.720p.WEB.x264-GRP",
            "Lonely.Giants.2021.720p.WEBRip.x264-GRP"] {
            assert!(winner(release).is_some(),"{release} still matches");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn monster_ed_gein_full_torrent_title_beats_older_anime_episode_mapping(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-monster-long-title-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt0434706','Monster','monster',2004,'series','JP',1,0,'tvSeries'),
                ('tt13207736','Monster: The Ed Gein Story',
                    'monstertheedgeinstory',2025,'series','US',0,10,'tvSeries');
            INSERT INTO seasons VALUES('tt0434706',1),('tt13207736',3);
            INSERT INTO season_episodes VALUES
                ('tt0434706',1,74,74),('tt13207736',3,8,8);
            INSERT INTO imdb_ratings VALUES('tt0434706',100000),
                ('tt13207736',10);
            INSERT INTO anime_series_map VALUES('tt0434706','100','monsteranidb',0);
            INSERT INTO anidb_titles VALUES('100','Monster','monster','main','en');
            INSERT INTO episode_map VALUES('100','monsteranidb',1,1,1,74,0,'');")
            .unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for title in [
            "Monster.the.ed.gein.story.s1.e1",
            "Monster.The.Ed.Gein.Story.S01E01.1080p.WEB-DL",
            "Monster.the.ed.gein.s1.e1",
            "Monster.The.Ed.Gein.S01E01.1080p.WEB-DL",
            "Scene.Monster.The.Ed.Gein.Story.S01E01.1080p.WEB-DL",
            "Scene.Monster.The.Ed.Gein.S01E01.1080p.WEB-DL",
            "Scene.Monster.The.Ed.Gein.2025.S01E01",
            // The torrent is a suffix OF the IMDb title. No "Monster"
            // word is present in the release at all.
            "The.ed.gein.story.s01e01",
            "The.Ed.Gein.Story.2025.S01E01",
            "Ed.Gein.Story.S01E01.1080p.WEB-DL",
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:title.into(),year:None,series:None}).unwrap();
            let selected=result.selected.expect("full Ed Gein title match");
            assert_eq!(selected["imdb_id"],"tt13207736",
                "a less relevant but episode-verified anime cannot hijack {title}: {selected}");
            assert_eq!(selected["match_source"],"imdb");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

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
                -- Popular sequel with the *same* exact AKA must not hijack
                -- the canonical title, even when it has more IMDb votes.
                ('tt14986406','Bleach: Thousand-Year Blood War',
                 'bleachthousandyearbloodwar',2022,'series','',0,0,'tvSeries'),
                ('tt14986406','Bleach','bleach',2022,'series','US',0,51,'tvSeries'),
                ('tt0000202','The Duke of Death and His Maid',
                  'thedukeofdeathandhismaid',2021,'series','',1,0,'tvSeries'),
                ('tt0000303','Chaos;Head','chaoshead',2008,'series','',1,0,'tvSeries'),
                ('tt0000404','Reborn as a Vending Machine',
                 'rebornasavendingmachine',2023,'series','',1,0,'tvSeries'),
                ('tt0000505','Bleach side story','bleachsidestory',2005,
                 'movie','',1,0,'movie');
            INSERT INTO seasons VALUES ('tt0000101',16),('tt14986406',4),
                ('tt0000202',3),
                ('tt0000303',1),('tt0000404',3);
            INSERT INTO season_episodes VALUES
                ('tt0000101',1,20,20),('tt0000101',2,21,21),
                ('tt14986406',1,13,13),
                ('tt0000202',2,12,12),
                ('tt0000303',1,12,12),('tt0000404',3,12,12);
            INSERT INTO imdb_ratings VALUES
                ('tt0000101',90000),('tt14986406',120000);
            INSERT INTO anime_series_map VALUES
                ('tt0000101','100','bleachtv',0),
                ('tt14986406','901','bleachtybw',0),
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
                ('901','Bleach','bleach','synonym','en'),
                ('901','Bleach Thousand Year Blood War',
                 'bleachthousandyearbloodwar','main','en'),
                ('101','Bleach','bleach','synonym','en'),
                ('500','Bleach','bleach','synonym','en'),
                ('200','Shinigami Bocchan to Kuro Maid',
                 'shinigamibocchantokuromaid','synonym','ja'),
                ('300','Chaos Head','chaoshead','main','en'),
                ('400','Jihanki 3','jihanki3','synonym','ja');
            INSERT INTO episode_map VALUES
                ('100','bleachtv',1,-1,1,366,0,''),
                ('901','bleachtybw',1,1,1,13,0,''),
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
        for release in ["Bleach S01","bleach.s1"] {
            let d=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let selected=d.selected.expect("Bleach S01 IMDb match");
            assert_eq!(selected["imdb_id"],"tt0000101",
                "a sequel's abbreviated AKA cannot hijack the original: {release}");
            assert_eq!(selected["episode_range"]["complete"],true,"{selected}");
            assert_eq!(selected["anime_range_diagnostics"]["unmapped_count"],0);
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":"tt0000101:1:1","sid_end":"tt0000101:1:20",
                "asid_start":"100:1","asid_end":"100:20"
            }));
        }
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
    fn historical_bleach_s07_release_publishes_real_imdb_anidb_endpoints(){
        // Explicitly model the previously verified production crosswalk:
        // Bleach S07E01..20 -> AniDB 2369 absolute episodes 132..151.
        // Deterministic fixture, not a live IMDb/AniDB metadata snapshot.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-bleach-s07-{}-{stamp}.sqlite3",std::process::id()));
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
                ('tt0434665','Bleach','bleach',2004,'series','',1,0,'tvSeries');
            INSERT INTO imdb_ratings VALUES('tt0434665',90000);
            INSERT INTO seasons VALUES('tt0434665',16);
            INSERT INTO season_episodes VALUES
                ('tt0434665',7,20,20),('tt0434665',15,26,26);
            INSERT INTO anime_series_map VALUES
                ('tt0434665','2369','74796',0);
            INSERT INTO anime_ids VALUES('tt0434665','anidb','2369');
            INSERT INTO anidb_titles VALUES
                ('2369','Bleach','bleach','main','en');
            INSERT INTO episode_map VALUES
                ('2369','74796',1,-1,1,366,0,''),
                ('2369','74796',1,7,132,151,-131,''),
                ('2369','74796',1,15,317,342,-316,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for (release,start,end,asid_start,asid_end) in [
            ("Bleach.S07.MULTi.1080p.WEB.H264-UwU",1,20,132,151),
            ("Bleach.S07E01.1080p.WEB",1,1,132,132),
            ("Bleach.S07E20.1080p.WEB",20,20,151,151),
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let selected=result.selected.expect("verified Bleach IMDb identity");
            assert_eq!(selected["imdb_id"],"tt0434665","{release}: {selected}");
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":format!("tt0434665:7:{start}"),
                "sid_end":format!("tt0434665:7:{end}"),
                "asid_start":format!("2369:{asid_start}"),
                "asid_end":format!("2369:{asid_end}")
            }),"{release}");
        }
        // The production Anime-Lists mapping for season 15 is 317..342,
        // offset -316. This is different from older synthetic tests, which
        // intentionally modeled a different season boundary.
        for raw in ["Bleach 320","Bleach_320","Bleach.320"] {
            let result=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:None}).unwrap();
            let selected=result.selected.expect("Bleach absolute episode 320");
            assert_eq!(selected["imdb_id"],"tt0434665");
            assert_eq!(public_match_response(Some(selected)),json!({
                "sid_start":"tt0434665:15:4","sid_end":"tt0434665:15:4",
                "asid_start":"2369:320","asid_end":"2369:320"
            }),"{raw}");
        }
        // The learner must not retroactively turn an unverified raw
        // episode 400 into a fake published episode/ASID.
        let invalid=diagnose(&path,&MatchRequest{
            title:"Bleach 400".into(),year:None,series:None}).unwrap();
        let selected=invalid.selected.expect("retain Bleach IMDb title only");
        assert_eq!(selected["imdb_id"],"tt0434665");
        let resp=public_match_response(Some(selected));
        assert!(resp["asid_start"].is_null()&&resp["asid_end"].is_null(),
            "out-of-range anime absolute episode must not publish ASID: {resp}");
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
    fn historical_jujutsu_title_episodes_do_not_select_unrelated_anime_part(){
        // A minimal exact-ID fixture based on previously reported Jujutsu
        // titles; the anime crosswalk is explicit test data, not a claim
        // that the metadata imported on every server has this shape.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-jujutsu-history-{}-{stamp}.sqlite3",std::process::id()));
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
                ('tt10601584','Jujutsu Kaisen','jujutsukaisen',2020,
                    'series','',1,0,'tvSeries');
            INSERT INTO imdb_ratings VALUES('tt10601584',100000);
            INSERT INTO seasons VALUES('tt10601584',2);
            INSERT INTO season_episodes VALUES
                ('tt10601584',1,24,24),('tt10601584',2,24,24);
            INSERT INTO anime_series_map VALUES
                ('tt10601584','16169','jujutsu-tvdb',0);
            INSERT INTO anime_ids VALUES
                ('tt10601584','anidb','16169');
            INSERT INTO anidb_titles VALUES
                ('16169','Jujutsu Kaisen','jujutsukaisen','main','en');
            INSERT INTO episode_map VALUES
                ('16169','jujutsu-tvdb',1,1,1,24,0,''),
                ('16169','jujutsu-tvdb',1,2,1,24,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for (title,season,episode) in [
            ("Jujutsu Kaisen - 10",1,10),
            ("Jujutsu Kaisen S2 - 07",2,7),
        ]{
            let d=diagnose(&path,&MatchRequest{
                title:title.into(),year:None,series:None}).unwrap();
            let selected=d.selected.expect("historical Jujutsu IMDb identity");
            assert_eq!(selected["imdb_id"],"tt10601584","{title}: {selected}");
            assert_eq!(selected["season"],season,"{title}: {selected}");
            assert_eq!(selected["episode"],episode,"{title}: {selected}");
            let response=public_match_response(Some(selected));
            assert_eq!(response["sid_start"],
                format!("tt10601584:{season}:{episode}"),"{title}: {response}");
            assert_eq!(response["asid_start"],
                format!("16169:{episode}"),"{title}: {response}");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
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
    fn jojo_s02e37_dubbed_xvid_preserves_imdb_and_egypt_anidb_episode(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-jojo-s02e37-{}-{stamp}.sqlite3",std::process::id()));
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
            -- IMDb's original name is Japanese romanization, while the
            -- release uses an English anime alias. This intentionally
            -- tests indexed AniDB-to-IMDb identity fallback, not a
            -- fabricated English title row in IMDb's basic data.
            INSERT INTO titles VALUES
                ('tt2359704','JoJo no Kimyo na Boken',
                 'jojonokimyonaboken',2012,'series','',1,0,'tvSeries'),
                ('tt0248119','JoJo''s Bizarre Adventure',
                 'jojosbizarreadventure',1993,'series','',1,0,'tvMiniSeries');
            INSERT INTO seasons VALUES ('tt2359704',5),('tt0248119',1);
            INSERT INTO season_episodes VALUES
                ('tt2359704',2,48,48),('tt0248119',1,13,13);
            INSERT INTO anime_series_map VALUES
                ('tt0248119','9000','222222',0),
                ('tt2359704','10000','262954',0),
                ('tt2359704','10001','262954',0),
                ('tt2359704','10002','262954',0);
            INSERT INTO anime_ids VALUES('tt2359704','anidb','10000');
            INSERT INTO anidb_titles VALUES
                ('9000','JoJo''s Bizarre Adventure',
                 'jojosbizarreadventure','main','en'),
                ('10000','JoJo''s Bizarre Adventure',
                 'jojosbizarreadventure','main','en'),
                ('10001','JoJo''s Bizarre Adventure Stardust Crusaders',
                 'jojosbizarreadventurestardustcrusaders','main','en'),
                ('10002','JoJo''s Bizarre Adventure Stardust Crusaders Egypt',
                 'jojosbizarreadventurestardustcrusadersegypt','main','en');
            INSERT INTO episode_map VALUES
                ('9000','222222',1,1,1,13,0,''),
                ('10000','262954',1,1,1,26,0,''),
                ('10001','262954',1,2,1,24,0,''),
                ('10002','262954',1,2,1,24,24,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let raw="JoJos Bizarre Adventure S02E37 DUBBED XviD-AFG";
        let parsed=matchlab_parser::parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!([2]),"{parsed:?}");
        assert_eq!(parsed["episodes"],json!([37]),"{parsed:?}");
        assert_eq!(imdb_matcher::clean_title(parsed["title"].as_str().unwrap()),
            "jojosbizarreadventure","{parsed:?}");

        // The exact English IMDb title belongs to the 1993 OVA, but the
        // English anime alias also identifies the 2012 TV series.
        // This fixture must reproduce the actual wrong-first-candidate bug.
        let db=Connection::open(&path).unwrap();
        let initial=imdb_matcher::search(&db,"JoJos Bizarre Adventure",None,true,2,
            raw,true,20).unwrap();
        assert_eq!(initial.first().map(|row|row.imdb_id.as_str()),
            Some("tt0248119"),"fixture must reproduce wrong 1993 match: {initial:?}");
        drop(db);
        let req=MatchRequest{title:raw.into(),year:None,series:None};
        for policy in [MatchPolicy::DashboardDiagnostic,MatchPolicy::PublicApi]{
            let diag=diagnose_with_policy(&path,&req,policy).unwrap();
            let winner=diag.selected.unwrap_or_else(||panic!(
                "No JoJo match; parsed={:?}, candidates={:?}, reason={}",
                diag.parsed,diag.imdb_candidates,diag.reason));
            assert_eq!(winner["imdb_id"],"tt2359704","{winner}");
            assert_eq!(winner["season"],2,"{winner}");
            assert_eq!(winner["episode"],37,"{winner}");
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":"tt2359704:2:37","sid_end":"tt2359704:2:37",
                "asid_start":"10002:13","asid_end":"10002:13"
            }));
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn verified_imdb_episode_bounds_precede_other_identity_open_crosswalk(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-jojo-open-crosswalk-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
                imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            INSERT INTO titles VALUES
                ('tt2359704','JoJo''s Bizarre Adventure','jojosbizarreadventure',
                    2012,'series','US',1,0,'tvSeries'),
                ('tt0248119','JoJo''s Bizarre Adventure','jojosbizarreadventure',
                    1993,'series','US',1,0,'tvMiniSeries');
            INSERT INTO seasons VALUES('tt2359704',5),('tt0248119',2);
            INSERT INTO season_episodes VALUES
                ('tt2359704',2,48,48),('tt0248119',2,6,6);
            -- Reverse the ordinary ranking so the evidence promotion itself
            -- is exercised, independently of the later crosswalk scan.
            INSERT INTO imdb_ratings VALUES('tt2359704',10),('tt0248119',1000000);
            INSERT INTO anime_series_map VALUES
                ('tt2359704','9304','262954',0),('tt0248119','789','83950',0);
            INSERT INTO anime_ids VALUES
                ('tt2359704','anidb','9304'),('tt0248119','anidb','789');
            INSERT INTO anidb_titles VALUES
                ('9304','JoJo''s Bizarre Adventure','jojosbizarreadventure','main','en'),
                ('789','JoJo''s Bizarre Adventure','jojosbizarreadventure','main','en');
            -- The TV work has no S02 crosswalk in this sparse snapshot.
            -- An open OVA default is not evidence against its known S02.
            INSERT INTO episode_map VALUES
                ('9304','262954',1,1,1,26,0,''),
                ('789','83950',1,2,0,0,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        let raw="JoJos Bizarre Adventure S02E37 DUBBED XviD-AFG";
        let initial=imdb_matcher::search(&db,"JoJos Bizarre Adventure",None,true,2,
            raw,true,20).unwrap();
        assert_eq!(initial[0].imdb_id,"tt0248119");
        let preferred=imdb_matcher::prefer_verified_episode_identity(
            &db,initial,"JoJos Bizarre Adventure",2,37).unwrap();
        assert_eq!(preferred[0].imdb_id,"tt2359704",
            "positive IMDb coverage outranks an open-ended alternate");
        for (release,end) in [(raw,37),
            ("JoJos Bizarre Adventure S02E37-E48 DUBBED XviD-AFG",48)]{
            let result=diagnose_with_policy(&path,&MatchRequest{
                title:release.into(),year:None,series:None},MatchPolicy::PublicApi)
                .unwrap();
            let winner=result.selected.expect("verified TV episode");
            assert_eq!(winner["imdb_id"],"tt2359704");
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":"tt2359704:2:37",
                "sid_end":format!("tt2359704:2:{end}"),
                "asid_start":null,"asid_end":null
            }),"{release}");
        }
        // The preservation rule needs the COMPLETE requested range. An
        // earlier endpoint alone must not count as verified pack coverage.
        db.execute("UPDATE season_episodes SET max_episode=40
            WHERE imdb_id='tt2359704'",[]).unwrap();
        db.execute("DELETE FROM season_episodes WHERE imdb_id='tt0248119'",[])
            .unwrap();
        tantivy_search::warmup(&db).unwrap();
        let partial=diagnose_with_policy(&path,&MatchRequest{
            title:"JoJos Bizarre Adventure S02E37-E48 DUBBED XviD-AFG".into(),
            year:None,series:None},MatchPolicy::PublicApi).unwrap();
        assert_ne!(partial.selected.unwrap()["imdb_id"],"tt2359704",
            "max 40 cannot prove the complete E37-E48 request");
        // Outlier episode numbers cannot establish positive IMDb evidence.
        db.execute("UPDATE season_episodes SET max_episode=100000
            WHERE imdb_id='tt2359704'",[]).unwrap();
        tantivy_search::warmup(&db).unwrap();
        let outlier=diagnose_with_policy(&path,&MatchRequest{
            title:raw.into(),year:None,series:None},MatchPolicy::PublicApi).unwrap();
        assert_eq!(outlier.selected.unwrap()["imdb_id"],"tt0248119");
        // Missing bounds remain uncertain and preserve the established
        // crosswalk fallback; no negative metadata rule is introduced.
        db.execute("DELETE FROM season_episodes",[]).unwrap();
        tantivy_search::warmup(&db).unwrap();
        let unknown=diagnose_with_policy(&path,&MatchRequest{
            title:raw.into(),year:None,series:None},MatchPolicy::PublicApi).unwrap();
        assert_eq!(unknown.selected.unwrap()["imdb_id"],"tt0248119");
        drop(db);
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
                ('tt2359704','9304','262954',1),
                ('tt2359704','16187','262954',6),
                ('tt2359704','17286','262954',0),
                ('tt2359704','17664','262954',0);
            INSERT INTO anime_ids VALUES('tt2359704','anidb','9304');
            INSERT INTO anidb_titles VALUES
                ('16187','JoJo`s Bizarre Adventure: Stone Ocean',
                 'jojosbizarreadventurestoneocean','official','en'),
                ('17286','JoJo`s Bizarre Adventure: Stone Ocean (Part 2)',
                 'jojosbizarreadventurestoneoceanpart2','official','en'),
                ('17664','JoJo`s Bizarre Adventure: Stone Ocean (Part 3)',
                 'jojosbizarreadventurestoneoceanpart3','official','en');
            -- Anime-Lists maps Stone Ocean's three AniDB cours into the
            -- same TVDB/IMDb season five, offset by 0, 12, and 24.
            -- Unbounded defaults are season evidence, not independent
            -- proof that AniDB work 16187 owns episode 27.
            INSERT INTO episode_map VALUES
                ('9304','262954',1,1,0,0,0,''),
                ('16187','262954',1,5,1,0,0,''),
                ('17286','262954',1,5,1,0,12,''),
                ('17664','262954',1,5,1,0,24,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        // Official AniDB English aliases use a backtick, not an ASCII
        // apostrophe. The compact index key already agrees; verify strict
        // query-time words too, without rebuilding or changing the schema.
        for possessive in ["JoJo's","JoJo’s","JoJos","JoJo`s"]{
            let title=format!("{possessive} Bizarre Adventure Stone Ocean");
            assert_eq!(tantivy_search::anidb_ids_exact_title(&db,&title).unwrap(),
                std::collections::BTreeSet::from(["16187".to_owned()]));
            assert_eq!(tantivy_search::anime_aliases_exact(&db,&title).unwrap()
                .into_iter().map(|alias|alias.anidb_id)
                .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from(["16187".to_owned()]));
        }
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
        // A local P2 is a cour, not necessarily a new IMDb season.
        // Stone Ocean parts 1/2/3 all remain in IMDb season five.
        let second=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean P2E03".into(),
            year:None,series:None}).unwrap();
        let second=second.selected.expect("second Stone Ocean cour");
        assert_eq!(public_match_response(Some(second)),json!({
            "sid_start":"tt2359704:5:15","sid_end":"tt2359704:5:15",
            "asid_start":"17286:3","asid_end":"17286:3"}));
        let cour_only=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean P2".into(),
            year:None,series:None}).unwrap();
        assert_eq!(cour_only.parsed["anime_local_part_is_cour"],true);
        let cour_only=cour_only.selected.expect("named second cour");
        assert_eq!(cour_only["season"],5);
        assert!(cour_only["episode_range"].is_null(),
            "P2 without episodes must not imply all of S05: {cour_only}");
        let cour_range=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean P2 (01-12) [Batch]".into(),
            year:None,series:None}).unwrap();
        let cour_range=cour_range.selected.expect("bounded second cour");
        assert_eq!(public_match_response(Some(cour_range)),json!({
            "sid_start":"tt2359704:5:13","sid_end":"tt2359704:5:24",
            "asid_start":"17286:1","asid_end":"17286:12"}));

        // An explicit Part 3 title uses its own AniDB-local episode three,
        // corresponding to the parent show's IMDb/TVDB S05E27.
        let third=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean Part 3 S01E03".into(),
            year:None,series:None}).unwrap();
        let winner=third.selected.expect("Stone Ocean third cour");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt2359704:5:27","sid_end":"tt2359704:5:27",
            "asid_start":"17664:3","asid_end":"17664:3"}));
        for possessive in ["JoJo’s","JoJos","JoJo`s"]{
            for (suffix,start,end,asid_start,asid_end) in [
                ("S01E27.mkv",27,27,"17664:3","17664:3"),
                ("P2E03",15,15,"17286:3","17286:3"),
                ("P2 (01-12) [Batch]",13,24,"17286:1","17286:12"),
                ("Part 3 S01E03",27,27,"17664:3","17664:3"),
            ]{
                let raw=format!("{possessive} Bizarre Adventure Stone Ocean {suffix}");
                let result=diagnose(&path,&MatchRequest{
                    title:raw.clone(),year:None,series:None}).unwrap();
                let winner=result.selected.expect("official backtick part alias");
                assert_eq!(public_match_response(Some(winner)),json!({
                    "sid_start":format!("tt2359704:5:{start}"),
                    "sid_end":format!("tt2359704:5:{end}"),
                    "asid_start":asid_start,"asid_end":asid_end
                }),"{raw}");
            }
        }
        let title_only=diagnose(&path,&MatchRequest{
            title:"JoJo's Bizarre Adventure Stone Ocean".into(),
            year:None,series:None}).unwrap();
        let winner=title_only.selected.expect("named part title");
        assert_eq!(winner["imdb_id"],"tt2359704","{winner}");
        assert_eq!(winner["season"],5,"{winner}");
        // A same-name work can have enough GLOBAL S01 episodes while
        // Stone Ocean's S01 is LOCAL to its verified IMDb season five.
        // Introduce that independent collision only after the basic source
        // alias regressions above, then exercise both identity rank stages.
        let db=Connection::open(&path).unwrap();
        db.execute_batch("INSERT INTO titles VALUES
                ('tt2359704','JoJo''s Bizarre Adventure: Stone Ocean',
                 'jojosbizarreadventurestoneocean',2012,'series','US',0,1,'tvSeries'),
                ('tt9990002','JoJo''s Bizarre Adventure: Stone Ocean',
                 'jojosbizarreadventurestoneocean',2022,'series','US',1,0,'tvMiniSeries');
            INSERT INTO seasons VALUES('tt9990002',1);
            INSERT INTO season_episodes VALUES('tt9990002',1,38,38);
            INSERT INTO imdb_ratings VALUES('tt2359704',10),('tt9990002',1000000);")
            .unwrap();
        tantivy_search::warmup(&db).unwrap();
        let title="JoJo's Bizarre Adventure Stone Ocean";
        let raw="JoJo's Bizarre Adventure Stone Ocean S01E27.mkv";
        assert!(imdb_matcher::verified_named_part_remap(&db,
            "tt2359704",title,1,27,27).unwrap());
        assert!(!imdb_matcher::verified_named_part_remap(&db,
            "tt9990002",title,1,27,27).unwrap(),
            "the exact AniDB part belongs to another IMDb identity");
        for (start,end) in [(0,27),(27,26),(1,1_001),(27,i64::MAX)]{
            assert!(!imdb_matcher::verified_named_part_remap(&db,
                "tt2359704",title,1,start,end).unwrap());
        }
        let initial=imdb_matcher::search(&db,title,None,true,1,raw,true,20).unwrap();
        let preferred=imdb_matcher::prefer_verified_episode_identity(
            &db,initial,title,1,27).unwrap();
        assert_eq!(preferred[0].imdb_id,"tt2359704",
            "verified local S01 -> global S05 outranks a competing global S01 bound");
        for (raw,end) in [(raw,27),
            ("JoJo's Bizarre Adventure Stone Ocean S01E27-E30.mkv",30)]{
            let result=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:None}).unwrap();
            assert_eq!(public_match_response(result.selected),json!({
                "sid_start":"tt2359704:5:27",
                "sid_end":format!("tt2359704:5:{end}"),
                "asid_start":"17664:3","asid_end":format!("17664:{}",end-24)
            }),"verified local part must beat a competing global bound: {raw}");
        }
        drop(db);
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
    fn explicit_ep01_24_release_uses_verified_imdb_first_season_range(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-born-with-luck-{}-{stamp}.sqlite3",std::process::id()));
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
            INSERT INTO titles VALUES('tt32592348','Born with Luck','bornwithluck',2026,
                'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt32592348',1);
            INSERT INTO season_episodes VALUES('tt32592348',1,24,24);").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let title="Born.with.Luck.2026.EP01-24.HD1080P.X264.AAC.Mandarin.CHS.XLYS";
        let result=diagnose(&path,&MatchRequest{
            title:title.into(),year:None,series:None}).unwrap();
        assert_eq!(result.parsed["episode_start"],1,"{:?}",result.parsed);
        assert_eq!(result.parsed["episode_end"],24,"{:?}",result.parsed);
        assert_eq!(result.parsed["episode_range_numbering"],"absolute");
        let winner=result.selected.expect("IMDb series identity must match");
        assert_eq!(winner["imdb_id"],"tt32592348");
        assert_eq!(winner["episode_range"]["source"],
            "verified_imdb_single_season_episode_range");
        assert_eq!(winner["season"],1);
        assert_eq!(winner["episode"],1);
        assert_eq!(winner["season_end"],1);
        assert_eq!(winner["episode_end"],24);
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt32592348:1:1","sid_end":"tt32592348:1:24",
            "asid_start":null,"asid_end":null
        }));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn imdb_episode_fallback_requires_single_season_episode_bounds_and_no_anime_link(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE seasons(imdb_id TEXT PRIMARY KEY,
                max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO seasons VALUES('tt32592348',1);
            INSERT INTO season_episodes VALUES('tt32592348',1,24,24);").unwrap();
        let fallback=|title:&str,start:i64,end:i64|{
            verified_ordinary_imdb_episode_range(
                &db,"tt32592348","bornwithluck",title,start,end,None).unwrap()
        };
        assert_eq!(fallback("Born with Luck",1,24).unwrap()["complete"],true);
        assert!(fallback("Other Show",1,24).is_none());
        assert!(fallback("Born with Luck",1,25).is_none());
        assert!(fallback("Born with Luck",0,24).is_none());
        assert!(fallback("Born with Luck",24,1).is_none());
        db.execute("UPDATE seasons SET max_season=2",[]).unwrap();
        assert!(fallback("Born with Luck",1,24).is_none(),
            "multiple-season absolute numbering must remain unresolved");
        db.execute("UPDATE seasons SET max_season=1",[]).unwrap();
        db.execute("INSERT INTO anime_series_map VALUES(
            'tt32592348','321','654',0)",[]).unwrap();
        assert!(fallback("Born with Luck",1,24).is_none(),
            "known anime crosswalk takes precedence even when incomplete");
        db.execute("DELETE FROM anime_series_map",[]).unwrap();
        db.execute("INSERT INTO anime_ids VALUES(
            'tt32592348','anidb','321')",[]).unwrap();
        assert!(fallback("Born with Luck",1,24).is_none());
        db.execute("DELETE FROM anime_ids",[]).unwrap();
        db.execute("DELETE FROM season_episodes",[]).unwrap();
        assert!(fallback("Born with Luck",1,24).is_none(),
            "unknown IMDb episode bounds cannot authorize fallback");
    }

    #[test]
    fn explicit_season_ep_ranges_use_only_verified_non_anime_season_bounds(){
        let stamp=std::time::SystemTime::now().duration_since(
            std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-explicit-ep-range-{}-{stamp}.sqlite3",std::process::id()));
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
                anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,
                end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt1234567','Example Serial','exampleserial',
                2017,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt1234567',6);
            INSERT INTO season_episodes VALUES('tt1234567',1,6,6),
                ('tt1234567',3,6,6),('tt1234567',5,20,20),('tt1234567',6,22,22);").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for (raw,season,start,end) in [
            ("Example.Serial.S05.Ep11-20.720p.WEB-DL.mkv",5,11,20),
            ("Example.Serial.[S03].[E05-06].1080p.WEB-DL.mkv",3,5,6),
            ("Example.Serial.s1.ep1-6.1080p.WEBRip.mkv",1,1,6),
            ("Example.Serial.S06.Part2.Ep06-22.1080p.WEB-DL.mkv",6,6,22),
        ]{
            let result=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:None}).unwrap();
            // Syntax alone does not decide local versus global anime
            // numbering; only the selected non-anime metadata proves it.
            assert_eq!(result.parsed["episode_range_numbering"],"absolute");
            let winner=result.selected.expect("verified series identity");
            assert_eq!(winner["episode_range"]["source"],
                "verified_imdb_explicit_season_episode_range","{raw}: {winner:?}");
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":format!("tt1234567:{season}:{start}"),
                "sid_end":format!("tt1234567:{season}:{end}"),
                "asid_start":null,"asid_end":null}),"{raw}");
        }
        let db=Connection::open(&path).unwrap();
        let range=|season,start,end|verified_ordinary_imdb_episode_range(
            &db,"tt1234567","exampleserial","Example Serial",start,end,season).unwrap();
        assert!(range(None,1,6).is_none(),"do not infer S01 for multiple seasons");
        assert!(range(Some(5),11,21).is_none(),"full interval needs known bounds");
        assert!(range(Some(4),1,2).is_none(),"missing season episode table");
        assert!(range(Some(7),1,2).is_none(),"season exceeds imported coverage");
        assert!(range(Some(0),1,2).is_none());
        db.execute("INSERT INTO anime_series_map VALUES(
            'tt1234567','123','456',0)",[]).unwrap();
        assert!(range(Some(5),11,20).is_none(),"even an incomplete anime link blocks fallback");
        drop(db);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn season_tagged_anime_batch_keeps_explicit_season_against_absolute_crosswalk(){
        // A stale/incorrect AniDB absolute episode mapping may associate
        // episodes 01-24 with S03. The release explicitly advertises S2.
        // Season-relative IMDb bounds remain valid even when AniDB's
        // season-2 crosswalk has not been imported yet.
        for absolute_season in [2,3] {
            let stamp=std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path=std::env::temp_dir().join(format!(
                "matchlab-fire-force-s2-{absolute_season}-{}-{stamp}.sqlite3",
                std::process::id()));
            let db=Connection::open(&path).unwrap();
            db.execute_batch(&format!("
                CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                    year INTEGER,category TEXT,region TEXT,original INTEGER,
                    ordering INTEGER,type TEXT);
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
                INSERT INTO titles VALUES('tt2026001','Enen no Shouboutai',
                    'enennoshouboutai',2019,'series','',1,0,'tvSeries');
                INSERT INTO seasons VALUES('tt2026001',3);
                INSERT INTO season_episodes VALUES
                    ('tt2026001',2,24,24),('tt2026001',3,24,24);
                INSERT INTO anime_series_map VALUES('tt2026001','500',
                    'fireforce',0);
                INSERT INTO anime_ids VALUES('tt2026001','anidb','500');
                INSERT INTO anidb_titles VALUES('500','Enen no Shouboutai',
                    'enennoshouboutai','main','en');
                INSERT INTO episode_map VALUES('500','fireforce',1,-1,1,24,0,'');
                INSERT INTO episode_map VALUES('500','fireforce',1,
                    {absolute_season},1,24,0,'');
            ")).unwrap();
            tantivy_search::warmup(&db).unwrap();
            drop(db);

            let release="[SubsPlease] Enen no Shouboutai S2 (01-24) (720p) [Batch]";
            let parsed=matchlab_parser::parse_title(release,false).unwrap();
            assert_eq!(parsed["anime_episode_range_candidate"]["season_hint"],2,
                "{parsed:?}");
            assert_eq!(parsed["anime_episode_range_candidate"]["start"],1);
            assert_eq!(parsed["anime_episode_range_candidate"]["end"],24);
            let result=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let selected=result.selected.expect("canonical anime series found");
            assert_eq!(selected["imdb_id"],"tt2026001");
            assert_eq!(selected["season"],2,"{absolute_season}: {selected:?}");
            assert_eq!(selected["season_end"],2,"{absolute_season}: {selected:?}");
            assert_eq!(selected["episode"],1,"{absolute_season}: {selected:?}");
            assert_eq!(selected["episode_end"],24,"{absolute_season}: {selected:?}");
            let public=public_match_response(Some(selected));
            assert_eq!(public["sid_start"],"tt2026001:2:1",
                "{absolute_season}: {public}");
            assert_eq!(public["sid_end"],"tt2026001:2:24",
                "{absolute_season}: {public}");
            if absolute_season==2 {
                assert_eq!(public["asid_start"],"500:1","{public}");
                assert_eq!(public["asid_end"],"500:24","{public}");
            }else{
                assert!(public["asid_start"].is_null(),"{public}");
                assert!(public["asid_end"].is_null(),"{public}");
            }

            // Without S2, the exact same numerical batch is correctly
            // eligible for absolute AniDB mapping, including S03.
            let unscoped=diagnose(&path,&MatchRequest{
                title:"[SubsPlease] Enen no Shouboutai (01-24) (720p) [Batch]".into(),
                year:None,series:None}).unwrap();
            let unscoped=unscoped.selected.expect("unscoped anime batch");
            assert_eq!(unscoped["season"],absolute_season,
                "{absolute_season}: {unscoped:?}");
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
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
            INSERT INTO seasons VALUES('tt7654321',2);
            INSERT INTO season_episodes VALUES('tt1234567',1,10,10);
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

        // The imported IMDb episode snapshot has no season-2 row.
        // Preserve the explicit season, but never invent E01-E12.
        let missing_episode_table=diagnose(&path,&MatchRequest{
            title:"Example Show S02E01-E12".into(),year:None,series:None}).unwrap();
        let missing=missing_episode_table.selected.expect("IMDb series found");
        assert_eq!(missing["season"],2);
        assert_eq!(missing["season_end"],2);
        assert!(missing["episode"].is_null());
        assert!(missing["episode_end"].is_null());
        assert_eq!(public_match_response(Some(missing)),json!({
            "sid_start":"tt7654321","sid_end":"tt7654321",
            "asid_start":null,"asid_end":null
        }));

        let season_anime=diagnose(&path,&MatchRequest{
            title:"Spy x Family S01E01-E07".into(),year:None,series:None}).unwrap();
        let public=public_match_response(season_anime.selected);
        assert_eq!(public["sid_start"],"tt1234567:1:1");
        assert_eq!(public["sid_end"],"tt1234567:1:7");
        assert_eq!(public["asid_start"],"123:1");
        assert_eq!(public["asid_end"],"123:7");

        // IMDb season metadata covers 1..9 even though AniDB's anime
        // crosswalk covers only 1..7. Publish valid IMDb endpoints
        // without manufacturing full-range ASID endpoints.
        let partial_anime=diagnose(&path,&MatchRequest{
            title:"Spy x Family S01E01-E09".into(),year:None,series:None}).unwrap();
        let winner=partial_anime.selected.expect("known IMDb series");
        assert_eq!(winner["episode_range"]["source"],
            "verified_imdb_range_without_complete_anidb");
        let response=public_match_response(Some(winner));
        assert_eq!(response["sid_start"],"tt1234567:1:1");
        assert_eq!(response["sid_end"],"tt1234567:1:9");
        assert!(response["asid_start"].is_null());
        assert!(response["asid_end"].is_null());

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
            history_ready:Arc::new(AtomicBool::new(true)),
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
    async fn history_migration_does_not_block_http_or_statistics_readiness(){
        let unique=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let history=std::env::temp_dir().join(format!(
            "matchlab-async-history-{}-{unique}.sqlite3",std::process::id()));
        let state=AppState{
            db:Arc::new(PathBuf::from("/nonexistent/async-history-metadata.sqlite3")),
            history:Arc::new(history.clone()),
            match_slots:Arc::new(Semaphore::new(2)),
            index_phase:Arc::new(Mutex::new(IndexPhase::Indexing)),
            history_ready:Arc::new(AtomicBool::new(false)),
        };
        let (status,Json(live))=liveness().await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(live["status"],"alive");
        let (_,html)=dashboard().await;
        assert!(html.contains("html"));
        let (status,Json(stats))=dashboard_stats_request(State(state.clone())).await;
        assert_eq!(status,StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(stats["status"],"initializing");
        let (status,Json(history_result))=dashboard_history_request(
            State(state.clone()),Query(HistoryQuery{
                limit:None,offset:None,status:None
            })).await;
        assert_eq!(status,StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(history_result["status"],"initializing");
        // The migration is performed on Tokio's blocking pool; HTTP never
        // waits for it, and ordinary requests remain independent of ML.
        let to_initialize=history.clone();
        tokio::task::spawn_blocking(move||
            dashboard_history::initialize(&to_initialize)).await.unwrap().unwrap();
        state.history_ready.store(true,Ordering::Release);
        let (status,Json(stats))=dashboard_stats_request(State(state.clone())).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(stats["total"],0);
        let (status,Json(history_result))=dashboard_history_request(
            State(state),Query(HistoryQuery{
                limit:None,offset:None,status:None
            })).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(history_result["items"],json!([]));
        let _=std::fs::remove_file(&history);
        let _=std::fs::remove_file(format!("{}-wal",history.display()));
        let _=std::fs::remove_file(format!("{}-shm",history.display()));
    }

    #[tokio::test]
    async fn server_health_and_json_errors_work_while_metadata_is_indexing(){
        let state=AppState{
            db:Arc::new(PathBuf::from("/nonexistent/matchlab-metadata-test.sqlite3")),
            history:Arc::new(PathBuf::from("/nonexistent/matchlab-history-test.sqlite3")),
            match_slots:Arc::new(Semaphore::new(2)),
            index_phase:Arc::new(Mutex::new(IndexPhase::Indexing)),
            history_ready:Arc::new(AtomicBool::new(false)),
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
        assert_eq!(unmatched["season"],1,
            "an explicit season survives when its episode is invalid");
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
    fn regular_tv_prefers_verified_parent_then_grandparent_not_episode_title() {
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-regular-parent-title-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt0096697','The Simpsons','thesimpsons',1989,
                    'series','',1,0,'tvSeries'),
                ('tt0500001','Different Show','differentshow',2000,
                    'series','',1,0,'tvSeries'),
                ('tt0500002','Cops','cops',1989,
                    'series','',1,0,'tvSeries'),
                ('tt0300001','Sword Art Online','swordartonline',2012,
                    'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES ('tt0096697',36),('tt0500001',12),
                ('tt0500002',24),('tt0300001',3);
            INSERT INTO season_episodes VALUES
                ('tt0096697',9,25,25),
                ('tt0500001',9,25,25),
                ('tt0500002',6,20,20),
                ('tt0300001',1,12,12);
            INSERT INTO anime_series_map VALUES('tt0300001','100','456',0);
            INSERT INTO anime_ids VALUES('tt0300001','anidb','100');
            INSERT INTO anidb_titles VALUES(
                '100','Sword Art Online','swordartonline','main','en');
            INSERT INTO episode_map VALUES('100','456',1,1,1,12,0,'');
        ").unwrap();
        tantivy_search::warmup(&db).unwrap();

        let release="The Simpsons Season 9 (Complete S09 Remastered HQ 720p x265 HappyLee)/S09E24 Lost Our Lisa.mkv";
        let parsed=matchlab_parser::parse_title(release,false).unwrap();
        assert_eq!(parsed["seasons"],json!([9]),"{parsed:?}");
        assert_eq!(parsed["episodes"],json!([24]),"{parsed:?}");
        // One authoritative leaf-first identity walk replaces the older
        // pre-matching parent override. Parents are used only if the leaf
        // does not provide an eligible IMDb identity.
        let select=|release:&str|{
            let parsed=matchlab_parser::parse_title(release,false).unwrap();
            let seasons=numbers(parsed.get("seasons"));
            let episodes=numbers(parsed.get("episodes"));
            bottom_up_imdb_path_match(
                &db,&parsed,None,true,seasons.iter().copied().max().unwrap_or(0),
                !episodes.is_empty(),seasons.len()==1&&episodes.len()==1,
                |hits|hits
            ).unwrap().expect("a valid path title").1[0].imdb_id.clone()
        };
        assert_eq!(select(release),"tt0096697");
        assert_eq!(
            select("The Simpsons Season 9/Season 9/S09E24 Lost Our Lisa.mkv"),
            "tt0096697");
        // Numeric season folders are metadata; series names and additional
        // valid show identities must be proven, not joined together.
        let cops_flat="Cops Seasons 1-24/s6e10.avi";
        let cops_nested="Cops Seasons 1-24/6/s6e10.avi";
        let flat=matchlab_parser::parse_title(cops_flat,false).unwrap();
        let nested=matchlab_parser::parse_title(cops_nested,false).unwrap();
        assert_eq!(nested["title"],flat["title"]);
        assert_eq!(nested["seasons"],json!([6]));
        assert_eq!(nested["episodes"],json!([10]));
        assert_eq!(nested["path_title_candidates"],json!(["Cops","6"]));
        for release in [cops_flat,cops_nested,
            "Cops Seasons 1-24/Unknown Folder/6/s6e10.avi"]{
            assert_eq!(select(release),"tt0500002","{release}");
        }
        assert_eq!(select(
            "Different Show Season 9/The Simpsons Season 9/The.Simpsons.S09E24.mkv"),
            "tt0096697");
        // A verified leaf now takes priority over a different parent.
        assert_eq!(select("Different Show Season 9/The.Simpsons.S09E24.mkv"),
            "tt0096697");
        assert_eq!(select("Different Show Season 9/S09E24 Lost Our Lisa.mkv"),
            "tt0500001");
        assert_eq!(select("Videos/The.Simpsons.S09E24.mkv"),"tt0096697");
        assert_eq!(select("Different Show Season 9/The.Simpsons.S09.mkv"),
            "tt0096697");
        assert_eq!(select("Different Show Season 9/Sword.Art.Online.S01E01.mkv"),
            "tt0300001");

        drop(db);
        let d=diagnose(&path,&MatchRequest{title:release.into(),
            year:None,series:None}).unwrap();
        assert_eq!(d.parsed["title"],"The Simpsons");
        assert_eq!(d.parsed["seasons"],json!([9]));
        assert_eq!(d.parsed["episodes"],json!([24]));
        let selected=d.selected.expect("parent's The Simpsons IMDb title");
        assert_eq!(selected["imdb_id"],"tt0096697");
        assert_eq!(selected["season"],9);
        assert_eq!(selected["episode"],24);
        assert_eq!(public_match_response(Some(selected))["sid_start"],
            "tt0096697:9:24");

        // An independently matched leaf wins over a different series parent;
        // its parser snapshot and normalized identity must agree.
        let release="Different Show Season 9/The.Simpsons.S09E24.mkv";
        let leaf=diagnose(&path,&MatchRequest{
            title:release.into(),year:None,series:None}).unwrap();
        assert_eq!(leaf.normalized_title,"thesimpsons");
        assert_eq!(leaf.selected.as_ref().unwrap()["imdb_id"],"tt0096697");

        // Two episode files share parsed folder components, never the final
        // episode identity, even when ML persistent cache is disabled.
        for (leaf,sid) in [("S09E23.mkv","tt0096697:9:23"),
                            ("S09E24.mkv","tt0096697:9:24")]{
            let fingerprint=learning_cache::match_fingerprint(&path);
            let cached=whole_torrent_cached_diagnose(&path,&MatchRequest{
                title:format!("The Simpsons Season 9/{leaf}"),
                year:None,series:None},fingerprint.as_deref()).unwrap();
            assert_eq!(cached.selected.unwrap()["sid"],sid);
        }

        for release in ["Cops Seasons 1-24/s6e10.avi",
            "Cops Seasons 1-24/6/s6e10.avi",
            "Cops Seasons 1-24/Unknown Folder/6/s6e10.avi"]{
            let got=diagnose(&path,&MatchRequest{
                title:release.into(),year:None,series:None}).unwrap();
            let selected=got.selected.as_ref().unwrap_or_else(||panic!("{release}: {got:?}"));
            assert_eq!(selected["imdb_id"],"tt0500002","{release}");
            assert_eq!(selected["season"],6,"{release}");
            assert_eq!(selected["episode"],10,"{release}");
        }

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
            INSERT INTO seasons VALUES('tt0300001',4);
            INSERT INTO seasons VALUES('tt0300002',3);
            INSERT INTO season_episodes VALUES('tt0300001',1,12,12);
            INSERT INTO season_episodes VALUES('tt0300001',3,25,25);
            INSERT INTO season_episodes VALUES('tt0300001',4,12,12);
            INSERT INTO season_episodes VALUES('tt0300002',1,12,12);
            INSERT INTO anime_series_map VALUES('tt0300001','100','456',0);
            INSERT INTO anime_series_map VALUES('tt0300001','101','456',0);
            INSERT INTO anime_series_map VALUES('tt0300001','102','456',0);
            INSERT INTO anime_ids VALUES('tt0300001','anidb','100');
            INSERT INTO anidb_titles VALUES('100','Sword Art Online','swordartonline','main','en');
            INSERT INTO anidb_titles VALUES('101','Sword Art Online Alicization',
                'swordartonlinealicization','main','en');
            INSERT INTO anidb_titles VALUES('102','Sword Art Online Alicization Part 2',
                'swordartonlinealicizationpart2','main','en');
            INSERT INTO episode_map VALUES('100','456',1,1,1,12,0,'');
            INSERT INTO episode_map VALUES('101','456',1,3,1,12,0,'');
            INSERT INTO episode_map VALUES('102','456',1,4,1,12,0,'');").unwrap();
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

        // A numeric video filename gets its identity from two independently
        // parsed parent folders, but only when the combined name resolves to
        // an exact verified AniDB work and a real season crosswalk.
        for raw in [
            "[HorribleSubs] Sword Art Online/Alicization/02 [1080p].mkv",
            "Sword Art Online/Alicization/02.mkv",
        ] {
            let nested=find(raw);
            assert_eq!(nested.parsed["title"],"Sword Art Online - Alicization","{raw}");
            assert_eq!(nested.parsed["anime_title_verified"],true,"{raw}");
            let selected=nested.selected.expect("verified named-part video");
            assert_eq!(selected["imdb_id"],"tt0300001","{raw}");
            assert_eq!(selected["season"],3,"{raw}");
            assert_eq!(selected["episode"],2,"{raw}");
            assert_eq!(selected["anime"]["asid"],"101:2","{raw}");
            assert_eq!(public_match_response(Some(selected))["sid_start"],
                "tt0300001:3:2","{raw}");
        }

        // No season or episode token: the exact AniDB part title alone
        // identifies IMDb S03. The public season pack must use the *IMDb*
        // episode upper bound (25, with a recap in this fixture), not the
        // shorter AniDB range (only episodes 1..12 have crosswalk evidence).
        for raw in [
            "Sword Art Online Alicization",
            "Sword Art Online - Alicization",
            "[HorribleSubs] Sword Art Online - Alicization [1080p]",
        ] {
            let d=find(raw);
            let winner=d.selected.expect("title-only named anime part");
            assert_eq!(winner["imdb_id"],"tt0300001","{raw}: {winner}");
            assert_eq!(winner["season"],3,"{raw}: {winner}");
            assert_eq!(winner["episode"],1,"{raw}: {winner}");
            assert_eq!(winner["episode_end"],25,"{raw}: {winner}");
            let public=public_match_response(Some(winner));
            assert_eq!(public, json!({
                "sid_start":"tt0300001:3:1",
                "sid_end":"tt0300001:3:25",
                "asid_start":null,"asid_end":null
            }),"{raw}");
        }

        // Distinct local-vs-global S numbering is handled by the exact
        // AniDB part crosswalk, never by arithmetic alone.
        for (raw,season,asid) in [
            ("Sword Art Online - Alicization S2E02 [1080p].mkv",4,"102:2"),
            ("Sword Art Online - Alicization S3E02 [1080p].mkv",3,"101:2"),
        ]{
            let d=find(raw);
            assert_eq!(d.parsed["anime_local_part_verified"]["base_season"],3,
                "{raw}: {:?}",d.parsed);
            assert_eq!(d.parsed["seasons"],json!([season]),"{raw}");
            let winner=d.selected.expect("verified named-part local coordinate");
            assert_eq!(winner["imdb_id"],"tt0300001","{raw}: {winner}");
            assert_eq!(winner["season"],season,"{raw}: {winner}");
            assert_eq!(winner["episode"],2,"{raw}: {winner}");
            assert_eq!(winner["anime"]["asid"],asid,"{raw}: {winner}");
        }
        for (raw,season) in [
            ("Sword Art Online - Alicization P2 [1080p].mkv",4),
            ("Sword Art Online - Alicization S2 [1080p].mkv",4),
            ("Sword Art Online - Alicization S3 [1080p].mkv",3),
        ]{
            let d=find(raw);
            assert_eq!(d.parsed["seasons"],json!([season]),"{raw}");
            assert_eq!(d.parsed["episodes"],json!([]),"{raw}");
            let winner=d.selected.expect("verified named-part season pack");
            assert_eq!(winner["season"],season,"{raw}: {winner}");
            assert_eq!(winner["season_end"],season,"{raw}: {winner}");
            let public=public_match_response(Some(winner));
            assert_eq!(public["sid_start"],format!("tt0300001:{season}:1"),"{raw}");
            assert_eq!(public["sid_end"],format!("tt0300001:{season}:{}",if season==3{25}else{12}),"{raw}");
        }

        for (raw,season) in [
            ("[SubsPlease] Sword Art Online - Alicization S2 (01-12) (720p) [Batch]",4),
            ("[SubsPlease] Sword Art Online - Alicization P2 (01-12) (720p) [Batch]",4),
            ("[SubsPlease] Sword Art Online - Alicization S3 (01-12) (720p) [Batch]",3),
        ]{
            let d=find(raw);
            assert_eq!(d.parsed["anime_local_part_verified"]["base_season"],3,
                "{raw}: {:?}",d.parsed);
            let winner=d.selected.expect("verified local anime batch");
            assert_eq!(winner["season"],season,"{raw}: {winner}");
            assert_eq!(winner["season_end"],season,"{raw}: {winner}");
            assert_eq!(winner["episode"],1,"{raw}: {winner}");
            assert_eq!(winner["episode_end"],12,"{raw}: {winner}");
            let response=public_match_response(Some(winner));
            assert_eq!(response["sid_start"],format!("tt0300001:{season}:1"),"{raw}");
            assert_eq!(response["sid_end"],format!("tt0300001:{season}:12"),"{raw}");
        }
        // If the named part is real but a requested local season has no
        // mapped sibling, do NOT reinterpret it as global S04 just because
        // the IMDb work has S04. Preserve the identity, no fake SID/ASID.
        let unresolved=find("Sword Art Online - Alicization S4E02 [1080p].mkv");
        assert_eq!(unresolved.parsed["anime_local_part_unresolved"]["base_season"],
            3,"{:?}",unresolved.parsed);
        let unresolved=unresolved.selected.expect("retain exact anime identity");
        assert_eq!(unresolved["imdb_id"],"tt0300001");
        assert!(unresolved["season"].is_null(),"{unresolved}");
        assert!(unresolved["episode"].is_null(),"{unresolved}");
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
    fn bluray_stream_file_uses_movie_folder_not_stream_or_numeric_leaf(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt0241527','Harry Potter and the Sorcer''s Stone',
                    'harrypotterthesorcerersstone',2001,'movie','US',1,0,'movie'),
                ('tt3333333','Stream','stream',2024,'movie','US',1,0,'movie'),
                ('tt4444444','0000','0000',2000,'movie','US',1,0,'movie');
            INSERT INTO imdb_ratings VALUES
                ('tt0241527',5),('tt3333333',10000000),('tt4444444',10000000);")
            .unwrap();
        for path in [
            "Harry Potter and the Sorcerer's Stone/stream/0000.m2ts",
            "Harry Potter and the Sorcerer's Stone/BDMV/STREAM/00000.m2ts",
            "Harry Potter (2001-2012)/Harry Potter and the Sorcerer's Stone/BDMV/STREAM/00000.m2ts",
        ]{
            let parsed=matchlab_parser::parse_title(path,false).unwrap();
            assert_eq!(parsed["title"],"Harry Potter and the Sorcerer's Stone",
                "{path}: {parsed:?}");
            assert!(parsed.get("year").is_none(),"{path}: {parsed:?}");
            assert!(matchlab_parser::bluray_movie_stream_path(path));
            let gate=ImdbIdentityGate{
                policy:MatchPolicy::PublicApi,strict:true,year:None,
                has_season:false,has_quality:false,
                collection_year_range:movie_collection_year_range(&parsed),
                verified_bluray_stream:true,season_range_end:None,
                verified_anime_range:None,verified_absolute_episode:None,
            };
            let found=bottom_up_imdb_path_match(&db,&parsed,None,false,0,
                false,false,|hits|filter_request_imdb_candidates(&db,hits,gate))
                .unwrap().unwrap_or_else(||panic!("failed disc movie: {path}"));
            assert_eq!(found.0,"Harry Potter and the Sorcerer's Stone");
            assert_eq!(found.1[0].imdb_id,"tt0241527",
                "{path} must not match STREAM or 0000 despite more votes");
        }
        let path="Harry Potter and the Sorcerer's Stone/stream/0000.m2ts";
        let parsed=matchlab_parser::parse_title(path,false).unwrap();
        let gate=ImdbIdentityGate{
            policy:MatchPolicy::PublicApi,strict:true,year:None,
            has_season:false,has_quality:false,collection_year_range:None,
            verified_bluray_stream:true,season_range_end:None,
            verified_anime_range:None,verified_absolute_episode:None,
        };
        db.execute("INSERT INTO titles VALUES
            ('tt5555555','Harry Potter and the Sorcer''s Stone',
             'harrypotterthesorcerersstone',2002,'movie','US',0,1,'movie')",[])
            .unwrap();
        assert!(bottom_up_imdb_path_match(&db,&parsed,None,false,0,
            false,false,|hits|filter_request_imdb_candidates(&db,hits,gate))
            .unwrap().is_none(),
            "without a production year, shared-title remakes must remain ambiguous");
    }

    #[test]
    fn strict_year_also_filters_direct_anime_priority_identities(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Anime 2005','anime2005',2005,'series','',1,0,'tvSeries'),
                ('tt2222222','Anime 2021','anime2021',2021,'series','',1,0,'tvSeries'),
                ('tt3333333','Unknown year anime','unknownyearanime',0,'series','',1,0,'tvSeries');")
            .unwrap();
        let old=json!({"imdb_id":"tt1111111","season":1,"episode":4});
        let recent=json!({"imdb_id":"tt2222222","season":1,"episode":4});
        let unknown=json!({"imdb_id":"tt3333333","season":1,"episode":4});
        assert!(strict_anime_imdb_year_eligible(&db,&recent,Some(2021),true));
        assert!(!strict_anime_imdb_year_eligible(&db,&old,Some(2021),true));
        assert!(!strict_anime_imdb_year_eligible(&db,&unknown,Some(2021),true));
        assert!(!strict_anime_imdb_year_eligible(&db,&json!({"imdb_id":"tt0000000"}),Some(2021),true));
        assert!(!strict_anime_imdb_year_eligible(&db,&json!({}),Some(2021),true));
        assert!(strict_anime_imdb_year_eligible(&db,&old,None,true));
        assert!(strict_anime_imdb_year_eligible(&db,&old,Some(2021),false));
    }

    #[test]
    fn strict_named_film_in_year_range_pack_matches_without_inventing_a_film_year(){
        let path="Harry Potter (2001-2012)/Harry Potter and the Sorcerer's Stone";
        let parsed=matchlab_parser::parse_title(path,false).unwrap();
        assert_eq!(parsed["title"],"Harry Potter and the Sorcerer's Stone",
            "use the individual film's title, not the collection name");
        assert!(parsed.get("year").is_none(),
            "a collection range cannot become a strict individual year");
        let range=movie_collection_year_range(&parsed);
        assert_eq!(range,Some((2001,2012)));
        let db=Connection::open_in_memory().unwrap();
        let candidate=|id:&str,year:i32|imdb_matcher::Match{
            imdb_id:id.into(),
            title:"Harry Potter and the Sorcerer's Stone".into(),
            matched_title:"Harry Potter and the Sorcerer's Stone".into(),
            year,category:"movie".into(),media_type:"movie".into(),
            anime_ids:std::collections::BTreeMap::new(),
        };
        let gate=ImdbIdentityGate{
            policy:MatchPolicy::PublicApi,strict:true,year:None,
            has_season:false,has_quality:false,
            collection_year_range:range,verified_bluray_stream:false,season_range_end:None,
            verified_anime_range:None,verified_absolute_episode:None,
        };
        let filtered=filter_request_imdb_candidates(&db,vec![
            candidate("tt0241527",2001),
            candidate("tt9999999",2015),
            candidate("tt8888888",1998),
        ],gate);
        assert_eq!(filtered.iter().map(|c|c.imdb_id.as_str())
            .collect::<Vec<_>>(),vec!["tt0241527"]);
        // The child year is still authoritative; a yearless child in an
        // ordinary single-year parent still inherits the exact year.
        for (path,expected) in [
            ("Harry Potter (2001-2012)/Harry Potter 1 (2001)",Some(2001)),
            ("Harry Potter (2001-2012)/Harry Potter 2 (2002)",Some(2002)),
            ("Harry Potter (2001-2012)/Harry Potter and the Sorcerer's Stone (2001)",Some(2001)),
            ("Harry Potter (2001-2012)/Harry Potter and the Sorcerer's Stone",None),
            ("Harry Potter (2001)/Harry Potter and the Sorcerer's Stone",Some(2001)),
        ]{
            let parsed=matchlab_parser::parse_title(path,false).unwrap();
            assert_eq!(parsed.get("year").and_then(Value::as_i64),
                expected.map(i64::from),"{path}: {parsed:?}");
        }
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
    fn monster_dahmer_s01e01_does_not_select_2004_anime(){
        let stamp=std::time::SystemTime::now().duration_since(
            std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-monster-dahmer-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,
                clean_title TEXT,year INTEGER,category TEXT,region TEXT,
                original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                anidb_season INTEGER,tvdb_season INTEGER,start INTEGER,
                end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,
                clean_title TEXT,title_type TEXT,language TEXT);
            INSERT INTO titles VALUES('tt0434706','Monster','monster',2004,
                'series','JP',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0434706',1);
            INSERT INTO season_episodes VALUES('tt0434706',1,74,74);
            INSERT INTO anime_series_map VALUES('tt0434706','100','100',0);
            INSERT INTO anidb_titles VALUES('100','Monster','monster','main','en');
            INSERT INTO episode_map VALUES('100','100',1,1,1,74,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let result=diagnose(&path,&MatchRequest{
            title:"monster.the.jeffrey.dahmer.story.s01.e01".into(),
            year:None,series:None
        }).unwrap();
        assert_eq!(imdb_matcher::clean_title(
            result.parsed["title"].as_str().unwrap_or("")),
            "monsterthejeffreydahmerstory",
            "the parser must preserve the full title before IMDb/AniDB matching");
        assert!(result.selected.is_none(),
            "a short anime alias cannot hijack a different long TV title");
        let valid=diagnose(&path,&MatchRequest{
            title:"monster.s01.e01".into(),year:None,series:None
        }).unwrap();
        assert_eq!(valid.selected.unwrap()["imdb_id"],"tt0434706",
            "exact Monster episodes must still be matched normally");
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
    fn frieren_absolute_33_resolves_new_anidb_work_episode_five(){
        const RAW:&str="[Kirinashi] Sousou no Frieren - 33 [2322F691].mkv";
        let parsed=matchlab_parser::parse_title(RAW,false).unwrap();
        assert_eq!(parsed["anime_episode_candidate"]["episode"],33);
        assert_eq!(parsed["anime_episode_candidate"]["numbering"],"absolute");
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt22248376',1,28,28),('tt22248376',2,10,10);
            INSERT INTO anime_series_map VALUES
                ('tt22248376','17617','424536',0),
                ('tt22248376','18886','424536',0);
            -- Original 2023 AniDB series has a stale, open-ended
            -- season-one default. The 2026 sequel is a separate AniDB
            -- work, with its own locally numbered episodes 1-10.
            INSERT INTO episode_map VALUES
                ('17617','424536',1,1,0,0,0,''),
                ('18886','424536',1,2,1,10,0,'');").unwrap();
        let mut winner=json!({
            "imdb_id":"tt22248376","category":"series",
            "title":"Sousou no Frieren","matched_title":"Sousou no Frieren",
            "anime_ids":{"anidb":"17617"},"season":1,"episode":33,
            "anime":{"anidb_id":"17617","episode":33,"asid":"17617:33"},
            "sid":"tt22248376:1:33"
        });
        reconcile_series_coordinates(&db,&mut winner,&parsed).unwrap();
        assert_eq!(winner["coordinate_fallback"],"repaired_imdb_absolute_anime");
        assert_eq!((winner["season"].as_i64(),winner["episode"].as_i64()),
            (Some(2),Some(5)));
        assert_eq!(winner["anime_ids"]["anidb"],"18886");
        assert_eq!(winner["anime"]["anidb_id"],"18886");
        assert_eq!(winner["anime"]["episode"],5);
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt22248376:2:5","sid_end":"tt22248376:2:5",
            "asid_start":"18886:5","asid_end":"18886:5"
        }));
    }

    #[test]
    fn frieren_absolute_episode_33_uses_complete_imdb_season_bounds(){
        // Exercise the exact submitted filename, including fansub prefix
        // and CRC suffix, instead of only constructing a parsed fixture.
        let release="[Kirinashi] Sousou no Frieren - 33 [2322F691].mkv";
        let parsed_release=matchlab_parser::parse_title(release,false).unwrap();
        assert_eq!(parsed_release["anime_episode_candidate"]["episode"],33,
            "absolute anime hint was lost: {parsed_release:?}");
        assert_eq!(parsed_release["anime_episode_candidate"]["numbering"],"absolute");
        assert_eq!(
            imdb_matcher::clean_title(parsed_release["anime_episode_candidate"]["title"]
                .as_str().unwrap_or("")),
            imdb_matcher::clean_title("Sousou no Frieren")
        );
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt22248376',1,28,28),('tt22248376',2,10,10);
            INSERT INTO anime_series_map VALUES('tt22248376','12345','424536',0);")
            .unwrap();
        for (absolute,expected) in [
            (28,None),(29,Some((2,1))),(33,Some((2,5))),
            (38,Some((2,10))),(39,None),
        ]{
            assert_eq!(complete_imdb_absolute_episode(
                &db,"tt22248376",absolute).unwrap(),expected);
        }
        db.execute("INSERT INTO season_episodes VALUES('tt22248376',3,12,12)",[])
            .unwrap();
        assert_eq!(complete_imdb_absolute_episode(&db,"tt22248376",39)
            .unwrap(),Some((3,1)),"all seasons use the same verified bounds");
        let mut winner=json!({
            "imdb_id":"tt22248376","category":"series","title":"Sousou no Frieren",
            "matched_title":"Sousou no Frieren","anime_ids":{"anidb":"12345"},
            "season":1,"episode":null,"anime":null,"sid":"tt22248376"
        });
        let parsed=json!({
            "title":"Sousou no Frieren","group":"Kirinashi",
            "seasons":[],"episodes":[33],
            "anime_episode_candidate":{
                "title":"Sousou no Frieren","episode":33,
                "numbering":"absolute","verified":false}
        });
        reconcile_series_coordinates(&db,&mut winner,parsed.as_object().unwrap())
            .unwrap();
        assert_eq!(winner["season"],2);
        assert_eq!(winner["episode"],5);
        assert_eq!(winner["sid"],"tt22248376:2:5");
        assert_eq!(winner["coordinate_fallback"],"complete_imdb_absolute_anime");
        assert!(winner.pointer("/anime/asid").and_then(Value::as_str).is_none());
        assert_eq!(public_match_response(Some(winner))["sid_start"],"tt22248376:2:5");
        // Missing numbers, stale counts and absent IMDb target episodes
        // must not produce a synthetic coordinate.
        db.execute("UPDATE season_episodes SET episode_count=27
            WHERE imdb_id='tt22248376' AND season=1",[]).unwrap();
        assert_eq!(complete_imdb_absolute_episode(
            &db,"tt22248376",33).unwrap(),None);
        db.execute("UPDATE season_episodes SET episode_count=28
            WHERE imdb_id='tt22248376' AND season=1",[]).unwrap();
        db.execute("DELETE FROM season_episodes WHERE season=2",[]).unwrap();
        assert_eq!(complete_imdb_absolute_episode(
            &db,"tt22248376",33).unwrap(),None);
    }

    #[test]
    fn absolute_anime_imdb_fallback_respects_explicit_seasons_and_partial_crosswalks(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt22248376',1,28,28),('tt22248376',2,10,10);
            INSERT INTO anime_series_map VALUES('tt22248376','12345','424536',0);
            INSERT INTO episode_map VALUES('12345','424536',1,2,1,4,0,'');")
            .unwrap();
        let winner=json!({
            "imdb_id":"tt22248376","category":"series","title":"Sousou no Frieren",
            "matched_title":"Sousou no Frieren","anime_ids":{"anidb":"12345"},
            "season":null,"episode":null
        });
        let parsed=json!({
            "group":"Kirinashi","seasons":[],"episodes":[33],
            "anime_episode_candidate":{"title":"Sousou no Frieren",
                "episode":33,"numbering":"absolute"}
        });
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,parsed.as_object().unwrap(),false).unwrap(),None,
            "an incomplete season-2 crosswalk must not be bypassed");
        let mut explicit=parsed.clone();
        explicit["seasons"]=json!([2]);
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,explicit.as_object().unwrap(),false).unwrap(),None);
        let mut named=parsed.clone();
        named["anime_named_part_season_hint"]=json!(2);
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,named.as_object().unwrap(),false).unwrap(),None);
    }

    #[test]
    fn black_clover_absolute_145_repairs_invalid_open_ended_s01_crosswalk(){
        let parsed=matchlab_parser::parse_title(
            "[EMBER] Black Clover - 145.mkv",false).unwrap();
        assert_eq!(parsed["title"],"Black Clover");
        assert_eq!(parsed["group"],"EMBER");
        assert_eq!(parsed["anime_episode_candidate"]["episode"],145);
        assert_eq!(parsed["anime_episode_candidate"]["numbering"],"absolute");
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt7441658',1,51,51),('tt7441658',2,51,51),
                ('tt7441658',3,52,52),('tt7441658',4,16,16);
            INSERT INTO anime_series_map VALUES('tt7441658','12665','331753',0);
            INSERT INTO episode_map VALUES('12665','331753',1,-1,1,0,0,'');
            INSERT INTO episode_map VALUES('12665','331753',1,1,1,0,0,'');")
            .unwrap();
        let original=episode_crosswalk::resolve_absolute(&db,"tt7441658",145)
            .unwrap().expect("source data has an open-ended season-1 alias");
        assert_eq!((original.season,original.episode),(1,145));
        assert_eq!(complete_imdb_absolute_episode(
            &db,"tt7441658",145).unwrap(),Some((3,43)));
        let mut winner=json!({
            "imdb_id":"tt7441658","category":"series","title":"Black Clover",
            "matched_title":"Black Clover","anime_ids":{"anidb":"12665"},
            "season":1,"episode":145,
            "anime":{"anidb_id":"12665","episode":145,"asid":"12665:145"},
            "sid":"tt7441658:1:145"
        });
        reconcile_series_coordinates(&db,&mut winner,&parsed)
            .unwrap();
        assert_eq!(winner["season"],3);
        assert_eq!(winner["episode"],43);
        assert_eq!(winner["sid"],"tt7441658:3:43");
        assert_eq!(winner["coordinate_fallback"],"repaired_imdb_absolute_anime");
        assert!(winner.pointer("/anime/asid").and_then(Value::as_str).is_none());
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt7441658:3:43","sid_end":"tt7441658:3:43",
            "asid_start":null,"asid_end":null
        }));
    }

    #[test]
    fn black_clover_absolute_145_can_map_to_separate_verified_anidb_season(){
        // Synthetic split-series fixture: AniDB ID 12665 claims a weak
        // whole-series S01 map, while a DIFFERENT independently mapped
        // AniDB work supplies a bounded S03 episode 43. In this layout
        // episode 145 is NOT episode 145 of AniDB 12665.
        let parsed=matchlab_parser::parse_title(
            "[EMBER] Black Clover - 145.mkv",false).unwrap();
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt7441658',1,51,51),('tt7441658',2,51,51),
                ('tt7441658',3,52,52),('tt7441658',4,16,16);
            INSERT INTO anime_series_map VALUES
                ('tt7441658','12665','331753',0),
                ('tt7441658','20002','331753',0);
            INSERT INTO episode_map VALUES
                ('12665','331753',1,-1,1,0,0,''),
                ('12665','331753',1,1,1,0,0,''),
                ('20002','331753',1,3,1,52,0,'');").unwrap();
        let original=episode_crosswalk::resolve_absolute(&db,"tt7441658",145)
            .unwrap().unwrap();
        assert_eq!((original.anidb_id.as_str(),original.season,original.episode),
            ("12665",1,145));
        let season_id=episode_crosswalk::resolve_season(&db,"tt7441658",3,43)
            .unwrap().unwrap();
        assert_eq!((season_id.anidb_id.as_str(),season_id.anidb_episode),
            ("20002",43));
        let mut winner=json!({
            "imdb_id":"tt7441658","category":"series","title":"Black Clover",
            "matched_title":"Black Clover","anime_ids":{"anidb":"12665"},
            "season":1,"episode":145,
            "anime":{"anidb_id":"12665","episode":145,"asid":"12665:145"},
            "sid":"tt7441658:1:145"
        });
        reconcile_series_coordinates(&db,&mut winner,&parsed).unwrap();
        assert_eq!(winner["coordinate_fallback"],"repaired_imdb_absolute_anime");
        assert_eq!(winner["anime"]["anidb_id"],"20002");
        assert_eq!(winner["anime"]["episode"],43);
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt7441658:3:43","sid_end":"tt7441658:3:43",
            "asid_start":"20002:43","asid_end":"20002:43"
        }));
        // More than one distinct finite season work makes the source
        // ambiguous; never pick the first ID.
        db.execute("INSERT INTO anime_series_map VALUES
            ('tt7441658','20003','331753',0)",[]).unwrap();
        db.execute("INSERT INTO episode_map VALUES
            ('20003','331753',1,3,1,52,0,'')",[]).unwrap();
        let mut ambiguous=json!({
            "imdb_id":"tt7441658","category":"series","title":"Black Clover",
            "matched_title":"Black Clover","anime_ids":{"anidb":"12665"},
            "season":1,"episode":145,
            "anime":{"anidb_id":"12665","episode":145,"asid":"12665:145"}
        });
        reconcile_series_coordinates(&db,&mut ambiguous,&parsed).unwrap();
        assert!(ambiguous["episode"].is_null(),
            "ambiguous season mapping must not fabricate coordinates");
        assert!(ambiguous.pointer("/anime/asid").and_then(Value::as_str).is_none());
    }

    #[test]
    fn absolute_anime_fallback_requires_a_group_and_complete_season_counts(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt7441658',1,51,51),('tt7441658',2,51,51),
                ('tt7441658',3,52,52);
            INSERT INTO anime_series_map VALUES('tt7441658','12665','331753',0);")
            .unwrap();
        let winner=json!({"imdb_id":"tt7441658","title":"Black Clover",
            "matched_title":"Black Clover","anime_ids":{"anidb":"12665"},
            "season":null,"episode":null});
        let mut parsed=json!({"seasons":[],"episodes":[145],
            "anime_episode_candidate":{"title":"Black Clover",
            "episode":145,"numbering":"absolute"}});
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,parsed.as_object().unwrap(),false).unwrap(),None,
            "bare ungrouped episode has insufficient independent evidence");
        parsed["group"]=json!("EMBER");
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,parsed.as_object().unwrap(),false).unwrap(),Some((3,43)));
        db.execute("UPDATE season_episodes SET episode_count=50
            WHERE imdb_id='tt7441658' AND season=2",[]).unwrap();
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,parsed.as_object().unwrap(),false).unwrap(),None);
        db.execute("UPDATE season_episodes SET episode_count=51
            WHERE imdb_id='tt7441658' AND season=2",[]).unwrap();
        // A target-season crosswalk that resolves to a *different* AniDB
        // absolute episode is not corroboration for release episode 145.
        db.execute("INSERT INTO episode_map VALUES
            ('12665','331753',1,3,1,52,0,'')",[]).unwrap();
        assert_eq!(verified_unmapped_anime_absolute_episode(
            &db,&winner,parsed.as_object().unwrap(),false).unwrap(),None);
    }

    #[test]
    fn finite_anime_crosswalk_must_not_be_repaired_by_imdb_season_arithmetic(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
                imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(
                imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(
                anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
                start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO season_episodes VALUES
                ('tt7441658',1,51,51),('tt7441658',2,51,51),
                ('tt7441658',3,52,52);
            INSERT INTO anime_series_map VALUES('tt7441658','12665','331753',0);
            INSERT INTO episode_map VALUES
                ('12665','331753',1,-1,1,0,0,''),
                ('12665','331753',1,1,1,0,0,''),
                ('12665','331753',1,1,100,170,0,'');").unwrap();
        let parsed=matchlab_parser::parse_title("[EMBER] Black Clover - 145.mkv",false)
            .unwrap();
        let mut winner=json!({"imdb_id":"tt7441658","category":"series",
            "title":"Black Clover","matched_title":"Black Clover",
            "anime_ids":{"anidb":"12665"},"season":1,"episode":145,
            "anime":{"anidb_id":"12665","episode":145,"asid":"12665:145"},
            "sid":"tt7441658:1:145"});
        reconcile_series_coordinates(&db,&mut winner,&parsed).unwrap();
        assert!(winner["season"].is_null(),
            "finite crosswalk cannot be overridden by season arithmetic");
        assert!(winner["episode"].is_null());
        assert_eq!(winner["coordinate_fallback_blocked"],
            "anime_crosswalk_exceeds_known_imdb_bounds");
    }

    #[test]
    fn hanako_sequel_and_berserk_bd_filenames_preserve_episode_hints(){
        let hanako="[Erai-raws] Jibaku Shounen Hanako-kun 2 - 12 [720p CR WEB-DL AVC AAC][MultiSub][B63D35A4].mkv";
        let h=matchlab_parser::parse_title(hanako,false).unwrap();
        assert_eq!(h["anime_season_episode_candidate"]["title"],
            "Jibaku Shounen Hanako-kun","Hanako: {h:?}");
        assert_eq!(h["anime_season_episode_candidate"]["season"],2,"{h:?}");
        assert_eq!(h["anime_season_episode_candidate"]["episode"],12,"{h:?}");
        let b=matchlab_parser::parse_title("Berserk 2016 BD 01-12",false).unwrap();
        assert_eq!(b["anime_episode_range_candidate"]["start"],1,"{b:?}");
        assert_eq!(b["anime_episode_range_candidate"]["end"],12,"{b:?}");
        assert_eq!(b["anime_episode_range_candidate"]["title"],"Berserk",
            "BD and production year must not pollute candidate title: {b:?}");
    }

    #[test]
    fn hanako_second_season_fansub_maps_exact_hyphenated_title_to_episode(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-hanako-s2-{stamp}.sqlite3"));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,
                clean_title TEXT,year INTEGER,category TEXT,region TEXT,
                original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt10777610','Toilet-bound Hanako-kun','toiletboundhanakokun',
                    2020,'series','',1,0,'tvSeries'),
                ('tt10777610','Jibaku Shounen Hanako-kun','jibakushounenhanakokun',
                    2020,'series','',0,1,'tvSeries');
            INSERT INTO seasons VALUES('tt10777610',2);
            INSERT INTO season_episodes VALUES
                ('tt10777610',1,12,12),('tt10777610',2,24,24);
            INSERT INTO anime_series_map VALUES
                ('tt10777610','100','368364',0),
                ('tt10777610','101','368364',0);
            INSERT INTO anime_ids VALUES('tt10777610','anidb','100');
            INSERT INTO anidb_titles VALUES
                ('100','Jibaku Shounen Hanako-kun','jibakushounenhanakokun','main','en'),
                ('101','Jibaku Shounen Hanako-kun 2','jibakushounenhanokokun2','main','en');
            UPDATE anidb_titles SET clean_title='jibakushounenhanakokun2'
                WHERE anidb_id='101';
            INSERT INTO episode_map VALUES
                ('100','368364',1,1,1,12,0,''),
                ('101','368364',1,2,1,12,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let raw="[Erai-raws] Jibaku Shounen Hanako-kun 2 - 12 [720p CR WEB-DL AVC AAC][MultiSub][B63D35A4].mkv";
        let result=diagnose(&path,&MatchRequest{
            title:raw.into(),year:None,series:None}).unwrap();
        assert_eq!(result.parsed["anime_season_episode_candidate"]["title"],
            "Jibaku Shounen Hanako-kun","{:?}",result.parsed);
        let winner=result.selected.expect("exact named sequel IMDb identity");
        assert_eq!(winner["imdb_id"],"tt10777610");
        assert_eq!(winner["season"],2,"{winner:?}");
        assert_eq!(winner["episode"],12,"{winner:?}");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt10777610:2:12","sid_end":"tt10777610:2:12",
            "asid_start":"101:12","asid_end":"101:12"
        }));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn berserk_2016_bd_range_uses_first_year_full_imdb_season_when_anime_map_absent(){
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-berserk-bd-{stamp}.sqlite3"));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,
                clean_title TEXT,year INTEGER,category TEXT,region TEXT,
                original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt5847454','Berserk','berserk',2016,'series','',1,0,'tvSeries'),
                ('tt0318871','Berserk','berserk',1997,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt5847454',2),('tt0318871',1);
            INSERT INTO season_episodes VALUES
                ('tt5847454',1,12,12),('tt5847454',2,12,12),
                ('tt0318871',1,25,25);
            INSERT INTO anime_series_map VALUES
                ('tt5847454','2400','307111',0),
                ('tt0318871','2401','73752',0);
            INSERT INTO anime_ids VALUES('tt5847454','anidb','2400');
            INSERT INTO anidb_titles VALUES
                ('2400','Berserk','berserk','main','en'),
                ('2401','Berserk','berserk','main','en');
            INSERT INTO episode_map VALUES
                ('2400','307111',1,-1,1,0,0,''),
                ('2401','73752',1,1,1,25,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let release="Berserk 2016 BD 01-12";
        let result=diagnose(&path,&MatchRequest{
            title:release.into(),year:None,series:None}).unwrap();
        let winner=result.selected.expect("2016 IMDb series title");
        assert_eq!(winner["imdb_id"],"tt5847454","{winner:?}");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt5847454:1:1","sid_end":"tt5847454:1:12",
            "asid_start":null,"asid_end":null
        }));
        // Without release year, the two identically named anime series
        // must not be coerced into the 2016 full-season range.
        let ambiguous=diagnose(&path,&MatchRequest{
            title:"Berserk BD 01-12".into(),year:None,series:None}).unwrap();
        let ambiguous_public=public_match_response(ambiguous.selected);
        assert_ne!(ambiguous_public["sid_end"],"tt5847454:1:12");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn series_coordinate_contract_preserves_single_episode_and_season(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
            imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt1234567',1,24,24);").unwrap();
        let input=|season:Value,episode:Value|json!({
            "imdb_id":"tt1234567","category":"series","season":season,"episode":episode,
            "sid":"tt1234567","anime_ids":{}
        });
        let parse=|season:Value,episode:Value|{
            let v=json!({"seasons":season,"episodes":episode});
            v.as_object().unwrap().clone()
        };
        let mut explicit=input(Value::Null,Value::Null);
        reconcile_series_coordinates(&db,&mut explicit,
            &parse(json!([17]),json!([1]))).unwrap();
        assert_eq!(explicit["season"],17);
        assert_eq!(explicit["episode"],1);
        assert_eq!(explicit["sid"],"tt1234567:17:1");
        let mut only_episode=input(Value::Null,Value::Null);
        reconcile_series_coordinates(&db,&mut only_episode,
            &parse(json!([]),json!([4]))).unwrap();
        assert_eq!(only_episode["season"],1);
        assert_eq!(only_episode["episode"],4);
        assert_eq!(only_episode["coordinate_fallback"],"season_one_from_episode");
        assert_eq!(public_match_response(Some(only_episode))["sid_start"],"tt1234567:1:4");
        let mut season_only=input(Value::Null,Value::Null);
        reconcile_series_coordinates(&db,&mut season_only,
            &parse(json!([5]),json!([]))).unwrap();
        assert_eq!(season_only["season"],5);
        assert!(season_only["episode"].is_null());
        assert_eq!(public_match_response(Some(season_only)),json!({
            "sid_start":"tt1234567","sid_end":"tt1234567",
            "asid_start":null,"asid_end":null
        }));
        let mut neither=input(Value::Null,Value::Null);
        reconcile_series_coordinates(&db,&mut neither,
            &parse(json!([]),json!([]))).unwrap();
        assert!(neither["season"].is_null());
        assert!(neither["episode"].is_null());
    }
    #[test]
    fn special_episode_season_zero_survives_imdb_reconciliation(){
        let db=Connection::open_in_memory().unwrap();
        let mut winner=json!({"imdb_id":"tt1234567","category":"series",
            "season":null,"episode":null,"anime_ids":{}});
        let parsed=json!({"seasons":[0],"episodes":[2],
            "special_episode":true,"special_type":"ova"});
        reconcile_series_coordinates(
            &db,&mut winner,parsed.as_object().unwrap()).unwrap();
        assert_eq!(winner["season"],0);
        assert_eq!(winner["episode"],2);
        assert_eq!(winner["sid"],"tt1234567:0:2");
        let response=public_match_response(Some(winner));
        assert_eq!(response["sid_start"],"tt1234567:0:2");
        assert_eq!(response["sid_end"],"tt1234567:0:2");
    }
    #[test]
    fn special_without_episode_keeps_imdb_identity_not_fabricated_e01(){
        let db=Connection::open_in_memory().unwrap();
        let mut winner=json!({"imdb_id":"tt1234567","category":"series",
            "season":null,"episode":null,"anime_ids":{}});
        let parsed=json!({"seasons":[0],"episodes":[],
            "special_episode":true,"special_type":"oad"});
        reconcile_series_coordinates(
            &db,&mut winner,parsed.as_object().unwrap()).unwrap();
        assert_eq!(winner["season"],0);
        assert!(winner["episode"].is_null());
        let response=public_match_response(Some(winner));
        assert_eq!(response["sid_start"],"tt1234567");
        assert_eq!(response["sid_end"],"tt1234567");
    }
    #[test]
    fn series_fallback_keeps_verified_anime_mapping_and_anidb_only_results(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
            imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt1234567',1,26,26);").unwrap();
        let parsed=json!({"seasons":[1],"episodes":[320]});
        let mut verified=json!({"imdb_id":"tt1234567","category":"series",
            "season":15,"episode":4,
            "anime":{"asid":"42:320"},"anime_ids":{"anidb":"42"}});
        reconcile_series_coordinates(&db,&mut verified,parsed.as_object().unwrap()).unwrap();
        assert_eq!(verified["season"],15);
        assert_eq!(verified["episode"],4);
        let no_season=json!({"seasons":[],"episodes":[320]});
        let mut unsafe_absolute=json!({"imdb_id":"tt1234567","category":"series",
            "season":null,"episode":null,"anime_ids":{"anidb":"42"}});
        reconcile_series_coordinates(&db,&mut unsafe_absolute,
            no_season.as_object().unwrap()).unwrap();
        assert!(unsafe_absolute["season"].is_null());
        assert!(unsafe_absolute["episode"].is_null());
        assert_eq!(unsafe_absolute["coordinate_fallback_blocked"],
            "episode_exceeds_known_bounds_or_unresolved_anime_crosswalk");
        let mut anidb_only=json!({"imdb_id":null,"category":"series",
            "season":null,"episode":null,"anime":{"asid":"42:4"},
            "anidb_independent_verified":true});
        reconcile_series_coordinates(&db,&mut anidb_only,parsed.as_object().unwrap()).unwrap();
        assert_eq!(anidb_only["imdb_id"],Value::Null);
        assert!(anidb_only["season"].is_null());
        assert!(anidb_only["episode"].is_null());
        let ids=public_match_response(Some(anidb_only));
        assert!(ids["sid_start"].is_null());
        assert_eq!(ids["asid_start"],"42:4");
    }
    /// Detective Conan: Anime-Lists splits AniDB 266 into TVDB seasons
    /// (S31 = 1068-1108, S32 = 1109-1147), but IMDb groups the same episodes
    /// into much shorter seasons. "Detective Conan - 1120" therefore must
    /// not become IMDb S32E12; the verified ASID 266:1120 is kept.
    #[test]
    fn crosswalk_season_with_a_different_imdb_season_structure_keeps_only_the_asid(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,
                category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO seasons VALUES('tt0131179',54),('tt2098220',1);
            INSERT INTO season_episodes VALUES('tt0131179',31,16,16),('tt0131179',32,15,15),
                ('tt2098220',1,148,148);
            INSERT INTO anime_series_map VALUES('tt0131179','266','72454',1),
                ('tt2098220','8550','252322',1);
            INSERT INTO episode_map VALUES
                ('266','72454',1,31,1068,1108,-1067,''),
                ('266','72454',1,32,1109,1147,-1108,''),
                ('8550','252322',1,1,1,58,0,''),
                ('8550','252322',1,2,59,136,-58,'');").unwrap();
        for (title,imdb,anidb,season,episode,absolute) in [
            ("Detective Conan - 1120","tt0131179","266",32,12,1120),
            ("[HorribleSubs] Hunter X Hunter - 125 [1080p].mkv","tt2098220","8550",2,67,125),
        ]{
            let parsed=matchlab_parser::parse_title(title,false).unwrap();
            let asid=format!("{anidb}:{absolute}");
            let mut winner=json!({"imdb_id":imdb,"category":"series",
                "title":"x","matched_title":"x","anime_ids":{"anidb":anidb},
                "season":season,"episode":episode,
                "anime":{"anidb_id":anidb,"episode":absolute,"asid":asid},
                "sid":format!("{imdb}:{season}:{episode}")});
            reconcile_series_coordinates(&db,&mut winner,&parsed).unwrap();
            assert_eq!(winner["coordinate_fallback_blocked"],
                "anime_crosswalk_exceeds_known_imdb_bounds","{title}: {winner}");
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":imdb,"sid_end":imdb,"asid_start":asid,"asid_end":asid}),"{title}");
        }
        // An aligned season (equal lengths) is still published.
        db.execute_batch("UPDATE season_episodes SET max_episode=41 WHERE season=31;
            UPDATE season_episodes SET max_episode=39 WHERE season=32;").unwrap();
        let parsed=matchlab_parser::parse_title("Detective Conan - 1120",false).unwrap();
        let mut aligned=json!({"imdb_id":"tt0131179","category":"series",
            "title":"x","matched_title":"x","anime_ids":{"anidb":"266"},
            "season":32,"episode":12,
            "anime":{"anidb_id":"266","episode":1120,"asid":"266:1120"},
            "sid":"tt0131179:32:12"});
        reconcile_series_coordinates(&db,&mut aligned,&parsed).unwrap();
        assert_eq!(aligned["season"],32);
        assert_eq!(aligned["episode"],12);
    }

    #[test]
    fn final_anime_coordinates_reject_known_imdb_bounds_without_guessing_a_replacement(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE season_episodes(
            imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            INSERT INTO season_episodes VALUES('tt4958580',1,24,24);").unwrap();
        let parsed=json!({"seasons":[],"episodes":[17]});
        let mapped=|season,episode|json!({"imdb_id":"tt4958580","category":"series",
            "season":season,"episode":episode,"sid":format!("tt4958580:{season}:{episode}"),
            "anime":{"anidb_id":"11602","episode":17,"asid":"11602:17"},
            "anime_ids":{"anidb":"11602"}});
        let mut impossible=mapped(1,29);
        reconcile_series_coordinates(&db,&mut impossible,parsed.as_object().unwrap()).unwrap();
        assert_eq!(impossible["coordinate_fallback_blocked"],
            "anime_crosswalk_exceeds_known_imdb_bounds");
        assert_eq!(public_match_response(Some(impossible)),json!({
            "sid_start":"tt4958580","sid_end":"tt4958580",
            "asid_start":null,"asid_end":null
        }),"a rejected offset must not survive in either public endpoint or become guessed S1E17");

        let mut boundary=mapped(1,24);
        reconcile_series_coordinates(&db,&mut boundary,parsed.as_object().unwrap()).unwrap();
        assert_eq!(boundary["season"],1);
        assert_eq!(boundary["episode"],24);
        assert_eq!(boundary["anime"]["asid"],"11602:17");
        let mut unknown_season=mapped(2,29);
        reconcile_series_coordinates(&db,&mut unknown_season,parsed.as_object().unwrap()).unwrap();
        assert_eq!(unknown_season["season"],2);
        assert_eq!(unknown_season["episode"],29);
        assert_eq!(unknown_season["anime"]["asid"],"11602:17",
            "missing IMDb bounds must not invalidate independently mapped coordinates");
    }
    #[test]
    fn unresolved_season_range_retains_identity_without_fake_episode(){
        let selected=json!({"imdb_id":"tt1234567","category":"series",
            "season":2,"episode":null,"season_end":2,"episode_end":null,
            "episode_range":{"complete":false,"segments":[],
                "start_season":2,"end_season":2}});
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt1234567","sid_end":"tt1234567",
            "asid_start":null,"asid_end":null
        }));
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
    fn large_shortlist_season_gate_matches_original_scalar_queries(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE seasons(
            imdb_id TEXT PRIMARY KEY,max_season INTEGER);").unwrap();
        let mut hits=Vec::new();
        for n in 0..345 {
            let id=format!("tt{n:07}");
            if n%8!=0 {
                db.execute("INSERT INTO seasons VALUES(?1,?2)",
                    rusqlite::params![id,if n%3==0{12}else{5}]).unwrap();
            }
            let hit=imdb_matcher::Match{
                imdb_id:id.clone(),title:"Series".into(),year:2020,
                category:"series".into(),matched_title:"Series".into(),
                anime_ids:std::collections::BTreeMap::new(),media_type:"tvSeries".into(),
            };
            hits.push(hit.clone());
            if n%17==0 {hits.push(hit);}
        }
        let expected=hits.iter().filter(|candidate|
            db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
                [&candidate.imdb_id],|row|row.get::<_,i64>(0))
                .is_ok_and(|max|max>=9))
            .map(|candidate|candidate.imdb_id.clone()).collect::<Vec<_>>();
        let gate=ImdbIdentityGate{
            policy:MatchPolicy::DashboardDiagnostic,strict:false,
            year:None,has_season:true,has_quality:true,
            collection_year_range:None,verified_bluray_stream:false,season_range_end:Some(9),verified_anime_range:None,
            verified_absolute_episode:None,
        };
        let actual=filter_request_imdb_candidates(&db,hits,gate)
            .into_iter().map(|c|c.imdb_id).collect::<Vec<_>>();
        assert_eq!(actual,expected);
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
    fn cached_diagnostics_roundtrip_without_static_string_lifetimes(){
        let sample=Diagnostic{
            parsed:json!({"title":"Example","seasons":[1],"episodes":[7]}),
            normalized_title:"example".into(),
            imdb_candidates:vec![Candidate{
                imdb_id:"tt1234567".into(),title:"Example".into(),year:2022,
                media_type:"tvSeries".into(),matched_title:"Example".into(),
                anime_ids:std::collections::BTreeMap::new(),source:"imdb".into()
            }],
            anime_candidate:None,anidb_candidate:None,selected:Some(json!({
                "imdb_id":"tt1234567","season":1,"episode":7
            })),reason:"Verified".into(),
        };
        let restored:Diagnostic=serde_json::from_value(
            serde_json::to_value(&sample).unwrap()).unwrap();
        assert_eq!(restored.selected,sample.selected);
        assert_eq!(restored.imdb_candidates[0].source,"imdb");
    }
    #[test]
    fn history_retains_request_context_without_polluting_cached_diagnostics(){
        let sample=Diagnostic{
            parsed:json!({"title":"Example","seasons":[1],"episodes":[7]}),
            normalized_title:"example".into(),imdb_candidates:vec![],
            anime_candidate:None,anidb_candidate:None,selected:None,
            reason:"Unmatched fixture".into(),
        };
        let cache=query_cache::QueryCache::new(1,Duration::from_secs(30));
        let diagnostic=cache.get_or_compute("fixture".into(),||Ok(sample),|_|true).unwrap();
        let pristine=serde_json::to_value(&diagnostic).unwrap();
        let folder=std::env::temp_dir().join(format!("matchlab-history-context-{}-{}",
            std::process::id(),std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let path=folder.join("history.sqlite3");
        dashboard_history::initialize(&path).unwrap();
        // Provenance is independent of ML enablement: both request policies
        // are always recorded by the same history decoration path.
        for (policy,source) in [(MatchPolicy::PublicApi,"public_api"),
            (MatchPolicy::DashboardDiagnostic,"dashboard_diagnostic")]{
            for (year,series) in [(Some(2005),Some(false)),(None,None),(None,Some(true))]{
                let req=MatchRequest{title:"Example.S01E07".into(),year,series};
                let details=history_details_with_request(pristine.clone(),&req,policy);
                assert_eq!(details["request_context"],json!({"year":year,"series":series}));
                assert_eq!(details["ml_source_policy"],source);
                assert_eq!(details["matcher_context"],json!({
                    "strict_mode":strict_match_mode(),
                    "semantic_revision":MATCHER_SEMANTIC_REVISION
                }));
                assert!(!details["request_context"].as_object().unwrap().contains_key("title"));
                dashboard_history::record(&path,&req.title,"unmatched",None,&details,1);
                let stored=dashboard_history::history(&path,1,0,None).unwrap();
                assert_eq!(stored["items"][0]["details"]["request_context"],
                    details["request_context"],"persist explicit nulls and false overrides");
                assert_eq!(stored["items"][0]["details"]["ml_source_policy"],source);
                assert_eq!(stored["items"][0]["details"]["matcher_context"],
                    details["matcher_context"]);
                let error=history_details_with_request(json!({"error":"fixture"}),&req,policy);
                assert_eq!(error["request_context"],details["request_context"]);
                assert_eq!(error["ml_source_policy"],source);
                assert_eq!(error["matcher_context"],details["matcher_context"]);
                assert_eq!(error["error"],"fixture");
            }
        }
        let cached=cache.get_or_compute("fixture".into(),
            ||panic!("history decoration must not invalidate the diagnostic"),|_|true).unwrap();
        assert_eq!(serde_json::to_value(&cached).unwrap(),pristine);
        assert!(pristine.get("request_context").is_none());
        assert!(pristine.get("ml_source_policy").is_none());
        assert!(pristine.get("matcher_context").is_none());
        assert_eq!(public_match_response(cached.selected),public_match_response(diagnostic.selected));
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn changed_regex_generation_invalidates_30_second_parser_memory_cache(){
        let at=Instant::now();
        assert!(cache_generation_current(4,at,4));
        assert!(!cache_generation_current(4,at,5),
            "a cached parse from an obsolete regex generation must be recomputed");
    }
    #[test]
    fn parser_snapshot_retries_changes_and_preserves_captured_provenance(){
        use std::cell::Cell;
        let generation=Cell::new(4);
        let attempts=Cell::new(0);
        let (value,saved,persistent)=stable_parse_snapshot(||generation.get(),||{
            let at=generation.get();
            attempts.set(attempts.get()+1);
            if attempts.get()==1{generation.set(5);}
            Ok((json!({"generation":at}),Some(format!("namespace-{at}"))))
        }).unwrap();
        assert_eq!(attempts.get(),2);
        assert_eq!(value["generation"],5);
        assert_eq!(saved,5);
        assert_eq!(persistent.as_deref(),Some("namespace-5"));
        generation.set(6);
        assert_eq!(saved,5,"later updates cannot relabel this cached value");
    }
    #[test]
    fn repeatedly_changing_parser_rules_fail_without_publishing_a_snapshot(){
        use std::cell::Cell;
        let generation=Cell::new(0);
        let result=stable_parse_snapshot(||generation.get(),||{
            generation.set(generation.get()+1);
            Ok((json!({"title":"unstable"}),None))
        });
        assert!(result.unwrap_err().contains("parser rules changed"));
        assert_eq!(generation.get(),2,"retries must be bounded");
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
                let selected=result.selected.unwrap_or_else(||panic!(
                    "no match: {raw}; parsed={:?}; imdb_candidates={:?}; reason={}",
                    result.parsed,result.imdb_candidates,result.reason));
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
        assert_eq!(unknown["season"],1,
            "explicit S01 survives the missing IMDb episode table");
        assert!(unknown["episode"].is_null(),
            "an unverified episode outside bounded AniDB evidence is withheld");
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
    fn unmarked_crosswalk_promotion_respects_the_top_complete_title_and_exact_anime_aliases(){
        let candidate=|id:&str,title:&str|Candidate{
            imdb_id:id.into(),title:title.into(),year:2020,media_type:"tvSeries".into(),
            matched_title:title.into(),anime_ids:Default::default(),source:"imdb".into(),
        };
        let top=candidate("tt4644488","Dragon Ball Super");
        let weaker=candidate("tt0088509","Dragon Ball");
        let exact=candidate("tt9000800","Dragon Ball Super");
        let translated=candidate("tt9000801","A Different Canonical Name");
        let mut anime=std::collections::BTreeSet::new();
        assert!(!unmarked_crosswalk_identity_eligible(Some(&top),&weaker,"Dragon Ball Super",&anime));
        assert!(unmarked_crosswalk_identity_eligible(Some(&top),&exact,"Dragon Ball Super",&anime),
            "equally complete identities retain their established episode ranking");
        anime.insert(translated.imdb_id.clone());
        assert!(unmarked_crosswalk_identity_eligible(Some(&top),&translated,"Dragon Ball Super",&anime),
            "a complete AniDB alias may name a translated canonical IMDb title or part");
        let recovered=candidate("tt9000802","Monster The Ed Gein Story");
        let shorter=candidate("tt9000803","Monster");
        assert!(unmarked_crosswalk_identity_eligible(Some(&recovered),&shorter,"Monster",&anime),
            "a lower exact short alias must not establish a new anchor over the stronger top release fit");
        assert!(!complete_candidate_alias(&candidate("tt9000804","BLEACH 千年血戦篇"),"BLEACH 中"),
            "equal ASCII keys do not prove equal Unicode titles");
        assert!(!complete_candidate_alias(&candidate("tt9000805","FooBar"),"Foo Bar"),
            "compact keys do not erase original word boundaries");
    }
    #[test]
    fn dated_identity_veto_requires_known_future_year_and_no_independent_airdate(){
        let selected=|year|json!({"category":"series","year":year});
        assert!(unresolved_date_predates_identity(&selected(2025),"2019-02-15",None));
        assert!(unresolved_date_predates_identity(&selected(2025),"2024-12-31",None),
            "a hypothetical timezone adjustment is not proof without an episode airdate");
        for year in [0,2015,2019]{
            assert!(!unresolved_date_predates_identity(&selected(year),"2019-02-15",None));
        }
        assert!(!unresolved_date_predates_identity(&selected(2025),"2019-02-15",Some(2025)));
        let special=json!({"category":"series","year":2025,"season":0,"episode":1,
            "air_date":"2024-12-31"});
        assert!(!unresolved_date_predates_identity(&special,"2024-12-31",None),
            "an independently resolved pre-premiere special remains valid");
        let timezone=json!({"category":"series","year":2025,"air_date":"2025-01-01"});
        assert!(!unresolved_date_predates_identity(&timezone,"2024-12-31",None));
        assert!(!unresolved_date_predates_identity(&selected(2025),"invalid",None));
    }
    #[test]
    fn unmarked_dragon_ball_super_keeps_its_exact_identity_without_guessed_season_coordinates(){
        // Real AnimeAPI/AnimeLists links: Super11210 has only an absolute
        // default; original231 has positive-season maps. Independent public
        // season metadata uses Super maxima14/13/19/30/55, so absolute104
        // has no verified season conversion in this imported crosswalk.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-exact-anime-identity-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
                ('tt4644488','Dragon Ball Super','dragonballsuper',2015,'series','',1,0,'tvSeries'),
                ('tt0088509','Dragon Ball','dragonball',1986,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt4644488',5),('tt0088509',6);
            INSERT INTO season_episodes VALUES
                ('tt4644488',1,14,14),('tt4644488',2,13,13),('tt4644488',3,19,19),
                ('tt4644488',4,30,30),('tt4644488',5,55,55),('tt0088509',5,31,31);
            INSERT INTO anidb_titles VALUES
                ('11210','Dragon Ball Super','dragonballsuper','main','en'),
                ('231','Dragon Ball','dragonball','main','en');
            INSERT INTO anime_series_map VALUES
                ('tt4644488','11210','295068',1),('tt0088509','231','76666',1);
            INSERT INTO episode_map VALUES
                ('11210','295068',1,-1,1,0,0,''),('231','76666',1,-1,1,0,0,''),
                ('231','76666',1,2,14,28,-13,''),('231','76666',1,3,29,68,-28,''),
                ('231','76666',1,4,69,101,-68,''),('231','76666',1,5,102,132,-101,''),
                ('231','76666',1,6,133,153,-132,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for episode in [80,104,116]{
            for raw in [format!("Dragon Ball Super - {episode} WEB-DL"),
                format!("Dragon Ball Super Episode {episode} WEB-DL")]{
                let result=diagnose(&path,&MatchRequest{title:raw.clone(),year:None,series:None}).unwrap();
                assert_eq!(public_match_response(result.selected),json!({
                    "sid_start":"tt4644488","sid_end":"tt4644488",
                    "asid_start":null,"asid_end":null
                }),"the exact sequel must not become original Dragon Ball: {raw}");
            }
        }
        let original=diagnose(&path,&MatchRequest{
            title:"Dragon Ball - 104 WEB-DL".into(),year:None,series:None}).unwrap();
        assert_eq!(public_match_response(original.selected),json!({
            "sid_start":"tt0088509:5:3","sid_end":"tt0088509:5:3",
            "asid_start":"231:104","asid_end":"231:104"
        }));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }
    #[test]
    fn anime_episode_candidate_does_not_give_a_movie_its_tvdb_siblings_episodes(){
        // Public metadata: Movie 01 (411/tt0814243) and the One Piece
        // TV series (69/tt0388629) share TVDB 81797. The film's exact
        // AniDB short alias is "One Piece Movie". Its season-zero link
        // must never borrow the series' S02 mapping for a film IMDb SID.
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-anime-movie-family-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE season_episodes(imdb_id TEXT,season INTEGER,
                episode_count INTEGER,max_episode INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,
                tvdb_season INTEGER,start INTEGER,end INTEGER,offset INTEGER,map TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
                ('tt0814243','One Piece: The Movie','onepiecethemovie',2000,'movie','',1,0,'movie'),
                ('tt0388629','One Piece','onepiece',1999,'series','',1,0,'tvSeries'),
                ('tt9000123','Linked Miniseries','linkedminiseries',2020,'series','',1,0,'tvMiniSeries'),
                ('tt9000123','Legacy Film Label','legacyfilmlabel',2020,'movie','',0,1,'movie'),
                ('tt9000125','Metadata Unclassified','metadataunclassified',2020,'series','',1,0,'');
            INSERT INTO seasons VALUES('tt0388629',3),('tt9000123',1);
            INSERT INTO season_episodes VALUES('tt0388629',2,22,22),('tt9000123',1,4,4);
            INSERT INTO anidb_titles VALUES
                ('411','One Piece Movie','onepiecemovie','short','en'),
                ('411','Typed Alias','typedalias','syn','en'),
                ('69','One Piece','onepiece','main','en'),
                ('123','Linked Miniseries','linkedminiseries','main','en'),
                ('123','Typed Alias','typedalias','syn','en'),
                ('124','Metadata Pending','metadatapending','main','en'),
                ('125','Metadata Unclassified','metadataunclassified','main','en');
            INSERT INTO anime_series_map VALUES
                ('411','tt0814243','81797',0),('69','tt0388629','81797',0),
                ('123','tt9000123','mini',1),('124','tt9000124','pending',1),
                ('125','tt9000125','unclassified',1);
            INSERT INTO episode_map VALUES
                ('411','81797',1,0,1,1,1,''),
                ('69','81797',1,1,1,8,0,''),('69','81797',1,2,9,30,-8,''),
                ('123','mini',1,1,1,3,0,''),('124','pending',1,1,1,4,0,''),
                ('125','unclassified',1,1,1,4,0,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();

        assert!(anime_candidate(&db,"One Piece Movie",None,14).unwrap().is_none());
        assert!(anime_candidate(&db,"One Piece Movie",Some(2),6).unwrap().is_none());
        let series=anime_candidate(&db,"One Piece",None,14).unwrap().unwrap();
        assert_eq!(series["imdb_id"],"tt0388629");
        assert_eq!(series["season"],2);
        assert_eq!(series["episode"],6);
        assert_eq!(anime_candidate(&db,"Linked Miniseries",Some(1),3)
            .unwrap().unwrap()["imdb_id"],"tt9000123",
            "a positively indexed series remains eligible despite an inconsistent film alias");
        assert_eq!(anime_candidate(&db,"Typed Alias",Some(1),3)
            .unwrap().unwrap()["imdb_id"],"tt9000123",
            "a rejected exact film alias must not block an equally exact verified series alias");
        let sparse=anime_candidate_evidence(&db,"Typed Alias",Some(1),4).unwrap();
        assert!(sparse.candidate.is_none());
        assert!(sparse.rejected_exact_nonseries);
        assert!(sparse.eligible_exact_ids.contains("tt9000123"),
            "an equally exact series identity remains eligible without a complete crosswalk");
        assert_eq!(anime_candidate(&db,"Metadata Pending",Some(1),3)
            .unwrap().unwrap()["imdb_id"],"tt9000124",
            "an absent IMDb catalog row must not erase verified anime-only coverage");
        assert_eq!(anime_candidate(&db,"Metadata Unclassified",Some(1),3)
            .unwrap().unwrap()["imdb_id"],"tt9000125",
            "a blank IMDb type is not positive evidence of a nonseries identity");
        drop(db);

        for raw in ["One Piece Movie S02E06","One Piece Movie Episode 14"]{
            let result=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:Some(true)}).unwrap();
            assert_eq!(public_match_response(result.selected),json!({
                "sid_start":null,"sid_end":null,"asid_start":null,"asid_end":null
            }),"the complete matcher must not replace the rejected movie with another TV SID: {raw}");
        }
        let series=diagnose(&path,&MatchRequest{
            title:"One Piece S02E06".into(),year:None,series:Some(true)}).unwrap();
        assert_eq!(public_match_response(series.selected),json!({
            "sid_start":"tt0388629:2:6","sid_end":"tt0388629:2:6",
            "asid_start":"69:14","asid_end":"69:14"
        }));
        let sparse=diagnose(&path,&MatchRequest{
            title:"Typed Alias S01E04 WEB-DL".into(),year:None,series:Some(true)}).unwrap();
        assert_eq!(public_match_response(sparse.selected),json!({
            "sid_start":"tt9000123:1:4","sid_end":"tt9000123:1:4",
            "asid_start":null,"asid_end":null
        }),"an equally exact AniDB series alias must preserve valid IMDb coordinates without inventing a sparse ASID");
        let film=diagnose(&path,&MatchRequest{
            title:"One Piece The Movie 2000 BluRay".into(),year:Some(2000),
            series:Some(false)}).unwrap();
        assert_eq!(public_match_response(film.selected),json!({
            "sid_start":"tt0814243","sid_end":"tt0814243",
            "asid_start":null,"asid_end":null
        }),"ordinary film identity matching remains available");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
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
    fn anime_verified_shorter_alias_can_beat_unverified_longer_ova(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT,
                tvdb_id TEXT DEFAULT '',trakt_season INTEGER DEFAULT 0);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT DEFAULT '',tvdb_season INTEGER,
                anidb_season INTEGER,map TEXT,offset INTEGER,start INTEGER,end INTEGER);
            INSERT INTO anidb_titles VALUES
                ('100','exampleanimeexpansion'),
                ('200','exampleanime');
            INSERT INTO anime_series_map(anidb_id,imdb_id) VALUES
                ('100','tt0000998'),('200','tt0000999');
            INSERT INTO episode_map(anidb_id,tvdb_id,tvdb_season,anidb_season,map,offset,start,end)
                VALUES('100','same',1,1,'',0,1,5),
                      ('200','other',1,1,'',0,1,60);").unwrap();
        let matched=anime_candidate(&db,"exampleanimeexpansion",Some(1),37)
            .unwrap().expect("the verified TV identity must survive an older OVA");
        assert_eq!(matched["imdb_id"],"tt0000999");
        assert_eq!(matched["anidb_id"],"200");
        assert_eq!(matched["anidb_episode"],37);
    }
    #[test]
    fn lego_masters_au_public_match_selects_australian_imdb_series(){
        // AU is explicit *source-title* identity evidence; IMDb votes are
        // only a tie-breaker. IMDb's actual primary title for the Australian
        // series is the expanded "Lego Masters Australia".
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-lego-au-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,
                clean_title TEXT,year INTEGER,category TEXT,region TEXT,
                original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt10285320','Lego Masters Australia',
                    'legomastersaustralia',2019,'series','',1,0,'tvSeries'),
                ('tt9615014','Lego Masters','legomasters',
                    2020,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt10285320',8),('tt9615014',5);
            INSERT INTO season_episodes VALUES
                ('tt10285320',3,14,14),('tt9615014',3,16,16);
            INSERT INTO imdb_ratings VALUES
                ('tt10285320',1),('tt9615014',1000000);").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        for (raw,expected) in [
            ("LEGO Masters AU S03E09 HDTV x264-FQM","tt10285320"),
            ("Lego.Masters.Australia.S03E09.HDTV.x264-FQM","tt10285320"),
            ("LEGO Masters S03E09 HDTV x264-FQM","tt9615014"),
        ] {
            let result=diagnose(&path,&MatchRequest{
                title:raw.into(),year:None,series:None}).unwrap();
            let winner=result.selected.expect("IMDb series must be selected");
            assert_eq!(winner["imdb_id"],expected,
                "{raw}: winner={winner:?}, candidates={:?}",result.imdb_candidates);
            assert_eq!(public_match_response(Some(winner)),json!({
                "sid_start":format!("{expected}:3:9"),
                "sid_end":format!("{expected}:3:9"),
                "asid_start":null,"asid_end":null
            }),"{raw}");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn parent_chinese_episode_with_advertising_only_leaf_is_not_a_season_pack(){
        // Absolute episode 62 is independent of the S01 directory label.
        // The fixture's S02E03 mapping is synthetic: actual coordinates
        // must come from the imported AniDB/TVDB episode crosswalk.
        const RAW:&str="【高清剧集网发布 www.BBEGGE.com】沧元图[第62集][国语配音+中文字幕].The.Demon.Hunter.S01.2023.2160p.WEB-DL.H265.AAC-ColorTV/【更多高清剧集下载请访问 www.BPHDTV.com】【更多剧集打包下载请访问 www.BPHDTV.com】.mkv";
        let stamp=std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-parent-cjk-episode-{}-{stamp}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,
                clean_title TEXT,year INTEGER,category TEXT,region TEXT,
                original INTEGER,ordering INTEGER,type TEXT);
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
                ('tt27497430','The Demon Hunter','thedemonhunter',
                    2023,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt27497430',2);
            INSERT INTO season_episodes VALUES
                ('tt27497430',1,26,26),('tt27497430',2,14,14);
            INSERT INTO anime_series_map VALUES
                ('tt27497430','4000','tvdb-4000',0);
            INSERT INTO anime_ids VALUES('tt27497430','anidb','4000');
            INSERT INTO anidb_titles VALUES
                ('4000','The Demon Hunter','thedemonhunter','main','en');
            INSERT INTO episode_map VALUES
                ('4000','tvdb-4000',1,2,60,66,-59,'');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let result=diagnose(&path,&MatchRequest{
            title:RAW.into(),year:None,series:None}).unwrap();
        assert_eq!(result.parsed["title"],"The Demon Hunter","{:?}",result.parsed);
        assert_eq!(result.parsed["seasons"],json!([1]));
        assert_eq!(result.parsed["episodes"],json!([]));
        assert_eq!(result.parsed["absolute_episode_candidate"],json!(62));
        let winner=result.selected.expect("indexed IMDb identity");
        assert_eq!(winner["imdb_id"],"tt27497430");
        assert_eq!(winner["match_source"],"verified_parent_absolute_episode");
        assert_eq!(public_match_response(Some(winner)),json!({
            "sid_start":"tt27497430:2:3","sid_end":"tt27497430:2:3",
            "asid_start":"4000:62","asid_end":"4000:62"
        }));
        // If the AniDB import lacks an episode relationship, retain
        // only the real IMDb identity. In particular S01E01..E26 is a
        // FALSE season pack because the parent announces 第62集.
        let db=Connection::open(&path).unwrap();
        db.execute("DELETE FROM episode_map",[]).unwrap();
        // Any modification of the SQLite snapshot changes its Tantivy
        // generation fingerprint. Warm the new immutable generation before
        // issuing another /match request, just as production startup does.
        tantivy_search::warmup(&db).unwrap();
        drop(db);
        let unresolved=diagnose(&path,&MatchRequest{
            title:RAW.into(),year:None,series:None}).unwrap();
        let selected=unresolved.selected.expect("keep IMDb identity");
        assert!(selected["episode_range"].is_null(),
            "absolute episode may not silently expand into entire season");
        assert_eq!(public_match_response(Some(selected)),json!({
            "sid_start":"tt27497430","sid_end":"tt27497430",
            "asid_start":null,"asid_end":null
        }));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
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
