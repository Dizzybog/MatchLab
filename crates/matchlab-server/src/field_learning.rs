//! Local, evidence-gated field-level regex induction.
//!
//! Unlike the absolute-anime coordinate learner, this learner only promotes
//! independently readable *technical release tags*. Other fields remain
//! observable but cannot be overwritten without an independent validator.
//! A pattern is reconstructed from an allowlisted token, never executed from
//! a model response, a filename, or the SQLite database.
//!
//! Ordinary matching never opens SQLite: the worker atomically hot-swaps an
//! immutable in-memory ruleset. The whole module is disabled by default.
use regex::{Regex,RegexBuilder,RegexSet,RegexSetBuilder};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock, RwLock,
};

const EPOCH: i64 = 3;
const MIN_EXAMPLES: i64 = 8;
const MIN_TITLES: i64 = 4;
const MIN_OVERRIDE_EXAMPLES: i64 = 16;
const MIN_OVERRIDE_TITLES: i64 = 6;
const MAX_RULES: usize = 96;
const MAX_EVIDENCE: i64 = 12_000;
const MAX_TITLE_BYTES: usize = 240;
static ACTIVE:OnceLock<RwLock<ActiveRules>>=OnceLock::new();
static REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct FieldRule {
    field: String,
    signal: String,
    value: Value,
    regex: Regex,
    examples: i64,
    titles: i64,
    canonical_examples: i64,
}

