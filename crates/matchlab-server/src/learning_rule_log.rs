//! Operator-visible inventory of *active* custom parser regex and the durable
//! offline archive. No torrent filenames, training samples, or IMDb IDs logged.
//! This runs only in the learning worker, never on the matching hot path.
use rusqlite::Connection;
use serde_json::{json,Value};
use std::collections::{BTreeMap,BTreeSet};
use std::sync::atomic::{AtomicBool,AtomicU64,Ordering};

static ACTIVE_REGEX:AtomicU64=AtomicU64::new(0);
static TOTAL_RULES:AtomicU64=AtomicU64::new(0);
static COMPILED_PATTERNS:AtomicU64=AtomicU64::new(0);
static ARCHIVED_REGEX:AtomicU64=AtomicU64::new(0);
static INVENTORY_READY:AtomicBool=AtomicBool::new(false);

/// Lock-free, privacy-safe dashboard snapshot. The learner updates these
/// counters after a successful DB scan; HTTP requests never open SQLite.
pub fn status()->Value{
    let active=ACTIVE_REGEX.load(Ordering::Relaxed);
    // The background worker publishes total before active. Protect against
    // sampling the two atomic counters across a concurrent refresh.
    let total=TOTAL_RULES.load(Ordering::Relaxed).max(active);
    json!({
        "ready":INVENTORY_READY.load(Ordering::Acquire),
        "active":active,
        "total":total,
        "archived":ARCHIVED_REGEX.load(Ordering::Relaxed),
        "compiled_patterns":COMPILED_PATTERNS.load(Ordering::Relaxed),
        "scope":"unique_generated_rule_identities_active_subset_of_total"
    })
}

#[derive(Default)]
pub struct Observer {
    previous:Option<BTreeMap<String,(String,String)>>,
    last_archive:i64,
    last_total:usize,
    last_active:usize,
}

/// Rule identities are semantic, not compiled regex handler instances.
/// A learned TV family yields season + episode regex handlers, but is one
/// generated rule. Source patch suggestions and unverified proposals remain
/// separate historical identities until independently reviewed.
fn active_rule_identities()->BTreeSet<String>{
    let mut keys=BTreeSet::new();
    for (field,signal,_) in crate::field_learning::regexes_for_log(){
        keys.insert(format!("field:{field}:{signal}"));
    }
    for (marker,_) in crate::learned_regex::regexes_for_log(){
        keys.insert(format!("anime:{marker}"));
    }
    if let Some(families)=crate::tv_learning::status()["families"].as_array(){
        for family in families.iter().filter_map(Value::as_str){
            keys.insert(format!("tv:{family}"));
        }
    }
    keys
}

/// Map archive entries onto the same rule identities as the live registry.
/// Distinct GitHub proposal fingerprints are not equal to distinct rules:
/// source revisions or separate season/episode handlers must not double-count.
fn archived_rule_identity(row:&Value,fingerprint:&str)->String{
    let kind=if row["kind"]=="historical_regex_review"{
        row["original_kind"].as_str().unwrap_or("")
    }else{
        row["kind"].as_str().unwrap_or("")
    };
    match kind {
        "learned_field_regex"=>{
            if let (Some(field),Some(signal))=(
                row["field"].as_str(),row["signal"].as_str()){
                return format!("field:{field}:{signal}");
            }
        },
        "verified_regex_proposal"=>{
            if let Some(marker)=row["marker"].as_str(){
                return format!("anime:{marker}");
            }
        },
        "learned_tv_regex"=>{
            if let Some(family)=row["family"].as_str(){
                return format!("tv:{family}");
            }
        },
        _=>{}
    }
    // A new source-regex patch or review-only pattern can be a separate
    // candidate, never an additional live rule until independently activated.
    format!("proposal:{fingerprint}")
}

fn unique_rule_count(db:&Connection,active:&BTreeSet<String>)
    ->Result<usize,String>
{
    let mut keys=active.clone();
    let mut stmt=db.prepare(
        "SELECT fingerprint,payload FROM ml_shared_rule_archive"
    ).map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|row|
        Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))
        .map_err(|e|e.to_string())?;
    for row in rows{
        let (fingerprint,payload)=row.map_err(|e|e.to_string())?;
        let key=serde_json::from_str::<Value>(&payload)
            .ok().map(|v|archived_rule_identity(&v,&fingerprint))
            .unwrap_or_else(||format!("proposal:{fingerprint}"));
        keys.insert(key);
    }
    Ok(keys.len())
}

