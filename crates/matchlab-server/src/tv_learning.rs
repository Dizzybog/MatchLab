//! Additive / corrective ordinary-TV coordinate regex learning.
//! A restricted, explicit grammar produces candidates; the background
//! worker checks independent filename evidence and multiple canonical
//! confirmations before publishing compiled rules. No model inference,
//! database lookup or regex compilation occurs on the request path.
use regex::Regex;
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::sync::{OnceLock,RwLock,atomic::{AtomicU64,Ordering}};

const EPOCH:u32=2;
const MIN_EXAMPLES:i64=8;
const MIN_IDENTITIES:i64=4;
const OVERRIDE_EXAMPLES:i64=16;
const OVERRIDE_IDENTITIES:i64=6;
static TEMPLATES:OnceLock<Vec<(&'static str,Regex)>>=OnceLock::new();
static ACTIVE:OnceLock<RwLock<Vec<Active>>>=OnceLock::new();
static REVISION:AtomicU64=AtomicU64::new(0);

#[derive(Clone,Debug,PartialEq,Eq)]
struct Coordinates { season:i64, episode:i64 }
#[derive(Clone,Debug,PartialEq,Eq)]
struct Active { family:String, may_override:bool }
fn registry()->&'static RwLock<Vec<Active>>{
    ACTIVE.get_or_init(||RwLock::new(Vec::new()))
}
pub fn revision()->u64{REVISION.load(Ordering::Acquire)}
fn build()->String{
    format!("{}:{EPOCH}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"))
}
fn hash(s:&str)->String{format!("{:x}",Sha256::digest(s.as_bytes()))}
fn templates()->&'static Vec<(&'static str,Regex)>{
    TEMPLATES.get_or_init(||{
        [
            ("sxe",r"(?i)(?:^|[._ -])s(?P<s>[0-9]{1,3})[._ -]*e(?P<e>[0-9]{1,5})(?:$|[._ -])"),
            ("x",r"(?i)(?:^|[._ -])(?P<s>[0-9]{1,3})x(?P<e>[0-9]{1,5})(?:$|[._ -])"),
            ("words",r"(?i)(?:^|[._ -])season[._ -]+(?P<s>[0-9]{1,3})[._ -]+episode[._ -]+(?P<e>[0-9]{1,5})(?:$|[._ -])"),
        ].into_iter().map(|(k,pattern)|
            (k,Regex::new(pattern).expect("safe, bounded coordinate grammar")))
            .collect()
    })
}
fn safe_name(name:&str)->bool{
    (8..=240).contains(&name.len())
        && !name.contains(['/','\\','@','\r','\n','\0'])
        && !["password=","token=","apikey=","secret=","magnet:"]
            .iter().any(|s|name.to_ascii_lowercase().contains(s))
}
fn capture(name:&str,re:&Regex)->Option<Coordinates>{
    let mut found=re.captures_iter(name);
    let caps=found.next()?;
    if found.next().is_some(){return None;}
    let whole=caps.get(0)?;
    // The boundary regex consumes its trailing separator. A second
    // SxxExx marker can start at that separator, so captures_iter alone
    // would miss it. Re-scan the shared boundary to reject ranges/packs.
    if whole.end()<name.len() {
        let start=whole.end().saturating_sub(1);
        if re.is_match(&name[start..]){return None;}
    }
    let prefix=name[..whole.start()].trim_end_matches(['.','_','-',' ']);
    if prefix.len()<4 || !prefix.chars().any(char::is_alphabetic){
        return None;
    }
    let s=caps.name("s")?.as_str().parse::<i64>().ok()?;
    let e=caps.name("e")?.as_str().parse::<i64>().ok()?;
    ((1..=120).contains(&s) && (1..=99_999).contains(&e))
        .then_some(Coordinates{season:s,episode:e})
}
fn detected(name:&str)->Option<(&'static str,Coordinates)>{
    if !safe_name(name){return None;}
    // A release pack/range must not be collapsed to its first episode,
    // including S02E07-E08 and 2x07-08 variants that have only one
    // SxxExx/NxM prefix. Reject before learning and before hot application.
    static RANGE:OnceLock<Regex>=OnceLock::new();
    let range=RANGE.get_or_init(||Regex::new(
        r"(?i)(?:s[0-9]{1,3}[ ._-]*e[0-9]{1,5}|[0-9]{1,3}x[0-9]{1,5}|season[ ._-]+[0-9]{1,3}[ ._-]+episode[ ._-]+[0-9]{1,5})[ ._-]*(?:-|to|through)[ ._-]*(?:e(?:p(?:isode)?)?[ ._-]*)?[0-9]{1,5}(?:$|[ ._-])"
    ).expect("bounded TV range guard"));
    if range.is_match(name){return None;}
    let mut winner=None;
    for (family,re) in templates(){
        if let Some(coordinates)=capture(name,re){
            if winner.is_some(){return None;} // ambiguous or conflicting
            winner=Some((*family,coordinates));
        }
    }
    winner
}
/// Independent raw-name reference for the background parser auditor.
/// Never supplies an output field or a training label by itself.
pub(crate) fn explicit_coordinates(name:&str)->Option<(i64,i64)>{
    detected(name).map(|(_,coordinates)|(
        coordinates.season,coordinates.episode))
}
fn identity(parsed:&Value)->Option<String>{
    let raw=parsed["title"].as_str()?;
    let normalized=crate::matchlab_matcher::clean_title(raw);
    (4..=140).contains(&normalized.len()).then(||hash(&normalized))
}
fn aligned(parsed:&Value,c:&Coordinates)->bool{
    parsed["seasons"].as_array().is_some_and(|a|
        a.len()==1 && a[0].as_i64()==Some(c.season))
    && parsed["episodes"].as_array().is_some_and(|a|
        a.len()==1 && a[0].as_i64()==Some(c.episode))
}
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS learned_tv_regex_evidence(
            example_hash TEXT PRIMARY KEY,family TEXT NOT NULL,
            identity_hash TEXT NOT NULL,canonical_aligned INTEGER NOT NULL,
            source_build TEXT NOT NULL,observed_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS learned_tv_regex_build_idx
            ON learned_tv_regex_evidence(source_build,family,identity_hash);"
    ).map_err(|e|e.to_string())
}
pub fn count(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row("SELECT count(*) FROM learned_tv_regex_evidence
        WHERE source_build=?1",[build()],|r|r.get(0))
        .map_err(|e|e.to_string())
}
pub fn observe(db:&Connection,name:&str,parsed:&Value,count:&mut i64)
    ->Result<Vec<(String,String)>,String>{
    let Some((family,coords))=detected(name)else{return Ok(Vec::new());};
    let Some(identity)=identity(parsed)else{return Ok(Vec::new());};
    let example=hash(&format!("{family}:{}",name.to_ascii_lowercase()));
    let epoch=build();
    let known:i64=db.query_row(
        "SELECT EXISTS(SELECT 1 FROM learned_tv_regex_evidence
         WHERE example_hash=?1 AND source_build=?2)",
        params![example,epoch],|r|r.get(0)).map_err(|e|e.to_string())?;
    if known!=0 {return Ok(Vec::new());}
    // One bounded training allocation; coordinate samples cannot monopolize
    // all space reserved for technical field observations.
    let capacity=(crate::learning_cache::training_max_examples()/5).max(20);
    if *count>=capacity{
        let old=(capacity/20).clamp(1,256);
        *count=count.saturating_sub(db.execute(
            "DELETE FROM learned_tv_regex_evidence WHERE example_hash IN
             (SELECT example_hash FROM learned_tv_regex_evidence
              WHERE source_build=?1 ORDER BY observed_at,example_hash LIMIT ?2)",
            params![epoch,old]).map_err(|e|e.to_string())? as i64);
        if *count>=capacity{return Ok(Vec::new());}
    }
    db.execute("INSERT INTO learned_tv_regex_evidence(
        example_hash,family,identity_hash,canonical_aligned,source_build,observed_at)
        VALUES(?1,?2,?3,?4,?5,strftime('%s','now'))
        ON CONFLICT(example_hash) DO UPDATE SET
        family=excluded.family,identity_hash=excluded.identity_hash,
        canonical_aligned=excluded.canonical_aligned,source_build=excluded.source_build,
        observed_at=excluded.observed_at",
        params![example,family,identity,i64::from(aligned(parsed,&coords)),epoch])
        .map_err(|e|e.to_string())?;
    *count+=1;
    Ok(vec![
        ("seasons".into(),family.into()),
        ("episodes".into(),family.into())
    ])
}
fn select(db:&Connection)->Result<Vec<Active>,String>{
    init(db)?;
    let mut stmt=db.prepare(
        "SELECT family,count(*) AS n,count(DISTINCT identity_hash) AS titles,
                sum(canonical_aligned) AS confirmations
         FROM learned_tv_regex_evidence WHERE source_build=?1
         GROUP BY family
         HAVING n>=?2 AND titles>=?3 AND confirmations>=2
         ORDER BY family"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![build(),MIN_EXAMPLES,MIN_IDENTITIES],|r|{
        let family:String=r.get(0)?;
        let examples:i64=r.get(1)?;
        let identities:i64=r.get(2)?;
        let confirmations:i64=r.get(3)?;
        Ok(Active{family,may_override:examples>=OVERRIDE_EXAMPLES
            &&identities>=OVERRIDE_IDENTITIES&&confirmations>=4})
    }).map_err(|e|e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())
}
// Each promoted pattern is only a regex. The source handler supplies
// MatchLab's existing array(integer) transform and target field.
fn capture_pattern(family:&str,field:&str)->Option<&'static str>{
    match (family,field){
        ("sxe","seasons")=>Some(r"(?i)(?:^|[._ -])s([0-9]{1,3})[._ -]*e[0-9]{1,5}(?:$|[._ -])"),
        ("sxe","episodes")=>Some(r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,5})(?:$|[._ -])"),
        ("x","seasons")=>Some(r"(?i)(?:^|[._ -])([0-9]{1,3})x[0-9]{1,5}(?:$|[._ -])"),
        ("x","episodes")=>Some(r"(?i)(?:^|[._ -])[0-9]{1,3}x([0-9]{1,5})(?:$|[._ -])"),
        ("words","seasons")=>Some(r"(?i)(?:^|[._ -])season[._ -]+([0-9]{1,3})[._ -]+episode[._ -]+[0-9]{1,5}(?:$|[._ -])"),
        ("words","episodes")=>Some(r"(?i)(?:^|[._ -])season[._ -]+[0-9]{1,3}[._ -]+episode[._ -]+([0-9]{1,5})(?:$|[._ -])"),
        _=>None,
    }
}
fn additions_for(active:&[Active])->Vec<matchlab_parser::HandlerAddition>{
    let catalog=matchlab_parser::existing_regex_rules();
    let mut out=Vec::new();
    for rule in active {
        for field in ["seasons","episodes"]{
            let Some(pattern)=capture_pattern(&rule.family,field)else{continue;};
            let template=catalog.iter().find(|entry|
                entry["field"]==field
                &&entry["transform"]=="array(integer)"
                &&entry["options"]["skipIfAlreadyFound"]==true
                &&entry["index"].as_u64().is_some()
            );
            if let Some(index)=template.and_then(|r|r["index"].as_u64()){
                out.push(matchlab_parser::HandlerAddition{
                    source_index:index as usize,pattern:pattern.to_owned()
                });
            }
        }
    }
    out
}
pub fn handler_additions()->Vec<matchlab_parser::HandlerAddition>{
    registry().read().ok().map(|r|additions_for(&r)).unwrap_or_default()
}
pub fn reconcile(db:&Connection)->Result<(),String>{
    let updated=select(db)?;
    if registry().read().ok().is_some_and(|g|*g==updated)
        && revision()!=0{return Ok(());}
    let identity=updated.iter().map(|r|
        format!("{}:{}",r.family,r.may_override))
        .collect::<Vec<_>>().join("\n");
    let digest=Sha256::digest(identity.as_bytes());
    let mut version=[0u8;8];
    version.copy_from_slice(&digest[..8]);
    // Test and compile all new regex atomically before publishing status.
    // The normal parser, not the learner, will interpret season/episode.
    crate::field_learning::publish_parser_rules_with_tv(additions_for(&updated))?;
    let mut lock=registry().write().map_err(|e|e.to_string())?;
    *lock=updated;
    REVISION.store(u64::from_le_bytes(version),Ordering::Release);
    Ok(())
}
/// Independently replayable, privacy-safe proposals for the community.
/// Only fixed pattern families and aggregate evidence, never filenames.
pub fn community_proposals(db:&Connection)->Result<Vec<Value>,String>{
    let active=select(db)?;
    let mut out=Vec::new();
    for rule in active {
        let Some((_,expression))=templates().iter()
            .find(|(name,_)|*name==rule.family)else{continue;};
        let (count,identities,confirmed):(i64,i64,i64)=db.query_row(
            "SELECT COUNT(*),COUNT(DISTINCT identity_hash),
                SUM(canonical_aligned)
             FROM learned_tv_regex_evidence
             WHERE family=?1 AND source_build=?2",
            params![rule.family,build()],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
        ).map_err(|e|e.to_string())?;
        if count<12||identities<4||confirmed<2{continue;}
        let (Some(season_regex),Some(episode_regex))=(
            capture_pattern(&rule.family,"seasons"),
            capture_pattern(&rule.family,"episodes")
        )else{continue;};
        out.push(json!({
            "kind":"learned_tv_regex",
            "fingerprint":hash(&format!("community-tv:{}",rule.family)),
            "family":rule.family,
            "fields":["seasons","episodes"],
            "regex":expression.as_str(),
            "native_patterns":{"seasons":season_regex,"episodes":episode_regex},
            "evidence_count":count,
            "distinct_titles":identities,
            "canonical_confirmations":confirmed,
            "source_build":build(),
            "verification":"explicit_tv_marker_and_canonical_confirmations",
        }));
    }
    Ok(out)
}

