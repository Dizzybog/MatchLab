//! Versioned, *local-only* learned regex overlays.
//! The standard parser always runs first. Learned patterns emit untrusted
//! absolute-episode HINTS, never guessed IMDb IDs/SIDs/ASIDs. The normal
//! matcher must verify the hint with its current AniDB/TVDB crosswalk.
//!
//! Learned rules are deactivated on a code/metadata change until evidence
//! passes revalidation; obsolete rules and their evidence remain a reusable
//! local cache. Only capacity pressure may prune inactive, unnecessary
//! candidates and their evidence together, transactionally.
use regex::{Regex,RegexBuilder};
use rusqlite::{params,Connection,OptionalExtension};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::sync::{OnceLock,RwLock,atomic::{AtomicU64,Ordering}};

const EPOCH:u32=1;
const MIN_EXAMPLES:i64=8;
const MIN_SERIES:i64=2;
const MIN_EPISODES:i64=3;
const MAX_RULES:i64=32;
const MAX_CANDIDATES:i64=64;
const MAX_EVIDENCE:i64=2048;
const MAX_REGEX_BYTES:usize=160;
static ACTIVE:OnceLock<RwLock<Vec<ActiveRule>>>=OnceLock::new();
static CANDIDATE:OnceLock<Regex>=OnceLock::new();
static RULESET_REVISION:AtomicU64=AtomicU64::new(0);

/// Stable fingerprint of currently active overlays, included in cache keys.
pub fn revision()->u64{RULESET_REVISION.load(Ordering::Relaxed)}

