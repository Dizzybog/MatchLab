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
use regex::{Regex, RegexBuilder};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock, RwLock,
};

const EPOCH: i64 = 1;
const MIN_EXAMPLES: i64 = 8;
const MIN_TITLES: i64 = 4;
const MIN_OVERRIDE_EXAMPLES: i64 = 16;
const MIN_OVERRIDE_TITLES: i64 = 6;
const MAX_RULES: usize = 96;
const MAX_EVIDENCE: i64 = 12_000;
const MAX_TITLE_BYTES: usize = 240;
static ACTIVE: OnceLock<RwLock<Vec<FieldRule>>> = OnceLock::new();
static REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct FieldRule {
    field: String,
    signal: String,
    value: Value,
    regex: Regex,
    examples: i64,
    titles: i64,
}

fn registry() -> &'static RwLock<Vec<FieldRule>> {
    ACTIVE.get_or_init(|| RwLock::new(Vec::new()))
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
                values.len() <= 8 && values.iter().any(|value| {
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
fn context_valid(raw: &str, field: &str, signal: &str) -> bool {
    let title = leaf(raw);
    let Some(re) = pattern_for(field, signal) else { return false; };
    // Repeated markers or a bare filename that's *just* a field token are
    // ambiguous. Likewise, "German Connection" is not a language tag.
    if re.find_iter(title).take(2).count() != 1 { return false; }
    let Some(matched) = re.find(title) else { return false; };
    if matched.start() == 0 || matched.end() < 3 { return false; }
    if field == "languages" {
        let lower = title.to_ascii_lowercase();
        let release_context = ["1080p", "2160p", "720p", "web", "bluray",
            "bdrip", "hdtv", "x264", "x265", "hevc", "mkv", "mp4"];
        if !release_context.iter().any(|item| lower.contains(item)) { return false; }
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
            ON learned_field_evidence(field,signal,source_build);
         CREATE TABLE IF NOT EXISTS learned_field_rules(
            field TEXT NOT NULL,signal TEXT NOT NULL,canonical TEXT NOT NULL,
            verified_build TEXT NOT NULL,example_count INTEGER NOT NULL,
            identity_count INTEGER NOT NULL, PRIMARY KEY(field,signal));"
    ).map_err(|e| e.to_string())
}
pub fn observe(db: &Connection, title: &str, details: &Value) -> Result<(), String> {
    if !safe_title(title) { return Ok(()); }
    // Crucial anti-feedback guard: never accept a previously learned overlay
    // as training truth. Re-parse from the canonical Rust parser.
    let canonical = matchlab_parser::parse_title(title,false)
        .map_err(|e| e.to_string())?;
    let parsed = Value::Object(canonical);
    let selected = &details["selected"];
    let Some(identity) = observed_identity(&parsed, selected) else { return Ok(()); };
    let Some(object) = parsed.as_object() else { return Ok(()); };
    let source_build = build();
    for (field, signal) in signals(title) {
        if !context_valid(title, &field, &signal) { continue; }
        let Some(value) = object.get(&field) else { continue; };
        if !validated_value(&field, &signal, value) { continue; }
        let canonical = serde_json::to_string(value).map_err(|e| e.to_string())?;
        let key = digest(&format!("{}:{field}:{signal}", title.to_ascii_lowercase()));
        let existing: i64 = db.query_row(
            "SELECT count(*) FROM learned_field_evidence WHERE source_build=?1",
            [&source_build], |row| row.get(0)).map_err(|e| e.to_string())?;
        if existing >= MAX_EVIDENCE { return Ok(()); }
        db.execute(
            "INSERT OR IGNORE INTO learned_field_evidence(
                field,signal,example_hash,identity_hash,canonical,source_build,first_seen)
             VALUES(?1,?2,?3,?4,?5,?6,strftime('%s','now'))",
            params![field,signal,key,identity,canonical,source_build])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
/// Old evidence is reusable for forensic review but cannot authorize rules
/// for a different parser build. Reclaim it only when the global quota is
/// exceeded; never discard active-build evidence to make room for guesses.
pub fn evict_obsolete(db: &Connection, limit: i64) -> Result<i64, String> {
    if limit <= 0 { return Ok(0); }
    db.execute(
        "DELETE FROM learned_field_evidence WHERE rowid IN (
            SELECT rowid FROM learned_field_evidence WHERE source_build<>?1
            ORDER BY first_seen, rowid LIMIT ?2)",params![build(),limit])
        .map(|n| n as i64).map_err(|e| e.to_string())
}
fn select_rules(db: &Connection) -> Result<Vec<FieldRule>, String> {
    let source_build = build();
    let mut stmt = db.prepare(
        "SELECT field,signal,canonical,count(*) AS examples,
                count(DISTINCT identity_hash) AS titles
         FROM learned_field_evidence WHERE source_build=?1
         GROUP BY field,signal,canonical
         HAVING examples>=?2 AND titles>=?3
         ORDER BY examples DESC,field,signal,canonical LIMIT 512"
    ).map_err(|e| e.to_string())?;
    let matches = stmt.query_map(
        params![source_build, MIN_EXAMPLES, MIN_TITLES],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,
            row.get::<_,String>(2)?,row.get::<_,i64>(3)?,
            row.get::<_,i64>(4)?)))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let mut grouped: BTreeMap<(String,String), Vec<FieldRule>> = BTreeMap::new();
    for (field, signal, canonical, examples, titles) in matches {
        let Ok(value) = serde_json::from_str::<Value>(&canonical) else { continue; };
        if !validated_value(&field, &signal, &value) { continue; }
        let Some(regex) = pattern_for(&field, &signal) else { continue; };
        grouped.entry((field.clone(), signal.clone())).or_default()
            .push(FieldRule{field,signal,value,regex,examples,titles});
    }
    let mut active = Vec::new();
    for ((field,signal), candidates) in grouped {
        // A single technical token mapping to multiple canonical outputs
        // is poisoned or ambiguous, even if one candidate appears more often.
        if candidates.len() != 1 { continue; }
        let rule = candidates.into_iter().next().expect("one candidate");
        let conflicts: i64 = db.query_row(
            "SELECT count(*) FROM learned_field_evidence
             WHERE field=?1 AND signal=?2 AND source_build=?3
               AND canonical<>?4",
            params![field,signal,source_build,
                serde_json::to_string(&rule.value).map_err(|e| e.to_string())?],
            |row| row.get(0)).map_err(|e| e.to_string())?;
        if conflicts != 0 { continue; }
        active.push(rule);
        if active.len() == MAX_RULES { break; }
    }
    Ok(active)
}
fn install(mut incoming: Vec<FieldRule>) {
    incoming.sort_by(|a,b| (&a.field,&a.signal).cmp(&(&b.field,&b.signal)));
    let fingerprint = incoming.iter().map(|rule|
        format!("{}:{}:{}:{}:{}", rule.field,rule.signal,rule.value,
            rule.examples,rule.titles)).collect::<Vec<_>>().join("\n");
    let bytes = Sha256::digest(fingerprint.as_bytes());
    let mut id = [0u8;8];
    id.copy_from_slice(&bytes[..8]);
    if let Ok(mut guard) = registry().write() {
        *guard = incoming;
        REVISION.store(u64::from_le_bytes(id), Ordering::Release);
    }
}
pub fn reconcile(db: &Connection) -> Result<(), String> {
    init(db)?;
    let current = build();
    let active = select_rules(db)?;
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
fn propose_from(title: &str, parsed: &mut Value, active: &[FieldRule]) {
    if active.is_empty() || !safe_title(title) { return; }
    let Some(map) = parsed.as_object_mut() else { return; };
    // Never let an unverified field rule change title, production year,
    // IMDb/AniDB identity or season/episode coordinates.
    let mut candidates: BTreeMap<&str, Vec<&FieldRule>> = BTreeMap::new();
    for rule in active {
        if !context_valid(title,&rule.field,&rule.signal) { continue; }
        if !rule.regex.is_match(leaf(title)) { continue; }
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
            _ => false,
        };
        // Existing values need more independent support than missing values.
        if !empty && (rule.examples < MIN_OVERRIDE_EXAMPLES
            || rule.titles < MIN_OVERRIDE_TITLES) {
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
pub fn apply(title: &str, parsed: &mut Value) {
    if !crate::local_learning::active() { return; }
    if let Ok(guard) = registry().read() {
        propose_from(title,parsed,&guard);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn rule(field:&str,signal:&str,value:Value,examples:i64,titles:i64)->FieldRule {
        FieldRule {field:field.into(),signal:signal.into(),value,
            regex:pattern_for(field,signal).unwrap(),examples,titles}
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
        let n:i64=db.query_row("SELECT count(*) FROM learned_field_evidence",[],
            |row|row.get(0)).unwrap();
        // An observation is only accepted if the *base* parser recognized
        // and independently supplied the exact same value.
        let original=matchlab_parser::parse_title("Example.x265.WEB-DL",false).unwrap();
        assert_eq!(n>0,original["codec"].as_str()==Some("x265"));
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
