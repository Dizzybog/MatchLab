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
static CURATED:OnceLock<Vec<ActiveRule>>=OnceLock::new();
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
/// Reviewed rules committed through GitHub are part of the release image,
/// not downloaded at runtime. They are tiny, deterministic and do not need
/// ONNX or local opt-in to work. Identity/coordinates still need current
/// crosswalk verification in the matcher.
fn curated_rules()->&'static Vec<ActiveRule>{
    CURATED.get_or_init(||{
        const SOURCE:&str=include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),"/../../learning/community_rules.json"
        ));
        let items:Vec<Value>=serde_json::from_str(SOURCE).unwrap_or_default();
        items.into_iter().take(32).filter_map(|entry|{
            let marker=entry["marker"].as_str()?;
            let expected=pattern_for(marker)?;
            if entry["regex"].as_str()!=Some(expected.as_str()){return None;}
            let pattern=regex_for(marker)?;
            negative_probes(marker,&pattern).then_some(ActiveRule{
                marker:marker.to_owned(),pattern
            })
        }).collect()
    })
}
/// Regex recognition returns a candidate to the ordinary MatchLab matcher.
/// Learning never inserts or changes a parser field; IMDb/AniDB verification
/// and any resulting coordinate changes belong exclusively to MatchLab.
fn propose_from(title:&str,parsed:&Value,active:&[ActiveRule])->Option<Value>{
    if active.is_empty(){return None;}
    let map=parsed.as_object()?;
    let mut rest=title;
    let mut markers=0usize;
    for _ in 0..3 {
        let Some(caps)=marker_re().captures(rest)else{break;};
        let Some(full)=caps.get(0)else{break;};
        if caps.get(1).is_some_and(|m|safe_marker(&m.as_str().to_ascii_lowercase())){
            markers+=1;
            if markers>1{return None;}
        }
        let mut advance=full.end();
        if full.as_str().ends_with([' ','.','_','-']){
            advance=advance.saturating_sub(1);
        }
        if advance==0||advance>=rest.len(){break;}
        rest=&rest[advance..];
    }
    static EXPLICIT:OnceLock<Regex>=OnceLock::new();
    let explicit=EXPLICIT.get_or_init(||Regex::new(
        r"(?i)(?:^|[ ._-])s[0-9]{1,3}[ ._-]*e[0-9]{1,4}(?:$|[ ._-])"
    ).expect("explicit episode guard"));
    if explicit.is_match(title)
        || map.get("anime_title_verified").and_then(Value::as_bool)==Some(true){
        return None;
    }
    let mut winner=None;
    for rule in active {
        let Some((name,episode))=match_release(&rule.pattern,title)else{continue;};
        if map.get("episodes").and_then(Value::as_array).is_some_and(|episodes|
            episodes.len()==1 && episodes[0].as_i64()==Some(episode))
            && map.get("seasons").and_then(Value::as_array)
                .is_none_or(|seasons|seasons.is_empty()){
            return None;
        }
        if winner.replace((name.to_owned(),episode,rule.marker.clone())).is_some(){
            return None;
        }
    }
    winner.map(|(name,episode,marker)|json!({
        "title":name,"episode":episode,"marker":marker,
        "source":"learned_regex","needs_crosswalk_verification":true
    }))
}
/// No parser output mutation. The MatchLab matcher interprets the verified
/// candidate using its existing IMDb/AniDB mapping and exception rules.
pub fn candidate(title:&str,parsed:&Value)->Option<Value>{
    let reviewed=curated_rules();
    if !crate::local_learning::active() {
        return propose_from(title,parsed,reviewed);
    }
    if let Ok(local)=rules().read() {
        let mut list=Vec::with_capacity(reviewed.len()+local.len());
        for rule in reviewed.iter().chain(local.iter()){
            if list.iter().any(|item:&ActiveRule|item.marker==rule.marker){continue;}
            if let Some(pattern)=regex_for(&rule.marker){
                list.push(ActiveRule{marker:rule.marker.clone(),pattern});
            }
        }
        propose_from(title,parsed,&list)
    }else{
        propose_from(title,parsed,reviewed)
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
            release_title TEXT NOT NULL DEFAULT '',
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
        ("learned_regex_evidence","release_title","release_title TEXT NOT NULL DEFAULT ''"),
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
    let Ok(Some(found))=crate::episode_crosswalk::resolve_absolute(metadata,imdb,absolute)
        else{return false;};
    found.imdb_id==imdb && found.season==season && found.episode==episode
        && found.anidb_id==anidb && found.anidb_episode==anidb_ep
}
fn validate_examples(db:&Connection,metadata:&Connection,marker:&str)->Result<bool,String>{
    let Some(regex)=regex_for(marker)else{return Ok(false);};
    let mut stmt=db.prepare("SELECT imdb_id,absolute_episode,season,episode,
        anidb_id,anidb_episode,release_title
        FROM learned_regex_evidence WHERE marker=?1 AND epoch=?2")
        .map_err(|e|e.to_string())?;
    let mut valid=0i64;
    let rows=stmt.query_map(params![marker,EPOCH],|r|Ok((
        r.get::<_,String>(0)?,r.get::<_,i64>(1)?,
        r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,
        r.get::<_,String>(4)?,r.get::<_,i64>(5)?,
        r.get::<_,String>(6)?)))
        .map_err(|e|e.to_string())?;
    for row in rows{
        let (imdb,abs,season,episode,anidb,anidb_ep,title)=row.map_err(|e|e.to_string())?;
        // Previous schema cached no raw title. Keep those observations,
        // but never let them authorize a live override or GitHub proposal.
        if title.is_empty(){return Ok(false);}
        let Some((base,num))=match_release(&regex,&title)else{return Ok(false);};
        if num!=abs || crate::imdb_matcher::clean_title(base).is_empty(){
            return Ok(false);
        }
        if !check_verified(metadata,&imdb,abs,season,episode,&anidb,anidb_ep){
            return Ok(false);
        }
        let Ok(parsed)=matchlab_parser::parse_title_canonical(&title,false)else{return Ok(false);};
        let ordinary=Value::Object(parsed);
        if ordinary["anime_episode_candidate"]["episode"].as_i64()==Some(abs)
            ||ordinary["episodes"].as_array().is_some_and(|list|
               list.len()==1 && list[0].as_i64()==Some(abs)){
            return Ok(false); // already solved by the current core parser
        }
        valid+=1;
    }
    Ok(valid>=MIN_EXAMPLES)
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
    let _=marker;
    !negatives.iter().any(|t|match_release(re,t).is_some())
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
/// Export only high-evidence, independently revalidated *rule candidates*.
/// This is not a calibrated ML probability. At least 12 distinct examples,
/// 4 independent IMDb identities and 5 different episodes are required.
/// Actual GitHub merge still requires independent main-branch validation.
pub fn proposals(db:&Connection,metadata:&Connection)->Result<Vec<Value>,String>{
    let revision=metadata_revision(metadata)?;
    let mut stmt=db.prepare("SELECT a.marker FROM learned_regex_active a
        JOIN learned_regex_candidates c ON c.marker=a.marker
        WHERE a.verified_build=?1 AND a.verified_metadata=?2
          AND c.obsolete=0 ORDER BY a.marker LIMIT 32")
        .map_err(|e|e.to_string())?;
    let markers=stmt.query_map(params![build(),revision],|r|r.get::<_,String>(0))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    let mut out=Vec::new();
    for marker in markers{
        let Some(pattern)=pattern_for(&marker)else{continue;};
        if !negative_probes(&marker,&regex_for(&marker).ok_or("invalid compiled pattern")?){
            continue;
        }
        let (n,series,episodes):(i64,i64,i64)=db.query_row(
            "SELECT count(*),count(DISTINCT imdb_id),
               count(DISTINCT absolute_episode)
             FROM learned_regex_evidence
             WHERE marker=?1 AND epoch=?2",
            params![marker,EPOCH],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
            .map_err(|e|e.to_string())?;
        if n<12||series<4||episodes<5||!validate_examples(db,metadata,&marker)?{
            continue;
        }
        let mut statement=db.prepare(
            "SELECT release_title,imdb_id,absolute_episode,season,episode,
                    anidb_id,anidb_episode
             FROM learned_regex_evidence
             WHERE marker=?1 AND epoch=?2
             ORDER BY example_hash LIMIT 12"
        ).map_err(|e|e.to_string())?;
        let evidence=statement.query_map(params![marker,EPOCH],|r|Ok((
            r.get::<_,String>(0)?,r.get::<_,String>(1)?,
            r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,
            r.get::<_,i64>(4)?,r.get::<_,String>(5)?,
            r.get::<_,i64>(6)?)))
            .map_err(|e|e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
        let mut examples=Vec::new();
        for (title,imdb,absolute,season,episode,anidb,anidb_ep) in evidence{
            // Public Issues are never an appropriate place for paths,
            // credentials or personally identifying filenames.
            if title.len()>180||title.contains(['/', '\\','@','\n','\r','\0']){continue;}
            let lower=title.to_lowercase();
            if ["http:","https:","magnet:","token=","password=",
                "apikey=","secret="].iter().any(|part|lower.contains(part)){continue;}
            if !check_verified(metadata,&imdb,absolute,season,episode,&anidb,anidb_ep){
                examples.clear();break;
            }
            examples.push(json!({
                "title":title,"imdb_id":imdb,"absolute_episode":absolute,
                "season":season,"episode":episode,
                "anidb_id":anidb,"anidb_episode":anidb_ep
            }));
        }
        if examples.len()<8{continue;}
        out.push(json!({
            "kind":"verified_regex_proposal",
            "fingerprint":hash(&format!("community-regex:{marker}")),
            "marker":marker,"regex":pattern,
            "source_build":build(),
            "source_metadata":revision,
            "evidence_count":n,
            "distinct_imdb":series,
            "distinct_episodes":episodes,
            "verification":"local_exact_imdb_title_and_anidb_tvdb_crosswalk",
            "examples":examples
        }));
    }
    Ok(out)
}
/// Called only by global storage maintenance AFTER the configured
/// learning database quota has been exceeded. Never delete an active rule.
pub fn evict_obsolete(db:&Connection,max_rows:i64)->Result<i64,String>{
    if max_rows<=0{return Ok(0);}
    init(db)?;
    db.execute("DELETE FROM learned_regex_candidates WHERE marker IN (
       SELECT c.marker FROM learned_regex_candidates c
       LEFT JOIN learned_regex_active a ON a.marker=c.marker
       WHERE c.obsolete=1 AND a.marker IS NULL
       ORDER BY c.obsolete_at,c.created_at,c.marker LIMIT ?1
    )",[max_rows]).map(|n|n as i64).map_err(|e|e.to_string())
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
/// Keep all locally activated anime regex identities in the offline archive,
/// including those below the stricter community-sharing threshold.
pub fn archive_proposals(db:&Connection)->Result<Vec<Value>,String>{
    init(db)?;
    let mut stmt=db.prepare("SELECT marker,verified_build FROM learned_regex_active
        ORDER BY marker").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    Ok(rows.into_iter().filter_map(|(marker,build)|{
        let regex=pattern_for(&marker)?;
        Some(json!({"kind":"verified_regex_proposal",
            "fingerprint":hash(&format!("community-regex:{marker}")),
            "marker":marker,"regex":regex,"source_build":build,
            "verification":"archive_active_rule_review_only",
            "evidence_count":0,"distinct_imdb":0}))
    }).collect())
}

/// A single independently verified anime mapping is *one observation*, not
/// permission to patch the parser. At least 8 diverse examples are required.
pub fn observe(db:&Connection,metadata:&Connection,title:&str,
    details:&Value,base:&Value)->Result<(),String>{
    if title.len()>240||title.contains(['/', '\\','@','\n','\r','\0']){return Ok(());}
    let parsed=&details["parsed"];
    let selected=&details["selected"];
    // Matched and unmatched titles can teach a pattern. We must never
    // learn from a selected movie (numeric suffixes may be film titles).
    if selected["category"]=="movie"{return Ok(());}
    let selected_imdb=selected["imdb_id"].as_str();
    if selected_imdb.is_some_and(|imdb|
        !imdb.starts_with("tt")||!(9..=10).contains(&imdb.len())||
        !imdb[2..].bytes().all(|b|b.is_ascii_digit())){return Ok(());}
    if base.get("anime_episode_range_candidate").is_some()
        ||base.get("anime_season_episode_candidate").is_some()
        ||base["anime_title_verified"]==true{return Ok(());}
    let mut seen=0;
    for caps in marker_re().captures_iter(title){
        let (Some(marker),Some(_number))=(caps.get(1),caps.get(2))else{continue;};
        let marker=marker.as_str().to_ascii_lowercase();
        if !safe_marker(&marker) {continue;}
        let Some(re)=regex_for(&marker)else{continue;};
        let Some((name,absolute))=match_release(&re,title)else{continue;};
        // Learn an *exception* only if the built-in parser missed or
        // misinterpreted this actual absolute coordinate. This also lets
        // the model correct existing context-dependent regex handlers.
        if base["episodes"].as_array().is_some_and(|episodes|
            episodes.len()==1 && episodes[0].as_i64()==Some(absolute))
            || base["anime_episode_candidate"]["episode"].as_i64()==Some(absolute)
            || base["absolute_episode_candidate"].as_i64()==Some(absolute){
            continue;
        };
        if name.len()<2 {continue;}
        let expected=crate::imdb_matcher::clean_title(name);
        // Exact Tantivy anime alias identity, not an expensive SQLite
        // text query. This also lets *unmatched* releases supply independent
        // learning examples when their name and episode mapping are unique.
        if expected.len()<4{continue;}
        let aliases=crate::tantivy_search::anime_aliases(
            metadata,&expected,expected.len()).unwrap_or_default();
        let mut identities=aliases.into_iter().filter(|alias|
            alias.clean_title==expected).map(|alias|alias.imdb_id)
            .collect::<Vec<_>>();
        identities.sort();identities.dedup();
        if identities.len()!=1{continue;}
        let imdb=&identities[0];
        if selected_imdb.is_some_and(|old|old!=imdb){continue;}
        let Some(mapped)=crate::episode_crosswalk::resolve_absolute(
            metadata,imdb,absolute).map_err(|e|e.to_string())?
            else{continue;};
        if mapped.imdb_id!=*imdb {continue;}
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
             anidb_id,anidb_episode,epoch,release_title) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(marker,example_hash) DO UPDATE SET
               imdb_id=excluded.imdb_id,absolute_episode=excluded.absolute_episode,
               season=excluded.season,episode=excluded.episode,
               anidb_id=excluded.anidb_id,anidb_episode=excluded.anidb_episode,
               epoch=excluded.epoch,release_title=excluded.release_title",
             params![marker,fingerprint,imdb,absolute,mapped.season,
                mapped.episode,mapped.anidb_id,mapped.anidb_episode,EPOCH,title])
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
    fn verified_local_rule_can_propose_fixing_an_existing_wrong_parser_field(){
        let active=vec![ActiveRule{
            marker:"epis".into(),pattern:regex_for("epis").unwrap()
        }];
        let parsed=json!({"title":"Bleach","seasons":[3],"episodes":[7]});
        let before=parsed.clone();
        let suggestion=propose_from("Bleach.EPIS.300",&parsed,&active).unwrap();
        assert_eq!(suggestion["episode"],300);
        assert_eq!(suggestion["needs_crosswalk_verification"],true);
        assert_eq!(parsed,before,"ML must never mutate parser fields");
        assert!(propose_from("Bleach.S03E07.EPIS300",&parsed,&active).is_none());
    }
    #[test]
    fn ambiguous_competing_regex_patterns_never_modify_fields(){
        let active=vec![
            ActiveRule{marker:"epis".into(),pattern:regex_for("epis").unwrap()},
            ActiveRule{marker:"epic".into(),pattern:regex_for("epic").unwrap()}
        ];
        let parsed=json!({"title":"Bleach","episodes":[],"seasons":[]});
        assert!(propose_from("Bleach.EPIS300.EPIC301",&parsed,&active).is_none());
    }
    #[test]
    fn obsolete_rule_and_supporting_evidence_cascade_together(){
        let mut db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);").unwrap();
        init(&db).unwrap();
        db.execute("INSERT INTO learned_regex_candidates(marker,created_at) VALUES('epis',1)",[]).unwrap();
        db.execute("INSERT INTO learned_regex_evidence(
            marker,example_hash,imdb_id,absolute_episode,season,episode,anidb_id,anidb_episode) VALUES(
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
        local.execute("INSERT INTO learned_regex_evidence(
            marker,example_hash,imdb_id,absolute_episode,season,episode,anidb_id,anidb_episode) VALUES(
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
        // Below capacity: keep the redundant rule and its evidence as cache.
        assert!(reserve_space(&local,"new",true,2,2).unwrap());
        assert_eq!(local.query_row(
            "SELECT count(*) FROM learned_regex_evidence",[],
            |r|r.get::<_,i64>(0)).unwrap(),1);
        // A new observation would exceed the strict limit: evict only then.
        assert!(reserve_space(&local,"new",true,1,1).unwrap());
        assert_eq!(local.query_row(
            "SELECT count(*) FROM learned_regex_evidence",[],
            |r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[test]
    fn incompatible_rule_epoch_quarantines_cache_without_erasing_examples(){
        let mut local=Connection::open_in_memory().unwrap();
        local.execute_batch("CREATE TABLE settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);
             INSERT INTO settings VALUES('learned_regex_epoch','0');").unwrap();
        init(&local).unwrap();
        local.execute("INSERT INTO learned_regex_candidates(marker,created_at) VALUES('epis',1)",[]).unwrap();
        local.execute("INSERT INTO learned_regex_evidence(
            marker,example_hash,imdb_id,absolute_episode,season,episode,anidb_id,anidb_episode) VALUES(
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
        let epoch:i64=local.query_row(
            "SELECT epoch FROM learned_regex_evidence WHERE marker='epis'",
            [],|r|r.get(0)).unwrap();
        assert_eq!(epoch,0,"old cached labels cannot authorize rules in a new epoch");
    }
    #[test]
    fn full_cache_of_useful_rules_refuses_eviction(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);").unwrap();
        init(&db).unwrap();
        db.execute("INSERT INTO learned_regex_candidates(marker,created_at)
           VALUES('good',1)",[]).unwrap();
        db.execute("INSERT INTO learned_regex_evidence(
           marker,example_hash,imdb_id,absolute_episode,season,episode,anidb_id,anidb_episode)
           VALUES('good','hash','tt1234567',5,1,5,'100',5)",[]).unwrap();
        assert!(!reserve_space(&db,"other",true,1,1).unwrap());
        assert_eq!(db.query_row("SELECT count(*) FROM learned_regex_evidence",
            [],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
    #[test]
    fn disabled_rules_never_patch_parser_outputs(){
        install(vec![]);
        let parsed=json!({"title":"Bleach","episodes":[],"seasons":[]});
        assert!(candidate("Bleach_EPIS300",&parsed).is_none());
    }
}