struct ActiveRules{
    rules:Vec<FieldRule>,
    matcher:Option<RegexSet>,
}
fn registry()->&'static RwLock<ActiveRules>{
    ACTIVE.get_or_init(||RwLock::new(ActiveRules{
        rules:Vec::new(),matcher:None
    }))
}
pub fn revision() -> u64 {
    REVISION.load(Ordering::Acquire)
}
fn digest(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}
fn build() -> String {
    format!("{}:{EPOCH}", option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"))
}
fn normalize(value: &str) -> String {
    value.chars().filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase).collect()
}
fn leaf(title: &str) -> &str {
    title.rsplit(&['/', '\\'][..]).next().unwrap_or(title)
}
fn safe_title(title: &str) -> bool {
    title.len() <= MAX_TITLE_BYTES
        && !title.contains(['/', '\\', '@', '\n', '\r', '\0'])
        && !["http:", "https:", "magnet:", "token=", "password=", "apikey=",
            "secret="].iter().any(|fragment| title.to_ascii_lowercase().contains(fragment))
}
fn technical_field(field: &str) -> bool {
    matches!(field, "resolution" | "codec" | "quality" | "audio"
        | "bit_depth" | "hdr" | "channels" | "languages" | "subbed"
        | "dubbed" | "extension")
}
fn safe_signal(field: &str, signal: &str) -> bool {
    technical_field(field) && (2..=24).contains(&signal.len())
        && signal.bytes().all(|b| b.is_ascii_lowercase()
            || b.is_ascii_digit() || b == b'_' || b == b'.')
        && signal.bytes().any(|b| b.is_ascii_alphabetic() || b.is_ascii_digit())
}
fn pattern_for(field: &str, signal: &str) -> Option<Regex> {
    if !safe_signal(field, signal) { return None; }
    // Turn a *literal* separator-normalized marker into a bounded,
    // linear-time regex. No arbitrary learned regex is executed.
    let parts: Vec<_> = signal.split(['_', '.']).filter(|part| !part.is_empty())
        .map(regex::escape).collect();
    if parts.is_empty() || parts.len() > 4 { return None; }
    let body = parts.join(r"[ ._-]*");
    let source = format!(r"(?i)(?:^|[ ._-])(?:{body})(?:$|[ ._-])");
    if source.len() > 140 { return None; }
    RegexBuilder::new(&source).size_limit(16 * 1024).build().ok()
}
fn language_code(signal: &str) -> Option<&'static str> {
    match signal {
        "norwegian" | "norsk" => Some("no"),
        "swedish" | "svenska" => Some("sv"),
        "danish" | "dansk" => Some("da"),
        "french" => Some("fr"),
        "german" => Some("de"),
        "italian" => Some("it"),
        "japanese" => Some("ja"),
        _ => None,
    }
}
fn consistent(field: &str, signal: &str, value: &Value) -> bool {
    match field {
        "languages" => {
            let Some(code) = language_code(signal) else { return false; };
            value.as_array().is_some_and(|values|
                values.len() == 1 && values.iter().any(|value| {
                    value.as_str().is_some_and(|v|
                        v.eq_ignore_ascii_case(code)
                        || (code == "no" && matches!(v, "nb" | "nn")))
                }))
        }
        "dubbed" | "subbed" => value.as_bool() == Some(true),
        "bit_depth" => {
            let prefix = signal.trim_end_matches("bits").trim_end_matches("bit");
            value.as_i64().is_some_and(|n| n.to_string() == prefix)
                || value.as_str().is_some_and(|s| normalize(s) == normalize(signal))
        }
        "resolution" if matches!(signal, "4k" | "8k") => {
            value.as_str().is_some_and(|s|
                normalize(s) == signal
                || (signal == "4k" && s == "2160p")
                || (signal == "8k" && s == "4320p"))
        }
        _ => value.as_str().is_some_and(|s|
            !s.is_empty() && s.len() <= 48 && normalize(s) == normalize(signal)),
    }
}
fn validated_value(field: &str, signal: &str, value: &Value) -> bool {
    if !consistent(field, signal, value) { return false; }
    match value {
        Value::String(s) => s.len() <= 48
            && s.chars().all(|c| c.is_ascii_alphanumeric()
                || matches!(c, '.' | '-' | '_' | ' ' | '+')),
        Value::Number(n) => n.as_i64().is_some_and(|v| (0..=9999).contains(&v)),
        Value::Bool(v) => *v,
        Value::Array(a) => field == "languages" && a.len() <= 8
            && a.iter().all(|v| v.as_str().is_some_and(|s|
                s.len() <= 5 && s.bytes().all(|b| b.is_ascii_alphabetic()))),
        _ => false,
    }
}
// Independent raw-marker references can label an absent *technical* field
// when the canonical parser misses it. They cannot relabel an existing
// result, supply IMDb coordinates, or learn an unknown label by repetition.
fn independent_marker_value(field:&str,signal:&str)->Option<Value>{
    let value=match (field,signal){
        ("resolution","4320p"|"2160p"|"1080p"|"720p"|"576p"|"480p")=>json!(signal),
        ("codec","x264"|"x265"|"h264"|"h265"|"hevc"|"av1")=>json!(signal),
        ("quality","web_dl"|"webdl")=>json!("WEB-DL"),
        ("quality","webrip")=>json!("WEBRip"),
        ("quality","bluray")=>json!("BluRay"),
        ("quality","hdtv")=>json!("HDTV"),
        ("audio","aac"|"ac3"|"eac3"|"ddp"|"dts"|"flac"|"truehd"|"opus")=>json!(signal),
        ("bit_depth","8bit"|"10bit"|"12bit")=>json!(signal),
        ("channels","5.1"|"7.1"|"2.0")=>json!(signal),
        ("extension","mkv"|"mp4"|"avi"|"m4v"|"webm"|"m2ts")=>json!(signal),
        ("subbed","subbed"|"hardsubbed")|("dubbed","dubbed")=>json!(true),
        _=>return None,
    };
    validated_value(field,signal,&value).then_some(value)
}
// Compile negative probes once. Compiling many regexes on the HTTP hot
// path is forbidden: ordinary parsing must remain low-CPU.
fn conflict_patterns()->&'static BTreeMap<&'static str,Vec<(&'static str,Regex)>>{
    static INDEX:OnceLock<BTreeMap<&'static str,Vec<(&'static str,Regex)>>>
        =OnceLock::new();
    INDEX.get_or_init(||{
        let families:&[(&'static str,&[&'static str])]=&[
            ("resolution",&["4320p","2160p","1080p","720p","576p","480p","4k","8k"]),
            ("codec",&["x264","x265","h264","h265","hevc","av1","vp9"]),
            ("quality",&["web_dl","webdl","webrip","bluray","bdrip","brrip","hdtv","dvdrip"]),
            ("audio",&["aac","ac3","eac3","ddp","dts","flac","truehd","opus"]),
            ("bit_depth",&["8bit","10bit","12bit","10bits","12bits"]),
            ("channels",&["5.1","7.1","2.0"]),
            ("languages",&["norwegian","norsk","swedish","svenska","danish","dansk",
                "french","german","italian","japanese"]),
        ];
        let mut result=BTreeMap::new();
        for &(field,signals) in families{
            let variants=signals.iter().filter_map(|&signal|
                pattern_for(field,signal).map(|regex|(signal,regex))).collect();
            result.insert(field,variants);
        }
        result
    })
}
fn independent_context_with_re(raw:&str,field:&str,signal:&str,re:&Regex)->bool{
    if !context_valid_with_re(raw,field,re){return false;}
    let name=leaf(raw);
    if field=="extension" && !name.to_ascii_lowercase()
        .ends_with(&format!(".{signal}")){return false;}
    !conflict_patterns().get(field).is_some_and(|alternatives|
        alternatives.iter().any(|(other,regex)|
            *other!=signal && regex.is_match(name)))
}
fn independent_context_valid(raw:&str,field:&str,signal:&str)->bool{
    let Some(re)=pattern_for(field,signal)else{return false;};
    independent_context_with_re(raw,field,signal,&re)
}
fn empty_field(value:Option<&Value>)->bool{
    match value{
        None|Some(Value::Null)=>true,
        Some(Value::String(s))=>s.trim().is_empty(),
        Some(Value::Array(a))=>a.is_empty(),
        Some(Value::Object(o))=>o.is_empty(),
        _=>false,
    }
}
fn training_label(title:&str,field:&str,signal:&str,
    parsed:&serde_json::Map<String,Value>)->Option<(Value,bool)>{
    if !independent_context_valid(title,field,signal){return None;}
    if let Some(value)=parsed.get(field)
        && validated_value(field,signal,value){
        return Some((value.clone(),false));
    }
    // A present but disagreeing field is NOT considered missing. Only
    // independently explicit markers can train a missing-field rule.
    let absent=empty_field(parsed.get(field))
        || (matches!(field,"subbed"|"dubbed")
            && parsed.get(field)==Some(&Value::Bool(false)));
    if !absent{return None;}
    independent_marker_value(field,signal).map(|v|(v,true))
}
fn signals(title: &str) -> Vec<(String, String)> {
    // Audit the original raw release, not parser output: marker extraction
    // does not depend on the suspected incorrect field.
    let empty = json!({});
    let mut out = BTreeSet::new();
    for diagnostic in crate::field_audit::audit(title, &empty, &Value::Null) {
        let (Some(code), Some(field), Some(signal)) = (
            diagnostic["code"].as_str(), diagnostic["field"].as_str(),
            diagnostic["signal"].as_str()
        ) else { continue; };
        if !code.starts_with("field_missing_") || !safe_signal(field, signal) {
            continue;
        }
        if pattern_for(field, signal).is_some_and(|re| re.is_match(leaf(title))) {
            out.insert((field.to_owned(), signal.to_owned()));
        }
    }
    out.into_iter().collect()
}
fn context_valid(raw:&str,field:&str,signal:&str)->bool{
    let Some(re)=pattern_for(field,signal)else{return false;};
    context_valid_with_re(raw,field,&re)
}
fn context_valid_with_re(raw:&str,field:&str,re:&Regex)->bool{
    let title=leaf(raw);
    // Repeated markers or a bare filename that's *just* a field token are
    // ambiguous. Likewise, "German Connection" is not a language tag.
    if re.find_iter(title).take(2).count() != 1 { return false; }
    let Some(matched) = re.find(title) else { return false; };
    if matched.start() == 0 || matched.end() < 3 { return false; }
    if field == "languages" {
        let lower = title.to_ascii_lowercase();
        let release_context = ["1080p", "2160p", "720p", "web", "bluray",
            "bdrip", "hdtv", "x264", "x265", "hevc", "mkv", "mp4"];
        // A language word in the title itself (e.g. "The Danish Girl")
        // is not a release-language tag merely because 1080p follows.
        if !release_context.iter().any(|item|
            lower.find(item).is_some_and(|index|index < matched.start())) {
            return false;
        }
    }
    if field == "channels" {
        let lower=title.to_ascii_lowercase();
        let audio=["aac","ac3","eac3","ddp","dts","flac","truehd","opus"];
        // Numeric titles such as "Show 5.1" are common. Do not infer
        // audio layout without an actual codec token in the release.
        if !audio.iter().any(|item|lower.contains(item)){return false;}
    }
    true
}
fn observed_identity(parsed: &Value, selected: &Value) -> Option<String> {
    // The exact same title under five different encodes is only one title
    // identity, not five independent proofs of a regex.
    let name = parsed["title"].as_str()?;
    let normalized = crate::matchlab_matcher::clean_title(name);
    if !(4..=140).contains(&normalized.len()) { return None; }
    if let Some(imdb) = selected["imdb_id"].as_str()
        && imdb.starts_with("tt")
        && (9..=10).contains(&imdb.len())
        && imdb[2..].bytes().all(|b| b.is_ascii_digit())
    {
        return Some(digest(&format!("{imdb}:{normalized}")));
    }
    Some(digest(&normalized))
}
pub fn init(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS learned_field_evidence(
            field TEXT NOT NULL, signal TEXT NOT NULL, example_hash TEXT NOT NULL,
            identity_hash TEXT NOT NULL, canonical TEXT NOT NULL,
            source_build TEXT NOT NULL, first_seen INTEGER NOT NULL,
            PRIMARY KEY(field,signal,example_hash));
         CREATE INDEX IF NOT EXISTS learned_field_cohort
            ON learned_field_evidence(source_build,field,signal,canonical,identity_hash);
         CREATE TABLE IF NOT EXISTS learned_field_gap_evidence(
             field TEXT NOT NULL,signal TEXT NOT NULL,example_hash TEXT NOT NULL,
             identity_hash TEXT NOT NULL,canonical TEXT NOT NULL,
             source_build TEXT NOT NULL,first_seen INTEGER NOT NULL,
             PRIMARY KEY(field,signal,example_hash));
         CREATE INDEX IF NOT EXISTS learned_field_gap_cohort
             ON learned_field_gap_evidence(source_build,field,signal,canonical,identity_hash);
         CREATE TABLE IF NOT EXISTS learned_field_rules(
            field TEXT NOT NULL,signal TEXT NOT NULL,canonical TEXT NOT NULL,
            verified_build TEXT NOT NULL,example_count INTEGER NOT NULL,
            identity_count INTEGER NOT NULL, PRIMARY KEY(field,signal));"
    ).map_err(|e| e.to_string())
}
#[cfg(test)]
pub fn observe(db: &Connection, title: &str, details: &Value)
    -> Result<Vec<(String,String)>,String> {
    observe_with_budget(db,title,details,&mut count_evidence(db)?)
}
pub fn count_evidence(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row(
        "SELECT (SELECT COUNT(*) FROM learned_field_evidence WHERE source_build=?1)
              +(SELECT COUNT(*) FROM learned_field_gap_evidence WHERE source_build=?1)",
        [build()],|row|row.get(0)).map_err(|e|e.to_string())
}
pub fn observe_with_budget(db:&Connection,title:&str,details:&Value,
    count:&mut i64)->Result<Vec<(String,String)>,String>{
    let mut labels=Vec::new();
    if !safe_title(title){return Ok(labels);}
    // Crucial anti-feedback guard: never accept a previously learned overlay
    // as training truth. Re-parse from the canonical Rust parser.
    let canonical = matchlab_parser::parse_title(title,false)
        .map_err(|e| e.to_string())?;
    let parsed = Value::Object(canonical);
    let selected = &details["selected"];
    let Some(identity)=observed_identity(&parsed,selected)else{return Ok(labels);};
    let Some(object)=parsed.as_object()else{return Ok(labels);};
    let source_build = build();
    for (field, signal) in signals(title) {
        if !context_valid(title, &field, &signal) { continue; }
        let Some((value,from_gap))=training_label(title,&field,&signal,object)
            else {continue;};
        let canonical=serde_json::to_string(&value).map_err(|e|e.to_string())?;
        let key=digest(&format!("{}:{field}:{signal}",title.to_ascii_lowercase()));
        if *count>=MAX_EVIDENCE{
            let lookup=if from_gap{
                "SELECT count(*) FROM learned_field_gap_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3"
            }else{
                "SELECT count(*) FROM learned_field_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3"
            };
            let known:i64=db.query_row(lookup,params![field,signal,key],
                |row|row.get(0)).map_err(|e|e.to_string())?;
            if known==0{continue;}
        }
        // Statically selected statements; nothing derived from filenames
        // enters SQL identifiers or statements, only bound parameters.
        let sql=if from_gap{
            "INSERT INTO learned_field_gap_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
             VALUES(?1,?2,?3,?4,?5,?6,strftime('%s','now'))
             ON CONFLICT(field,signal,example_hash) DO UPDATE SET
                identity_hash=excluded.identity_hash,canonical=excluded.canonical,
                source_build=excluded.source_build,first_seen=excluded.first_seen"
        }else{
            "INSERT INTO learned_field_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
             VALUES(?1,?2,?3,?4,?5,?6,strftime('%s','now'))
             ON CONFLICT(field,signal,example_hash) DO UPDATE SET
                identity_hash=excluded.identity_hash,canonical=excluded.canonical,
                source_build=excluded.source_build,first_seen=excluded.first_seen"
        };
        let previously_known:i64=db.query_row(
            if from_gap {
                "SELECT EXISTS(SELECT 1 FROM learned_field_gap_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3)"
            }else{
                "SELECT EXISTS(SELECT 1 FROM learned_field_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3)"
            },params![field,signal,key],|row|row.get(0)).map_err(|e|e.to_string())?;
        db.execute(sql,params![field,signal,key,identity,canonical,source_build])
            .map_err(|e|e.to_string())?;
        if previously_known==0{
            *count+=1;
            labels.push((field,signal));
        }
    }
    Ok(labels)
}
/// Old evidence is reusable for forensic review but cannot authorize rules
/// for a different parser build. Reclaim it only when the global quota is
/// exceeded; never discard active-build evidence to make room for guesses.
pub fn evict_obsolete(db: &Connection, limit: i64) -> Result<i64, String> {
    if limit <= 0 { return Ok(0); }
    init(db)?;
    let obsolete=db.execute(
        "DELETE FROM learned_field_gap_evidence WHERE rowid IN (
            SELECT rowid FROM learned_field_gap_evidence WHERE source_build<>?1
            ORDER BY first_seen,rowid LIMIT ?2)",params![build(),limit])
        .map_err(|e|e.to_string())? as i64;
    let remaining=limit.saturating_sub(obsolete);
    if remaining==0{return Ok(obsolete);}
    let original=db.execute(
        "DELETE FROM learned_field_evidence WHERE rowid IN (
            SELECT rowid FROM learned_field_evidence WHERE source_build<>?1
            ORDER BY first_seen,rowid LIMIT ?2)",params![build(),remaining])
        .map_err(|e|e.to_string())? as i64;
    Ok(obsolete+original)
}
fn select_rules(db: &Connection) -> Result<Vec<FieldRule>, String> {
    let source_build = build();
    let mut stmt = db.prepare(
        "SELECT field,signal,canonical,count(*) AS examples,
                count(DISTINCT identity_hash) AS titles,
                sum(CASE WHEN origin=0 THEN 1 ELSE 0 END) AS canonical_examples
         FROM (
           SELECT field,signal,canonical,identity_hash,0 AS origin
             FROM learned_field_evidence WHERE source_build=?1
           UNION ALL
           SELECT field,signal,canonical,identity_hash,1 AS origin
             FROM learned_field_gap_evidence WHERE source_build=?1
         ) GROUP BY field,signal,canonical
         HAVING examples>=?2 AND titles>=?3
         ORDER BY examples DESC,field,signal,canonical LIMIT 512"
    ).map_err(|e| e.to_string())?;
    let matches = stmt.query_map(
        params![source_build, MIN_EXAMPLES, MIN_TITLES],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,
            row.get::<_,String>(2)?,row.get::<_,i64>(3)?,
            row.get::<_,i64>(4)?,row.get::<_,i64>(5)?)))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let mut grouped: BTreeMap<(String,String), Vec<FieldRule>> = BTreeMap::new();
    for (field, signal, canonical, examples, titles, canonical_examples) in matches {
        let Ok(value) = serde_json::from_str::<Value>(&canonical) else { continue; };
        if !validated_value(&field, &signal, &value) { continue; }
        let Some(regex) = pattern_for(&field, &signal) else { continue; };
        grouped.entry((field.clone(), signal.clone())).or_default()
            .push(FieldRule{field,signal,value,regex,examples,titles,canonical_examples});
    }
    let mut active = Vec::new();
    for ((field,signal), candidates) in grouped {
        // A single technical token mapping to multiple canonical outputs
        // is poisoned or ambiguous, even if one candidate appears more often.
        if candidates.len() != 1 { continue; }
        let rule = candidates.into_iter().next().expect("one candidate");
        let conflicts: i64 = db.query_row(
            "SELECT
               (SELECT count(*) FROM learned_field_evidence
                  WHERE field=?1 AND signal=?2 AND source_build=?3 AND canonical<>?4)
              +(SELECT count(*) FROM learned_field_gap_evidence
                  WHERE field=?1 AND signal=?2 AND source_build=?3 AND canonical<>?4)",
            params![field,signal,source_build,
                serde_json::to_string(&rule.value).map_err(|e| e.to_string())?],
            |row| row.get(0)).map_err(|e| e.to_string())?;
        if conflicts != 0 { continue; }
        active.push(rule);
        if active.len() == MAX_RULES { break; }
    }
    Ok(active)
}
fn may_override(rule:&FieldRule)->bool{
    rule.examples>=MIN_OVERRIDE_EXAMPLES && rule.titles>=MIN_OVERRIDE_TITLES
        && rule.canonical_examples>=2
}
fn semantic_identity(rule:&FieldRule)->(String,String,Value,bool){
    (rule.field.clone(),rule.signal.clone(),rule.value.clone(),may_override(rule))
}
fn install(mut incoming: Vec<FieldRule>) {
    incoming.sort_by(|a,b| (&a.field,&a.signal).cmp(&(&b.field,&b.signal)));
    // Only executable semantics belong in the cache namespace; new
    // examples that do not change a rule must not invalidate a hot cache.
    let fingerprint = incoming.iter().map(|rule|
        format!("{}:{}:{}:{}", rule.field,rule.signal,rule.value,may_override(rule)))
        .collect::<Vec<_>>().join("\n");
    let bytes = Sha256::digest(fingerprint.as_bytes());
    let mut id = [0u8;8];
    id.copy_from_slice(&bytes[..8]);
    // One automaton prunes up to 96 rules in one pass. The original
    // compiled regex still checks boundaries, field-specific conflicts
    // and positive/negative probes for each candidate.
    let matcher=RegexSetBuilder::new(incoming.iter().map(|rule|rule.regex.as_str()))
        .size_limit(1024*1024).build().ok();
    if let Ok(mut guard)=registry().write(){
        *guard=ActiveRules{rules:incoming,matcher};
        REVISION.store(u64::from_le_bytes(id),Ordering::Release);
    }
}
pub fn reconcile(db: &Connection) -> Result<(), String> {
    init(db)?;
    let current = build();
    let active = select_rules(db)?;
    // Publishing the same semantic rules every 15 seconds would cause
    // unnecessary SQLite WAL churn, cache generation changes and CPU work.
    let mut by_rule=active.iter().map(semantic_identity)
        .collect::<Vec<_>>();
    by_rule.sort_by(|a,b| (&a.0,&a.1).cmp(&(&b.0,&b.1)));
    let already_installed = registry().read().ok().is_some_and(|guard|{
        let mut old=guard.rules.iter().map(semantic_identity)
            .collect::<Vec<_>>();
        old.sort_by(|a,b| (&a.0,&a.1).cmp(&(&b.0,&b.1)));
        old == by_rule
    });
    if already_installed && revision()!=0 { return Ok(()); }
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM learned_field_rules",[])
        .map_err(|e| e.to_string())?;
    for rule in &active {
        tx.execute(
            "INSERT INTO learned_field_rules(field,signal,canonical,verified_build,
                example_count,identity_count) VALUES(?1,?2,?3,?4,?5,?6)",
            params![rule.field,rule.signal,
                serde_json::to_string(&rule.value).map_err(|e| e.to_string())?,
                current,rule.examples,rule.titles])
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    install(active);
    Ok(())
}
fn propose_with_prefilter(title:&str,parsed:&mut Value,active:&[FieldRule],
    matcher:Option<&RegexSet>) {
    if active.is_empty() || !safe_title(title) { return; }
    let Some(map) = parsed.as_object_mut() else { return; };
    // Never let an unverified field rule change title, production year,
    // IMDb/AniDB identity or season/episode coordinates.
    let mut candidates: BTreeMap<&str, Vec<&FieldRule>> = BTreeMap::new();
    let matched=matcher.map(|set|set.matches(leaf(title)));
    for (index,rule) in active.iter().enumerate(){
        if matched.as_ref().is_some_and(|found|!found.matched(index)){continue;}
        if !rule.regex.is_match(leaf(title)){continue;}
        if !independent_context_with_re(title,&rule.field,&rule.signal,&rule.regex){
            continue;
        }
        candidates.entry(&rule.field).or_default().push(rule);
    }
    let mut changes = Vec::new();
    for (field, rules) in candidates {
        if rules.len() != 1 { continue; }
        let rule = rules[0];
        let current = map.get(field).cloned().unwrap_or(Value::Null);
        if current == rule.value { continue; }
        let empty = match &current {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Bool(false) if matches!(field,"subbed"|"dubbed") => true,
            _ => false,
        };
        // Existing values need more independent support than missing values.
        if !empty && !may_override(rule) {
            continue;
        }
        // A release may carry multiple conflicting specifications (e.g.
        // WEB-DL plus WEBRip). The existing parse has explicit evidence
        // too; never let population frequency decide which is correct.
        if !empty && current.as_str().is_some_and(|old| {
            let token = normalize(old);
            !token.is_empty() && normalize(leaf(title)).contains(&token)
        }) {
            continue;
        }
        // A multilingual release may legitimately have many languages;
        // never overwrite an already populated language list.
        if field=="languages" && !empty { continue; }
        if !validated_value(field,&rule.signal,&rule.value) { continue; }
        changes.push(json!({"field":field,"marker":rule.signal,
            "previous":current,"value":rule.value,"evidence":rule.examples,
            "independent_titles":rule.titles}));
        map.insert(field.to_owned(),rule.value.clone());
    }
    if !changes.is_empty() {
        map.insert("learned_field_overrides".into(),Value::Array(changes));
    }
}
#[cfg(test)]
fn propose_from(title:&str,parsed:&mut Value,active:&[FieldRule]){
    propose_with_prefilter(title,parsed,active,None);
}
/// Privacy-preserving operator observability. Never return filenames,
/// release history, hashes, canonical labels from private observations or IDs.
pub fn status() -> Value {
    let mut fields = BTreeMap::<String, usize>::new();
    if let Ok(active) = registry().read() {
        for rule in active.rules.iter() {
            *fields.entry(rule.field.clone()).or_default() += 1;
        }
        return json!({"enabled":crate::local_learning::active(),
            "active_rule_count":active.rules.len(),"active_rules_by_field":fields,
            "revision":format!("{:016x}",revision()),
            "training_source":"canonical_rust_parser_and_allowlisted_raw_markers",
            "model":"rust_ftrl_proximal",
            "online_learner":crate::online_model::OnlineModel::status(),
            "identity_and_coordinate_override":"separate_metadata_verification"});
    }
    json!({"enabled":crate::local_learning::active(),
        "active_rule_count":0,"active_rules_by_field":{},
        "error":"rule_registry_unavailable"})
}
pub fn apply(title: &str, parsed: &mut Value) {
    if !crate::local_learning::active() { return; }
    if let Ok(guard) = registry().read() {
        propose_with_prefilter(title,parsed,&guard.rules,guard.matcher.as_ref());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn rule(field:&str,signal:&str,value:Value,examples:i64,titles:i64)->FieldRule {
        FieldRule {field:field.into(),signal:signal.into(),value,
            regex:pattern_for(field,signal).unwrap(),examples,titles,
            canonical_examples:examples}
    }
    #[test]
    fn safe_regex_is_generated_from_literal_tokens_only() {
        assert!(pattern_for("codec","x265").unwrap().is_match("Film.1080p.x265.WEB-DL"));
        assert!(pattern_for("quality","web_dl").unwrap().is_match("Film WEB-DL x265"));
        assert!(pattern_for("codec","x265.*").is_none());
        assert!(pattern_for("imdb_id","tt1234567").is_none());
        assert!(!pattern_for("audio","aac").unwrap().is_match("PaaCific"));
    }
    #[test]
    fn learns_missing_and_overrides_wrong_technical_fields_only_with_more_evidence() {
        let weak=rule("codec","x265",json!("x265"),8,4);
        let strong=rule("quality","web_dl",json!("WEB-DL"),16,6);
        let mut parsed=json!({"title":"Film","quality":"WEBRip","codec":null,
            "seasons":[3],"episodes":[5],"year":2024});
        propose_from("Film.x265.WEB-DL",&mut parsed,&[weak,strong]);
        assert_eq!(parsed["codec"],"x265");
        assert_eq!(parsed["quality"],"WEB-DL");
        assert_eq!(parsed["seasons"],json!([3]));
        assert_eq!(parsed["episodes"],json!([5]));
        assert_eq!(parsed["year"],2024);
        assert_eq!(parsed["learned_field_overrides"].as_array().unwrap().len(),2);
    }
    #[test]
    fn existing_explicit_release_marker_defeats_a_conflicting_learned_rule() {
        let mut parsed=json!({"quality":"WEBRip","codec":"x264"});
        propose_from("Film.WEBRip.WEB-DL.x264",&mut parsed,&[
            rule("quality","web_dl",json!("WEB-DL"),64,20),
            rule("codec","x265",json!("x265"),64,20)
        ]);
        assert_eq!(parsed["quality"],"WEBRip");
        assert_eq!(parsed["codec"],"x264");
    }
    #[test]
    fn prefiltered_set_preserves_the_same_rule_decisions(){
        let active=vec![
            rule("resolution","2160p",json!("2160p"),20,8),
            rule("codec","x265",json!("x265"),20,8),
            rule("audio","aac",json!("AAC"),20,8)
        ];
        let compiled=RegexSetBuilder::new(active.iter().map(|r|r.regex.as_str()))
            .build().unwrap();
        for name in ["Film.2160p.WEB-DL.x265","Film.720p.WEBRip.AAC",
            "Film.1080p.2160p.x265","Film.WEB-DL"]{
            let mut baseline=json!({"title":"Film"});
            let mut optimized=baseline.clone();
            propose_from(name,&mut baseline,&active);
            propose_with_prefilter(name,&mut optimized,&active,Some(&compiled));
            assert_eq!(baseline,optimized,"prefilter changed {name}");
        }
    }
    #[test]
    fn even_a_single_active_rule_must_reject_competing_raw_markers() {
        let mut parsed=json!({"title":"Film"});
        propose_from("Film.1080p.2160p",&mut parsed,&[
            rule("resolution","1080p",json!("1080p"),20,8)
        ]);
        assert!(parsed.get("resolution").is_none());
        assert!(!consistent("languages","norwegian",&json!(["no","sv"])));
    }
    #[test]
    fn competing_signals_fail_closed() {
        let mut parsed=json!({"title":"Film","resolution":"720p"});
        propose_from("Film.1080p.2160p",&mut parsed,&[
            rule("resolution","1080p",json!("1080p"),20,8),
            rule("resolution","2160p",json!("2160p"),20,8)
        ]);
        assert_eq!(parsed["resolution"],"720p");
    }
    #[test]
    fn conservative_language_and_number_validation() {
        assert!(consistent("languages","norwegian",&json!(["no"])));
        assert!(!consistent("languages","norwegian",&json!(["en"])));
        assert!(consistent("bit_depth","10bit",&json!(10)));
        assert!(!validated_value("title","bleach",&json!("Other")));
        let mut parsed=json!({"languages":["fr"]});
        propose_from("Film.German.1080p",&mut parsed,&[
            rule("languages","german",json!(["de"]),32,10)
        ]);
        assert_eq!(parsed["languages"],json!(["fr"]));
    }
    #[test]
    fn cached_overlay_cannot_become_a_training_label() {
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let details=json!({"parsed":{"title":"Example","codec":"x265",
            "learned_field_overrides":[{"field":"codec","value":"x265"}]}});
        observe(&db,"Example.x265.WEB-DL",&details).unwrap();
        let n:i64=db.query_row("SELECT count(*) FROM learned_field_evidence WHERE field=\'codec\'",[],
            |row|row.get(0)).unwrap();
        // An observation is only accepted if the *base* parser recognized
        // and independently supplied the exact same value.
        let original=matchlab_parser::parse_title("Example.x265.WEB-DL",false).unwrap();
        assert_eq!(n>0,original["codec"].as_str().is_some_and(|value|normalize(value)=="x265"));
    }
    #[test]
    fn title_words_and_bare_numbers_are_not_release_tags() {
        assert!(!context_valid("The Danish Girl.1080p.WEB-DL",
            "languages","danish"));
        assert!(context_valid("Film.1080p.Danish.WEB-DL",
            "languages","danish"));
        assert!(!context_valid("Film.5.1.WEB-DL","channels","5.1"));
        assert!(context_valid("Film.AAC.5.1.WEB-DL","channels","5.1"));
    }
    #[test]
    fn missing_field_clues_are_independent_but_strict() {
        let empty=serde_json::Map::new();
        assert_eq!(training_label("Film.2160p.WEB-DL","resolution","2160p",&empty),
            Some((json!("2160p"),true)));
        assert_eq!(training_label("Film.x265.WEB-DL","codec","x265",&empty),
            Some((json!("x265"),true)));
        assert_eq!(training_label("Film.1080p.DUBBED","dubbed","dubbed",&empty),
            Some((json!(true),true)));
        let mut default_flag=serde_json::Map::new();
        default_flag.insert("dubbed".into(),json!(false));
        assert_eq!(training_label("Film.1080p.DUBBED","dubbed","dubbed",&default_flag),
            Some((json!(true),true)));
        assert!(training_label("Film.4K.WEB-DL","resolution","4k",&empty).is_none());
        assert!(training_label("Film.1080p.2160p","resolution","1080p",&empty).is_none());
        assert!(training_label("Film.1080p.2160p","resolution","2160p",&empty).is_none());
        assert!(training_label("Film.AAC.DTS.WEB-DL","audio","aac",&empty).is_none());
        let mut confident=serde_json::Map::new();
        confident.insert("codec".into(),json!("x264"));
        assert!(training_label("Film.x264.x265","codec","x264",&confident).is_none());
        assert!(training_label("The Danish Girl.1080p","languages","danish",&empty).is_none());
        assert!(training_label("Film.1080p.mkv.WEB-DL","extension","mkv",&empty).is_none());
        let mut populated=serde_json::Map::new();
        populated.insert("resolution".into(),json!("720p"));
        assert!(training_label("Film.2160p","resolution","2160p",&populated).is_none());
        populated.insert("resolution".into(),json!("2160p"));
        assert_eq!(training_label("Film.2160p","resolution","2160p",&populated),
            Some((json!("2160p"),false)));
    }
    #[test]
    fn gap_labels_cannot_train_from_the_previous_learned_result() {
        let mut parsed=serde_json::Map::new();
        parsed.insert("codec".into(),json!("x264"));
        parsed.insert("learned_field_overrides".into(),json!([
            {"field":"codec","value":"x265"}]));
        assert!(training_label("Film.x265","codec","x265",&parsed).is_none());
    }
    #[test]
    fn rule_revision_changes_when_override_authority_is_earned() {
        let weak=rule("resolution","2160p",json!("2160p"),8,4);
        let mut strong=weak.clone();
        strong.examples=16;
        strong.titles=6;
        strong.canonical_examples=2;
        assert_ne!(semantic_identity(&weak),semantic_identity(&strong));
        assert!(!may_override(&weak));
        assert!(may_override(&strong));
    }
    #[test]
    fn a_default_false_dubbed_flag_is_an_absence_not_a_confirmed_negative() {
        let mut parsed=json!({"title":"Film","dubbed":false});
        propose_from("Film.1080p.DUBBED",&mut parsed,&[
            rule("dubbed","dubbed",json!(true),8,4)
        ]);
        assert_eq!(parsed["dubbed"],true);
    }
    #[test]
    fn gap_only_rule_fills_missing_but_cannot_override_parser() {
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..16 {
            db.execute("INSERT INTO learned_field_gap_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES(?1,?2,?3,?4,?5,?6,1)",
                params!["resolution","2160p",format!("gap{n}"),
                    format!("title{n}"),r#""2160p""#,build()]).unwrap();
        }
        let rules=select_rules(&db).unwrap();
        assert_eq!(rules.len(),1);
        assert_eq!(rules[0].canonical_examples,0);
        let mut missing=json!({"title":"Film"});
        propose_from("Film.2160p",&mut missing,&rules);
        assert_eq!(missing["resolution"],"2160p");
        let mut populated=json!({"title":"Film","resolution":"720p"});
        propose_from("Film.2160p",&mut populated,&rules);
        assert_eq!(populated["resolution"],"720p");
        for n in 0..2 {
            db.execute("INSERT INTO learned_field_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES(?1,?2,?3,?4,?5,?6,1)",
                params!["resolution","2160p",format!("canonical{n}"),
                    format!("trusted{n}"),r#""2160p""#,build()]).unwrap();
        }
        let rules=select_rules(&db).unwrap();
        assert_eq!(rules[0].canonical_examples,2);
        propose_from("Film.2160p",&mut populated,&rules);
        assert_eq!(populated["resolution"],"2160p");
    }
    #[test]
    fn conflicting_gap_and_canonical_evidence_quarantines_rule() {
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..12 {
            db.execute("INSERT INTO learned_field_gap_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES(?1,?2,?3,?4,?5,?6,1)",
                params!["resolution","2160p",format!("gap{n}"),
                    format!("title{n}"),r#""2160p""#,build()]).unwrap();
        }
        db.execute("INSERT INTO learned_field_evidence(
            field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
            VALUES(?1,?2,?3,?4,?5,?6,1)",
            params!["resolution","2160p","conflict","disagree",
                r#""1080p""#,build()]).unwrap();
        assert!(select_rules(&db).unwrap().is_empty());
    }
    #[test]
    fn requires_diverse_label_agreement_before_activation() {
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..16 {
            db.execute("INSERT INTO learned_field_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES('codec','x265',?1,?2,?3,?4,1)",
                params![n.to_string(),format!("film{}",n%8),
                    if n==15 {r#""x264""#} else {r#""x265""#},build()])
                .unwrap();
        }
        assert!(select_rules(&db).unwrap().is_empty(),
            "conflicting observations must quarantine a learned rule");
        db.execute("DELETE FROM learned_field_evidence WHERE canonical=?1",
            [r#""x264""#]).unwrap();
        assert_eq!(select_rules(&db).unwrap().len(),1);
    }
}
