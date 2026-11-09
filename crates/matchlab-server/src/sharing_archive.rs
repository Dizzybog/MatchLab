//! Offline-first, durable journal of locally validated regex proposals.
//! It is independent of GitHub sharing and does not treat learned parse output
//! as ground truth; inactive rules remain in the archive for human review.
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::Value;
use std::path::Path;

const MAX_PROPOSAL_BYTES:usize=12_000;
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch("CREATE TABLE IF NOT EXISTS community_rule_archive(
        fingerprint TEXT PRIMARY KEY,
        source_build TEXT NOT NULL,
        payload TEXT NOT NULL,
        first_seen INTEGER NOT NULL DEFAULT (strftime('%s','now')),
        last_seen INTEGER NOT NULL DEFAULT (strftime('%s','now'))
    );
    CREATE INDEX IF NOT EXISTS community_rule_archive_order
        ON community_rule_archive(first_seen,fingerprint);")
        .map_err(|e|e.to_string())
}
fn valid_proposal(row:&Value)->bool{
    let Some(fp)=row["fingerprint"].as_str()else{return false;};
    let Some(source)=row["source_build"].as_str()else{return false;};
    let Some(kind)=row["kind"].as_str()else{return false;};
    let Some((sha,epoch))=source.rsplit_once(':')else{return false;};
    fp.len()==64 && fp.bytes().all(|b|b.is_ascii_hexdigit())
        && sha.len()==40 && sha.bytes().all(|b|b.is_ascii_hexdigit())
        && epoch.parse::<u32>().is_ok()
        && matches!(kind,"verified_regex_proposal"|"learned_field_regex"
            |"learned_tv_regex"|"learned_source_regex_patch"
            |"learned_field_regex_review")
}
pub fn ingest(db:&Connection,proposals:&[Value])->Result<usize,String>{
    init(db)?;
    let mut added=0;
    for proposal in proposals{
        if !valid_proposal(proposal){continue;}
        let payload=serde_json::to_string(proposal).map_err(|e|e.to_string())?;
        if payload.len()>MAX_PROPOSAL_BYTES{continue;}
        let fp=proposal["fingerprint"].as_str().unwrap_or_default();
        let source=proposal["source_build"].as_str().unwrap_or_default();
        let previous:Option<(String,String)>=db.query_row(
            "SELECT source_build,payload FROM community_rule_archive
             WHERE fingerprint=?1",[fp],|row|Ok((row.get(0)?,row.get(1)?)))
            .optional().map_err(|e|e.to_string())?;
        match previous{
            None=>{
                added+=db.execute("INSERT OR IGNORE INTO community_rule_archive(
                     fingerprint,source_build,payload) VALUES(?1,?2,?3)",
                    params![fp,source,payload]).map_err(|e|e.to_string())?;
            },
            Some((old_source,old_payload))=>{
                // Later image revisions cannot overwrite historical source
                // evidence. Within a revision, retain the strongest proof.
                let old:Value=serde_json::from_str(&old_payload).unwrap_or(Value::Null);
                let stronger=old_source==source
                    && proposal["evidence_count"].as_i64().unwrap_or(0)
                       >old["evidence_count"].as_i64().unwrap_or(0);
                if stronger {
                    db.execute("UPDATE community_rule_archive
                        SET payload=?2,last_seen=strftime('%s','now')
                        WHERE fingerprint=?1",params![fp,payload])
                        .map_err(|e|e.to_string())?;
                }
            }
        }
    }
    Ok(added)
}
/// Archive regardless of MATCHLAB_COMMUNITY_LEARNING_ENABLED. This executes
/// after independent canonical parsing and the regex safety/mapping gates.
pub fn snapshot(db:&Connection,metadata:&Path)->Result<usize,String>{
    let mut proposals=crate::field_learning::community_proposals(db)?;
    proposals.extend(crate::tv_learning::community_proposals(db)?);
    proposals.extend(crate::candidate_discovery::community_proposals(db)?);
    if let Ok(meta)=Connection::open_with_flags(
        metadata,OpenFlags::SQLITE_OPEN_READ_ONLY){
        proposals.extend(crate::learned_regex::proposals(db,&meta)?);
    }
    ingest(db,&proposals)
}
/// Oldest snapshots first, even if they are no longer active locally.
pub fn load(db:&Connection)->Result<Vec<Value>,String>{
    let mut stmt=db.prepare("SELECT payload FROM community_rule_archive
        ORDER BY first_seen,fingerprint").map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|row|row.get::<_,String>(0))
        .map_err(|e|e.to_string())?;
    let mut out=Vec::new();
    for row in rows{
        let raw=row.map_err(|e|e.to_string())?;
        if let Ok(value)=serde_json::from_str::<Value>(&raw)
            && valid_proposal(&value){out.push(value);}
    }
    Ok(out)
}
pub fn stats(db:&Connection)->Result<i64,String>{
    init(db)?;
    db.query_row("SELECT COUNT(*) FROM community_rule_archive",[],|r|r.get(0))
        .map_err(|e|e.to_string())
}
#[cfg(test)]
mod tests{
    use super::*;
    use serde_json::json;
    fn sample(fp:&str,source:&str,n:i64)->Value{
        json!({"kind":"learned_field_regex","fingerprint":fp,
            "source_build":source,"evidence_count":n,
            "field":"codec","signal":"x266","regex":"safe"})
    }
    #[test]
    fn offline_archive_survives_rule_retirement_and_image_upgrades(){
        let db=Connection::open_in_memory().unwrap();
        let fp="a".repeat(64);
        let old=format!("{}:5","1".repeat(40));
        assert_eq!(ingest(&db,&[sample(&fp,&old,12)]).unwrap(),1);
        assert_eq!(ingest(&db,&[sample(&fp,&old,20)]).unwrap(),0);
        assert_eq!(load(&db).unwrap()[0]["evidence_count"],20);
        // Reconciled rules can disappear from the active runtime without
        // deleting or rewriting the archived rule.
        assert_eq!(ingest(&db,&[]).unwrap(),0);
        assert_eq!(ingest(&db,&[sample(&fp,&format!("{}:5","2".repeat(40)),99)]).unwrap(),0);
        let history=load(&db).unwrap();
        assert_eq!(history.len(),1);
        assert_eq!(history[0]["source_build"],old);
        assert_eq!(history[0]["evidence_count"],20);
    }
    #[test]
    fn archived_regexes_survive_without_any_github_opt_in(){
        let db=Connection::open_in_memory().unwrap();
        assert_eq!(ingest(&db,&[sample(&"b".repeat(64),
            &format!("{}:5","1".repeat(40)),12)]).unwrap(),1);
        assert_eq!(stats(&db).unwrap(),1);
    }
}