pub fn status()->Value{
    let rules=registry().read().ok().map(|r|r.clone()).unwrap_or_default();
    json!({"active_rule_count":rules.len(),
        "high_evidence_families":rules.iter().filter(|r|r.may_override).count(),
        "families":rules.iter().map(|r|r.family.clone()).collect::<Vec<_>>(),
        "scope":"ordinary TV SxxExx, NxM and Season N Episode M"})
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn raw_coordinates_are_bounded_and_ambiguous_names_fail_closed(){
        assert_eq!(detected("Show.S02E07.1080p").unwrap().1,
            Coordinates{season:2,episode:7});
        assert_eq!(detected("Show.Season.2.Episode.7.1080p").unwrap().1,
            Coordinates{season:2,episode:7});
        assert!(detected("Show.02x07.720p").is_some());
        assert!(detected("Show.S02E07.S03E08.1080p").is_none());
        assert!(detected("Show.S02E07-E08.1080p").is_none());
        assert!(detected("Show.02x07-08.1080p").is_none());
        assert!(detected("Show.2024.WEB-DL").is_none());
    }
    #[test]
    fn tv_learner_only_publishes_patterns_not_values(){
        let active=Active{family:"sxe".into(),may_override:false};
        let rules=additions_for(&[active]);
        assert_eq!(rules.len(),2);
        let source=matchlab_parser::existing_regex_rules();
        let fields=rules.iter().map(|r|
            source[r.source_index]["field"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(fields,vec!["seasons","episodes"]);
        let compiled=rules.iter().map(|r|Regex::new(&r.pattern).unwrap()).collect::<Vec<_>>();
        assert!(compiled.iter().all(|r|r.is_match("Show.S02E07.1080p")));
    }
    #[test]
    fn promotion_needs_diverse_titles_and_canonical_replay(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let mut count=0;
        for n in 0..7 {
            let name=format!("Series{n}.S02E07.1080p");
            let parsed=json!({"title":format!("Series{n}"),
                "seasons":[2],"episodes":[7]});
            observe(&db,&name,&parsed,&mut count).unwrap();
        }
        assert!(select(&db).unwrap().is_empty());
        observe(&db,"Series7.S02E07.1080p",
            &json!({"title":"Series7","seasons":[2],"episodes":[7]}),
            &mut count).unwrap();
        let rules=select(&db).unwrap();
        assert_eq!(rules.len(),1);
        assert!(!rules[0].may_override);
        for n in 8..16 {
            let name=format!("Series{n}.S02E07.1080p");
            let parsed=json!({"title":format!("Series{n}"),
                "seasons":[2],"episodes":[7]});
            observe(&db,&name,&parsed,&mut count).unwrap();
        }
        assert!(select(&db).unwrap()[0].may_override);
        let proposals=community_proposals(&db).unwrap();
        assert_eq!(proposals.len(),1);
        assert_eq!(
            proposals[0]["native_patterns"]["episodes"],
            capture_pattern("sxe","episodes").unwrap());
        assert_eq!(
            proposals[0]["native_patterns"]["seasons"],
            capture_pattern("sxe","seasons").unwrap());
    }
    #[test]
    fn training_never_uses_previous_overlay_as_canonical(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let mut n=0;
        let parsed=json!({"title":"Show","seasons":[2],"episodes":[7],
            "learned_tv_regex_changes":[{"field":"seasons","new":[2]}]});
        observe(&db,"Show.S02E07.1080p",&parsed,&mut n).unwrap();
        assert_eq!(n,1);
        let selected=select(&db).unwrap();
        assert!(selected.is_empty());
    }
}