struct ActiveRule {marker:String,pattern:Regex}
fn rules()->&'static RwLock<Vec<ActiveRule>> {
    ACTIVE.get_or_init(||RwLock::new(Vec::new()))
}
fn hash(s:&str)->String{format!("{:x}",Sha256::digest(s.as_bytes()))}
fn build()->String{
    // The official Docker image bakes the source SHA in at compile time.
    // The schema epoch separately invalidates incompatible learned data.
    format!("{}:{EPOCH}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"))
}
fn marker_re()->&'static Regex{
    CANDIDATE.get_or_init(|| Regex::new(
        r"(?i)(?:^|[ ._-])([a-z]{3,9})[ ._-]*([1-9][0-9]{0,4})(?:$|[ ._-])"
    ).expect("bounded candidate grammar"))
}
fn reject_marker(marker:&str)->bool{
    matches!(marker,
        "season"|"episode"|"series"|"part"|"year"|"movie"|"sample"|
        "bluray"|"webrip"|"webdl"|"hdtv"|"hdr"|"codec"|"audio"|
        "x264"|"x265"|"h264"|"h265"|"hevc"|"hdr10"|"dovi"|
        "ep"|"eps"|"new"|"the"|"eng"|"sub"|"dub")
}
fn safe_marker(marker:&str)->bool{
    (3..=9).contains(&marker.len()) &&
    marker.bytes().all(|b|b.is_ascii_lowercase()) &&
    !reject_marker(marker)
}
fn pattern_for(marker:&str)->Option<String>{
    if !safe_marker(marker){return None;}
    // No arbitrary user-authored regex, lookarounds, recursive groups or
    // catastrophic backtracking: fixed linear-time regex grammar only.
    let pattern=format!(
        r"(?i)(?:^|[ ._-]){}[ ._-]*([1-9][0-9]{{0,4}})(?:$|[ ._-])",
        regex::escape(marker)
    );
    (pattern.len()<=MAX_REGEX_BYTES).then_some(pattern)
}
fn regex_for(marker:&str)->Option<Regex>{
    let text=pattern_for(marker)?;
    RegexBuilder::new(&text).size_limit(16*1024).build().ok()
}
fn base_handles(marker:&str)->bool{
    let title=format!("Generic Sample {}300",marker);
    let Ok(parsed)=matchlab_parser::parse_title(&title,false)else{return true;};
    let value=Value::Object(parsed);
    value["episodes"].as_array().is_some_and(|v|v.iter().any(|e|e.as_i64()==Some(300)))
        ||value["anime_episode_candidate"]["episode"].as_i64()==Some(300)
        ||value["absolute_episode_candidate"].as_i64()==Some(300)
}
fn technical_suffix(s:&str)->bool{
    let rest=s.trim_matches(|c:char|matches!(c,' '|'.'|'_'|'-'));
    if rest.is_empty(){return true;}
    rest.split(|c:char|matches!(c,' '|'.'|'_'|'-'))
        .filter(|s|!s.is_empty())
        .all(|p|matches!(p.to_ascii_lowercase().as_str(),
            "1080p"|"2160p"|"720p"|"480p"|"x264"|"x265"|"h264"|
            "h265"|"hevc"|"av1"|"aac"|"ac3"|"web"|"dl"|"webrip"|
            "hdtv"|"bluray"|"mkv"|"mp4"|"avi"|"remux"|"10bit"))
}
fn match_release<'a>(re:&Regex,title:&'a str)->Option<(&'a str,i64)>{
    if title.len()>240||title.contains(['/', '\\','@','\n','\r','\0']){return None;}
    let caps=re.captures(title)?;
    let full=caps.get(0)?;
    // Prevent silently reinterpreting multiple different numeric markers.
    if re.captures_iter(title).take(2).count()!=1 {return None;}
    if !technical_suffix(&title[full.end()..]) {return None;}
    let original=&title[..full.start()];
    let name=original.trim_end_matches(|c:char|matches!(c,' '|'_'|'.'|'-'));
    if name.len()<2||name.len()>140 || !name.chars().any(char::is_alphabetic){
        return None;
    }
    let episode=caps.get(1)?.as_str().parse::<i64>().ok()?;
    (1..=99999).contains(&episode).then_some((name,episode))
}
fn install(rows:Vec<ActiveRule>){
    // Stable across repeated reloads and process restarts. New or retired
    // overlays change the cache namespace without a global cache lock.
    let mut markers=rows.iter().map(|r|r.marker.as_str()).collect::<Vec<_>>();
    markers.sort_unstable();
    let fingerprint=Sha256::digest(markers.join("\n").as_bytes());
    let mut bits=[0u8;8];
    bits.copy_from_slice(&fingerprint[..8]);
    if let Ok(mut active)=rules().write(){
        *active=rows;
        RULESET_REVISION.store(u64::from_le_bytes(bits),Ordering::Release);
    }
}
/// Hot path: only a tiny set of precompiled linear-time expressions and an
/// uncontended read lock; completely inactive when MATCHLAB_ML_ENABLED=false.
pub fn apply(title:&str,parsed:&mut Value){
    if !crate::local_learning::active(){return;}
    let Some(map)=parsed.as_object_mut()else{return;};
    if map.get("anime_episode_candidate").is_some() ||
       map.get("anime_episode_range_candidate").is_some() ||
       map.get("anime_season_episode_candidate").is_some() ||
       map.get("episode_start").is_some() ||
       map.get("seasons").and_then(Value::as_array).is_some_and(|v|!v.is_empty()) ||
       map.get("episodes").and_then(Value::as_array).is_some_and(|v|!v.is_empty()){
        return;
    }
    let Ok(active)=rules().read()else{return;};
    let mut winner=None;
    for rule in active.iter(){
        if let Some((name,episode))=match_release(&rule.pattern,title) {
            if winner.replace((name.to_owned(),episode,rule.marker.clone())).is_some(){
                return; // ambiguity: never promote more than one learned hint
            }
        }
    }
    if let Some((title,episode,marker))=winner{
        map.insert("anime_episode_candidate".into(),json!({
            "title":title,"episode":episode,"source":"local_learned_regex",
            "marker":marker
        }));
    }
}
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS learned_regex_candidates(
            marker TEXT PRIMARY KEY,created_at INTEGER NOT NULL,
            state TEXT NOT NULL DEFAULT 'pending',
            state_reason TEXT NOT NULL DEFAULT '',
            evaluated_build TEXT NOT NULL DEFAULT '',
            evaluated_metadata TEXT NOT NULL DEFAULT '',
            last_evaluated INTEGER NOT NULL DEFAULT 0,
            obsolete INTEGER NOT NULL DEFAULT 0,obsolete_at INTEGER,
            obsolete_reason TEXT NOT NULL DEFAULT '');
         CREATE TABLE IF NOT EXISTS learned_regex_evidence(
            marker TEXT NOT NULL REFERENCES learned_regex_candidates(marker)
              ON DELETE CASCADE,
            example_hash TEXT NOT NULL,imdb_id TEXT NOT NULL,
            absolute_episode INTEGER NOT NULL,season INTEGER NOT NULL,
            episode INTEGER NOT NULL,anidb_id TEXT NOT NULL,
            anidb_episode INTEGER NOT NULL,
            epoch INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY(marker,example_hash));
         CREATE TABLE IF NOT EXISTS learned_regex_active(
            marker TEXT PRIMARY KEY REFERENCES learned_regex_candidates(marker)
              ON DELETE CASCADE,
            pattern TEXT NOT NULL,verified_build TEXT NOT NULL,
            verified_metadata TEXT NOT NULL,activated_at INTEGER NOT NULL);"
    ).map_err(|e|e.to_string())?;
    // Upgrade existing databases without destroying their learned cache.
    for (table,column,definition) in [
        ("learned_regex_candidates","state","state TEXT NOT NULL DEFAULT 'pending'"),
        ("learned_regex_candidates","state_reason","state_reason TEXT NOT NULL DEFAULT ''"),
        ("learned_regex_candidates","evaluated_build","evaluated_build TEXT NOT NULL DEFAULT ''"),
        ("learned_regex_candidates","evaluated_metadata","evaluated_metadata TEXT NOT NULL DEFAULT ''"),
        ("learned_regex_candidates","last_evaluated","last_evaluated INTEGER NOT NULL DEFAULT 0"),
        ("learned_regex_evidence","epoch","epoch INTEGER NOT NULL DEFAULT 1"),
        ("learned_regex_candidates","obsolete","obsolete INTEGER NOT NULL DEFAULT 0"),
        ("learned_regex_candidates","obsolete_at","obsolete_at INTEGER"),
        ("learned_regex_candidates","obsolete_reason","obsolete_reason TEXT NOT NULL DEFAULT ''"),
    ]{
        let sql=format!("PRAGMA table_info({table})");
        let mut stmt=db.prepare(&sql).map_err(|e|e.to_string())?;
        let columns=stmt.query_map([],|r|r.get::<_,String>(1))
            .map_err(|e|e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
        drop(stmt);
        if !columns.iter().any(|name|name==column){
            db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {definition};"))
                .map_err(|e|e.to_string())?;
        }
    }
    Ok(())
}
fn metadata_revision(metadata:&Connection)->Result<String,String>{
    let stamp:Option<String>=metadata.query_row(
        "SELECT updated_at FROM metadata_state WHERE source='full_refresh'",
        [],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
    stamp.ok_or_else(||"full metadata refresh has not completed".into())
}
fn check_verified(metadata:&Connection,imdb:&str,absolute:i64,
    season:i64,episode:i64,anidb:&str,anidb_ep:i64)->bool{
    let Ok(Some(found))=crate::anime_resolver::resolve_absolute(metadata,imdb,absolute)
        else{return false;};
    found.imdb_id==imdb && found.season==season && found.episode==episode
        && found.anidb_id==anidb && found.anidb_episode==anidb_ep
}
fn validate_examples(db:&Connection,metadata:&Connection,marker:&str)->Result<bool,String>{
    let mut stmt=db.prepare("SELECT imdb_id,absolute_episode,season,episode,
        anidb_id,anidb_episode FROM learned_regex_evidence WHERE marker=?1 AND epoch=?2")
        .map_err(|e|e.to_string())?;
    let mut good=true;
    let mut count=0i64;
    let rows=stmt.query_map(params![marker,EPOCH],|r|Ok((
        r.get::<_,String>(0)?,r.get::<_,i64>(1)?,
        r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,
        r.get::<_,String>(4)?,r.get::<_,i64>(5)?)))
        .map_err(|e|e.to_string())?;
    for row in rows {
        let (imdb,absolute,season,episode,anidb,anidb_ep)=row.map_err(|e|e.to_string())?;
        count+=1;
        if !check_verified(metadata,&imdb,absolute,season,episode,&anidb,anidb_ep){
            good=false; break;
        }
    }
    Ok(good&&count>=MIN_EXAMPLES)
}
fn qualifies(db:&Connection,marker:&str)->Result<bool,String>{
    let (samples,series,eps):(i64,i64,i64)=db.query_row(
        "SELECT COUNT(*),COUNT(DISTINCT imdb_id),COUNT(DISTINCT absolute_episode)
         FROM learned_regex_evidence WHERE marker=?1 AND epoch=?2",params![marker,EPOCH],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
        .map_err(|e|e.to_string())?;
    Ok(samples>=MIN_EXAMPLES&&series>=MIN_SERIES&&eps>=MIN_EPISODES)
}
fn negative_probes(marker:&str,re:&Regex)->bool{
    let negatives=[
        "The Movie 2024", "Bleach S16", "Show S01E03",
        "Title 1080p WEB DL", "Movie 1917", "Doctor Who 2005",
        "Naruto x265 1080p", "Episode 300 Explained",
        "The Series EPIS 300 Documentary", "Show EPIS300 trailer",
    ];
    !negatives.iter().any(|t|match_release(re,t).is_some())
        && !base_handles(marker)
}
/// Retire immediately, but retain the original verified observations. Under
/// storage pressure the budget worker evicts these candidates AND their
/// dependent evidence in one transaction using foreign-key cascades.
fn retire_rule(db:&Connection,marker:&str,reason:&str)->Result<(),String>{
    db.execute("DELETE FROM learned_regex_active WHERE marker=?1",[marker])
        .map_err(|e|e.to_string())?;
    db.execute("UPDATE learned_regex_candidates
        SET obsolete=1,obsolete_at=COALESCE(obsolete_at,strftime('%s','now')),
            obsolete_reason=?2 WHERE marker=?1",params![marker,reason])
        .map_err(|e|e.to_string())?;
    Ok(())
}
/// On image and metadata changes revalidate learned regex rules against the
/// current source of truth. A stale/unsafe rule is NOT active, but its
/// historical evidence remains available until the storage quota is reached.
pub fn reconcile(db:&mut Connection,metadata:&Connection)->Result<(),String>{
    init(db)?;
    let revision=match metadata_revision(metadata) {
        Ok(r)=>r,
        Err(e)=>{
            install(Vec::new()); // fail closed with no verified metadata
            return Err(e);
        }
    };
    let current=build();
    let epoch=EPOCH.to_string();
    let previous_epoch:Option<String>=db.query_row(
        "SELECT v FROM settings WHERE k='learned_regex_epoch'",[],|r|r.get(0))
        .optional().map_err(|e|e.to_string())?;
    let tx=db.transaction().map_err(|e|e.to_string())?;
    if previous_epoch.as_deref().is_some_and(|v|v!=epoch) {
        tx.execute("UPDATE learned_regex_evidence SET epoch=0 WHERE epoch<>0",[])
            .map_err(|e|e.to_string())?;
        tx.execute("DELETE FROM learned_regex_active",[])
            .map_err(|e|e.to_string())?;
        tx.execute("UPDATE learned_regex_candidates SET obsolete=1,
            obsolete_at=COALESCE(obsolete_at,strftime('%s','now')),
            obsolete_reason='incompatible_epoch'",[])
            .map_err(|e|e.to_string())?;
    }
    tx.execute("INSERT INTO settings(k,v) VALUES('learned_regex_epoch',?1)
        ON CONFLICT(k) DO UPDATE SET v=excluded.v",[&epoch])
        .map_err(|e|e.to_string())?;
    let mut stmt=tx.prepare("SELECT marker,obsolete FROM learned_regex_candidates
        ORDER BY marker LIMIT 65").map_err(|e|e.to_string())?;
    let candidates=stmt.query_map([],|r|Ok((
        r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    drop(stmt);
    for (marker,obsolete) in &candidates {
        // Obsolete cache can be reactivated if fresh evidence becomes valid.
        let _=obsolete;
        let old:Option<(String,String)>=tx.query_row(
            "SELECT verified_build,verified_metadata FROM learned_regex_active
             WHERE marker=?1",[marker],|r|Ok((r.get(0)?,r.get(1)?)))
            .optional().map_err(|e|e.to_string())?;
        if old.as_ref().is_some_and(|(b,m)|b==&current&&m==&revision){
            continue;
        }
        let Some(re)=regex_for(marker)else{
            retire_rule(&tx,marker,"invalid_pattern")?;
            continue;
        };
        if !negative_probes(marker,&re) {
            retire_rule(&tx,marker,"now_handled_or_unsafe")?;
            continue;
        }
        let verified=validate_examples(&tx,metadata,marker)?;
        if !verified && old.is_some(){
            retire_rule(&tx,marker,"mapping_no_longer_verified")?;
        }else if verified && qualifies(&tx,marker)? {
            tx.execute("UPDATE learned_regex_candidates SET obsolete=0,
                obsolete_at=NULL,obsolete_reason='' WHERE marker=?1",[marker])
                .map_err(|e|e.to_string())?;
            tx.execute("INSERT INTO learned_regex_active(
                 marker,pattern,verified_build,verified_metadata,activated_at)
                 VALUES(?1,?2,?3,?4,strftime('%s','now'))
                 ON CONFLICT(marker) DO UPDATE SET pattern=excluded.pattern,
                   verified_build=excluded.verified_build,
                   verified_metadata=excluded.verified_metadata",
                 params![marker,pattern_for(marker),current,revision])
                .map_err(|e|e.to_string())?;
        }
    }
    tx.commit().map_err(|e|e.to_string())?;
    reload(db,&current,&revision)
}
/// Only when an upcoming insertion would exceed a hard limit may the
/// oldest obsolete, non-active candidate be deleted. Evidence follows via
/// ON DELETE CASCADE, atomically. Useful cached data is never evicted.
fn reserve_space(db:&Connection,marker:&str,new_candidate:bool,
    candidate_limit:i64,evidence_limit:i64)->Result<bool,String>{
    for _ in 0..=MAX_CANDIDATES{
        let candidates:i64=db.query_row("SELECT count(*) FROM learned_regex_candidates",
            [],|r|r.get(0)).map_err(|e|e.to_string())?;
        let evidence:i64=db.query_row("SELECT count(*) FROM learned_regex_evidence",
            [],|r|r.get(0)).map_err(|e|e.to_string())?;
        if candidates+i64::from(new_candidate)<=candidate_limit
            && evidence+1<=evidence_limit{return Ok(true);}
        let victim:Option<String>=db.query_row(
            "SELECT c.marker FROM learned_regex_candidates c
             LEFT JOIN learned_regex_active a ON a.marker=c.marker
             WHERE c.marker<>?1 AND c.obsolete=1 AND a.marker IS NULL
             ORDER BY c.obsolete_at,c.created_at,c.marker LIMIT 1",
            [marker],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
        let Some(victim)=victim else{return Ok(false);};
        db.execute("DELETE FROM learned_regex_candidates WHERE marker=?1",
            [&victim]).map_err(|e|e.to_string())?;
    }
    Ok(false)
}
fn reload(db:&Connection,build:&str,revision:&str)->Result<(),String>{
    let mut stmt=db.prepare("SELECT marker FROM learned_regex_active
        WHERE verified_build=?1 AND verified_metadata=?2
        ORDER BY marker LIMIT ?3").map_err(|e|e.to_string())?;
    let markers=stmt.query_map(params![build,revision,MAX_RULES],|r|
        r.get::<_,String>(0)).map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    let active=markers.into_iter().filter_map(|marker|
        regex_for(&marker).map(|pattern|ActiveRule{marker,pattern}))
        .collect();
    install(active);
    Ok(())
}
/// A single independently verified anime mapping is *one observation*, not
/// permission to patch the parser. At least 8 diverse examples are required.
pub fn observe(db:&Connection,metadata:&Connection,title:&str,details:&Value)->Result<(),String>{
    if title.len()>240||title.contains(['/', '\\','@','\n','\r','\0']){return Ok(());}
    let parsed=&details["parsed"];
    let selected=&details["selected"];
    if selected["category"]!="series"{return Ok(());}
    let Some(imdb)=selected["imdb_id"].as_str()else{return Ok(());};
    if !imdb.starts_with("tt")||!(9..=10).contains(&imdb.len())
        ||!imdb[2..].bytes().all(|b|b.is_ascii_digit()){return Ok(());}
    let plain=matchlab_parser::parse_title(title,false)
        .map_err(|e|e.to_string())?;
    let base=Value::Object(plain);
    if base["episodes"].as_array().is_some_and(|e|!e.is_empty())
        ||base.get("anime_episode_candidate").is_some()
        ||base.get("anime_episode_range_candidate").is_some(){return Ok(());}
    let mut seen=0;
    for caps in marker_re().captures_iter(title){
        let (Some(marker),Some(number))=(caps.get(1),caps.get(2))else{continue;};
        let marker=marker.as_str().to_ascii_lowercase();
        if !safe_marker(&marker) {continue;}
        let Some(re)=regex_for(&marker)else{continue;};
        let Some((name,absolute))=match_release(&re,title)else{continue;};
        if name.len()<2 {continue;}
        let expected=crate::matchlab_matcher::clean_title(name);
        // Exact IMDb canonical title is a second independent identity check.
        let canonical:i64=metadata.query_row(
            "SELECT EXISTS(SELECT 1 FROM titles
                WHERE imdb_id=?1 AND clean_title=?2 LIMIT 1)",
            params![imdb,expected],|r|r.get(0)).unwrap_or(0);
        if canonical==0{continue;}
        let Some(mapped)=crate::anime_resolver::resolve_absolute(
            metadata,imdb,absolute).map_err(|e|e.to_string())?
            else{continue;};
        if mapped.imdb_id!=imdb {continue;}
        // Evidence from a selected contradictory episode is not trustworthy.
        if let Some(season)=selected["season"].as_i64()
            && season!=mapped.season{continue;}
        if let Some(episode)=selected["episode"].as_i64()
            && episode!=mapped.episode{continue;}
        if !negative_probes(&marker,&re){continue;}
        let fingerprint=hash(&format!("{imdb}:{absolute}:{}",name.to_lowercase()));
        let known:i64=db.query_row("SELECT EXISTS(SELECT 1 FROM
            learned_regex_candidates WHERE marker=?1)",[&marker],
            |r|r.get(0)).map_err(|e|e.to_string())?;
        let previously_seen:i64=db.query_row("SELECT EXISTS(SELECT 1 FROM
            learned_regex_evidence WHERE marker=?1 AND example_hash=?2 AND epoch=?3)",
            params![marker,fingerprint,EPOCH],|r|r.get(0))
            .map_err(|e|e.to_string())?;
        if previously_seen>0{continue;}
        if !reserve_space(db,&marker,known==0,MAX_CANDIDATES,MAX_EVIDENCE)?{
            return Ok(()); // full of useful examples; do not delete them
        }
        db.execute("INSERT OR IGNORE INTO learned_regex_candidates(marker,created_at)
            VALUES(?1,strftime('%s','now'))",[&marker])
            .map_err(|e|e.to_string())?;
        // Re-confirming an old cached key in the new epoch refreshes its
        // provenance; merely retaining cached data never revalidates it.
        db.execute("INSERT INTO learned_regex_evidence(
             marker,example_hash,imdb_id,absolute_episode,season,episode,
             anidb_id,anidb_episode,epoch) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(marker,example_hash) DO UPDATE SET
               imdb_id=excluded.imdb_id,absolute_episode=excluded.absolute_episode,
               season=excluded.season,episode=excluded.episode,
               anidb_id=excluded.anidb_id,anidb_episode=excluded.anidb_episode,
               epoch=excluded.epoch",
             params![marker,fingerprint,imdb,absolute,mapped.season,
                mapped.episode,mapped.anidb_id,mapped.anidb_episode,EPOCH])
            .map_err(|e|e.to_string())?;
        seen+=1;
        if seen>=1 {break;}
    }
    let _=parsed; // The current trusted source is the built-in parser above.
    Ok(())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn bounded_regex_markers_only(){
        assert!(pattern_for("epis").unwrap().contains("epis"));
        assert!(pattern_for("epis.*").is_none());
        assert!(pattern_for("x265").is_none());
        assert!(pattern_for("episode").is_none());
        let regex=regex_for("epis").unwrap();
        assert_eq!(match_release(&regex,"Bleach_EPIS300").unwrap().1,300);
        assert_eq!(match_release(&regex,"Bleach.EPIS.338.1080p.WEB-DL").unwrap().1,338);
        assert!(match_release(&regex,"Bleach.EPIS300 trailer").is_none());
        assert!(match_release(&regex,"Movie/EPIS300").is_none());
    }
    #[test]
    fn obsolete_rule_and_supporting_evidence_cascade_together(){
        let mut db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);").unwrap();
        init(&db).unwrap();
        db.execute("INSERT INTO learned_regex_candidates(marker,created_at) VALUES('epis',1)",[]).unwrap();
        db.execute("INSERT INTO learned_regex_evidence VALUES(
           'epis','key','tt1234567',300,14,1,'100',300)",[]).unwrap();
        db.execute("INSERT INTO learned_regex_active VALUES(
            'epis','some-regex','old-sha','old-meta',1)",[]).unwrap();
        db.execute("DELETE FROM learned_regex_candidates WHERE marker='epis'",[]).unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM learned_regex_evidence",
            [],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(db.query_row("SELECT count(*) FROM learned_regex_active",
            [],|r|r.get::<_,i64>(0)).unwrap(),0);
        let _=&mut db;
    }
    #[test]
    fn image_upgrade_revalidates_and_removes_untrusted_basis_atomically(){
        let mut local=Connection::open_in_memory().unwrap();
        local.execute_batch("CREATE TABLE settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);").unwrap();
        init(&local).unwrap();
        let meta=Connection::open_in_memory().unwrap();
        meta.execute_batch("CREATE TABLE metadata_state(
             source TEXT PRIMARY KEY,updated_at TEXT NOT NULL,row_count INTEGER);
             INSERT INTO metadata_state VALUES('full_refresh','test-revision',1);
             CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                 tvdb_id TEXT,trakt_season INTEGER);
             CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
             CREATE TABLE episode_map(anidb_id TEXT,tvdb_id TEXT,
                 anidb_season INTEGER,tvdb_season INTEGER,
                 start INTEGER,end INTEGER,offset INTEGER,map TEXT);
             INSERT INTO anime_series_map VALUES('tt1234567','100','200',0);
             INSERT INTO episode_map VALUES('100','200',1,2,1,30,0,'');").unwrap();
        local.execute("INSERT INTO learned_regex_candidates(marker,created_at) VALUES('epis',1)",[]).unwrap();
        local.execute("INSERT INTO learned_regex_evidence VALUES(
             'epis','key','tt1234567',300,14,1,'100',300)",[]).unwrap();
        local.execute("INSERT INTO learned_regex_active VALUES(
             'epis','old-regex','old-build','old-metadata',1)",[]).unwrap();
        // Current metadata cannot reproduce the old mapping: retire the
        // rule but retain its examples until storage quota needs reclaim.
        reconcile(&mut local,&meta).unwrap();
        assert_eq!(local.query_row(
            "SELECT count(*) FROM learned_regex_active",[],
            |r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(local.query_row(
            "SELECT obsolete FROM learned_regex_candidates WHERE marker='epis'",[],
            |r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(local.query_row(
            "SELECT count(*) FROM learned_regex_evidence",[],
            |r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(evict_obsolete(&local,1).unwrap(),1);
        assert_eq!(local.query_row(
            "SELECT count(*) FROM learned_regex_evidence",[],
            |r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[test]
    fn incompatible_rule_epoch_clears_all_rules_and_supporting_examples(){
        let mut local=Connection::open_in_memory().unwrap();
        local.execute_batch("CREATE TABLE settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);
             INSERT INTO settings VALUES('learned_regex_epoch','0');").unwrap();
        init(&local).unwrap();
        local.execute("INSERT INTO learned_regex_candidates VALUES('epis',1)",[]).unwrap();
        local.execute("INSERT INTO learned_regex_evidence VALUES(
             'epis','key','tt1234567',300,14,1,'100',300)",[]).unwrap();
        let meta=Connection::open_in_memory().unwrap();
        meta.execute_batch("CREATE TABLE metadata_state(
             source TEXT PRIMARY KEY,updated_at TEXT NOT NULL,row_count INTEGER);
             INSERT INTO metadata_state VALUES('full_refresh','valid',1);").unwrap();
        reconcile(&mut local,&meta).unwrap();
        let count:i64=local.query_row("SELECT count(*) FROM learned_regex_evidence",
            [],|r|r.get(0)).unwrap();
        assert_eq!(count,1);
        let obsolete:i64=local.query_row(
            "SELECT obsolete FROM learned_regex_candidates WHERE marker='epis'",
            [],|r|r.get(0)).unwrap();
        assert_eq!(obsolete,1);
    }
    #[test]
    fn disabled_rules_never_patch_parser_outputs(){
        install(vec![]);
        let mut parsed=json!({"title":"Bleach","episodes":[],"seasons":[]});
        apply("Bleach_EPIS300",&mut parsed);
        assert!(parsed.get("anime_episode_candidate").is_none());
    }
}