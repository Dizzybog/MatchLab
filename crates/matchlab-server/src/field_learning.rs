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
use regex::{Regex,RegexBuilder};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock, RwLock,
};

const EPOCH: i64 = 5;
const MIN_EXAMPLES: i64 = 8;
const MIN_TITLES: i64 = 4;
// A rediscovered native marker is evidence, not a new executable rule.
// Require independently observed *missing* fields in several releases.
const MIN_GAP_EXAMPLES: i64 = 4;
const MIN_GAP_TITLES: i64 = 2;
const MIN_OVERRIDE_EXAMPLES: i64 = 16;
const MIN_OVERRIDE_TITLES: i64 = 6;
const MAX_RULES: usize = 96;
const MAX_TITLE_BYTES: usize = 240;
static ACTIVE:OnceLock<RwLock<ActiveRules>>=OnceLock::new();
static REVISION: AtomicU64 = AtomicU64::new(0);
static EVIDENCE_ONLY_CANDIDATES: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_HANDLER_COUNT:OnceLock<usize>=OnceLock::new();

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
}
fn registry()->&'static RwLock<ActiveRules>{
    ACTIVE.get_or_init(||RwLock::new(ActiveRules{
        rules:Vec::new()
    }))
}
pub fn revision() -> u64 {
    REVISION.load(Ordering::Acquire)
}
/// Log only locally active regex syntax, never raw release names.
pub(crate) fn regexes_for_log()->Vec<(String,String,String)>{
    registry().read().ok().map(|g|g.rules.iter().map(|r|
        (r.field.clone(),r.signal.clone(),r.regex.as_str().to_owned()))
        .collect()).unwrap_or_default()
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
fn canonical_codec(signal: &str) -> Option<&'static str> {
    match signal {
        "x264" | "h264" | "avc" => Some("avc"),
        "x265" | "h265" | "hevc" => Some("hevc"),
        "av1" => Some("av1"),
        _ => None,
    }
}
// These labels are independently readable, bounded release markers. They
// are never inferred from previously learned parser output.
fn canonical_audio(signal:&str)->Option<&'static str>{
    match signal {
        "aac"=>Some("AAC"),"ac3"=>Some("Dolby Digital"),
        "eac3"|"ddp"=>Some("Dolby Digital Plus"),
        "dts"=>Some("DTS Lossy"),"flac"=>Some("FLAC"),
        "truehd"=>Some("TrueHD"),"opus"=>Some("OPUS"),
        _=>None,
    }
}
fn canonical_hdr(signal:&str)->Option<&'static str>{
    match signal {
        "dovi"|"dolbyvision"=>Some("DV"),
        "hdr10"=>Some("HDR10"),"hdr10plus"=>Some("HDR10+"),
        _=>None
    }
}
fn independently_verified_array(field:&str,signal:&str)->Option<Value>{
    let result=match field{
        "audio"=>canonical_audio(signal)?,
        "hdr"=>canonical_hdr(signal)?,
        "channels"=>match signal{
            "5.1"=>"5.1","7.1"=>"7.1","2.0"=>"2.0",_=>return None
        },
        "languages"=>language_code(signal)?,
        _=>return None
    };
    Some(json!([result]))
}
fn consistent(field: &str, signal: &str, value: &Value) -> bool {
    match field {
        "codec" => canonical_codec(signal).is_some_and(|canonical|
            value.as_str().is_some_and(|s|s.eq_ignore_ascii_case(canonical))),
        "audio"|"hdr"|"channels"|"languages"=>{
            let Some(expected)=independently_verified_array(field,signal)
                else{return false;};
            let Some(values)=value.as_array()else{return false;};
            let Some(label)=expected[0].as_str()else{return false;};
            values.len()==1 &&values[0].as_str().is_some_and(|found|
                found.eq_ignore_ascii_case(label)
                || (field=="languages"&&label=="no"
                    &&matches!(found,"nb"|"nn")))
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
        Value::Array(a) => matches!(field,"languages"|"audio"|"hdr"|"channels")
            &&a.len()==1 &&a.iter().all(|v|v.as_str().is_some_and(|s|
                !s.is_empty()&&s.len()<=32&&s.bytes().all(|b|
                    b.is_ascii_alphanumeric()
                    ||matches!(b,b' '|b'-'|b'+'|b'.')))),
        _ => false,
    }
}
// Independent raw-marker references can label an absent *technical* field
// when the canonical parser misses it. They cannot relabel an existing
// result, supply IMDb coordinates, or learn an unknown label by repetition.
fn independent_marker_value(field:&str,signal:&str)->Option<Value>{
    let value=match (field,signal){
        ("audio"|"hdr"|"channels"|"languages",_) =>
            independently_verified_array(field,signal)?,
        ("resolution",value) if crate::field_audit::supported_pixel_height(value)=>json!(value),
        ("codec","x264"|"x265"|"h264"|"h265"|"avc"|"hevc"|"av1")=>
            json!(canonical_codec(signal)?),
        ("quality","web_dl"|"webdl")=>json!("WEB-DL"),
        ("quality","webrip")=>json!("WEBRip"),
        ("quality","bluray")=>json!("BluRay"),
        ("quality","hdtv")=>json!("HDTV"),
        ("bit_depth","8bit"|"10bit"|"12bit")=>json!(signal),
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
            ("resolution",&["4320p","3200p","2880p","2160p","1800p","1600p","1440p",
                "1200p","1080p","1024p","960p","900p","864p","800p","720p",
                "640p","600p","576p","540p","480p","360p","240p","4k","8k"]),
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
    if matches!(field,"audio"|"hdr"|"subbed"|"dubbed") {
        // A word such as "Dubbed", "Atmos" or "HDR" in an ordinary title
        // is not independent release metadata. Require technical context
        // before treating it as a trustworthy ML training reference.
        let lower=title.to_ascii_lowercase();
        if !["1080p","2160p","720p","480p","web-dl","webdl",
            "webrip","bluray","bdrip","x264","x265","h264","h265"]
            .iter().any(|marker|lower.contains(marker)){
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
    let normalized = crate::imdb_matcher::clean_title(name);
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
         CREATE INDEX IF NOT EXISTS learned_field_build_cohort
            ON learned_field_evidence(source_build,field,signal,canonical,identity_hash);
         CREATE TABLE IF NOT EXISTS learned_field_gap_evidence(
             field TEXT NOT NULL,signal TEXT NOT NULL,example_hash TEXT NOT NULL,
             identity_hash TEXT NOT NULL,canonical TEXT NOT NULL,
             source_build TEXT NOT NULL,first_seen INTEGER NOT NULL,
             PRIMARY KEY(field,signal,example_hash));
         CREATE INDEX IF NOT EXISTS learned_field_gap_build_cohort
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
    let parsed=Value::Object(matchlab_parser::parse_title(title,false)
        .map_err(|e|e.to_string())?);
    observe_with_budget(db,title,details,&parsed,&mut count_evidence(db)?)
}
pub fn count_evidence(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row(
        "SELECT (SELECT COUNT(*) FROM learned_field_evidence WHERE source_build=?1)
              +(SELECT COUNT(*) FROM learned_field_gap_evidence WHERE source_build=?1)",
        [build()],|row|row.get(0)).map_err(|e|e.to_string())
}
/// Keep the training corpus rolling when it is full. Preserve evidence
/// for active rules; prune only the oldest current-build examples from
/// unpromoted candidates. This runs on the learning worker, never the API.
fn reclaim_unpromoted(db:&Connection,limit:i64)->Result<i64,String>{
    let batch=(limit/20).clamp(1,256);
    let current=build();
    let mut removed=0i64;
    for table in ["learned_field_gap_evidence","learned_field_evidence"]{
        let sql=match table{
            "learned_field_gap_evidence"=>
                "DELETE FROM learned_field_gap_evidence WHERE rowid IN (
                    SELECT e.rowid FROM learned_field_gap_evidence e
                    LEFT JOIN learned_field_rules r ON r.field=e.field
                        AND r.signal=e.signal AND r.verified_build=?1
                    WHERE e.source_build=?1 AND r.field IS NULL
                    ORDER BY e.first_seen,e.rowid LIMIT ?2)",
            _=>
                "DELETE FROM learned_field_evidence WHERE rowid IN (
                    SELECT e.rowid FROM learned_field_evidence e
                    LEFT JOIN learned_field_rules r ON r.field=e.field
                        AND r.signal=e.signal AND r.verified_build=?1
                    WHERE e.source_build=?1 AND r.field IS NULL
                    ORDER BY e.first_seen,e.rowid LIMIT ?2)"
        };
        let left=batch.saturating_sub(removed);
        if left==0{break;}
        removed+=db.execute(sql,params![current,left])
            .map_err(|e|e.to_string())? as i64;
    }
    Ok(removed)
}
pub fn observe_with_budget(db:&Connection,title:&str,details:&Value,
    parsed:&Value,count:&mut i64)->Result<Vec<(String,String)>,String>{
    let mut labels=Vec::new();
    if !safe_title(title){return Ok(labels);}
    // The worker supplies a *fresh canonical parser result*, not the
    // possibly cached or ML-decorated parsed response from request history.
    let selected=&details["selected"];
    let Some(identity)=observed_identity(&parsed,selected)else{return Ok(labels);};
    let Some(object)=parsed.as_object()else{return Ok(labels);};
    let source_build = build();
    for (field, signal) in signals(title) {
        if !context_valid(title, &field, &signal) { continue; }
        let Some((value,from_gap))=training_label(title,&field,&signal,object)
            else {continue;};
        let canonical=serde_json::to_string(&value).map_err(|e|e.to_string())?;
        let key=digest(&format!("{}:{field}:{signal}",title.to_ascii_lowercase()));
        let capacity=crate::learning_cache::training_max_examples();
        if *count>=capacity{
            let lookup=if from_gap{
                "SELECT count(*) FROM learned_field_gap_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3
                   AND source_build=?4"
            }else{
                "SELECT count(*) FROM learned_field_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3
                   AND source_build=?4"
            };
            let known:i64=db.query_row(lookup,params![field,signal,key,source_build],
                |row|row.get(0)).map_err(|e|e.to_string())?;
            if known==0{
                let removed=reclaim_unpromoted(db,capacity)?;
                *count=(*count).saturating_sub(removed);
                if *count>=capacity{continue;}
            }
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
                 WHERE field=?1 AND signal=?2 AND example_hash=?3
                   AND source_build=?4)"
            }else{
                "SELECT EXISTS(SELECT 1 FROM learned_field_evidence
                 WHERE field=?1 AND signal=?2 AND example_hash=?3
                   AND source_build=?4)"
            },params![field,signal,key,source_build],|row|row.get(0)).map_err(|e|e.to_string())?;
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
    select_gap_rules(db,true)
}
/// Shared review proposals can describe a verified missing-field marker
/// even when no safe live Rust handler exists. Only live rules affect
/// compiled patterns, revision hashes and user-facing active counts.
fn select_gap_rules(db:&Connection,live_only:bool)->Result<Vec<FieldRule>,String>{
    let source_build = build();
    let mut stmt = db.prepare(
        "SELECT field,signal,canonical,count(*) AS examples,
                count(DISTINCT identity_hash) AS titles,
                sum(CASE WHEN origin=0 THEN 1 ELSE 0 END) AS canonical_examples,
                sum(CASE WHEN origin=1 THEN 1 ELSE 0 END) AS gap_examples,
                count(DISTINCT CASE WHEN origin=1 THEN identity_hash END) AS gap_titles
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
            row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,
            row.get::<_,i64>(6)?,row.get::<_,i64>(7)?)))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let mut grouped: BTreeMap<(String,String), Vec<FieldRule>> = BTreeMap::new();
    let mut evidence_only=0u64;
    for (field, signal, canonical, examples, titles,
         canonical_examples, gap_examples, gap_titles) in matches {
        let Ok(value) = serde_json::from_str::<Value>(&canonical) else { continue; };
        if !validated_value(&field, &signal, &value) { continue; }
        let Some(regex) = pattern_for(&field, &signal) else { continue; };
        // Explicit comparison against the original Rust parser is done
        // during training_label: gap evidence is only emitted when the
        // canonical field was missing (not when it disagreed). Observations
        // already understood by the parser continue training the FTRL
        // model, but cannot install a redundant executable regex.
        if gap_examples<MIN_GAP_EXAMPLES || gap_titles<MIN_GAP_TITLES {
            evidence_only+=1;
            continue;
        }
        let rule=FieldRule{field:field.clone(),signal:signal.clone(),
            value,regex,examples,titles,canonical_examples};
        // Only count rules that can actually be attached to a native
        // parser handler. Extension, language and subtitle markers still
        // contribute training evidence, but must not inflate "active".
        if live_only
            && source_handler_additions(std::slice::from_ref(&rule)).is_empty(){
            evidence_only+=1;
            continue;
        }
        grouped.entry((field, signal)).or_default().push(rule);
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
    if live_only{
        EVIDENCE_ONLY_CANDIDATES.store(evidence_only,Ordering::Relaxed);
    }
    Ok(active)
}
/// Migration helper: snapshot old, previously activated rules before a
/// fresh parser-build reconcile overwrites the materialized active-rule table.
/// Historical evidence is review-only; it cannot authorize hot activation.
pub fn persisted_rule_archive(db:&Connection)->Result<Vec<Value>,String>{
    init(db)?;
    let mut stmt=db.prepare("SELECT field,signal,canonical,verified_build,
        example_count,identity_count FROM learned_field_rules
        ORDER BY field,signal").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|r|Ok((
        r.get::<_,String>(0)?,r.get::<_,String>(1)?,
        r.get::<_,String>(2)?,r.get::<_,String>(3)?,
        r.get::<_,i64>(4)?,r.get::<_,i64>(5)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    let mut out=Vec::new();
    for (field,signal,canonical,source_build,examples,titles) in rows {
        let Ok(value)=serde_json::from_str::<Value>(&canonical)else{continue;};
        let Some(regex)=pattern_for(&field,&signal)else{continue;};
        if !validated_value(&field,&signal,&value){continue;}
        let key=format!("community-field:{field}:{signal}:{canonical}");
        out.push(json!({"kind":"learned_field_regex",
            "fingerprint":digest(&key),"source_build":source_build,
            "field":field,"signal":signal,"regex":regex.as_str(),
            "canonical":value,"evidence_count":examples,
            "distinct_titles":titles,"canonical_confirmations":0,
            "verification":"archive_active_rule_review_only"}));
    }
    Ok(out)
}

/// Public, opt-in community suggestions. Only reconstructed regex grammar,
/// hashed evidence totals and bundled-handler patterns are exported:
/// no private torrent names, IMDb identifiers or raw observations.
pub fn community_proposals(db:&Connection)->Result<Vec<Value>,String>{
    proposals(db,false)
}
pub fn archive_proposals(db:&Connection)->Result<Vec<Value>,String>{
    proposals(db,true)
}
fn proposals(db:&Connection,archive:bool)->Result<Vec<Value>,String>{
    let rules=select_gap_rules(db,false)?;
    let mut out=Vec::new();
    for rule in &rules {
        if !archive && (rule.examples<12 ||rule.titles<4){continue;}
        let canonical=serde_json::to_string(&rule.value).map_err(|e|e.to_string())?;
        let key=format!("community-field:{}:{}:{canonical}",rule.field,rule.signal);
        out.push(json!({
            "kind":"learned_field_regex",
            "fingerprint":digest(&key),
            "field":rule.field,"signal":rule.signal,
            "regex":rule.regex.as_str(),
            "canonical":rule.value,
            "evidence_count":rule.examples,
            "distinct_titles":rule.titles,
            "canonical_confirmations":rule.canonical_examples,
            "source_build":build(),
            "verification":if source_handler_additions(
                std::slice::from_ref(rule)).is_empty(){
                "review_only_no_native_handler"
            }else{
                "local_canonical_and_explicit_release_marker"
            },
        }));
    }
    // Read the original source-handler catalog in the background only.
    // Share BOTH the old and proposed new regex for human review.
    for patch in source_handler_patches(&rules){
        let source=matchlab_parser::existing_regex_rules();
        let Some(original)=source.get(patch.index)else{continue;};
        let field=original["field"].as_str().unwrap_or_default();
        let transform=original["transform"].as_str().unwrap_or_default();
        let candidate=rules.iter().find(|rule|{
            rule.field==field
                &&format!("value('{}')",rule.value.as_str().unwrap_or(""))
                    ==transform
                &&patch.replacement_pattern.ends_with(rule.regex.as_str().trim_end_matches("$"))
        });
        // Same rule may be available as an extension on more than one
        // source pattern; do not manufacture evidence if unavailable.
        let Some(rule)=candidate.or_else(||rules.iter().find(|r|
            r.field==field && may_override(r)
                &&patch.replacement_pattern.contains(r.regex.as_str())
        ))else{continue;};
        let key=format!("community-patch:{}:{}:{}",
            patch.index,patch.original_pattern,patch.replacement_pattern);
        out.push(json!({
            "kind":"learned_source_regex_patch",
            "fingerprint":digest(&key),
            "handler_index":patch.index,
            "field":rule.field,"signal":rule.signal,
            "original_regex":patch.original_pattern,
            "proposed_regex":patch.replacement_pattern,
            "transform":transform,
            "evidence_count":rule.examples,
            "distinct_titles":rule.titles,
            "canonical_confirmations":rule.canonical_examples,
            "source_build":build(),
            "verification":"source_catalog_and_canonical_release_markers",
        }));
    }
    out.truncate(32);
    Ok(out)
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
    if let Ok(mut guard)=registry().write(){
        *guard=ActiveRules{rules:incoming};
        REVISION.store(u64::from_le_bytes(id),Ordering::Release);
    }
}
/// This is the source-aware part of ML learning: the worker examines every
/// generated Rust-parser handler, not just an external list of overlay tags.
/// It can construct a broader, bounded replacement for an exact existing
/// handler while retaining its field, transform, flags and priority.
/// Restrict live source patches to clear technical markers; risky title,
/// year, IMDb and unmarked episode regex remain review-only.
fn source_handler_patches(active:&[FieldRule])
    ->Vec<matchlab_parser::HandlerPatch> {
    let catalog=matchlab_parser::existing_regex_rules();
    let mut patches=Vec::new();
    let mut replaced=std::collections::BTreeSet::new();
    for rule in active {
        if !may_override(rule) ||
            !matches!(rule.field.as_str(),"resolution"|"codec"|"bit_depth") {
            continue;
        }
        let Some(canonical)=rule.value.as_str()else{continue;};
        let expected_transform=format!("value('{canonical}')");
        let raw_marker=rule.signal.replace('_',".");
        let probe=format!("Example.{raw_marker}.WEB-DL");
        // If the original parser already recognizes the exact marker and
        // value, a rewrite adds no benefit and would only spend CPU.
        let already=matchlab_parser::parse_title_canonical(&probe,false)
            .ok().and_then(|v|v.get(&rule.field).cloned())
            .is_some_and(|v|v==rule.value);
        if already{continue;}
        for source in &catalog {
            let (Some(index),Some(pattern))=(
                source["index"].as_u64().map(|n|n as usize),
                source["pattern"].as_str()
            )else{continue;};
            if source["field"]!=rule.field ||
                source["transform"].as_str()!=Some(expected_transform.as_str())
                ||pattern.len()>180||replaced.contains(&index){continue;}
            // Keep old behavior as one branch, then add the newly verified
            // literal marker. This grows a handler rather than deleting a
            // working rule, and only ever compiles linear-time Rust regexes.
            let replacement=format!("(?:{pattern})|(?:{})",rule.regex.as_str());
            let Ok(test)=RegexBuilder::new(&replacement)
                .size_limit(256*1024).build()else{continue;};
            if !test.is_match(&probe){continue;}
            if [
                "The Danish Girl.2015", "Doctor Who 2005", "Film 1917",
                "Example.S01E03", "Episode 300 Explained"
            ].iter().any(|negative| {
                test.is_match(negative) && !Regex::new(pattern)
                    .is_ok_and(|original|original.is_match(negative))
            }) {continue;}
            patches.push(matchlab_parser::HandlerPatch{
                index,original_pattern:pattern.to_string(),
                replacement_pattern:replacement
            });
            replaced.insert(index);
            break;
        }
        if patches.len()==8{break;}
    }
    patches
}

/// Learned regex is interpreted using an existing MatchLab handler.
/// Its field, transform and options are not written by the model.
fn source_handler_additions(active:&[FieldRule])
    ->Vec<matchlab_parser::HandlerAddition>{
    let catalog=matchlab_parser::existing_regex_rules();
    let mut out=Vec::new();
    for rule in active {
        // Context-sensitive tags need additional native parser guards before
        // automation. They can still be shared as human-review proposals.
        // All independently validated technical fields are eligible for
        // ADD; the installed parser alone performs the native transformation.
        // Other fields still enter training/review but cannot promote without
        // an independent reference validator.
        if !technical_field(&rule.field){continue;}
        let transform=match (rule.field.as_str(),&rule.value) {
            ("extension",Value::String(_))=>"lowercase".to_owned(),
            (_,Value::String(value))=>format!("value('{value}')"),
            (_,Value::Array(labels)) if labels.len()==1 =>
                match labels[0].as_str(){
                    Some(label)=>format!("uniq_concat(value('{label}'))"),
                    None=>continue
                },
            (_,Value::Bool(true))=>"boolean".to_owned(),
            _=>continue,
        };
        let template=catalog.iter().find(|row|
            row["field"]==rule.field
            &&row["transform"].as_str()==Some(transform.as_str())
            &&row["index"].as_u64().is_some()
        );
        let Some(index)=template.and_then(|row|row["index"].as_u64())
            .map(|i|i as usize)else{continue;};
        out.push(matchlab_parser::HandlerAddition{
            source_index:index,pattern:rule.regex.as_str().to_owned()
        });
        if out.len()>=96{break;}
    }
    out
}

/// Publish only regex into MatchLab's parser. No ML field postprocessor.
/// Read the same source patches and additions the live engine would
/// install. Background replay must include them to catch cross-rule
/// conflicts and redundant/ineffective corrective regexes.
pub(crate) fn source_rules_without_general(
    tv_additions:Vec<matchlab_parser::HandlerAddition>
)->Result<(Vec<matchlab_parser::HandlerPatch>,
    Vec<matchlab_parser::HandlerAddition>),String>{
    let fields=registry().read().map_err(|e|e.to_string())?.rules.clone();
    let patches=source_handler_patches(&fields);
    let mut additions=source_handler_additions(&fields);
    additions.extend(tv_additions);
    Ok((patches,additions))
}
pub(crate) fn current_source_rules(
    tv_additions:Vec<matchlab_parser::HandlerAddition>
)->Result<(Vec<matchlab_parser::HandlerPatch>,
    Vec<matchlab_parser::HandlerAddition>),String>{
    let (patches,mut additions)=source_rules_without_general(tv_additions)?;
    additions.extend(crate::general_learning::handler_additions());
    Ok((patches,additions))
}
pub fn publish_parser_rules_with_tv(
    tv_additions:Vec<matchlab_parser::HandlerAddition>
)->Result<(),String>{
    publish_parser_rules_with_tv_and_corrections(
        tv_additions,crate::corrective_learning::handler_corrections())
}
pub fn publish_parser_rules_with_tv_and_corrections(
    tv_additions:Vec<matchlab_parser::HandlerAddition>,
    corrections:Vec<matchlab_parser::HandlerCorrection>
)->Result<(),String>{
    let (patches,additions)=current_source_rules(tv_additions)?;
    let mut corrections=corrections;
    corrections.extend(crate::general_learning::handler_corrections());
    matchlab_parser::install_learned_handler_regex_with_corrections(
        &patches,&additions,&corrections)
        .map_err(|e|format!("learned parser regex rejected: {e}"))
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
    let patches=source_handler_patches(&active);
    let mut additions=source_handler_additions(&active);
    additions.extend(crate::tv_learning::handler_additions());
    additions.extend(crate::general_learning::handler_additions());
    let mut corrections=crate::corrective_learning::handler_corrections();
    corrections.extend(crate::general_learning::handler_corrections());
    matchlab_parser::install_learned_handler_regex_with_corrections(
        &patches,&additions,&corrections)
        .map_err(|e|format!("learned parser regex rejected: {e}"))?;
    install(active);
    Ok(())
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
            "evidence_only_candidates":EVIDENCE_ONLY_CANDIDATES.load(Ordering::Relaxed),
            "activation_policy":"requires_multiple_canonical_field_gaps_and_native_handler",
            "revision":format!("{:016x}",revision()),
            "training_source":"canonical_rust_parser_and_allowlisted_raw_markers",
            "model":"rust_ftrl_proximal",
            "online_learner":crate::online_model::OnlineModel::status(),
            "candidate_discovery":crate::candidate_discovery::status(),
            "tv_coordinate_learning":crate::tv_learning::status(),
            "original_handler_rules":*ORIGINAL_HANDLER_COUNT.get_or_init(||matchlab_parser::existing_regex_rules().len()),
            "source_rule_patch_revision":format!("{:016x}",matchlab_parser::handler_patch_revision()),
            "identity_and_coordinate_override":"separate_metadata_verification"});
    }
    json!({"enabled":crate::local_learning::active(),
        "active_rule_count":0,"active_rules_by_field":{},
        "error":"rule_registry_unavailable"})
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
    fn learned_additions_are_regex_only_and_reuse_original_handler_semantics() {
        let mut codec=rule("codec","x265",json!("hevc"),8,4);
        let weak=source_handler_additions(&[codec.clone()]);
        assert_eq!(weak.len(),1);
        let catalog=matchlab_parser::existing_regex_rules();
        let template=&catalog[weak[0].source_index];
        assert_eq!(template["field"],"codec");
        assert_eq!(template["transform"],"value('hevc')");
        assert!(weak[0].pattern.contains("x265"));
        codec.examples=20;
        codec.titles=8;
        let strong=source_handler_additions(&[codec]);
        assert_eq!(strong.len(),1);
        assert_eq!(strong[0].pattern,weak[0].pattern);
    }
    #[test]
    fn context_sensitive_markers_require_raw_release_evidence(){
        assert!(!independent_context_valid(
            "The.Danish.Girl.2015.1080p","languages","danish"));
        assert!(!independent_context_valid(
            "Show.Name.5.1.1080p","channels","5.1"));
        assert!(independent_context_valid(
            "Example.Show.1080p.AAC.5.1","channels","5.1"));
        assert!(!independent_context_valid(
            "The.Dubbed.Movie.2024","dubbed","dubbed"));
        assert!(!independent_context_valid(
            "HDR.Explained.2024","hdr","hdr10"));
        assert!(!independent_context_valid(
            "AAC.Anthology.2024","audio","aac"));
        assert!(independent_context_valid(
            "Example.Show.1080p.AAC","audio","aac"));
    }
    #[test]
    fn independently_verified_array_fields_can_generate_native_regex_rules(){
        for (field,signal,expected) in [
            ("audio","aac",json!(["AAC"])),
            ("audio","eac3",json!(["Dolby Digital Plus"])),
            ("hdr","dovi",json!(["DV"])),
            ("channels","5.1",json!(["5.1"])),
            ("languages","norsk",json!(["no"])),
        ]{
            assert_eq!(independent_marker_value(field,signal),Some(expected.clone()),
                "{field}/{signal} lacks independent canonical evidence");
            let item=rule(field,signal,expected,24,8);
            let additions=source_handler_additions(&[item]);
            assert!(!additions.is_empty(),
                "{field}/{signal} should reuse a native Rust handler");
        }
        // Native HDR10 has a pattern-dependent transform; a generic
        // cloned handler cannot guarantee the same semantics. It remains
        // evidence-only until a safe field validator/transform is available.
        assert_eq!(independent_marker_value("hdr","hdr10"),
            Some(json!(["HDR10"])));
        assert!(source_handler_additions(&[rule(
            "hdr","hdr10",json!(["HDR10"]),24,8)]).is_empty());
        let subs=rule("subbed","subbed",json!(true),24,8);
        assert!(!source_handler_additions(&[subs]).is_empty());
        let file=rule("extension","mkv",json!("mkv"),24,8);
        assert!(!source_handler_additions(&[file]).is_empty());
        // No discovered marker may install a rule without an independently
        // validated label with a native parser transform.
        assert!(independent_marker_value("audio","mystery_audio").is_none());
        assert!(pattern_for("title","show").is_none(),
            "unverified title labels must never become live regex");
    }
    #[test]
    fn conservative_language_and_number_validation() {
        assert!(consistent("languages","norwegian",&json!(["no"])));
        assert!(!consistent("languages","norwegian",&json!(["en"])));
        assert!(consistent("bit_depth","10bit",&json!(10)));
        assert!(!validated_value("title","bleach",&json!("Other")));
        assert_eq!(source_handler_additions(&[
            rule("languages","german",json!(["de"]),32,10)
        ]).len(),1);
        assert!(!independent_context_valid(
            "The.German.Connection.2005.1080p","languages","german"));
    }
    #[test]
    fn learned_codec_values_follow_the_original_parser_api() {
        for (marker, expected) in [
            ("x264", "avc"), ("h264", "avc"), ("avc", "avc"),
            ("x265", "hevc"), ("h265", "hevc"), ("hevc", "hevc"),
            ("av1", "av1"),
        ] {
            let release=format!("Example.{marker}.WEB-DL");
            let original=matchlab_parser::parse_title_canonical(&release,false).unwrap();
            let inferred=independent_marker_value("codec",marker);
            assert_eq!(inferred,Some(json!(expected)),"wrong codec for {marker}");
            assert!(consistent("codec",marker,&json!(expected)));
            if marker != expected {
                assert!(!consistent("codec",marker,&json!(marker)),
                    "raw alias must not replace canonical values");
            }
            if let Some(value)=original.get("codec") {
                assert_eq!(value.as_str(),Some(expected),
                    "canonical parser disagrees for {marker}");
            }
        }
        assert!(independent_marker_value("codec","unknown").is_none());
        assert!(!validated_value("codec","x265",&json!("x265")));
    }
    #[test]
    fn cached_overlay_cannot_become_a_training_label() {
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let details=json!({"parsed":{"title":"Example","codec":"hevc",
            "learned_field_overrides":[{"field":"codec","value":"hevc"}]}});
        observe(&db,"Example.x265.WEB-DL",&details).unwrap();
        let n:i64=db.query_row("SELECT count(*) FROM learned_field_evidence WHERE field=\'codec\'",[],
            |row|row.get(0)).unwrap();
        // An observation is only accepted if the *base* parser recognized
        // and independently supplied the exact same value.
        let original=matchlab_parser::parse_title("Example.x265.WEB-DL",false).unwrap();
        assert_eq!(n>0,original["codec"].as_str().is_some_and(|value|normalize(value)=="hevc"));
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
        assert_eq!(training_label("Film.1440p.WEB-DL","resolution","1440p",&empty),
            Some((json!("1440p"),true)));
        assert!(training_label("Film.1440p.1080p","resolution","1440p",&empty).is_none());
        assert_eq!(training_label("Film.x265.WEB-DL","codec","x265",&empty),
            Some((json!("hevc"),true)));
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
        confident.insert("codec".into(),json!("avc"));
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
        parsed.insert("codec".into(),json!("avc"));
        parsed.insert("learned_field_overrides".into(),json!([
            {"field":"codec","value":"hevc"}]));
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
    fn native_successes_are_training_evidence_not_new_regex_rules(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..24 {
            db.execute("INSERT INTO learned_field_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES('codec','x264',?1,?2,'\"avc\"',?3,1)",
                params![format!("sample{n}"),format!("series{n}"),build()]
            ).unwrap();
        }
        assert!(select_rules(&db).unwrap().is_empty(),
            "native x264 recognitions must not be promoted as ML discoveries");
    }
    #[test]
    fn non_executable_gap_evidence_cannot_inflate_active_rules(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..16 {
            db.execute("INSERT INTO learned_field_gap_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES('hdr','hdr10',?1,?2,'[\"HDR10\"]',?3,1)",
                params![format!("sample{n}"),format!("series{n}"),build()]
            ).unwrap();
        }
        assert!(select_rules(&db).unwrap().is_empty(),
            "a pattern-dependent native transform remains review-only");
    }
    #[test]
    fn unsupported_field_gaps_still_reach_human_review_proposals(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..16{
            db.execute("INSERT INTO learned_field_gap_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES('hdr','hdr10',?1,?2,'[\"HDR10\"]',?3,1)",
                params![format!("gap{n}"),format!("title{n}"),build()]
            ).unwrap();
        }
        assert!(select_rules(&db).unwrap().is_empty());
        let proposals=community_proposals(&db).unwrap();
        assert!(proposals.iter().any(|p|
            p["field"]=="hdr"
                &&p["signal"]=="hdr10"
                &&p["verification"]=="review_only_no_native_handler"),
            "unsupported field improvements must remain shareable for human review");
    }
    #[test]
    fn a_single_missing_observation_is_not_a_validated_improvement(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..20 {
            let table=if n==0{"learned_field_gap_evidence"}
                else{"learned_field_evidence"};
            let sql=format!("INSERT INTO {table}(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES('resolution','1440p',?1,?2,'\"1440p\"',?3,1)");
            db.execute(&sql,params![format!("sample{n}"),
                format!("series{n}"),build()]).unwrap();
        }
        assert!(select_rules(&db).unwrap().is_empty(),
            "numerous native successes cannot override insufficient gap evidence");
    }
    #[test]
    fn gap_only_rule_is_regex_evidence_not_a_field_override() {
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
        // A gap rule may add parser regex, but does not change any JSON
        // field itself; existing bundled parser rules retain priority.
        assert_eq!(source_handler_additions(&rules).len(),1);
        for n in 0..2 {
            db.execute("INSERT INTO learned_field_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES(?1,?2,?3,?4,?5,?6,1)",
                params!["resolution","2160p",format!("canonical{n}"),
                    format!("trusted{n}"),r#""2160p""#,build()]).unwrap();
        }
        let rules=select_rules(&db).unwrap();
        assert_eq!(rules[0].canonical_examples,2);
        assert!(may_override(&rules[0]));
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
    fn corpus_capacity_reclaims_unpromoted_evidence_not_active_rules(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        for n in 0..10{
            db.execute("INSERT INTO learned_field_gap_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
                VALUES(?1,?2,?3,?4,?5,?6,1)",
                params!["resolution","1440p",format!("old{n}"),
                    format!("identity{n}"),r#""1440p""#,build()]).unwrap();
        }
        db.execute("INSERT INTO learned_field_rules(
            field,signal,canonical,verified_build,example_count,identity_count)
            VALUES('resolution','1440p',?1,?2,10,10)",
            params![r#""1440p""#,build()]).unwrap();
        assert_eq!(reclaim_unpromoted(&db,100).unwrap(),0);
        db.execute("DELETE FROM learned_field_rules",[]).unwrap();
        assert_eq!(reclaim_unpromoted(&db,100).unwrap(),5);
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
                    if n==15 {r#""avc""#} else {r#""hevc""#},build()])
                .unwrap();
        }
        assert!(select_rules(&db).unwrap().is_empty(),
            "conflicting observations must quarantine a learned rule");
        db.execute("DELETE FROM learned_field_evidence WHERE canonical=?1",
            [r#""avc""#]).unwrap();
        // All remaining examples are successful original-parser parses;
        // they remain evidence but must never become an active new rule.
        assert!(select_rules(&db).unwrap().is_empty());
    }
}
