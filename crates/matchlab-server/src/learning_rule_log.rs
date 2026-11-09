//! Operator-visible inventory of *active* custom parser regex and the durable
//! offline archive. No torrent filenames, training samples, or IMDb IDs logged.
//! This runs only in the learning worker, never on the matching hot path.
use rusqlite::Connection;
use serde_json::{json,Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool,AtomicU64,Ordering};

static ACTIVE_REGEX:AtomicU64=AtomicU64::new(0);
static ARCHIVED_REGEX:AtomicU64=AtomicU64::new(0);
static INVENTORY_READY:AtomicBool=AtomicBool::new(false);

/// Lock-free, privacy-safe dashboard snapshot. The learner updates these
/// counters after a successful DB scan; HTTP requests never open SQLite.
pub fn status()->Value{
    json!({
        "ready":INVENTORY_READY.load(Ordering::Acquire),
        "active":ACTIVE_REGEX.load(Ordering::Relaxed),
        "archived":ARCHIVED_REGEX.load(Ordering::Relaxed),
        "scope":"active_compiled_custom_regex_and_unique_historical_archive"
    })
}

#[derive(Default)]
pub struct Observer {
    previous:Option<BTreeMap<String,(String,String)>>,
    last_archive:i64,
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
        ||observer.last_archive!=archived {
        tracing::info!(
            active_custom_rules=db_field+db_anime+tv_families,
            field_rules_in_database=db_field,
            anime_rules_in_database=db_anime,
            tv_rule_families=tv_families,
            archived_custom_rules_in_database=archived,
            compiled_regex_patterns=actual.len(),
            startup=first,
            "custom ML regex inventory");
    }
    ACTIVE_REGEX.store(actual.len() as u64,Ordering::Relaxed);
    ARCHIVED_REGEX.store(archived.max(0) as u64,Ordering::Relaxed);
    INVENTORY_READY.store(true,Ordering::Release);
    observer.previous=Some(actual);
    observer.last_archive=archived;
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
    }
}
