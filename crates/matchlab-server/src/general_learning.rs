//! Generic, self-supervised native-handler regex induction for every parser
//! field. The learner uses meaning-preserving release-tag separator mutations:
//! a successfully parsed canonical release is the reference; a punctuation-
//! only variant must retain the COMPLETE result, or it is not a safe rule.
//! No network, inference or ML weights execute on the parsing hot path.
use matchlab_parser::{HandlerAddition,HandlerCorrection,ParseContext};
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::collections::BTreeMap;
use std::sync::{Mutex,OnceLock,RwLock,atomic::{AtomicU64,Ordering}};

const EPOCH:u32=1;
const MIN_FIXES:usize=4;
const MIN_IDENTITIES:usize=2;
const MIN_CORRECTION_FIXES:usize=8;
const MIN_CORRECTION_IDENTITIES:usize=4;
const MAX_ACTIVE:usize=16;
fn capacity()->i64{
    crate::ml_config::get().general_examples
}
const MAX_CANDIDATES:usize=4;
const REVALIDATE_BATCH:i64=128;
const MAX_TESTS:usize=128;
const REGRESSION_TITLES:&str=include_str!("../tests/release_titles.txt");
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
static CURSOR:AtomicU64=AtomicU64::new(0);
static OBSERVED:AtomicU64=AtomicU64::new(0);
// SQLite owns the authoritative revision; the in-process cache is an
// accelerator only. It never bypasses the promotion/replay safety checks.
static CANDIDATE_CACHE:OnceLock<Mutex<Option<(u64,Vec<Active>)>>>=OnceLock::new();
fn evidence_revision(db:&Connection)->Result<u64,String>{
    let number:i64=db.query_row("SELECT revision FROM ml_general_evidence_revision
        WHERE id=1",[],|row|row.get(0)).map_err(|e|e.to_string())?;
    u64::try_from(number).map_err(|_|"invalid general evidence revision".to_owned())
}

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
    // The generated catalog contains sparse original source IDs. Index
    // positions once, instead of rescanning ~450 handlers per candidate.
    static SOURCE_INDEX:OnceLock<std::collections::HashMap<usize,usize>>=OnceLock::new();
    let offsets=SOURCE_INDEX.get_or_init(||catalogue().iter().enumerate()
        .filter_map(|(offset,value)|Some((value["index"].as_u64()? as usize,offset)))
        .collect());
    offsets.get(&index).and_then(|offset|catalogue().get(*offset))
}
fn safe(name:&str)->bool{
    (12..=240).contains(&name.len())&&name.is_ascii()
        &&!name.contains(['/','\\','@','\r','\n','\0'])
        &&!["token=","password=","secret=","apikey=","magnet:","http:"]
            .iter().any(|s|name.to_ascii_lowercase().contains(s))
}
fn regression_titles()->Vec<String>{
    NEGATIVES.iter().copied()
        .chain(REGRESSION_TITLES.lines()
            .map(str::trim)
            .filter(|s|!s.is_empty()&&!s.starts_with('#')))
        .map(str::to_owned).collect()
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
// Live activation is deliberately narrower than observation. A
// self-supervised punctuation mutation does NOT independently prove that
// an already nonempty value is wrong. Corrections must use a dedicated
// independent reference validator (TV coordinates currently do).
fn approved_addition_source(row:&Value)->bool{
    let Some(field)=row["field"].as_str()else{return false;};
    source_valid(row) &&is_fixed_transform(row)
        &&row["options"]["skipIfAlreadyFound"]==true
        &&matches!(field,
            "edition"|"quality"|"resolution"|"codec"|"bit_depth"
            |"network"|"upscaled"|"subbed"|"dubbed"|"proper"
            |"repack"|"retail"|"remastered"|"unrated"
            |"uncensored"|"hardcoded"|"commentary"|"preair")
}
fn bounded_generated_regex(pattern:&str)->bool{
    let prefix="(?i)(?:^|[ ._-])(";
    let suffix=")(?:$|[ ._-])";
    let Some(body)=pattern.strip_prefix(prefix)
        .and_then(|p|p.strip_suffix(suffix))else{return false;};
    let parts=body.split("[ ._-]+").collect::<Vec<_>>();
    (2..=6).contains(&parts.len())&&pattern.len()<=256
        &&parts.iter().all(|s|!s.is_empty()&&s.len()<=24
            &&s.bytes().all(|b|b.is_ascii_alphanumeric()))
}
fn promotable(rule:&Active)->bool{
    rule.mode=="add"&&bounded_generated_regex(&rule.pattern)
        // A canonical parse is only self-supervision. Never promote an
        // arbitrary matched substring from a title as a new global rule.
        // The phrase must also belong to a fixed, known release vocabulary.
        &&shareable_pattern(&rule.field,&rule.pattern)
        &&source(rule.source_index).is_some_and(|s|
            s["field"]==rule.field &&approved_addition_source(s))
}
fn identity(ctx:&ParseContext)->Option<String>{
    let title=ctx.result.get("title")?.as_str()?.trim();
    let normalized=crate::imdb_matcher::clean_title(title);
    (4..=140).contains(&normalized.len()).then(||hash(&normalized))
}
/// Tokenize a canonical native match into safe structural components.
/// This covers separator variations *and* letter/number tag boundaries
/// (e.g. HDR10 -> HDR.10, AV1 -> AV-1). No guessed synonyms are learned.
fn parts(marker:&str)->Option<Vec<String>>{
    let marker=marker.trim_matches(|c:char|
        matches!(c,' '|'.'|'_'|'-'));
    if !(2..=48).contains(&marker.len())||!marker.is_ascii(){return None;}
    let mut words=Vec::<String>::new();
    let mut token=String::new();
    let mut prev:Option<u8>=None;
    for byte in marker.bytes(){
        if matches!(byte,b' '|b'.'|b'_'|b'-'){
            if !token.is_empty(){words.push(std::mem::take(&mut token));}
            prev=None;continue;
        }
        if !byte.is_ascii_alphanumeric(){return None;}
        let split=prev.is_some_and(|prior|
            prior.is_ascii_digit()!=byte.is_ascii_digit());
        if split&&!token.is_empty(){words.push(std::mem::take(&mut token));}
        token.push(byte as char);
        prev=Some(byte);
    }
    if !token.is_empty(){words.push(token);}
    (2..=6).contains(&words.len())
        .then_some(words).filter(|v|
            v.iter().all(|w|!w.is_empty()&&w.len()<=24))
}
fn family_pattern(marker:&str)->Option<String>{
    let words=parts(marker)?;
    let body=words.iter().map(|word|regex::escape(word))
        .collect::<Vec<_>>().join("[ ._-]+");
    let pattern=format!("(?i)(?:^|[ ._-])({body})(?:$|[ ._-])");
    (pattern.len()<=256).then_some(pattern)
}
fn variants(source:&str,marker:&str)->Vec<String>{
    let marker=marker.trim_matches(|c:char|
        matches!(c,' '|'.'|'_'|'-'));
    if source.matches(marker).count()!=1{return Vec::new();}
    let Some(start)=source.find(marker)else{return Vec::new();};
    let bytes=marker.as_bytes();
    let mut boundary=None;
    let mut punct=false;
    for idx in 1..bytes.len(){
        let b=bytes[idx];
        let prev=bytes[idx-1];
        if matches!(b,b' '|b'.'|b'_'|b'-')
            &&idx+1<bytes.len()
            &&prev.is_ascii_alphanumeric()&&bytes[idx+1].is_ascii_alphanumeric(){
            boundary=Some(idx);punct=true;break;
        }
    }
    if boundary.is_none(){
        for idx in 1..bytes.len(){
            if bytes[idx].is_ascii_digit()!=bytes[idx-1].is_ascii_digit()
                &&bytes[idx].is_ascii_alphanumeric()
                &&bytes[idx-1].is_ascii_alphanumeric(){
                boundary=Some(idx);break;
            }
        }
    }
    let Some(offset)=boundary else{return Vec::new();};
    let mut candidates=Vec::new();
    if punct {
        // Repeated separators are semantically identical release tags but
        // often escape older one-delimiter native regexes.
        let mut name=source.to_owned();
        name.insert(start+offset,bytes[offset] as char);
        if name!=source{candidates.push(name);}
    }
    for sep in [b'_',b'-',b'.',b' ']{
        if punct&&bytes[offset]==sep{continue;}
        let mut name=source.to_owned();
        let target=(sep as char).to_string();
        if punct{name.replace_range(start+offset..start+offset+1,&target);}
        else{name.insert_str(start+offset,&target);}
        if name!=source {candidates.push(name);}
    }
    candidates
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
    // Replayed candidates call the PUBLIC parse_title() path-merging
    // wrapper. Context parsing intentionally retains internal empty arrays
    // (e.g. languages=[]), while the public wrapper can omit them.
    // The comparison reference must therefore use the same output shape.
    // Do not run the full path-level parser until we have a plausible,
    // independently derived mutation. The canonical context was already
    // parsed once by the worker for every incoming training title.
    let mut matches=ctx.matched.iter().filter_map(|(field,match_info)|{
        let index=match_info.source_index?;
        let template=source(index)?;
        if template["field"]!=field.as_str()||!source_valid(template){return None;}
        if !present(ctx.result.get(field)?){return None;}
        let fragment=match_info.raw_match.as_str();
        let pattern=family_pattern(fragment)?;
        let altered=variants(name,fragment);
        if altered.is_empty(){return None;}
        Some((index,field.to_owned(),pattern,altered))
    }).collect::<Vec<_>>();
    matches.sort_by_key(|(index,field,_,_)|(*index,field.clone()));
    // Never amplify background parser CPU on every incoming request.
    // The rotation distributes all handler families across the corpus.
    if matches.len()>1{
        let digest=Sha256::digest(name.as_bytes());
        let position=digest[0] as usize%matches.len();
        matches.rotate_left(position);
        matches.truncate(1);
    }
    if matches.is_empty(){return Vec::new();}
    let Ok(expected_public)=matchlab_parser::parse_title_canonical(name,false)
        else{return Vec::new();};
    let expected_public=Value::Object(expected_public);
    let mut out=Vec::new();
    for (index,field,pattern,mutants) in matches{
        let Some(reference)=ctx.result.get(&field)else{continue;};
        let Some(template)=source(index)else{continue;};
        let Ok(compiled)=regex::Regex::new(&pattern)else{continue;};
        // Limit to three counterfactual parses per source marker and
        // accept the first whose missing/wrong field the grammar can repair.
        for mutant in mutants.into_iter().take(3){
            if !compiled.is_match(&mutant){continue;}
            let Ok(parsed)=matchlab_parser::parse_title_canonical(&mutant,false)else{
                continue;
            };
            let previous=Value::Object(parsed);
            let Some(failure)=classify(&previous[&field],reference)else{continue;};
            let mode=if failure=="missing"{"add"}else if is_fixed_transform(template){
                "correct"
            }else{continue;};
            let key=hash(&format!("{field}:{index}:{mode}:{pattern}"));
            out.push(Evidence{
                key,field:field.clone(),source_index:index,pattern:pattern.clone(),
                mode:mode.into(),identity:identity.clone(),
                source_release:name.into(),variant_release:mutant,
                expected:expected_public.clone()
            });
            break;
        }
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
    );
    CREATE TABLE IF NOT EXISTS ml_general_evidence_revision(
        id INTEGER PRIMARY KEY CHECK(id=1),revision INTEGER NOT NULL);
    INSERT OR IGNORE INTO ml_general_evidence_revision VALUES(1,1);
    CREATE TRIGGER IF NOT EXISTS ml_general_revision_insert
        AFTER INSERT ON ml_general_examples BEGIN
        UPDATE ml_general_evidence_revision SET revision=revision+1 WHERE id=1;
    END;
    CREATE TRIGGER IF NOT EXISTS ml_general_revision_delete
        AFTER DELETE ON ml_general_examples BEGIN
        UPDATE ml_general_evidence_revision SET revision=revision+1 WHERE id=1;
    END;
    CREATE TRIGGER IF NOT EXISTS ml_general_revision_update
        AFTER UPDATE ON ml_general_examples BEGIN
        UPDATE ml_general_evidence_revision SET revision=revision+1 WHERE id=1;
    END;").map_err(|e|e.to_string())
}
pub fn count(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row("SELECT count(*) FROM ml_general_examples
        WHERE source_build=?1",[build()],|row|row.get(0))
        .map_err(|e|e.to_string())
}
fn insert(db:&Connection,e:&Evidence)->Result<bool,String>{
    let sample=hash(&format!("{}:{}",e.source_release,e.variant_release));
    let changed=db.execute("INSERT OR IGNORE INTO ml_general_examples(
        rule_key,example_hash,identity_hash,field,source_index,pattern,mode,
        source_release,variant_release,expected,source_build,observed_at
    )VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,strftime('%s','now'))",
        params![e.key,sample,e.identity,e.field,e.source_index as i64,
            e.pattern,e.mode,e.source_release,e.variant_release,
            e.expected.to_string(),build()]
    ).map_err(|e|e.to_string())?;
    Ok(changed>0)
}
pub fn observe(db:&Connection,name:&str,ctx:&ParseContext,
    budget:&mut i64)->Result<(),String>{
    if *budget>=capacity(){
        // A finite training budget must not permanently stop the learner.
        // Never evict independently verified active rule evidence.
        let removed=db.execute("DELETE FROM ml_general_examples WHERE
            (rule_key,example_hash) IN (
                SELECT rule_key,example_hash FROM ml_general_examples
                WHERE rule_key NOT IN (
                    SELECT rule_key FROM ml_general_verified_rules)
                ORDER BY observed_at,example_hash LIMIT 64
            )",[]).map_err(|e|e.to_string())? as i64;
        *budget=budget.saturating_sub(removed);
        if *budget>=capacity(){return Ok(());}
    }
    let evidence=derive(name,ctx);
    for e in evidence{
        if *budget>=capacity(){break;}
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
    let old=stmt.query_map(params![build(),REVALIDATE_BATCH],|row|Ok((
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
    let revision=evidence_revision(db)?;
    let cache=CANDIDATE_CACHE.get_or_init(||Mutex::new(None));
    if let Ok(guard)=cache.lock(){
        if let Some((seen,ready))=guard.as_ref(){
            if *seen==revision{return Ok(ready.clone());}
        }
    }
    let mut stmt=db.prepare("SELECT rule_key,field,source_index,pattern,mode,
        count(*) AS n,count(DISTINCT identity_hash) AS titles
        FROM ml_general_examples WHERE source_build=?1
        GROUP BY rule_key,field,source_index,pattern,mode
        HAVING (mode='add' AND n>=?2 AND titles>=?3)
            OR (mode='correct' AND n>=?4 AND titles>=?5)
        ORDER BY n DESC,rule_key").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![build(),MIN_FIXES as i64,
        MIN_IDENTITIES as i64,MIN_CORRECTION_FIXES as i64,
        MIN_CORRECTION_IDENTITIES as i64],|row|Ok(Active{
        key:row.get(0)?,field:row.get(1)?,
        source_index:row.get::<_,i64>(2)? as usize,
        pattern:row.get(3)?,mode:row.get(4)?,
        fixes:0,checked:0
    })).map_err(|e|e.to_string())?;
    let proposals=rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    // Invalid persisted rules and arbitrary field overwrites cannot become
    // executable even if a local SQLite database was imported or modified.
    let eligible=proposals.into_iter().filter(promotable).collect::<Vec<_>>();
    if let Ok(mut guard)=cache.lock(){*guard=Some((revision,eligible.clone()));}
    Ok(eligible)
}
fn read_examples(db:&Connection,key:&str,limit:usize)->Result<Vec<Evidence>,String>{
    let mut stmt=db.prepare("SELECT field,source_index,pattern,mode,
       identity_hash,source_release,variant_release,expected FROM (
         SELECT field,source_index,pattern,mode,identity_hash,
                source_release,variant_release,expected,
                ROW_NUMBER() OVER (
                    PARTITION BY identity_hash ORDER BY example_hash
                ) AS identity_rank
         FROM ml_general_examples WHERE source_build=?1 AND rule_key=?2
       ) WHERE identity_rank<=2
       ORDER BY identity_rank,identity_hash LIMIT ?3")
        .map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![build(),key,limit as i64],
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
    for rule in all{
        if !promotable(rule){return Ok(None);}
        // Re-test the new candidate on multiple independent observations.
        // Already verified rules keep two cross-family regression probes.
        let limit=if rule.key==newest {8}else{2};
        examples.extend(read_examples(db,&rule.key,limit)?);
    }
    // Each example is replayed as original + variant; bound the TOTAL
    // titles, not merely the number of evidence rows.
    if examples.len().saturating_mul(2)
        +regression_titles().len()+16>MAX_TESTS{
        return Ok(None);
    }
    let mut titles=Vec::new();
    for example in &examples{
        titles.push(example.source_release.clone());
        titles.push(example.variant_release.clone());
    }
    titles.extend(regression_titles());
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
    let required=all.iter().find(|rule|rule.key==newest)
        .map(|rule|if rule.mode=="correct"{MIN_CORRECTION_FIXES}else{MIN_FIXES})
        .unwrap_or(MIN_FIXES);
    Ok((incremental>=required).then_some((covered,incremental)))
}
/// Re-check persisted active rules as one composed parser generation.
/// A new observation or native source update can invalidate an old regex:
/// fail closed and retire it instead of silently keeping stale behavior.
fn active_safe(db:&Connection,rules:&[Active])->Result<bool,String>{
    if rules.is_empty(){return Ok(true);}
    let mut examples=Vec::new();
    for rule in rules{
        if !promotable(rule){return Ok(false);}
        let rows=read_examples(db,&rule.key,2)?;
        if rows.len()<2 ||rows[0].identity==rows[1].identity{
            // Evidence was deleted, became stale, or covers only one work.
            // Never grandfather a verified regex without live references.
            return Ok(false);
        }
        examples.extend(rows);
    }
    let titles=examples.iter()
        .flat_map(|e|[e.source_release.clone(),e.variant_release.clone()])
        .chain(regression_titles())
        .collect::<Vec<_>>();
    if titles.len()>MAX_TESTS{return Ok(false);}
    let (patches,mut additions)=crate::field_learning::source_rules_without_general(
        crate::tv_learning::handler_additions())?;
    let native_additions=additions.clone();
    let tv=crate::corrective_learning::handler_corrections();
    let baseline=match matchlab_parser::replay_handler_rules(
        &patches,&native_additions,&tv,&titles){
        Ok(v)=>v,Err(_)=>return Ok(false)
    };
    let (extra,corrections)=rule_parts(rules);
    additions.extend(extra);
    let mut all=tv.clone();
    all.extend(corrections);
    let replay=match matchlab_parser::replay_handler_rules(
        &patches,&additions,&all,&titles){
        Ok(v)=>v,Err(_)=>return Ok(false)
    };
    let mut repaired=BTreeMap::<String,usize>::new();
    for (i,e) in examples.iter().enumerate(){
        if Value::Object(replay[2*i].clone())!=e.expected
            ||Value::Object(replay[2*i+1].clone())!=e.expected {
            return Ok(false);
        }
        if baseline[2*i+1]!=replay[2*i+1]{
            *repaired.entry(e.key.clone()).or_default()+=1;
        }
    }
    if rules.iter().any(|r|repaired.get(&r.key).copied().unwrap_or(0)==0){
        // The native parser or another rule now performs the same repair.
        return Ok(false);
    }
    let offset=examples.len()*2;
    Ok(baseline[offset..]==replay[offset..])
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
    let mut ready=active().read().map(|r|r.clone())
        .map_err(|e|e.to_string())?;
    let mut rejected=0u64;
    // A formerly active rule is never grandfathered in when fresh
    // evidence invalidates it or other rules change. Remove the newest
    // corrections until the complete parser is safe again.
    while !ready.is_empty()&&!active_safe(db,&ready)?{
        ready.pop();
        rejected+=1;
    }
    let pool=candidates(db)?;
    if !pool.is_empty(){
        let start=CURSOR.fetch_add(MAX_CANDIDATES as u64,Ordering::AcqRel)
            as usize%pool.len();
        for offset in 0..pool.len().min(MAX_CANDIDATES){
            if ready.len()>=MAX_ACTIVE{break;}
            let rule=pool[(start+offset)%pool.len()].clone();
            if ready.iter().any(|old|old.key==rule.key){continue;}
            let mut candidate=ready.clone();
        candidate.push(rule.clone());
        match replay(db,&ready,&candidate,&rule.key)?{
            Some((checked,fixed)) if fixed>=
                (if rule.mode=="correct"{MIN_CORRECTION_FIXES}else{MIN_FIXES})=>{
                let Some(last)=candidate.last_mut()else{continue;};
                last.fixes=fixed;last.checked=checked;
                ready=candidate;
            }
            _=>rejected+=1
            }
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
    // Fail closed: compile and validate the complete proposed generation
    // BEFORE persisting any candidate. A SQLite failure must not publish
    // unrecorded rules to a live parser.
    matchlab_parser::replay_handler_rules(
        &patches,&additions,&corrections,&[])
        .map_err(|e|format!("general candidate preflight rejected: {e}"))?;
    matchlab_parser::verify_live_regression_corpus(
        &patches,&additions,&corrections)
        .map_err(|e|format!("general candidate regression rejected: {e}"))?;
    persist(db,&ready)?;
    // Live parser publication is a single immutable generation swap.
    matchlab_parser::install_learned_handler_regex_with_corrections(
        &patches,&additions,&corrections)
        .map_err(|e|format!("general learned parser regex rejected: {e}"))?;
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
        "live_regex_publication_enabled":matchlab_parser::learned_regex_publication_enabled(),
        "source_regex_handlers":catalogue().len(),
        "catalogue_fields":families.len(),
        "catalogue_handler_counts":families,
        "catalogue_removing_handlers":removing,
        "candidate_limit":MAX_CANDIDATES,
        "candidate_selection":"rotating bounded batches; previously verified active rules remain installed",
        "configured_training_example_limit":capacity(),
        "active_rule_limit":MAX_ACTIVE,
        "active_rules_by_field":fields,
        "active_count":handler_additions().len()+handler_corrections().len(),
        "observations":OBSERVED.load(Ordering::Relaxed),
        "replay_rejected":REJECTED.load(Ordering::Relaxed),
        "observation_scope":"all original parser fields",
        "automatic_activation_scope":"known public metadata markers; native fixed scalar/boolean ADD only, skipIfAlreadyFound=true",
        "high_priority_general_corrections_enabled":false,
        "correction_policy":"nonempty TV coordinates require separate independent syntax validator; other fields review-only",
        "improvement_policy":"ADD >=4 repairs/2 identities; preserve full shipped regression corpus and all already-correct inputs; reject no-ops",
        "unseen_input_guarantee":"no mathematical zero-regression guarantee; bounded evidence and fail-closed publication",
        "model":"bounded online field-general regex induction; FTRL supplies advisory scores",
        "revision":format!("{:016x}",revision())})
}
/// Reject anything that could put a private work title, release group,
/// torrent site, or user-specific marker into a public GitHub rule.
fn shareable_field(field:&str)->bool{
    matches!(field,
        "resolution"|"codec"|"quality"|"audio"|"hdr"|"channels"
        |"languages"|"subbed"|"dubbed"|"extension"|"edition"
        |"network"|"bit_depth"|"seasons"|"episodes"|"date"
        |"upscaled"|"3d"|"region"|"release_types"|"extras")
}
// Public community proposals must not include a novel token copied from
// someone's local torrent title. Only these already-public release labels
// are safe to serialize. Other verified local patterns remain local.
fn shareable_pattern(field:&str,pattern:&str)->bool{
    if !shareable_field(field)||!bounded_generated_regex(pattern){return false;}
    let Some(words)=pattern.strip_prefix("(?i)(?:^|[ ._-])(")
        .and_then(|p|p.strip_suffix(")(?:$|[ ._-])"))else{return false;};
    let label=words.replace("[ ._-]+"," ").to_ascii_lowercase();
    let known:&[&str]=match field{
        "edition"=>&["ultimate edition","anniversary edition",
            "extended edition","collectors edition","directors cut"],
        "quality"=>&["web dl","web rip","blu ray","bd rip",
            "dvd rip","hd tv","tv rip","blu ray remux"],
        "codec"=>&["h 264","h 265","x 264","x 265","av 1"],
        "bit_depth"=>&["10 bit","12 bit","8 bit"],
        "audio"=>&["dolby digital","dolby digital plus","true hd"],
        "hdr"=>&["dolby vision","hdr 10","hdr 10 plus"],
        "network"=>&["apple tv","adult swim","animal planet"],
        "resolution"=>&["full hd","quad hd"],
        _=>&[],
    };
    known.contains(&label.as_str())
}
pub fn community_proposals(db:&Connection)->Result<Vec<Value>,String>{
    init(db)?;
    let mut out=Vec::new();
    let verified=active().read().map(|x|x.clone()).unwrap_or_default();
    for r in verified {
        if !shareable_pattern(&r.field,&r.pattern){continue;}
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
    fn untrusted_and_ambiguous_rules_never_become_executable(){
        let native=catalogue().iter().find(|r|
            r["field"]=="edition"
            &&r["transform"]=="value('Ultimate Edition')"
            &&r["options"]["skipIfAlreadyFound"]==true).unwrap();
        let index=native["index"].as_u64().unwrap() as usize;
        let pattern=family_pattern("Ultimate Edition").unwrap();
        let good=Active{key:hash("good"),field:"edition".into(),
            source_index:index,pattern:pattern.clone(),
            mode:"add".into(),fixes:4,checked:8};
        assert!(promotable(&good));
        assert!(!promotable(&Active{mode:"correct".into(),..good.clone()}));
        assert!(!promotable(&Active{field:"title".into(),..good.clone()}));
        assert!(!promotable(&Active{pattern:"(?i).*".into(),..good.clone()}));
        assert!(!promotable(&Active{
            pattern:family_pattern("Private Movie").unwrap(),..good.clone()
        }),"a title-derived unknown marker is not independently validated");
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        assert!(!active_safe(&db,&[good]).unwrap(),
            "a rule without original evidence must be retired");
    }
    #[test]
    fn community_regex_export_uses_only_public_metadata_vocabulary(){
        assert!(shareable_pattern("edition",&family_pattern(
            "Ultimate Edition").unwrap()));
        assert!(shareable_pattern("quality",&family_pattern(
            "WEB-DL").unwrap()));
        assert!(!shareable_pattern("edition",&family_pattern(
            "Private Movie").unwrap()));
        assert!(!shareable_pattern("network",&family_pattern(
            "Personal Server").unwrap()));
        assert!(!shareable_pattern("group",&family_pattern(
            "Private Group").unwrap()));
    }
    #[test]
    fn regression_replay_includes_real_release_smoke_corpus(){
        let titles=regression_titles();
        assert!(titles.len()>NEGATIVES.len());
        assert!(titles.iter().any(|s|s.contains("Bleach")));
        assert!(titles.iter().any(|s|s.contains("Dune.2021")));
    }
    #[test]
    fn community_never_exports_raw_title_or_release_group_regexes(){
        for field in ["title","group","site","country","torrent","adult"]{
            assert!(!shareable_field(field));
        }
        for field in ["resolution","audio","hdr","network","languages"]{
            assert!(shareable_field(field));
        }
    }
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
        let alternate=variants(original,"WEB-DL")[0].clone();
        assert_eq!(alternate,"Example.Show.2025.1080p.WEB--DL.x264");
        let pattern=family_pattern("WEB-DL").unwrap();
        let re=regex::Regex::new(&pattern).unwrap();
        assert!(re.is_match(&alternate));
        assert!(re.is_match(original));
        assert!(family_pattern("A.B").is_some());
        assert!(family_pattern("HDR10").is_some());
        assert!(family_pattern("AV1").is_some());
        assert!(family_pattern("AB").is_none());
        assert!(family_pattern("password=sensitive").is_none());
        let alpha_numeric=variants("Sample.Name.2025.HDR10.1080p","HDR10");
        assert_eq!(alpha_numeric[0],"Sample.Name.2025.HDR_10.1080p");
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
    fn generic_induction_exercises_real_non_tv_native_edition_handler(){
        let name="Example.Movie.2025.Ultimate Edition.1080p.WEB-DL.x265";
        let mut ctx=matchlab_parser::parse_title_canonical_context(name,false).unwrap();
        assert_eq!(ctx.result.get("edition"),Some(&json!("Ultimate Edition")));
        ctx.matched.retain(|field,_|field=="edition");
        let examples=derive(name,&ctx);
        assert!(!examples.is_empty(),
            "a repeated separator should expose a new native edition regex gap");
        for example in &examples{
            assert_eq!(example.field,"edition");
            assert_eq!(example.mode,"add");
            assert!(example.variant_release.contains("Ultimate  Edition"));
            let re=regex::Regex::new(&example.pattern).unwrap();
            assert!(re.is_match(&example.variant_release));
        }
    }
    #[test]
    fn observed_distinct_editions_can_propose_a_real_generic_regex(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let mut total=0;
        for work in ["Copper","Silver","Bronze","Golden","Platinum","Titanium"] {
            let name=format!("{work}.Movie.2025.Ultimate Edition.1080p.WEB-DL.x265");
            let mut ctx=matchlab_parser::parse_title_canonical_context(&name,false)
                .unwrap();
            assert_eq!(ctx.result["edition"],"Ultimate Edition");
            ctx.matched.retain(|field,_|field=="edition");
            let evidence=derive(&name,&ctx);
            assert!(!evidence.is_empty(),"{work} failed to produce evidence");
            for example in evidence {
                if example.field=="edition"&&example.mode=="add"{
                    total+=usize::from(insert(&db,&example).unwrap());
                }
            }
        }
        assert!(total>=MIN_FIXES);
        let candidate=candidates(&db).unwrap().into_iter()
            .find(|r|r.field=="edition"&&r.mode=="add")
            .expect("an independently confirmed native edition candidate");
        let measured=replay(&db,&[],&[candidate.clone()],&candidate.key)
            .unwrap().expect("must repair full parser result");
        assert!(measured.1>=MIN_FIXES);
        assert!(measured.0>=MIN_FIXES);
    }
    #[test]
    fn unverified_absent_labels_never_spawn_mutation_rules(){
        let mut ctx=matchlab_parser::parse_title_canonical_context(
            "Movie.2015.1080p.WEB-DL",false).unwrap();
        ctx.result.clear();
        assert!(derive("Movie.2015.1080p.WEB-DL",&ctx).is_empty());
    }
}
