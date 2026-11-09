//! Generic, self-supervised native-handler regex induction for every parser
//! field. The learner uses meaning-preserving release-tag separator mutations:
//! a successfully parsed canonical release is the reference; a punctuation-
//! only variant must retain the COMPLETE result, or it is not a safe rule.
//! No network, inference or ML weights execute on the parsing hot path.
use matchlab_parser::{HandlerAddition,HandlerCorrection,ParseContext};
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::collections::{BTreeMap,BTreeSet};
use std::sync::{OnceLock,RwLock,atomic::{AtomicU64,Ordering}};

const EPOCH:u32=1;
const MIN_FIXES:usize=4;
const MIN_IDENTITIES:usize=2;
const MAX_ACTIVE:usize=48;
const MAX_EXAMPLES:i64=16_384;
const MAX_CANDIDATES:usize=24;
const MAX_TESTS:usize=120;
const NEGATIVES:&[&str]=&[
    "The.Danish.Girl.2015.1080p",
    "The.German.Connection.2005.1080p",
    "A.Better.Tomorrow.1986.720p",
    "HDR.Explained.2024.1080p",
    "S01E01.1999.Movie.Title.1080p",
    "Episode.300.Explained.2024",
    "The100.2014.S01E01.1080p",
    "Example.S01E01.S02E02.1080p",
    "Example.S01E01-E04.1080p",
    "Example.2025.WEB-DL.AAC.5.1",
    "Example.2025.1080p.DDP5.1.HDR",
    "Film.1917.2019.2160p.BluRay",
    "Movie.2005.1080p.WEB-DL",
];
#[derive(Clone,Debug,PartialEq,Eq)]
struct Active {
    key:String,field:String,source_index:usize,
    pattern:String,mode:String,fixes:usize,checked:usize,
}
#[derive(Clone,Debug)]
struct Evidence{
    key:String,field:String,source_index:usize,pattern:String,
    mode:String,identity:String,
    source_release:String,variant_release:String,expected:Value,
}
static ACTIVE:OnceLock<RwLock<Vec<Active>>>=OnceLock::new();
static REVISION:AtomicU64=AtomicU64::new(0);
static REJECTED:AtomicU64=AtomicU64::new(0);
static OBSERVED:AtomicU64=AtomicU64::new(0);

