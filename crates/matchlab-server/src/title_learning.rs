//! Review-only self-supervised title-boundary learning from independent source
//! words compared to the canonical parser and a selected IMDb alias.
//! A matched IMDb title is never trusted on its own and never used as a
//! replacement title. All work runs in the bounded ML queue.
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::sync::atomic::{AtomicU64,Ordering};

const EPOCH:u32=1;
const MIN_OBSERVATIONS:i64=12;
const MIN_WORKS:i64=4;
const MAX_OBSERVATIONS:i64=20_000;
static OBSERVATIONS:AtomicU64=AtomicU64::new(0);
static READY_PATTERNS:AtomicU64=AtomicU64::new(0);

fn build()->String{format!("{}:{EPOCH}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"))}
fn digest(s:&str)->String{format!("{:x}",Sha256::digest(s.as_bytes()))}
fn safe(raw:&str)->bool{
    if !(8..=240).contains(&raw.len())||!raw.is_ascii()
        ||raw.contains(['/','\\','@','\r','\n','\0']){return false;}
    // Normalize once rather than allocating one lowercased buffer for
    // every blocked secret/URL fragment on the ML worker.
    let lower=raw.to_ascii_lowercase();
    !["password=","token=","secret=","apikey=","magnet:","http:"]
        .iter().any(|fragment|lower.contains(fragment))
}
fn words(s:&str)->Vec<String>{crate::title_normalization::clean_words(s)}
fn episode_marker(s:&str)->bool{
    if let Some(rest)=s.strip_prefix('s'){
        let (a,b)=rest.split_once('e').unwrap_or((rest,""));
        if (1..=3).contains(&a.len()) &&a.bytes().all(|c|c.is_ascii_digit())
            &&(b.is_empty()||(1..=5).contains(&b.len())
                &&b.bytes().all(|c|c.is_ascii_digit())){return true;}
    }
    if let Some((a,b))=s.split_once('x'){
        if (1..=3).contains(&a.len()) &&(1..=5).contains(&b.len())
            &&a.bytes().all(|c|c.is_ascii_digit())
            &&b.bytes().all(|c|c.is_ascii_digit()){return true;}
    }
    s.strip_prefix("ep").or_else(||s.strip_prefix('e'))
        .is_some_and(|n|(1..=5).contains(&n.len())
            &&n.bytes().all(|c|c.is_ascii_digit()))
}
fn boundary(s:&str)->Option<&'static str>{
    if episode_marker(s){return Some("episode");}
    if s.len()==4 &&s.bytes().all(|c|c.is_ascii_digit())
        &&s.parse::<i32>().is_ok_and(|n|(1900..=2099).contains(&n)){
        return Some("year");
    }
    if matches!(s,"1080p"|"2160p"|"720p"|"480p"|"4k"
        |"webdl"|"webrip"|"bluray"|"brrip"|"hdtv"|"remux"
        |"x264"|"x265"|"h264"|"h265"|"hevc"|"av1"){
        return Some("quality");
    }
    None
}
fn regex_for(signal:&str)->Option<&'static str>{
    match signal{
        "underextracted_episode"|"overextracted_episode"=>
            Some(r"(?i)(?:^|[ ._-])(?:s[0-9]{1,3}(?:[ ._-]*e[0-9]{1,5})?|[0-9]{1,3}x[0-9]{1,5}|e(?:p)?[0-9]{1,5})(?:$|[ ._-])"),
        "underextracted_year"|"overextracted_year"=>
            Some(r"(?i)(?:^|[ ._-])(?:19|20)[0-9]{2}(?:$|[ ._-])"),
        "underextracted_quality"|"overextracted_quality"=>
            Some(r"(?i)(?:^|[ ._-])(?:1080p|2160p|720p|480p|4k|webdl|webrip|bluray|brrip|hdtv|remux|x264|x265|h264|h265|hevc|av1)(?:$|[ ._-])"),
        _=>None
    }
}
fn signal(raw:&str,parsed:&Value,selected:&Value)->Option<&'static str>{
    if !safe(raw)||selected["match_source"]!="imdb"{return None;}
    let id=selected["imdb_id"].as_str()?;
    if !(9..=10).contains(&id.len())||!id.starts_with("tt")
        ||!id[2..].bytes().all(|c|c.is_ascii_digit()){return None;}
    let old=parsed["title"].as_str()?;
    let alias=selected["matched_title"].as_str()?;
    if !(6..=140).contains(&alias.len())||!alias.is_ascii()
        ||!old.is_ascii(){return None;}
    let original=words(raw);
    if original.len()<3||original.len()>60{return None;}
    let cut=original.iter().position(|w|boundary(w).is_some())?;
    if !(2..=24).contains(&cut){return None;}
    let suffix=boundary(&original[cut])?;
    let content=&original[..cut];
    let expected=words(alias);
    let previous=words(old);
    // Only the whole release-title span proves a correction. IMDb aliases
    // may be translated or refer to another anime part.
    if expected!=content||previous.is_empty()||previous==expected{return None;}
    let direction=if (expected.starts_with(&previous)
            ||expected.ends_with(&previous))
        &&(1..=5).contains(&(expected.len()-previous.len())){
        "underextracted"
    }else if previous.starts_with(&expected)
        &&(1..=5).contains(&(previous.len()-expected.len()))
        &&previous[expected.len()..].iter().all(|s|boundary(s).is_some()){
        "overextracted"
    }else{return None;};
    match (direction,suffix){
        ("underextracted","episode")=>Some("underextracted_episode"),
        ("underextracted","year")=>Some("underextracted_year"),
        ("underextracted","quality")=>Some("underextracted_quality"),
        ("overextracted","episode")=>Some("overextracted_episode"),
        ("overextracted","year")=>Some("overextracted_year"),
        ("overextracted","quality")=>Some("overextracted_quality"),
        _=>None
    }
}
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch("CREATE TABLE IF NOT EXISTS ml_title_boundary_evidence(
        sample_hash TEXT PRIMARY KEY,source_build TEXT NOT NULL,
        signal TEXT NOT NULL,imdb_id TEXT NOT NULL,observed_at INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS ml_title_boundary_cohort
        ON ml_title_boundary_evidence(source_build,signal,imdb_id);
    CREATE INDEX IF NOT EXISTS ml_title_boundary_oldest
        ON ml_title_boundary_evidence(observed_at,sample_hash);")
        .map_err(|e|e.to_string())
}
pub fn budget(db:&Connection)->Result<i64,String>{
    db.query_row("SELECT COUNT(*) FROM ml_title_boundary_evidence",[],
        |r|r.get(0)).map_err(|e|e.to_string())
}
/// Repeat requests still describe a real parser error, but they must not
/// increase evidence counts. Keep the classification separate from whether
/// this particular observation is new in SQLite.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct Observation{
    pub signal:&'static str,
    pub inserted:bool,
}
pub fn observe(db:&Connection,raw:&str,parsed:&Value,selected:&Value,
    count:&mut i64)->Result<Option<Observation>,String>{
    let Some(signal)=signal(raw,parsed,selected)else{return Ok(None);};
    let imdb=selected["imdb_id"].as_str().unwrap_or_default();
    // Raw torrent names and selected work names never enter export payloads.
    let source=build();
    let sample=digest(&format!("{source}:{signal}:{imdb}:{}",raw.to_ascii_lowercase()));
    if *count>=MAX_OBSERVATIONS{
        // Repeated requests must not evict a different work. Reclaim a
        // bounded oldest cohort only when the new observation is novel.
        let known:i64=db.query_row("SELECT EXISTS(SELECT 1 FROM
            ml_title_boundary_evidence WHERE sample_hash=?1)",
            [&sample],|r|r.get(0)).map_err(|e|e.to_string())?;
        if known!=0{return Ok(Some(Observation{signal,inserted:false}));}
        let removed=db.execute("DELETE FROM ml_title_boundary_evidence
            WHERE sample_hash IN (SELECT sample_hash
                FROM ml_title_boundary_evidence
                ORDER BY observed_at,sample_hash LIMIT 256)",
            []).map_err(|e|e.to_string())?;
        *count=count.saturating_sub(removed as i64);
        if *count>=MAX_OBSERVATIONS{
            return Ok(Some(Observation{signal,inserted:false}));
        }
    }
    let inserted=db.execute("INSERT OR IGNORE INTO ml_title_boundary_evidence
        (sample_hash,source_build,signal,imdb_id,observed_at)
        VALUES(?1,?2,?3,?4,strftime('%s','now'))",
        params![sample,source,signal,imdb]).map_err(|e|e.to_string())?;
    if inserted!=0{*count+=1;}
    Ok(Some(Observation{signal,inserted:inserted!=0}))
}
pub fn review_flag(signal:&str)->Value{
    json!({"code":if signal.starts_with("underextracted"){
        "field_underextracted_title"
    }else{"field_overextracted_title"},
        "field":"title","signal":signal,"confidence":"review","severity":"review",
        "message":"Verified raw title span differs from canonical parser title"})
}
pub fn community_proposals(db:&Connection)->Result<Vec<Value>,String>{
    init(db)?;
    let mut stmt=db.prepare("SELECT signal,COUNT(*),COUNT(DISTINCT imdb_id)
        FROM ml_title_boundary_evidence WHERE source_build=?1
        GROUP BY signal HAVING COUNT(*)>=?2 AND COUNT(DISTINCT imdb_id)>=?3
        ORDER BY signal LIMIT 6").map_err(|e|e.to_string())?;
    let found=stmt.query_map(params![build(),MIN_OBSERVATIONS,MIN_WORKS],
        |r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,
            r.get::<_,i64>(2)?))).map_err(|e|e.to_string())?;
    let mut out=Vec::new();
    for item in found{
        let (signal,observations,works)=item.map_err(|e|e.to_string())?;
        let Some(regex)=regex_for(&signal)else{continue;};
        out.push(json!({"kind":"learned_title_boundary_regex_review",
            "fingerprint":digest(&format!("title-boundary:{signal}:{regex}")),
            "field":"title","signal":signal,"regex":regex,
            "source_build":build(),"evidence_count":observations,
            "distinct_titles":works,"canonical_confirmations":0,
            "verification":"matcher_alias_matches_complete_raw_title_review_only",
            "review_only":true}));
    }
    Ok(out)
}
pub fn publish(db:&Connection)->Result<(),String>{
    let count=budget(db)?;
    let ready=community_proposals(db)?.len();
    OBSERVATIONS.store(count.max(0) as u64,Ordering::Relaxed);
    READY_PATTERNS.store(ready as u64,Ordering::Relaxed);
    Ok(())
}
pub fn status()->Value{
    json!({"mode":"verified_parser_matcher_title_discrepancies",
        "observations":OBSERVATIONS.load(Ordering::Relaxed),
        "review_patterns":READY_PATTERNS.load(Ordering::Relaxed),
        "min_examples":MIN_OBSERVATIONS,"min_imdb_works":MIN_WORKS,
        "maximum_local_evidence":MAX_OBSERVATIONS,
        "live_activation":false,"raw_titles_exported":false})
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn accepts_only_independently_aligned_complete_source_titles(){
        let selected=json!({"match_source":"imdb","imdb_id":"tt1234567",
            "matched_title":"The Office US"});
        for (raw,kind) in [
            ("The.Office.US.S01E01.1080p","underextracted_episode"),
            ("The.Office.US.2005.1080p","underextracted_year"),
            ("The.Office.US.1080p","underextracted_quality"),
        ]{
            assert_eq!(signal(raw,&json!({"title":"The Office"}),&selected),Some(kind));
        }
        assert_eq!(signal("The.Office.US.S01E01",
            &json!({"title":"The Office US"}),&selected),None);
        // Losing a leading title word is just as meaningful as losing a
        // trailing word; raw source evidence must still cover the alias.
        assert_eq!(signal("The.Office.US.S01E01",
            &json!({"title":"Office US"}),&selected),
            Some("underextracted_episode"));
        assert_eq!(signal("The.Office.US.S01E01.1080p",
            &json!({"title":"The Office US S01E01"}),&selected),
            Some("overextracted_episode"));
        let flagged=review_flag("underextracted_episode");
        let trusted=crate::field_audit::canonical_for_training(
            &json!({"title":"The Office","seasons":[1],"episodes":[1]}),
            &[flagged]);
        assert!(trusted.get("title").is_none(),
            "a suspect old title must not become a positive model label");
        assert_eq!(trusted["episodes"],json!([1]));
    }
    #[test]
    fn translated_wrong_aliases_paths_and_ambiguous_parts_are_not_labels(){
        let selected=json!({"match_source":"imdb","imdb_id":"tt1234567",
            "matched_title":"The Office US"});
        for raw in ["The.Office.GB.S01E01","The.Office.US.Extra.S01E01",
            "The.Office.US/S01E01.mkv","The.Office.US.Other.1080p"]{
            assert!(signal(raw,&json!({"title":"The Office"}),&selected).is_none(),"{raw}");
        }
        assert!(signal("The.Office.US.S01E01",
            &json!({"title":"Wrong Series"}),&selected).is_none());
        let translated=json!({"match_source":"imdb","imdb_id":"tt1234567",
            "matched_title":"English Name"});
        assert!(signal("Shinigami.Bocchan.S01E01",
            &json!({"title":"Shinigami"}),&translated).is_none());
        let anime=json!({"match_source":"anime","imdb_id":"tt1234567",
            "matched_title":"The Office US"});
        assert!(signal("The.Office.US.S01E01",
            &json!({"title":"The Office"}),&anime).is_none());
    }
    #[test]
    fn no_community_proposal_from_one_popular_work(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let mut count=0;
        let selected=json!({"match_source":"imdb","imdb_id":"tt1234567",
            "matched_title":"Alpha Beta Gamma"});
        for i in 1..=20{
            let raw=format!("Alpha.Beta.Gamma.S01E{:02}",i);
            assert!(observe(&db,&raw,&json!({"title":"Alpha Beta"}),
                &selected,&mut count).unwrap().is_some());
        }
        assert_eq!(count,20);
        assert!(community_proposals(&db).unwrap().is_empty(),
            "one IMDb identity is not independent evidence");
    }
    #[test]
    fn requires_independent_works_and_deduplicates_repeated_requests(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let mut count=budget(&db).unwrap();
        for i in 1..=12 {
            let imdb=format!("tt{:07}",(i-1)%4+1);
            let selected=json!({"match_source":"imdb","imdb_id":imdb,
                "matched_title":"Alpha Beta Gamma"});
            let raw=format!("Alpha.Beta.Gamma.S01E{:02}.WEB-DL",i);
            assert_eq!(observe(&db,&raw,&json!({"title":"Alpha Beta"}),
                &selected,&mut count).unwrap().map(|o|o.signal),
                Some("underextracted_episode"));
            assert_eq!(observe(&db,&raw,&json!({"title":"Alpha Beta"}),
                &selected,&mut count).unwrap(),
                Some(Observation{signal:"underextracted_episode",inserted:false}));
        }
        let proposals=community_proposals(&db).unwrap();
        assert_eq!(count,12);
        assert_eq!(proposals.len(),1);
        assert_eq!(proposals[0]["review_only"],true);
        assert_eq!(proposals[0]["distinct_titles"],4);
        assert!(!proposals[0].to_string().contains("Alpha"));
    }
}
