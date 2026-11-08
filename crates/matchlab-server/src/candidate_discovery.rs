//! Offline discovery of unfamiliar release markers, completely isolated
//! from regex activation and matching. Scores are *uncalibrated review ranks*.
//! Only opt-in local SQLite contains literal unknown tags; the HTTP status
//! reports aggregate counts, never filenames, markers, hashes, or identities.
use rusqlite::{params,Connection,OptionalExtension};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::collections::BTreeMap;
use std::sync::{OnceLock,RwLock};

const MAX_CANDIDATES:i64=256;
const MAX_EXAMPLES:i64=12_000;
const MAX_RAW_BYTES:usize=240;
const MAX_TOKENS:usize=12;
const MIN_RANK:f32=0.50;
static STATUS:OnceLock<RwLock<Value>>=OnceLock::new();
pub struct Budget{candidates:i64,examples:i64,evictions:u8}
fn hash(raw:&str)->String{format!("{:x}",Sha256::digest(raw.as_bytes()))}
fn valid_name(raw:&str)->bool{
    raw.len()<=MAX_RAW_BYTES && !raw.contains(['/', '\\', '@', '\n','\r','\0'])
        && !["token=","apikey=","password=","secret=","magnet:","http:"]
            .iter().any(|part|raw.to_ascii_lowercase().contains(part))
}
fn is_technical_anchor(word:&str)->bool{
    matches!(word,"1080p"|"720p"|"2160p"|"4320p"|"1440p"|"480p"
        |"web"|"webrip"|"webdl"|"bluray"|"hdtv"|"x264"|"x265"
        |"hevc"|"av1"|"aac"|"ac3"|"dts"|"ddp")
}
fn ordinary_token(word:&str)->bool{
    if !(3..=16).contains(&word.len())||!word.bytes().all(|b|b.is_ascii_alphanumeric())
        ||word.bytes().all(|b|b.is_ascii_digit()){return true;}
    if crate::field_audit::supported_pixel_height(word){return true;}
    matches!(word,"web"|"dl"|"webrip"|"webdl"|"bluray"|"bdrip"
        |"hdtv"|"hdrip"|"dvdrip"|"remux"|"proper"|"repack"
        |"amzn"|"netflix"|"nf"|"hulu"|"hdr"|"hdr10"|"dovi"
        |"x264"|"x265"|"h264"|"h265"|"hevc"|"avc"|"av1"
        |"vp9"|"aac"|"ac3"|"eac3"|"ddp"|"dts"|"flac"|"opus"
        |"mkv"|"mp4"|"avi"|"m4v"|"webm"|"m2ts"|"complete"
        |"subbed"|"dubbed"|"multi"|"dual"|"truehd"|"atmos"
        |"1080p"|"720p"|"2160p"|"4320p"|"480p"|"576p"
        |"torrent"|"sample"|"rarbg"|"tgx"|"yts")
}
fn unknown_tokens(title:&str)->Vec<String>{
    if !valid_name(title){return Vec::new();}
    let name=title.rsplit(&['/','\\'][..]).next().unwrap_or(title);
    let tokens:Vec<String>=name.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|part|!part.is_empty()).take(48)
        .map(str::to_ascii_lowercase).collect();
    let Some(anchor)=tokens.iter().position(|t|is_technical_anchor(t))else{
        return Vec::new();
    };
    let mut out=Vec::new();
    for token in tokens.into_iter().skip(anchor+1).take(MAX_TOKENS){
        if !ordinary_token(&token)&&!out.contains(&token){out.push(token);}
    }
    out
}
pub fn init(db:&Connection)->Result<(),String>{
    db.execute_batch("CREATE TABLE IF NOT EXISTS ml_unknown_candidates(
        marker TEXT NOT NULL,predicted_field TEXT NOT NULL,
        rank REAL NOT NULL,distinct_works INTEGER NOT NULL,
        last_seen INTEGER NOT NULL,
        PRIMARY KEY(marker,predicted_field));
      CREATE TABLE IF NOT EXISTS ml_unknown_examples(
        marker TEXT NOT NULL,predicted_field TEXT NOT NULL,
        identity_hash TEXT NOT NULL,
        PRIMARY KEY(marker,predicted_field,identity_hash));
      CREATE INDEX IF NOT EXISTS ml_unknown_rank
        ON ml_unknown_candidates(predicted_field,distinct_works);")
        .map_err(|e|e.to_string())
}
pub fn budget(db:&Connection)->Result<Budget,String>{
    init(db)?;
    let candidates=db.query_row("SELECT COUNT(*) FROM ml_unknown_candidates",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    let examples=db.query_row("SELECT COUNT(*) FROM ml_unknown_examples",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    Ok(Budget{candidates,examples,evictions:0})
}
fn reclaim_unqualified(db:&Connection,budget:&mut Budget)->Result<bool,String>{
    if budget.evictions>=8{return Ok(false);}
    let victim:Option<(String,String,i64)>=db.query_row(
        "SELECT marker,predicted_field,distinct_works FROM ml_unknown_candidates
         WHERE distinct_works<4 ORDER BY last_seen,distinct_works,marker LIMIT 1",[],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
    ).optional().map_err(|e|e.to_string())?;
    let Some((marker,field,_works))=victim else{return Ok(false);};
    let removed=db.execute("DELETE FROM ml_unknown_examples
        WHERE marker=?1 AND predicted_field=?2",params![marker,field])
        .map_err(|e|e.to_string())? as i64;
    db.execute("DELETE FROM ml_unknown_candidates
        WHERE marker=?1 AND predicted_field=?2",params![marker,field])
        .map_err(|e|e.to_string())?;
    budget.examples=budget.examples.saturating_sub(removed);
    budget.candidates=budget.candidates.saturating_sub(1);
    budget.evictions+=1;
    Ok(true)
}
pub fn observe(db:&Connection,title:&str,parsed:&Value,
    model:&crate::online_model::OnlineModel,remaining:&mut Budget)->Result<(),String>{
    let name=parsed["title"].as_str().unwrap_or("").to_ascii_lowercase();
    if !(4..=120).contains(&name.len()) {return Ok(());}
    let identity=hash(&name);
    for marker in unknown_tokens(title){
        if remaining.examples>=MAX_EXAMPLES.min(
            crate::learning_cache::training_max_examples())
            && !reclaim_unqualified(db,remaining)?{break;}
        // Learning predictions are only for review prioritization. The
        // actual value and regex are never used in a parser or matcher.
        let mut ranked:Option<(&str,f32)>=None;
        for field in ["resolution","codec","quality","audio","hdr"]{
            if let Some(score)=model.score(field,&marker,title)
                && score>=MIN_RANK
                && ranked.is_none_or(|(_,best)|score>best){
                ranked=Some((field,score));
            }
        }
        let Some((field,score))=ranked else {continue;};
        let known:i64=db.query_row(
            "SELECT EXISTS(SELECT 1 FROM ml_unknown_candidates
             WHERE marker=?1 AND predicted_field=?2)",
            params![marker,field],|r|r.get(0)).map_err(|e|e.to_string())?;
        if known==0&&remaining.candidates>=MAX_CANDIDATES
            && !reclaim_unqualified(db,remaining)?{continue;}
        let inserted=db.execute(
            "INSERT OR IGNORE INTO ml_unknown_examples(marker,predicted_field,identity_hash)
             VALUES(?1,?2,?3)",params![marker,field,identity])
            .map_err(|e|e.to_string())?;
        if inserted==0{continue;}
        remaining.examples+=1;
        db.execute("INSERT INTO ml_unknown_candidates(
            marker,predicted_field,rank,distinct_works,last_seen)
            VALUES(?1,?2,?3,1,strftime('%s','now'))
            ON CONFLICT(marker,predicted_field) DO UPDATE SET
              rank=excluded.rank,distinct_works=distinct_works+1,
              last_seen=excluded.last_seen",
            params![marker,field,score]).map_err(|e|e.to_string())?;
        if known==0{remaining.candidates+=1;}
    }
    Ok(())
}
pub fn publish(db:&Connection)->Result<(),String>{
    init(db)?;
    let mut stmt=db.prepare(
        "SELECT predicted_field,COUNT(*) FROM ml_unknown_candidates
         WHERE distinct_works>=4 GROUP BY predicted_field"
    ).map_err(|e|e.to_string())?;
    let mut by_field=BTreeMap::<String,i64>::new();
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))
        .map_err(|e|e.to_string())?;
    for row in rows{
        let (field,n)=row.map_err(|e|e.to_string())?;
        by_field.insert(field,n);
    }
    let pending: i64=by_field.values().sum();
    let report=json!({"review_only":true,"unknown_candidate_groups":pending,
        "review_candidates_by_field":by_field,
        "candidate_rule_activation":false,
        "raw_markers_never_in_status_or_community_export":true});
    let slot=STATUS.get_or_init(||RwLock::new(Value::Null));
    if let Ok(mut guard)=slot.write(){*guard=report;}
    Ok(())
}
pub fn status()->Value{
    STATUS.get().and_then(|slot|slot.read().ok().map(|v|v.clone()))
        .unwrap_or_else(||json!({"review_only":true,
            "unknown_candidate_groups":0,"candidate_rule_activation":false}))
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn discovers_only_trailing_unrecognized_tokens(){
        assert_eq!(unknown_tokens("Movie.2160p.X266.WEB-DL"),
            vec!["x266".to_owned()]);
        assert!(unknown_tokens("Movie.Custom.Marker").is_empty());
        assert!(unknown_tokens("The.Danish.Girl.1080p").is_empty());
        assert!(unknown_tokens("Film.1080p.x265.WEB-DL").is_empty());
    }
    #[test]
    fn bounded_review_store_reclaims_noise_but_protects_diverse_examples(){
        let db=Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        db.execute("INSERT INTO ml_unknown_candidates VALUES(
            'noise','codec',0.6,1,1)",[]).unwrap();
        db.execute("INSERT INTO ml_unknown_examples VALUES(
            'noise','codec','work1')",[]).unwrap();
        let mut usage=budget(&db).unwrap();
        assert!(reclaim_unqualified(&db,&mut usage).unwrap());
        assert_eq!(usage.candidates,0);
        assert_eq!(usage.examples,0);
        db.execute("INSERT INTO ml_unknown_candidates VALUES(
            'diverse','codec',0.8,8,1)",[]).unwrap();
        assert!(!reclaim_unqualified(&db,&mut usage).unwrap());
    }
    #[test]
    fn never_promotes_raw_unverified_strings_into_active_regex(){
        let db=Connection::open_in_memory().unwrap();
        let mut budget=budget(&db).unwrap();
        let model=crate::online_model::OnlineModel::new();
        observe(&db,"Film.2160p.X266",&json!({"title":"Film"}),
            &model,&mut budget).unwrap();
        assert_eq!(budget.candidates,0,"untrained scores must abstain");
        publish(&db).unwrap();
        assert_eq!(status()["candidate_rule_activation"],false);
        assert!(status().get("marker").is_none());
    }
}