/// Logs the full active ruleset on worker startup, then only new/removed or
/// changed rules. Counts reflect current SQLite and the in-memory TV overlay.
pub fn after_cycle(db:&Connection,observer:&mut Observer)->Result<(),String>{
    let mut actual=BTreeMap::<String,(String,String)>::new();
    for (field,signal,regex) in crate::field_learning::regexes_for_log(){
        actual.insert(format!("field:{field}:{signal}"),("field".into(),regex));
    }
    for (marker,regex) in crate::learned_regex::regexes_for_log(){
        actual.insert(format!("anime:{marker}"),("anime".into(),regex));
    }
    for addition in crate::tv_learning::handler_additions(){
        actual.insert(format!("tv:{}:{}",addition.source_index,addition.pattern),
            ("tv".into(),addition.pattern));
    }
    let db_field:i64=db.query_row("SELECT COUNT(*) FROM learned_field_rules",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    let db_anime:i64=db.query_row("SELECT COUNT(*) FROM learned_regex_active",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    let tv_families=crate::tv_learning::status()["active_rule_count"]
        .as_i64().unwrap_or(0);
    let archived=crate::community_rule_archive::count(db);
    let active_ids=active_rule_identities();
    let total_rules=unique_rule_count(db,&active_ids)?;
    let first=observer.previous.is_none();
    let prev=observer.previous.as_ref();
    for (key,(kind,pattern)) in &actual {
        if first||prev.and_then(|p|p.get(key))!=Some(&(kind.clone(),pattern.clone())){
            tracing::info!(rule=%key,category=%kind,regex=%pattern,
                "custom ML regex active");
        }
    }
    if let Some(previous)=prev {
        for key in previous.keys(){
            if !actual.contains_key(key) {
                tracing::info!(rule=%key,
                    "custom ML regex retired or superseded; offline archive retained");
            }
        }
    }
    if first || prev.is_some_and(|p|*p!=actual)
        ||observer.last_archive!=archived
        ||observer.last_total!=total_rules
        ||observer.last_active!=active_ids.len(){
        tracing::info!(
            active_custom_rules=db_field+db_anime+tv_families,
            field_rules_in_database=db_field,
            anime_rules_in_database=db_anime,
            tv_rule_families=tv_families,
            archived_custom_rules_in_database=archived,
            compiled_regex_patterns=actual.len(),
            active_unique_ml_rules=active_ids.len(),
            total_unique_ml_rules=total_rules,
            startup=first,
            "custom ML regex inventory");
    }
    TOTAL_RULES.store(total_rules as u64,Ordering::Relaxed);
    ACTIVE_REGEX.store(active_ids.len() as u64,Ordering::Relaxed);
    COMPILED_PATTERNS.store(actual.len() as u64,Ordering::Relaxed);
    ARCHIVED_REGEX.store(archived.max(0) as u64,Ordering::Relaxed);
    INVENTORY_READY.store(true,Ordering::Release);
    observer.previous=Some(actual);
    observer.last_archive=archived;
    observer.last_total=total_rules;
    observer.last_active=active_ids.len();
    Ok(())
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn empty_database_has_zero_rules_and_no_sensitive_history(){
        let db=Connection::open_in_memory().unwrap();
        crate::field_learning::init(&db).unwrap();
        crate::learned_regex::init(&db).unwrap();
        crate::community_rule_archive::init(&db).unwrap();
        let mut observer=Observer::default();
        after_cycle(&db,&mut observer).unwrap();
        assert_eq!(observer.last_archive,0);
        assert!(observer.previous.is_some());
        let counts=status();
        assert_eq!(counts["ready"],true);
        assert_eq!(counts["active"],0);
        assert_eq!(counts["archived"],0);
        assert_eq!(counts["total"],0);
        assert_eq!(counts["compiled_patterns"],0);
    }

    #[test]
    fn tv_family_counts_as_one_generated_rule_not_two_patterns(){
        let db=Connection::open_in_memory().unwrap();
        crate::community_rule_archive::init(&db).unwrap();
        let record=json!({"kind":"learned_tv_regex","family":"sxe"});
        db.execute("INSERT INTO ml_shared_rule_archive
            (fingerprint,payload,source_build,first_seen,last_seen)
            VALUES('test-tv',?1,'build',1,1)",
            [record.to_string()]).unwrap();
        let active=BTreeSet::from(["tv:sxe".to_owned()]);
        assert_eq!(unique_rule_count(&db,&active).unwrap(),1);
        // The runtime emits both season and episode handler regexes.
        let compiled_patterns=2;
        assert!(compiled_patterns>active.len());
    }

    #[test]
    fn historical_versions_and_current_rules_share_identity(){
        let db=Connection::open_in_memory().unwrap();
        crate::community_rule_archive::init(&db).unwrap();
        let old=json!({"kind":"learned_field_regex",
            "field":"codec","signal":"x266"});
        let historical=json!({"kind":"historical_regex_review",
            "original_kind":"learned_field_regex",
            "field":"codec","signal":"x266"});
        for (fingerprint,payload) in [("old",old),("new",historical)]{
            db.execute("INSERT INTO ml_shared_rule_archive
                (fingerprint,payload,source_build,first_seen,last_seen)
                VALUES(?1,?2,'build',1,1)",
                rusqlite::params![fingerprint,payload.to_string()]).unwrap();
        }
        let active=BTreeSet::from(["field:codec:x266".to_owned(),
            "anime:epis".to_owned()]);
        // One historically archived rule + one new active rule is two total,
        // not (two archive fingerprints + two active registry rows).
        assert_eq!(unique_rule_count(&db,&active).unwrap(),2);
    }

    #[test]
    fn review_only_regex_proposals_are_not_active_rules(){
        let db=Connection::open_in_memory().unwrap();
        crate::community_rule_archive::init(&db).unwrap();
        db.execute("INSERT INTO ml_shared_rule_archive
            (fingerprint,payload,source_build,first_seen,last_seen)
            VALUES('review',?1,'build',1,1)",
            [json!({"kind":"learned_field_regex_review",
                "field":"codec","signal":"unknown","review_only":true}).to_string()])
            .unwrap();
        let active=BTreeSet::new();
        assert_eq!(unique_rule_count(&db,&active).unwrap(),1);
        assert_eq!(active.len(),0);
    }
}
