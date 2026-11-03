//! Experimental native Rust matching service.
//! Runs against the same SQLite schema as the Python implementation.
//! This is a parallel implementation, not yet a drop-in replacement.
mod imdb_v4;
mod anime_resolver;
mod anime_title;
mod country_aliases;
use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone)]
struct AppState { db: Arc<PathBuf> }

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
            media_type:r.get(3)?,matched_title:r.get(4)?,source:"imdb"})
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

fn diagnose(db_path: &PathBuf, req: &MatchRequest) -> Result<Diagnostic,String> {
    if req.title.trim().is_empty() || req.title.len() > 4096 { return Err("title must be 1–4096 bytes".into()); }
    let parsed = ptt_core::parse_title(&req.title,false).map_err(|e|e.to_string())?;
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
    let seasons = numbers(parsed.get("seasons"));
    let mut episodes = numbers(parsed.get("episodes"));
    if episodes.is_empty() {
        if let Some(ep)=explicit_absolute_episode(&req.title){episodes.push(ep);}
    }
    let series = req.series.unwrap_or(!seasons.is_empty() || !episodes.is_empty());
    let year = req.year.or_else(||parsed.get("year").and_then(Value::as_i64).map(|n|n as i32));
    let db = open_db(db_path).map_err(|e|e.to_string())?;
    let imdb = imdb_v4::search(&db,title,year,series,seasons.iter().copied().max().unwrap_or(0),&req.title,!episodes.is_empty(),20)
        .map_err(|e|e.to_string())?;
    let imdb: Vec<Candidate> = imdb.into_iter().map(|m|Candidate{
        imdb_id:m.imdb_id,title:m.title,year:m.year,media_type:m.media_type,
        matched_title:m.matched_title,source:"imdb"}).collect();
    let anime = if series {
        episodes.first().map(|ep|anime_candidate(&db,&key,seasons.first().copied(),*ep))
            .transpose().map_err(|e|e.to_string())?.flatten()
    } else { None };
    let selected = if let Some(a) = &anime {
        let imdb_id = a["imdb_id"].as_str().unwrap_or_default();
        let season = a["season"].as_i64().unwrap_or(0);
        let episode = a["episode"].as_i64().unwrap_or(0);
        let anidb_id = a["anidb_id"].as_str().unwrap_or_default();
        let ani_episode = a["anidb_episode"].as_i64().unwrap_or(0);
        Some(json!({"imdb_id":imdb_id,"season":season,"episode":episode,
            "sid":format!("{imdb_id}:{season}:{episode}"),
            "anime":{"anidb_id":anidb_id,"episode":ani_episode,"asid":format!("{anidb_id}:{ani_episode}")},
            "match_source":"anime_priority"}))
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
            let mut selected_episode=episodes.first().copied();
            let mut anime_result=Value::Null;
            let mut reject=false;
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
                } else if anime_title::has_specific_alias(&db,&c.imdb_id,title)
                    .map_err(|e|e.to_string())? {
                    // Keep the highest-ranked IMDb candidate when an anime
                    // mapping cannot be verified; never return no match for a tie.
                    reject=false;
                }
            }
            if !reject {
                Some(json!({"imdb_id":c.imdb_id,"title":c.title,"year":c.year,
                    "category":if series {"series"} else {"movie"},
                    "season":selected_season,"episode":selected_episode,
                    "sid":if let (Some(se),Some(ep))=(selected_season,selected_episode){
                        format!("{}:{se}:{ep}",c.imdb_id)
                    }else{c.imdb_id.clone()},
                    "anime":anime_result,"match_source":"imdb"}))
            }else{None}
        }else{None}
    };
    let reason = if anime.is_some() {"Verified anime crosswalk"} else if selected.is_some() {
        "IMDb title candidate (experimental matching rules)"
    } else {"No eligible candidate"};
    Ok(Diagnostic{parsed: Value::Object(parsed),normalized_title:key,imdb_candidates:imdb,anime_candidate:anime,
        selected,reason:reason.into()})
}

async fn health(State(state):State<AppState>) -> (StatusCode,Json<Value>) {
    let ready = open_db(&state.db).and_then(|db|db.query_row(
        "SELECT count(*) FROM metadata_state WHERE source='full_refresh'",[],|r|r.get::<_,i64>(0)))
        .map(|n|n>0).unwrap_or(false);
    (if ready {StatusCode::OK} else {StatusCode::SERVICE_UNAVAILABLE},
        Json(json!({"status":if ready {"ready"} else {"initializing"}})))
}

async fn match_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>) {
    let db = Arc::clone(&state.db);
    match tokio::task::spawn_blocking(move ||diagnose(&db,&req)).await {
        Ok(Ok(d)) => (StatusCode::OK,Json(d.selected.unwrap_or(Value::Null))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST,Json(json!({"error":e}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":e.to_string()}))),
    }
}

async fn debug_request(State(state):State<AppState>,Json(req):Json<MatchRequest>)
    -> (StatusCode,Json<Value>) {
    let db = Arc::clone(&state.db);
    match tokio::task::spawn_blocking(move ||diagnose(&db,&req)).await {
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
    let db = env::var("MATCHLAB_DB").unwrap_or_else(|_|"/data/metadata.sqlite3".into());
    let host = env::var("MATCHLAB_HOST").unwrap_or_else(|_|"127.0.0.1:8766".into());
    let app = Router::new().route("/health",get(health))
        .route("/match",post(match_request))
        .route("/debug/match",post(debug_request))
        .with_state(AppState{db:Arc::new(PathBuf::from(db))});
    let listener = tokio::net::TcpListener::bind(&host).await?;
    tracing::info!("experimental Rust API listening on {host}");
    axum::serve(listener,app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
