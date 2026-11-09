//! Persistent catalog of learned regex proposals, independent of GitHub opt-in.
//! Source-build history is evidence for review, never permission to execute.
use rusqlite::{params,Connection};
use serde_json::{json,Value};
use std::collections::{BTreeMap,BTreeSet};

const MAX_ARCHIVE:i64=20_000;
const MAX_PAYLOAD:usize=16_000;

pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch("CREATE TABLE IF NOT EXISTS ml_shared_rule_archive(
        fingerprint TEXT PRIMARY KEY,
        payload TEXT NOT NULL,
        source_build TEXT NOT NULL,
        first_seen INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
    ); CREATE INDEX IF NOT EXISTS ml_shared_rule_archive_seen
        ON ml_shared_rule_archive(first_seen,fingerprint);")
        .map_err(|e|e.to_string())
}
fn current(db:&Connection,meta:Option<&Connection>)->Vec<Value>{
    let mut rules=crate::field_learning::community_proposals(db)
        .unwrap_or_default();
    rules.extend(crate::tv_learning::community_proposals(db)
        .unwrap_or_default());
    rules.extend(crate::candidate_discovery::community_proposals(db)
        .unwrap_or_default());
    if let Some(meta)=meta {
        rules.extend(crate::learned_regex::proposals(db,meta)
            .unwrap_or_default());
    }
    rules
}
fn historical(payload:&Value)->Option<Value>{
    let fingerprint=payload["fingerprint"].as_str()?;
    let source_build=payload["source_build"].as_str()?;
    let kind=payload["kind"].as_str()?;
    let pattern=payload["proposed_regex"].as_str()
        .or_else(||payload["regex"].as_str())?;
    if !(1..=1024).contains(&pattern.len()){return None;}
    let field=payload["field"].as_str().unwrap_or_else(||{
        if kind=="learned_tv_regex" {"seasons"} else {"anime_episode"}
    });
    let signal=payload["signal"].as_str()
        .or_else(||payload["marker"].as_str())
        .or_else(||payload["family"].as_str()).unwrap_or("");
    Some(json!({
        "kind":"historical_regex_review",
        "original_kind":kind,"fingerprint":fingerprint,
        "source_build":source_build,
        "field":field,"signal":signal,"regex":pattern,
        "evidence_count":payload["evidence_count"].as_i64().unwrap_or(0),
        "distinct_titles":payload["distinct_titles"].as_i64()
            .or_else(||payload["distinct_imdb"].as_i64()).unwrap_or(0),
        "review_only":true,"canonical_confirmations":0,
        "verification":"historical_archive_requires_independent_review"
    }))
}
fn archived_rules(db:&Connection)->Result<Vec<Value>,String>{
    let exists:i64=db.query_row("SELECT count(*) FROM sqlite_master
        WHERE type='table' AND name='ml_shared_rule_archive'",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    if exists==0{return Ok(Vec::new());}
    let mut stmt=db.prepare("SELECT payload FROM ml_shared_rule_archive
        ORDER BY first_seen,fingerprint LIMIT ?1").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([MAX_ARCHIVE],|r|r.get::<_,String>(0))
        .map_err(|e|e.to_string())?;
    rows.map(|r|r.map_err(|e|e.to_string())
            .and_then(|s|serde_json::from_str(&s).map_err(|e|e.to_string())))
        .collect()
}
/// Called once per initial worker connection BEFORE old active tables are
/// rebuilt for the current source revision. Older stored regex still become
/// review-only when exported; they never skip current-source proof.
pub fn snapshot_legacy(db:&Connection)->Result<usize,String>{
    init(db)?;
    let mut rules=crate::field_learning::persisted_rule_archive(db)?;
    rules.extend(crate::tv_learning::persisted_rule_archive(db)?);
    rules.extend(crate::learned_regex::archive_proposals(db)?);
    let mut added=0;
    for rule in rules {
        let Some(fp)=rule["fingerprint"].as_str()else{continue;};
        let Some(build)=rule["source_build"].as_str()else{continue;};
        if fp.len()!=64 || !fp.bytes().all(|b|b.is_ascii_hexdigit())
            ||build.len()>90 {continue;}
        let payload=rule.to_string();
        if payload.len()>MAX_PAYLOAD {continue;}
        let changed=db.execute("INSERT OR IGNORE INTO ml_shared_rule_archive
            (fingerprint,payload,source_build,first_seen,last_seen)
            VALUES(?1,?2,?3,strftime('%s','now'),strftime('%s','now'))",
            params![fp,payload,build]).map_err(|e|e.to_string())?;
        added+=changed;
    }
    Ok(added)
}

/// Called by the offline learning worker even when sharing is disabled.
/// Never remove historical entries when a rule is retired or a build changes.
pub fn snapshot(db:&Connection,meta:Option<&Connection>)->Result<usize,String>{
    init(db)?;
    let mut rules=current(db,meta);
    rules.extend(crate::field_learning::archive_proposals(db)
        .unwrap_or_default());
    rules.extend(crate::tv_learning::archive_proposals(db)
        .unwrap_or_default());
    rules.extend(crate::learned_regex::archive_proposals(db)
        .unwrap_or_default());
    let mut unique=BTreeMap::<String,Value>::new();
    for rule in rules {
        let Some(fp)=rule["fingerprint"].as_str()else{continue};
        if fp.len()!=64||!fp.bytes().all(|b|b.is_ascii_hexdigit()){continue;}
        let Some(build)=rule["source_build"].as_str()else{continue};
        if build.len()>90 {continue;}
        unique.entry(fp.to_owned()).and_modify(|prior|{
            // Keep the richer, independently verified version when present.
            if rule["verification"]!="archive_active_rule_review_only" {
                *prior=rule.clone();
            }
        }).or_insert(rule);
    }
    let count:i64=db.query_row("SELECT count(*) FROM ml_shared_rule_archive",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    let mut inserted=0;
    for (fp,proposal) in unique {
        let payload=proposal.to_string();
        if payload.len()>MAX_PAYLOAD{continue;}
        let build=proposal["source_build"].as_str().unwrap_or_default();
        let exists:i64=db.query_row("SELECT EXISTS(SELECT 1 FROM
            ml_shared_rule_archive WHERE fingerprint=?1)",[&fp],
            |r|r.get(0)).map_err(|e|e.to_string())?;
        if exists==0 && count+inserted>=MAX_ARCHIVE {
            return Err("local regex archive capacity reached; old rules preserved".into());
        }
        inserted+=i64::from(exists==0);
        db.execute("INSERT INTO ml_shared_rule_archive
            (fingerprint,payload,source_build,first_seen,last_seen)
            VALUES(?1,?2,?3,strftime('%s','now'),strftime('%s','now'))
            ON CONFLICT(fingerprint) DO UPDATE SET
               payload=excluded.payload,source_build=excluded.source_build,
               last_seen=excluded.last_seen",
            params![fp,payload,build]).map_err(|e|e.to_string())?;
    }
    Ok(inserted as usize)
}
/// Current independently checked rules remain normal proposals. Everything
/// else is preserved as REVIEW-ONLY, even across parser and metadata upgrades.
pub fn for_sharing(db:&Connection,meta:Option<&Connection>)->Result<Vec<Value>,String>{
    let live=current(db,meta).into_iter()
        .filter_map(|v|{ let fp=v["fingerprint"].as_str()?.to_owned(); Some((fp,v)) })
        .collect::<BTreeMap<_,_>>();
    let archive=archived_rules(db)?;
    let mut shared=BTreeSet::new();
    let mut out=Vec::new();
    for prior in archive {
        let Some(fp)=prior["fingerprint"].as_str()else{continue};
        if !shared.insert(fp.to_owned()){continue;}
        if let Some(active)=live.get(fp) {out.push(active.clone());}
        else if let Some(review)=historical(&prior) {out.push(review);}
    }
    // Graceful bootstrap if sharing is enabled before the first offline scan.
    for (fp,active) in live {
        if shared.insert(fp){out.push(active);}
    }
    Ok(out)
}
pub fn count(db:&Connection)->i64{
    db.query_row("SELECT count(*) FROM ml_shared_rule_archive",[],
        |r|r.get(0)).unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_history_survives_retirement_and_is_review_only(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        let proposal=json!({"fingerprint":"a".repeat(64),
            "source_build":format!("{}:5","b".repeat(40)),
            "kind":"learned_field_regex","field":"codec","signal":"x266",
            "regex":r"(?i)x266","evidence_count":15,"distinct_titles":6});
        db.execute("INSERT INTO ml_shared_rule_archive VALUES(?1,?2,?3,1,1)",
            params![proposal["fingerprint"].as_str().unwrap(),proposal.to_string(),
                proposal["source_build"].as_str().unwrap()]).unwrap();
        let pending=for_sharing(&db,None).unwrap();
        assert_eq!(pending.len(),1);
        assert_eq!(pending[0]["review_only"],true);
        assert_eq!(pending[0]["fingerprint"],proposal["fingerprint"]);
        assert_eq!(count(&db),1);
    }
}