fn active()->&'static RwLock<Vec<Active>>{
    ACTIVE.get_or_init(||RwLock::new(Vec::new()))
}
pub fn revision()->u64{REVISION.load(Ordering::Acquire)}
fn hash(s:&str)->String{format!("{:x}",Sha256::digest(s.as_bytes()))}
fn build()->String{
    format!("{}:{EPOCH}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"))
}
fn catalogue()->&'static Vec<Value>{
    static SOURCE:OnceLock<Vec<Value>>=OnceLock::new();
    SOURCE.get_or_init(matchlab_parser::existing_regex_rules)
}
fn source(index:usize)->Option<&'static Value>{
    catalogue().iter().find(|v|v["index"].as_u64()==Some(index as u64))
}
fn safe(name:&str)->bool{
    (12..=240).contains(&name.len())&&name.is_ascii()
        &&!name.contains(['/','\\','@','\r','\n','\0'])
        &&!["token=","password=","secret=","apikey=","magnet:","http:"]
            .iter().any(|s|name.to_ascii_lowercase().contains(s))
}
fn present(value:&Value)->bool{
    match value{
        Value::Null=>false,
        Value::Array(a)=>!a.is_empty(),
        Value::String(s)=>!s.trim().is_empty(),
        Value::Bool(v)=>*v,
        Value::Object(o)=>!o.is_empty(),
        _=>true,
    }
}
fn is_fixed_transform(source:&Value)->bool{
    let Some(transform)=source["transform"].as_str()else{return false;};
    transform=="boolean"
        ||(transform.starts_with("value('")&&!transform.contains("$1"))
        ||transform.starts_with("uniq_concat(value('")
}
fn allowed_transform(source:&Value)->bool{
    let Some(t)=source["transform"].as_str()else{return false;};
    is_fixed_transform(source)||matches!(t,
        "none"|"integer"|"first_integer"|"lowercase"|"uppercase"
        |"array(integer)"|"range_func"|"range_x_of_y_func"
        |"transform_resolution")||t.starts_with("date(")
}
fn source_valid(row:&Value)->bool{
    row["kind"]=="regex"&&allowed_transform(row)
        &&row["field"].as_str().is_some_and(|s|!s.is_empty())
        &&row["pattern"].as_str().is_some_and(|s|!s.is_empty())
}
fn identity(ctx:&ParseContext)->Option<String>{
    let title=ctx.result.get("title")?.as_str()?.trim();
    let normalized=crate::imdb_matcher::clean_title(title);
    (4..=140).contains(&normalized.len()).then(||hash(&normalized))
}
fn family_pattern(marker:&str)->Option<String>{
    let marker=marker.trim_matches(|c:char|
        matches!(c,' '|'.'|'_'|'-'));
    if !(3..=48).contains(&marker.len())||!marker.is_ascii(){return None;}
    let words=marker.split(|c|matches!(c,' '|'.'|'_'|'-'))
        .filter(|s|!s.is_empty()).collect::<Vec<_>>();
    if !(2..=6).contains(&words.len())
        ||words.iter().any(|word|word.len()>24
            ||!word.bytes().all(|b|b.is_ascii_alphanumeric())){
        return None;
    }
    // This boundary is shared by every field. The original native handler
    // still owns the conversion and the title-removal semantics.
    let body=words.iter().map(|word|regex::escape(word))
        .collect::<Vec<_>>().join("[ ._-]+");
    let pattern=format!("(?i)(?:^|[ ._-])({body})(?:$|[ ._-])");
    (pattern.len()<=256).then_some(pattern)
}
fn variant(source:&str,marker:&str)->Option<String>{
    let marker=marker.trim_matches(|c:char|matches!(c,' '|'.'|'_'|'-'));
    if source.matches(marker).count()!=1{return None;}
    let start=source.find(marker)?;
    let relative=marker.char_indices().find(|(i,c)|
        matches!(*c,' '|'.'|'_'|'-')&&*i>0
            &&*i+1<marker.len()
            &&marker.as_bytes()[*i-1].is_ascii_alphanumeric()
            &&marker.as_bytes()[*i+1].is_ascii_alphanumeric())?.0;
    let at=start+relative;
    // Every mutation is a punctuation-only equivalence. Choose a
    // deterministic rotation to exercise different separators over time.
    let replacement=match source.as_bytes()[at]{
        b'-'=>'.',b'.'=>'_',b'_'=>' ',b' '=>'-',_=>return None
    };
    let mut title=source.to_owned();
    title.replace_range(at..at+1,&replacement.to_string());
    (title!=source).then_some(title)
}
fn classify(old:&Value,expected:&Value)->Option<&'static str>{
    if old==expected{return None;}
    if !present(old){return Some("missing");}
    if let (Some(a),Some(b))=(old.as_array(),expected.as_array()){
        if a.len()>b.len(){return Some("over");}
        if a.len()<b.len(){return Some("under");}
    }
    Some("incorrect")
}
fn derive(name:&str,ctx:&ParseContext)->Vec<Evidence>{
    if !safe(name){return Vec::new();}
    let Some(identity)=identity(ctx)else{return Vec::new();};
    let mut matches=ctx.matched.iter().filter_map(|(field,match_info)|{
        let index=match_info.source_index?;
        let template=source(index)?;
        if template["field"]!=field.as_str()||!source_valid(template){return None;}
        if !present(ctx.result.get(field)?){return None;}
        let fragment=match_info.raw_match.as_str();
        let pattern=family_pattern(fragment)?;
        let altered=variant(name,fragment)?;
        Some((index,field.to_owned(),pattern,altered))
    }).collect::<Vec<_>>();
    matches.sort_by_key(|(index,field,_,_)|(*index,field.clone()));
    // Never amplify background parser CPU on every incoming request.
    // The rotation distributes all handler families across the corpus.
    if matches.len()>2{
        let digest=Sha256::digest(name.as_bytes());
        let position=digest[0] as usize%matches.len();
        matches.rotate_left(position);
        matches.truncate(2);
    }
    let mut out=Vec::new();
    for (index,field,pattern,mutant) in matches{
        let Ok(parsed)=matchlab_parser::parse_title_canonical(&mutant,false)else{
            continue;
        };
        let previous=Value::Object(parsed);
        let Some(expected)=ctx.result.get(&field)else{continue;};
        let old=&previous[&field];
        let Some(failure)=classify(old,expected)else{continue;};
        let Some(template)=source(index)else{continue;};
        let mode=if failure=="missing"{"add"}else if is_fixed_transform(template){
            "correct"
        }else{continue;};
        let key=hash(&format!("{field}:{index}:{mode}:{pattern}"));
        out.push(Evidence{
            key,field,source_index:index,pattern,
            mode:mode.into(),identity:identity.clone(),
            source_release:name.into(),variant_release:mutant,
            expected:Value::Object(ctx.result.clone())
        });
    }
    out
}
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch("CREATE TABLE IF NOT EXISTS ml_general_examples(
        rule_key TEXT NOT NULL,
        example_hash TEXT NOT NULL,
        identity_hash TEXT NOT NULL,
        field TEXT NOT NULL,
        source_index INTEGER NOT NULL,
        pattern TEXT NOT NULL,
        mode TEXT NOT NULL,
        source_release TEXT NOT NULL,
        variant_release TEXT NOT NULL,
        expected TEXT NOT NULL,
        source_build TEXT NOT NULL,
        observed_at INTEGER NOT NULL,
        PRIMARY KEY(rule_key,example_hash)
    );
    CREATE INDEX IF NOT EXISTS ml_general_candidate_index
        ON ml_general_examples(source_build,rule_key,identity_hash);
    CREATE TABLE IF NOT EXISTS ml_general_verified_rules(
        rule_key TEXT PRIMARY KEY,
        field TEXT NOT NULL,
        source_index INTEGER NOT NULL,
        pattern TEXT NOT NULL,
        mode TEXT NOT NULL,
        source_build TEXT NOT NULL,
        fixed_examples INTEGER NOT NULL,
        checked_examples INTEGER NOT NULL,
        verified_at INTEGER NOT NULL
    );").map_err(|e|e.to_string())
}
pub fn count(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row("SELECT count(*) FROM ml_general_examples
        WHERE source_build=?1",[build()],|row|row.get(0))
        .map_err(|e|e.to_string())
}
fn insert(db:&Connection,e:&Evidence)->Result<bool,String>{
    let sample=hash(&format!("{}:{}",e.source_release,e.variant_release));
    let changed=db.execute("INSERT INTO ml_general_examples(
        rule_key,example_hash,identity_hash,field,source_index,pattern,mode,
        source_release,variant_release,expected,source_build,observed_at
    )VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,strftime('%s','now'))
    ON CONFLICT(rule_key,example_hash) DO UPDATE SET
        identity_hash=excluded.identity_hash,expected=excluded.expected,
        source_build=excluded.source_build,observed_at=excluded.observed_at",
        params![e.key,sample,e.identity,e.field,e.source_index as i64,
            e.pattern,e.mode,e.source_release,e.variant_release,
            e.expected.to_string(),build()]
    ).map_err(|e|e.to_string())?;
    Ok(changed>0)
}
pub fn observe(db:&Connection,name:&str,ctx:&ParseContext,
    budget:&mut i64)->Result<(),String>{
    if *budget>=MAX_EXAMPLES{return Ok(());}
    let evidence=derive(name,ctx);
    for e in evidence{
        if *budget>=MAX_EXAMPLES{break;}
        if insert(db,&e)?{*budget+=1;}
    }
    OBSERVED.store(*budget as u64,Ordering::Relaxed);
    Ok(())
}
fn migrate(db:&Connection)->Result<(),String>{
    // Raw examples remain LOCAL. Re-derive all expected results on upgrades,
    // and retire any rule that was made redundant or semantically changed.
    let mut stmt=db.prepare("SELECT rule_key,example_hash,source_release
        FROM ml_general_examples WHERE source_build<>?1
        ORDER BY observed_at DESC LIMIT ?2").map_err(|e|e.to_string())?;
    let old=stmt.query_map(params![build(),MAX_EXAMPLES],|row|Ok((
        row.get::<_,String>(0)?,row.get::<_,String>(1)?,
        row.get::<_,String>(2)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    for (key,sample,name) in old{
        db.execute("DELETE FROM ml_general_examples WHERE
            rule_key=?1 AND example_hash=?2",params![key,sample])
            .map_err(|e|e.to_string())?;
        if let Ok(ctx)=matchlab_parser::parse_title_canonical_context(&name,false){
            for e in derive(&name,&ctx){insert(db,&e)?;}
        }
    }
    Ok(())
}
fn candidates(db:&Connection)->Result<Vec<Active>,String>{
    let mut stmt=db.prepare("SELECT rule_key,field,source_index,pattern,mode,
        count(*) AS n,count(DISTINCT identity_hash) AS titles
        FROM ml_general_examples WHERE source_build=?1
        GROUP BY rule_key,field,source_index,pattern,mode
        HAVING n>=?2 AND titles>=?3
        ORDER BY n DESC,rule_key LIMIT ?4").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![build(),MIN_FIXES as i64,
        MIN_IDENTITIES as i64,MAX_CANDIDATES as i64],|row|Ok(Active{
        key:row.get(0)?,field:row.get(1)?,
        source_index:row.get::<_,i64>(2)? as usize,
        pattern:row.get(3)?,mode:row.get(4)?,
        fixes:0,checked:0
    })).map_err(|e|e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())
}
fn read_examples(db:&Connection,key:&str)->Result<Vec<Evidence>,String>{
    let mut stmt=db.prepare("SELECT field,source_index,pattern,mode,
       identity_hash,source_release,variant_release,expected
       FROM ml_general_examples WHERE source_build=?1 AND rule_key=?2
       ORDER BY identity_hash,example_hash LIMIT ?3")
        .map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![build(),key,(MAX_TESTS-NEGATIVES.len()) as i64],
        |row|{
            let expected:String=row.get(7)?;
            Ok(Evidence{
                key:key.into(),field:row.get(0)?,
                source_index:row.get::<_,i64>(1)? as usize,
                pattern:row.get(2)?,mode:row.get(3)?,
                identity:row.get(4)?,source_release:row.get(5)?,
                variant_release:row.get(6)?,
                expected:serde_json::from_str(&expected).unwrap_or(Value::Null)
            })
        }).map_err(|e|e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())
}
fn rule_parts(rules:&[Active])->(Vec<HandlerAddition>,Vec<HandlerCorrection>){
    let mut add=Vec::new();
    let mut correct=Vec::new();
    for rule in rules{
        if rule.mode=="add"{
            add.push(HandlerAddition{
                source_index:rule.source_index,pattern:rule.pattern.clone()});
        }else{
            correct.push(HandlerCorrection{
                source_index:rule.source_index,
                pattern:rule.pattern.clone(),is_range:false});
        }
    }
    (add,correct)
}
fn replay(db:&Connection,previous:&[Active],all:&[Active],
    newest:&str)->Result<Option<(usize,usize)>,String>{
    if all.is_empty(){return Ok(Some((0,0)));}
    let mut examples=Vec::new();
    let mut keys=BTreeSet::new();
    for rule in all{
        keys.insert(rule.key.clone());
        examples.extend(read_examples(db,&rule.key)?);
    }
    // Each example is replayed as original + variant; bound the TOTAL
    // titles, not merely the number of evidence rows.
    if examples.len().saturating_mul(2)+NEGATIVES.len()+16>MAX_TESTS{
        return Ok(None);
    }
    let mut titles=Vec::new();
    for example in &examples{
        titles.push(example.source_release.clone());
        titles.push(example.variant_release.clone());
    }
    titles.extend(NEGATIVES.iter().map(|x|(*x).into()));
    // Additional natural positives from unrelated field families must be
    // preserved. This detects cross-field overextracts a narrow candidate
    // corpus could otherwise miss.
    let mut others=db.prepare("SELECT DISTINCT source_release
        FROM ml_general_examples WHERE source_build=?1
        ORDER BY source_release LIMIT 16").map_err(|e|e.to_string())?;
    let controls=others.query_map([build()],|row|row.get::<_,String>(0))
        .map_err(|e|e.to_string())?;
    let controls=controls.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    titles.extend(controls);
    if titles.len()>MAX_TESTS{return Ok(None);}
    // The baseline includes every existing native and ML addition/patch
    // except this learner. Candidate replay tests the entire combination.
    let (patches,base_additions)=crate::field_learning::source_rules_without_general(
        crate::tv_learning::handler_additions())?;
    let tv=crate::corrective_learning::handler_corrections();
    let mut prior_corrections=tv.clone();
    let (prior_additions,prior_extra)=rule_parts(previous);
    prior_corrections.extend(prior_extra);
    let mut previous_additions=base_additions.clone();
    previous_additions.extend(prior_additions);
    let mut augmented=tv.clone();
    let (additional,corrective)=rule_parts(all);
    augmented.extend(corrective);
    let mut additions=base_additions.clone();
    additions.extend(additional);
    let before=match matchlab_parser::replay_handler_rules(
        &patches,&previous_additions,&prior_corrections,&titles){
        Ok(v)=>v,Err(_)=>return Ok(None)
    };
    let after=match matchlab_parser::replay_handler_rules(
        &patches,&additions,&augmented,&titles){
        Ok(v)=>v,Err(_)=>return Ok(None)
    };
    let mut fixed=BTreeMap::<String,usize>::new();
    let mut covered=0usize;
    for (i,example) in examples.iter().enumerate(){
        let old_source=&before[2*i];
        let new_source=&after[2*i];
        // A source success is immutable proof of the punctuation mutation.
        if *old_source!=*new_source
            ||Value::Object(new_source.clone())!=example.expected{
            return Ok(None);
        }
        let old=&before[2*i+1];
        let new=&after[2*i+1];
        if Value::Object(new.clone())!=example.expected{return Ok(None);}
        if old!=new{
            if Value::Object(old.clone())==example.expected{
                return Ok(None);
            }
            *fixed.entry(example.key.clone()).or_default()+=1;
        }
        covered+=1;
    }
    let offset=examples.len()*2;
    if before[offset..]!=after[offset..]{return Ok(None);}
    let incremental=fixed.get(newest).copied().unwrap_or(0);
    // A syntactically different regex that gives the same result as
    // previous accepted rules is NOT a new improvement.
    Ok((incremental>=MIN_FIXES).then_some((covered,incremental)))
}
fn persist(db:&Connection,ready:&[Active])->Result<(),String>{
    let tx=db.unchecked_transaction().map_err(|e|e.to_string())?;
    tx.execute("DELETE FROM ml_general_verified_rules",[])
        .map_err(|e|e.to_string())?;
    for rule in ready{
        tx.execute("INSERT INTO ml_general_verified_rules
            (rule_key,field,source_index,pattern,mode,source_build,
             fixed_examples,checked_examples,verified_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,strftime('%s','now'))",
            params![rule.key,rule.field,rule.source_index as i64,
                rule.pattern,rule.mode,build(),rule.fixes as i64,
                rule.checked as i64]).map_err(|e|e.to_string())?;
    }
    tx.commit().map_err(|e|e.to_string())
}
pub fn handler_additions()->Vec<HandlerAddition>{
    active().read().map(|r|rule_parts(&r).0).unwrap_or_default()
}
pub fn handler_corrections()->Vec<HandlerCorrection>{
    active().read().map(|r|rule_parts(&r).1).unwrap_or_default()
}
pub fn reconcile(db:&Connection)->Result<(),String>{
    init(db)?;
    migrate(db)?;
    let mut ready=Vec::new();
    let mut rejected=0u64;
    for rule in candidates(db)? {
        if ready.len()>=MAX_ACTIVE{break;}
        let mut candidate=ready.clone();
        candidate.push(rule.clone());
        match replay(db,&ready,&candidate,&rule.key)?{
            Some((checked,fixed)) if fixed>=MIN_FIXES=>{
                let Some(last)=candidate.last_mut()else{continue;};
                last.fixes=fixed;last.checked=checked;
                ready=candidate;
            }
            _=>rejected+=1
        }
    }
    REJECTED.store(rejected,Ordering::Relaxed);
    if active().read().ok().is_some_and(|r|*r==ready)
        &&revision()!=0{return Ok(());}
    // Publish once after entire learning batch; if compilation fails,
    // neither persisted rules nor the live registry changes.
    let mut corrections=crate::corrective_learning::handler_corrections();
    let (adds,other)=rule_parts(&ready);
    corrections.extend(other);
    let (patches,mut additions)=crate::field_learning::source_rules_without_general(
        crate::tv_learning::handler_additions())?;
    additions.extend(adds);
    matchlab_parser::install_learned_handler_regex_with_corrections(
        &patches,&additions,&corrections)
        .map_err(|e|format!("general learned parser regex rejected: {e}"))?;
    persist(db,&ready)?;
    let fp=ready.iter().map(|r|r.key.as_str()).collect::<Vec<_>>().join("|");
    let digest=Sha256::digest(fp.as_bytes());
    let mut bytes=[0u8;8];bytes.copy_from_slice(&digest[..8]);
    *active().write().map_err(|e|e.to_string())?=ready;
    REVISION.store(u64::from_le_bytes(bytes).max(1),Ordering::Release);
    Ok(())
}
pub fn status()->Value{
    let fields=active().read().map(|v|{
        let mut groups=BTreeMap::<String,usize>::new();
        for rule in v.iter(){*groups.entry(rule.field.clone()).or_default()+=1;}
        groups
    }).unwrap_or_default();
    let mut families=BTreeMap::<String,usize>::new();
    let mut removing=BTreeMap::<String,usize>::new();
    for row in catalogue(){
        if let Some(field)=row["field"].as_str(){
            *families.entry(field.into()).or_default()+=1;
            if row["options"]["remove"]==true{
                *removing.entry(field.into()).or_default()+=1;
            }
        }
    }
    json!({"enabled":crate::local_learning::active(),
        "source_regex_handlers":catalogue().len(),
        "catalogue_fields":families.len(),
        "catalogue_handler_counts":families,
        "catalogue_removing_handlers":removing,
        "candidate_limit":MAX_CANDIDATES,
        "active_rule_limit":MAX_ACTIVE,
        "active_rules_by_field":fields,
        "active_count":handler_additions().len()+handler_corrections().len(),
        "observations":OBSERVED.load(Ordering::Relaxed),
        "replay_rejected":REJECTED.load(Ordering::Relaxed),
        "scope":"all fields with meaning-preserving independently verified native-handler transformations",
        "improvement_policy":"entire-parser replay must repair 4 independent examples with zero regressions",
        "model":"bounded online field-general regex induction; FTRL supplies advisory scores",
        "revision":format!("{:016x}",revision())})
}
pub fn community_proposals(db:&Connection)->Result<Vec<Value>,String>{
    init(db)?;
    let mut out=Vec::new();
    let verified=active().read().map(|x|x.clone()).unwrap_or_default();
    for r in verified {
        let fp=hash(&format!("general:{}:{}:{}:{}",r.field,r.source_index,r.mode,r.pattern));
        out.push(json!({
            "kind":"learned_general_regex_review",
            "fingerprint":fp,
            "source_build":build(),
            "field":r.field,
            "handler_index":r.source_index,
            "mode":r.mode,
            "regex":r.pattern,
            "evidence_count":r.checked,
            "distinct_titles":MIN_IDENTITIES,
            "canonical_confirmations":r.fixes,
            "review_only":true,
            "verification":"native_handler_metamorphic_replay_requires_upstream_review"
        }));
    }
    Ok(out)
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn dynamic_catalogue_covers_every_original_regex_field(){
        assert!(catalogue().len()>400);
        for field in ["title","year","group","edition","audio","hdr",
            "channels","languages","network","resolution","codec","quality",
            "seasons","episodes","date","extension","region"]{
            assert!(catalogue().iter().any(|row|row["field"]==field));
        }
    }
    #[test]
    fn separator_mutation_synthesizes_bounded_generic_regex(){
        let original="Example.Show.2025.1080p.WEB-DL.x264";
        let alternate=variant(original,"WEB-DL").unwrap();
        assert_eq!(alternate,"Example.Show.2025.1080p.WEB.DL.x264");
        let pattern=family_pattern("WEB-DL").unwrap();
        let re=regex::Regex::new(&pattern).unwrap();
        assert!(re.is_match(&alternate));
        assert!(re.is_match(original));
        assert!(family_pattern("A.B").is_some());
        assert!(family_pattern("AB").is_none());
        assert!(family_pattern("password=sensitive").is_none());
        assert!(variant(original,"WEB-DL").is_some());
    }
    #[test]
    fn all_field_evidence_and_noops_cannot_auto_publish_without_repairs(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let name="Example.Show.2025.1080p.WEB-DL.x264";
        let ctx=matchlab_parser::parse_title_canonical_context(name,false).unwrap();
        let sample=derive(name,&ctx);
        for e in &sample{
            assert!(!e.pattern.is_empty());
            assert!(source(e.source_index).is_some());
        }
        assert!(candidates(&db).unwrap().is_empty());
        assert!(replay(&db,&[],&[],"").unwrap().is_some());
        assert!(handler_additions().is_empty());
    }
    #[test]
    fn unverified_absent_labels_never_spawn_mutation_rules(){
        let mut ctx=matchlab_parser::parse_title_canonical_context(
            "Movie.2015.1080p.WEB-DL",false).unwrap();
        ctx.result.clear();
        assert!(derive("Movie.2015.1080p.WEB-DL",&ctx).is_empty());
    }
}
