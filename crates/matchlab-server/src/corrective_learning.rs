//! Evidence-gated corrective regexes for four classes of parser observations.
//! Missing fields retain their independent additive learner. This module
//! addresses nonempty but incorrect TV coordinates and incomplete TV ranges.
//! No ML weights or observations can directly rewrite a parsed output field.
use regex::Regex;
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::collections::{BTreeMap,BTreeSet};
use std::sync::{OnceLock,RwLock,atomic::{AtomicU64,Ordering}};

const EPOCH:u32=1;
const MAX_OBSERVATIONS:i64=1_024;
const MIN_OBSERVATIONS:i64=8;
const MIN_IDENTITIES:i64=4;
const MIN_ERRORS:i64=4;
const MIN_ERROR_IDENTITIES:i64=2;
const MIN_CONFIRMATIONS:i64=2;
static ACTIVE:OnceLock<RwLock<Vec<ActiveRule>>>=OnceLock::new();
static REVISION:AtomicU64=AtomicU64::new(0);
static REVIEWED:AtomicU64=AtomicU64::new(0);

#[derive(Clone,Debug,PartialEq,Eq)]
struct ActiveRule {
    family:String,
    corrections:Vec<matchlab_parser::HandlerCorrection>,
}
fn registry()->&'static RwLock<Vec<ActiveRule>>{
    ACTIVE.get_or_init(||RwLock::new(Vec::new()))
}
pub fn revision()->u64{REVISION.load(Ordering::Acquire)}
fn hash(value:&str)->String{format!("{:x}",Sha256::digest(value.as_bytes()))}
fn build()->String{
    format!("{}:{EPOCH}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"))
}
fn safe(name:&str)->bool{
    (8..=240).contains(&name.len()) &&
    !name.contains(['/','\\','\r','\n','\0','@']) &&
    !["password=","token=","apikey=","secret=","magnet:"]
        .iter().any(|word|name.to_ascii_lowercase().contains(word))
}
#[derive(Clone,Debug)]
struct Expected{
    family:&'static str,
    season:i64,
    episodes:Vec<i64>,
}
fn expected(name:&str)->Option<Expected>{
    if !safe(name){return None;}
    static RANGE:OnceLock<Regex>=OnceLock::new();
    let re=RANGE.get_or_init(||Regex::new(
        r"(?i)(?:^|[._ -])s([0-9]{1,3})[._ -]*e([0-9]{1,4})[._ -]*(?:-|to|through)[._ -]*(?:e(?:p)?)?[._ -]*([0-9]{1,4})(?:$|[._ -])"
    ).expect("bounded corrective TV episode range"));
    if let Some(caps)=re.captures(name){
        if re.captures_iter(name).take(2).count()!=1{return None;}
        let s=caps.get(1)?.as_str().parse::<i64>().ok()?;
        let e1=caps.get(2)?.as_str().parse::<i64>().ok()?;
        let e2=caps.get(3)?.as_str().parse::<i64>().ok()?;
        let prefix=&name[..caps.get(0)?.start()];
        if prefix.chars().filter(|c|c.is_alphabetic()).count()<4
            ||!(1..=120).contains(&s)||e1<=0||e2<=e1||e2-e1>32 {
            return None;
        }
        // Reject mixed packs, multiple SxxEyy tokens or a second range.
        if re.find_iter(name).count()!=1
            ||crate::tv_learning::explicit_coordinates(name).is_some(){
            return None;
        }
        return Some(Expected{family:"sxe_range",season:s,
            episodes:(e1..=e2).collect()});
    }
    let (family,season,episode)=crate::tv_learning::recognized_family(name)?;
    Some(Expected{family,season,episodes:vec![episode]})
}
fn ints(parsed:&Value,field:&str)->Vec<i64>{
    parsed[field].as_array().map(|v|
        v.iter().filter_map(Value::as_i64).collect()).unwrap_or_default()
}
fn category(expected:&Expected,parsed:&Value)->&'static str{
    let seasons=ints(parsed,"seasons");
    let episodes=ints(parsed,"episodes");
    let full_interval=expected.episodes.len()>1
        &&parsed["episode_start"].as_i64()==expected.episodes.first().copied()
        &&parsed["episode_end"].as_i64()==expected.episodes.last().copied()
        && (episodes.is_empty()
            ||episodes==[expected.episodes[0]]
            ||episodes==expected.episodes);
    if seasons==[expected.season]
        &&(episodes==expected.episodes||full_interval){return "aligned";}
    if seasons.is_empty()
        || (episodes.is_empty()&&!full_interval){return "missing";}
    if episodes.len()>expected.episodes.len()
        && expected.episodes.iter().all(|v|episodes.contains(v)){return "over";}
    if episodes.len()<expected.episodes.len()
        && episodes.iter().all(|v|expected.episodes.contains(v)){return "under";}
    if expected.episodes.len()==1
        && seasons==expected.episodes&&episodes==[expected.season]{
        return "misassigned";
    }
    "incorrect"
}
fn independent_identity(parsed:&Value)->Option<String>{
    let title=parsed["title"].as_str()?;
    let title=crate::imdb_matcher::clean_title(title);
    (4..=140).contains(&title.len()).then(||hash(&title))
}
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch("CREATE TABLE IF NOT EXISTS ml_corrective_observations(
        example_hash TEXT PRIMARY KEY,
        identity_hash TEXT NOT NULL,
        family TEXT NOT NULL,
        error_class TEXT NOT NULL,
        release_name TEXT NOT NULL,
        source_build TEXT NOT NULL,
        observed_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS ml_corrective_family_idx
            ON ml_corrective_observations(source_build,family,identity_hash);")
        .map_err(|e|e.to_string())
}
pub fn observe(db:&Connection,name:&str,parsed:&Value,
    budget:&mut i64)->Result<(),String>{
    let Some(reference)=expected(name)else{return Ok(());};
    let Some(identity)=independent_identity(parsed)else{return Ok(());};
    if *budget>=MAX_OBSERVATIONS{
        let old=db.execute("DELETE FROM ml_corrective_observations WHERE
            example_hash IN (SELECT example_hash FROM ml_corrective_observations
                WHERE source_build=?1 ORDER BY observed_at,example_hash LIMIT 32)",
            [build()]).map_err(|e|e.to_string())? as i64;
        *budget=budget.saturating_sub(old);
        if *budget>=MAX_OBSERVATIONS{return Ok(());}
    }
    let key=hash(&format!("{}:{name}",reference.family));
    let seen:i64=db.query_row("SELECT EXISTS(SELECT 1
        FROM ml_corrective_observations WHERE example_hash=?1 AND source_build=?2)",
        params![key,build()],|r|r.get(0)).map_err(|e|e.to_string())?;
    db.execute("INSERT INTO ml_corrective_observations
        (example_hash,identity_hash,family,error_class,release_name,source_build,observed_at)
        VALUES(?1,?2,?3,?4,?5,?6,strftime('%s','now'))
        ON CONFLICT(example_hash) DO UPDATE SET
            identity_hash=excluded.identity_hash,
            error_class=excluded.error_class,
            release_name=excluded.release_name,
            source_build=excluded.source_build,
            observed_at=excluded.observed_at",
        params![key,identity,reference.family,category(&reference,parsed),name,build()]
    ).map_err(|e|e.to_string())?;
    if seen==0{*budget+=1;}
    Ok(())
}
pub fn count(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row("SELECT COUNT(*) FROM ml_corrective_observations
        WHERE source_build=?1",[build()],|r|r.get(0)).map_err(|e|e.to_string())
}
fn source(field:&str,transform:&str)->Option<usize>{
    matchlab_parser::existing_regex_rules().into_iter().find(|v|
        v["field"]==field&&v["transform"]==transform
        &&v["options"]["remove"]==false
        &&v["options"]["skipIfAlreadyFound"]==true
    ).and_then(|row|row["index"].as_u64().map(|v|v as usize))
}
fn candidate(family:&str)->Option<Vec<matchlab_parser::HandlerCorrection>>{
    if family=="sxe_range"{
        // group 1 is the complete range, so the original range_func
        // expands the entire interval using its existing bounded transform.
        return Some(vec![matchlab_parser::HandlerCorrection{
            source_index:source("episodes","range_func")?,
            pattern:r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,4}[._ -]*-[._ -]*(?:e(?:p)?)?[._ -]*[0-9]{1,4})(?:$|[._ -])".into(),
            is_range:true
        }]);
    }
    let mut out=Vec::new();
    for field in ["seasons","episodes"]{
        let pattern=crate::tv_learning::capture_pattern(family,field)?;
        out.push(matchlab_parser::HandlerCorrection{
            source_index:source(field,"array(integer)")?,
            pattern:pattern.into(),is_range:false
        });
    }
    Some(out)
}
#[derive(Default)]
struct Counts {
    examples:i64,identities:BTreeSet<String>,
    errors:i64,error_identities:BTreeSet<String>,confirmations:i64,
    kinds:BTreeSet<String>,
}
fn eligible(db:&Connection)->Result<Vec<(String,Counts)>,String>{
    init(db)?;
    let mut stmt=db.prepare("SELECT family,identity_hash,error_class FROM
        ml_corrective_observations WHERE source_build=?1 ORDER BY family,identity_hash")
        .map_err(|e|e.to_string())?;
    let values=stmt.query_map([build()],|row|Ok((
        row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))
        .map_err(|e|e.to_string())?;
    let mut counts=BTreeMap::<String,Counts>::new();
    for row in values{
        let (family,id,kind)=row.map_err(|e|e.to_string())?;
        let entry=counts.entry(family).or_default();
        entry.examples+=1;
        entry.identities.insert(id.clone());
        if kind=="aligned"{entry.confirmations+=1;}
        else {
            entry.errors+=1;entry.error_identities.insert(id);
            entry.kinds.insert(kind);
        }
    }
    Ok(counts.into_iter().filter(|(_,v)|
        v.examples>=MIN_OBSERVATIONS
        &&v.identities.len() as i64>=MIN_IDENTITIES
        &&v.errors>=MIN_ERRORS
        &&v.error_identities.len() as i64>=MIN_ERROR_IDENTITIES
        &&v.confirmations>=MIN_CONFIRMATIONS
    ).collect())
}
fn review_candidate(db:&Connection,family:&str,
    corrections:&[matchlab_parser::HandlerCorrection])->Result<bool,String>{
    let mut stmt=db.prepare("SELECT release_name FROM ml_corrective_observations
        WHERE family=?1 AND source_build=?2
        ORDER BY observed_at DESC,example_hash LIMIT 100")
        .map_err(|e|e.to_string())?;
    let mut names=stmt.query_map(params![family,build()],|row|row.get::<_,String>(0))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    // Negative controls must remain unchanged: movie titles, numbers,
    // multi-episode packs, and an independent TV format.
    names.extend(["The100.2014.S01E01.1080p","District.9.2009.1080p",
        "Example.S01E01.S02E02.1080p","Example.S01E01-E03.1080p",
        "Episode.300.Explained.2025.1080p","The.Danish.Girl.2015.1080p",
        "Movie.1917.2019.1080p","Example.2005.2160p.WEB-DL"]
        .iter().map(|s|(*s).to_owned()));
    if names.len()>128{return Ok(false);}
    let replay=match matchlab_parser::replay_corrective_regex(corrections,&names){
        Ok(values)=>values,Err(_)=>return Ok(false)
    };
    let mut repaired=0usize;
    for (name,new) in names.iter().zip(replay){
        let old=match matchlab_parser::parse_title_canonical(name,false){
            Ok(value)=>value,Err(_)=>return Ok(false)
        };
        // Changes outside the two independently validated fields must
        // fail closed, including newly introduced output keys.
        let mut previous=old.clone();
        let mut candidate=new.clone();
        for field in ["seasons","episodes"]{
            previous.remove(field);
            candidate.remove(field);
        }
        if previous!=candidate{return Ok(false);}
        if let Some(reference)=expected(name)
            &&reference.family==family {
            if category(&reference,&Value::Object(new.clone()))!="aligned"{
                return Ok(false);
            }
            if category(&reference,&Value::Object(old))!="aligned"{
                repaired+=1;
            }
        }else if old.get("seasons")!=new.get("seasons")
            ||old.get("episodes")!=new.get("episodes"){
            return Ok(false);
        }
    }
    Ok(repaired>=MIN_ERRORS as usize)
}
pub fn handler_corrections()->Vec<matchlab_parser::HandlerCorrection>{
    registry().read().map(|rules|rules.iter()
        .flat_map(|r|r.corrections.iter().cloned()).collect())
        .unwrap_or_default()
}
pub fn reconcile(db:&Connection)->Result<(),String>{
    let candidates=eligible(db)?;
    let mut ready=Vec::new();
    let mut pending=0u64;
    for (family,_evidence) in candidates {
        let Some(corrections)=candidate(&family)else{continue;};
        if review_candidate(db,&family,&corrections)?{
            ready.push(ActiveRule{family,corrections});
        }else{pending+=1;}
    }
    REVIEWED.store(pending,Ordering::Relaxed);
    if registry().read().ok().is_some_and(|r|*r==ready)
        && revision()!=0 {return Ok(());}
    let next=ready.iter().flat_map(|r|r.corrections.iter().cloned())
        .collect::<Vec<_>>();
    crate::field_learning::publish_parser_rules_with_tv_and_corrections(
        crate::tv_learning::handler_additions(),next)?;
    let fingerprint=ready.iter().map(|r|r.family.as_str())
        .collect::<Vec<_>>().join("|");
    let digest=Sha256::digest(fingerprint.as_bytes());
    let mut bytes=[0u8;8];bytes.copy_from_slice(&digest[..8]);
    *registry().write().map_err(|e|e.to_string())?=ready;
    REVISION.store(u64::from_le_bytes(bytes).max(1),Ordering::Release);
    Ok(())
}
pub fn status()->Value{
    let families=registry().read().map(|r|
        r.iter().map(|v|v.family.clone()).collect::<Vec<_>>()).unwrap_or_default();
    json!({"active_correction_families":families,
        "correction_count":handler_corrections().len(),
        "eligible_but_replay_rejected":REVIEWED.load(Ordering::Relaxed),
        "requires":"8 observations / 4 distinct titles / 4 errors in 2 titles / 2 positive confirmations / successful replay",
        "model_role":"advisory; only native-handler regex rules are published",
        "four_error_classes":["missing","over","under","misassigned_or_incorrect"],
        "scope":"verified explicit TV coordinate grammars; unsafe field families remain review-only"})
}
pub fn community_proposals(db:&Connection)->Result<Vec<Value>,String>{
    let mut out=Vec::new();
    for (family,counts) in eligible(db)? {
        let Some(corrections)=candidate(&family)else{continue;};
        let active=handler_corrections();
        for correction in corrections{
            let fingerprint=hash(&format!("corrective:{family}:{}:{}:{}",
                correction.source_index,correction.is_range,correction.pattern));
            out.push(json!({"kind":"learned_corrective_regex",
                "fingerprint":fingerprint,"family":family,
                "field":if correction.is_range{"episodes"}else{"seasons_or_episodes"},
                "signal":format!("corrective_{family}"),
                "regex":correction.pattern,
                "handler_index":correction.source_index,
                "evidence_count":counts.examples,
                "distinct_titles":counts.identities.len(),
                "canonical_confirmations":counts.confirmations,
                "failure_classes":counts.kinds,
                "source_build":build(),
                "review_only":!active.contains(&correction),
                "verification":if active.contains(&correction){
                    "independent_coordinates_and_regression_replay"
                }else{"review_only_requires_safe_replay"}}));
        }
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_coordinates_have_separate_error_classes(){
        let e=expected("Example.Show.S02E07.1080p").unwrap();
        assert_eq!(e.family,"sxe");
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[7]})),"aligned");
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[]})),"missing");
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[7,8]})),"over");
        assert_eq!(category(&e,&json!({"seasons":[7],"episodes":[2]})),"misassigned");
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[9]})),"incorrect");
    }
    #[test]
    fn ranges_can_be_underextracted_but_not_confused_with_singles(){
        let e=expected("Example.Show.S02E03-E05.1080p").unwrap();
        assert_eq!(e.family,"sxe_range");
        assert_eq!(e.episodes,vec![3,4,5]);
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[3]})),"under");
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[3,4,5]})),"aligned");
        assert_eq!(category(&e,&json!({"seasons":[2],"episodes":[3],
            "episode_start":3,"episode_end":5,"episode_count":3})),"aligned");
        assert!(expected("Example.Show.S01E02.S02E03").is_none());
        assert!(expected("Example.Show.S01E04-E03").is_none());
    }
    #[test]
    fn corrective_regex_uses_only_fixed_native_handlers(){
        for family in ["sxe","x","words","sxe_range"]{
            let rules=candidate(family).expect("native field handler");
            assert!(!rules.is_empty());
            let names=match family{
                "sxe"=>"Example.Show.S02E07.1080p",
                "x"=>"Example.Show.2x07.1080p",
                "words"=>"Example.Show.Season 2 Episode 7.1080p",
                _=>"Example.Show.S02E03-E05.1080p"
            };
            let results=matchlab_parser::replay_corrective_regex(&rules,&[names.to_owned()])
                .expect("compile native correction");
            let expected=expected(names).unwrap();
            assert_eq!(category(&expected,&json!(results[0])),"aligned",
                "{family}: {}",json!(results[0]));
        }
    }
}
