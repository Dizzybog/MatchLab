//! Indexed IMDb/AniDB alias discovery; independent of ranked matching.
//! Public endpoint: GET or POST /alias with one of imdb_id, anidb_id, query, q.
use crate::{AppState, matchlab_matcher, open_db, tantivy_search};
use axum::{extract::{Query, State}, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json,Value};
use std::collections::BTreeSet;

const DEFAULT_LIMIT:usize=500;
const MAX_LIMIT:usize=2_000;
const MAX_OFFSET:usize=1_000_000;

#[derive(Debug,Deserialize,Default)]
pub struct AliasRequest {
    imdb_id:Option<String>,
    anidb_id:Option<String>,
    tvdb_id:Option<String>,
    mal_id:Option<String>,
    anilist_id:Option<String>,
    kitsu_id:Option<String>,
    // Future IDs imported into anime_ids can use id_type + id without an
    // application update, e.g. {"id_type":"kitsu","id":"123"}.
    id_type:Option<String>,
    id:Option<String>,
    query:Option<String>,
    q:Option<String>,
    limit:Option<usize>,
    offset:Option<usize>,
}

#[derive(Debug,Clone,PartialEq,Eq)]
enum Mode { Imdb, Anidb, Title, External(String) }
impl Mode {
    fn label(&self)->String{
        match self{
            Self::Imdb=>"imdb_id".into(),
            Self::Anidb=>"anidb_id".into(),
            Self::Title=>"title".into(),
            Self::External(kind)=>format!("{kind}_id"),
        }
    }
}
#[derive(Debug)]
struct Validated {
    mode:Mode,
    input:String,
    key:String,
    limit:usize,
    offset:usize,
}
fn positive_id(value:&str)->bool{
    value.len()<=20 && !value.is_empty() &&
        value.bytes().all(|v|v.is_ascii_digit()) &&
        value.parse::<u64>().is_ok_and(|n|n>0)
}
fn external_kind(value:&str)->Result<String,String>{
    let lower=value.trim().to_ascii_lowercase();
    let kind=match lower.as_str(){
        "thetvdb"=>"tvdb","myanimelist"=>"mal",other=>other,
    };
    if kind.is_empty()||kind.len()>32||
        !kind.bytes().all(|v|v.is_ascii_lowercase()||
            v.is_ascii_digit()||v==b'_'){
        return Err("id_type must contain 1–32 lowercase ASCII letters, digits or underscores".into());
    }
    Ok(kind.to_owned())
}
fn detect_q(raw:&str)->Mode{
    if crate::metadata_import::valid_imdb_id(raw){return Mode::Imdb;}
    if positive_id(raw){return Mode::Anidb;}
    if let Some((kind,_))=raw.split_once(':'){
        return match kind.to_ascii_lowercase().as_str(){
            "anidb"=>Mode::Anidb,
            "imdb"=>Mode::Imdb,
            "tvdb"|"thetvdb"=>Mode::External("tvdb".into()),
            "mal"|"myanimelist"=>Mode::External("mal".into()),
            "kitsu"=>Mode::External("kitsu".into()),
            "anilist"=>Mode::External("anilist".into()),
            _=>Mode::Title,
        };
    }
    Mode::Title
}
fn validated(request:AliasRequest)->Result<Validated,String>{
    let typed_pair=if request.id_type.is_some()||request.id.is_some(){
        let kind=request.id_type.as_deref()
            .ok_or("id_type and id must be provided together")?;
        let value=request.id.as_deref()
            .ok_or("id_type and id must be provided together")?;
        let kind=external_kind(kind)?;
        let mode=match kind.as_str(){
            "imdb"=>Mode::Imdb,
            "anidb"=>Mode::Anidb,
            _=>Mode::External(kind),
        };
        Some((mode,value.to_owned()))
    }else{None};
    let inputs=[
        request.imdb_id.map(|v|(Some(Mode::Imdb),v)),
        request.anidb_id.map(|v|(Some(Mode::Anidb),v)),
        request.tvdb_id.map(|v|(Some(Mode::External("tvdb".into())),v)),
        request.mal_id.map(|v|(Some(Mode::External("mal".into())),v)),
        request.anilist_id.map(|v|(Some(Mode::External("anilist".into())),v)),
        request.kitsu_id.map(|v|(Some(Mode::External("kitsu".into())),v)),
        typed_pair.map(|(m,v)|(Some(m),v)),
        request.query.map(|v|(Some(Mode::Title),v)),
        request.q.map(|v|(None,v)),
    ].into_iter().flatten().collect::<Vec<_>>();
    if inputs.len()!=1{
        return Err("supply exactly one ID, id_type+id, query or q".into());
    }
    let (explicit,raw)=inputs.into_iter().next().expect("exactly one input");
    let raw=raw.trim();
    if raw.is_empty()||raw.len()>512{
        return Err("alias input must contain 1–512 UTF-8 bytes".into());
    }
    let mode=explicit.clone().unwrap_or_else(||detect_q(raw));
    let payload=if explicit.is_none() {
        // Only q= accepts a type prefix. Explicit fields keep their value.
        match raw.split_once(':'){
            Some((kind,value)) if detect_q(raw)!=Mode::Title &&
                external_kind(kind).is_ok()=>value,
            _=>raw,
        }
    }else{raw};
    let key=match &mode{
        Mode::Imdb=>{
            if !crate::metadata_import::valid_imdb_id(payload){
                return Err("invalid IMDb ID; expected tt followed by digits".into());
            }
            payload.to_owned()
        },
        Mode::Anidb=>{
            if !positive_id(payload){return Err("invalid positive AniDB ID".into());}
            payload.parse::<u64>().map_err(|e|e.to_string())?.to_string()
        },
        Mode::External(_)=>{
            // Current animeApi IDs are positive integers. Generic metadata
            // IDs may contain ASCII letters, hyphens and underscores.
            if payload.len()>128||!payload.bytes().all(|b|
                b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_'|b'.')){
                return Err("invalid external identity".into());
            }
            if positive_id(payload){
                payload.parse::<u64>().map_err(|e|e.to_string())?.to_string()
            }else{payload.to_owned()}
        },
        Mode::Title=>{
            let clean=matchlab_matcher::clean_title(payload);
            if clean.is_empty()||clean.len()>512{
                return Err("title must contain a normalizable letter or digit".into());
            }
            clean
        },
    };
    let limit=request.limit.unwrap_or(DEFAULT_LIMIT);
    let offset=request.offset.unwrap_or(0);
    if limit==0||limit>MAX_LIMIT{
        return Err(format!("limit must be between 1 and {MAX_LIMIT}"));
    }
    if offset>MAX_OFFSET{
        return Err(format!("offset must be <= {MAX_OFFSET}"));
    }
    Ok(Validated{mode,input:raw.to_owned(),key,limit,offset})
}
fn lookup(db:&rusqlite::Connection,request:&Validated)->Result<Value,String>{
    let (aliases,direct_imdb)=match &request.mode{
        Mode::Imdb=>(
            tantivy_search::lookup_aliases(db,
                tantivy_search::AliasLookupKind::ImdbId,&request.key)?,
            BTreeSet::from([request.key.clone()])),
        Mode::Anidb=>(
            tantivy_search::lookup_aliases(db,
                tantivy_search::AliasLookupKind::AnidbId,&request.key)?,
            BTreeSet::new()),
        Mode::Title=>(
            tantivy_search::lookup_aliases(db,
                tantivy_search::AliasLookupKind::ExactTitle,&request.key)?,
            BTreeSet::new()),
        Mode::External(kind)=>{
            let ids=tantivy_search::imdb_ids_for_external_id(
                db,kind,&request.key)?;
            (tantivy_search::aliases_for_imdb_ids(db,&ids)?,ids)
        },
    };
    let mut imdb_ids:BTreeSet<_>=aliases.iter()
        .filter_map(|a|a.imdb_id.clone()).collect();
    // A linked identity may lack an IMDb title row. Still report its ID.
    if request.mode==Mode::Imdb && aliases.is_empty(){
        imdb_ids.clear();
    }else{
        imdb_ids.extend(direct_imdb);
    }
    let anidb_ids:BTreeSet<_>=aliases.iter()
        .filter_map(|a|a.anidb_id.clone()).collect();
    // The external IDs are grouped per IMDb work, not attributed to every
    // unrelated AniDB title that happens to share the same IMDb parent.
    let by_imdb=tantivy_search::external_ids_by_imdb(db,&imdb_ids)?;
    let mut ids=std::collections::BTreeMap::<String,BTreeSet<String>>::new();
    ids.insert("imdb".into(),imdb_ids.clone());
    ids.insert("anidb".into(),anidb_ids.clone());
    for type_name in ["tvdb","mal","anilist","kitsu"]{
        ids.entry(type_name.into()).or_default();
    }
    for mappings in by_imdb.values(){
        for (kind,values) in mappings{
            // Text query's anidb_ids remain *exact* alias matches.
            // Other types are IMDb-group linked identities.
            if kind!="anidb"{
                ids.entry(kind.clone()).or_default().extend(values.iter().cloned());
            }
        }
    }
    let get=|kind:&str|ids.get(kind).cloned().unwrap_or_default();
    let total=aliases.len();
    let records=aliases.into_iter().skip(request.offset)
        .take(request.limit).collect::<Vec<_>>();
    let has_more=request.offset.saturating_add(records.len())<total;
    Ok(json!({
        "mode":request.mode.label(),
        "input":request.input,
        "normalized":if request.mode==Mode::Title {Some(request.key.as_str())} else {None},
        "imdb_ids":imdb_ids,
        "anidb_ids":anidb_ids,
        "tvdb_ids":get("tvdb"),
        "mal_ids":get("mal"),
        "anilist_ids":get("anilist"),
        "kitsu_ids":get("kitsu"),
        "ids":ids,
        "linked_ids_by_imdb":by_imdb,
        "aliases":records,
        "total":total,
        "limit":request.limit,
        "offset":request.offset,
        "has_more":has_more
    }))
}

async fn alias_response(state:AppState,request:AliasRequest)->(StatusCode,Json<Value>){
    let request=match validated(request){
        Ok(v)=>v,
        Err(error)=>return (StatusCode::BAD_REQUEST,Json(json!({"error":error}))),
    };
    // Share the bounded CPU-intensive task semaphore with /match. Even
    // short queries may enumerate several thousand exact title aliases.
    let permit=match state.match_slots.clone().acquire_owned().await{
        Ok(v)=>v,
        Err(_)=>return (StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"alias lookup is unavailable"}))),
    };
    let result=tokio::task::spawn_blocking(move||{
        let _permit=permit;
        let db=open_db(&state.db).map_err(|e|e.to_string())?;
        lookup(&db,&request)
    }).await;
    match result{
        Ok(Ok(value))=>(StatusCode::OK,Json(value)),
        Ok(Err(error))=>(StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error}))),
        Err(error)=>(StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()}))),
    }
}

pub async fn get_alias(State(state):State<AppState>,Query(request):Query<AliasRequest>)
    ->(StatusCode,Json<Value>){alias_response(state,request).await}
pub async fn post_alias(State(state):State<AppState>,Json(request):Json<AliasRequest>)
    ->(StatusCode,Json<Value>){alias_response(state,request).await}

#[cfg(test)]
mod tests{
    use super::*;
    use rusqlite::Connection;
    use std::time::{SystemTime,UNIX_EPOCH};

    #[test]
    fn validates_modes_and_rejects_ambiguous_or_unsafe_input(){
        let parse=|text:&str|serde_json::from_str::<AliasRequest>(text).unwrap();
        let by_imdb=validated(parse(r#"{"q":"tt0388629"}"#)).unwrap();
        assert_eq!(by_imdb.mode,Mode::Imdb);
        let by_anidb=validated(parse(r#"{"q":"00069"}"#)).unwrap();
        assert_eq!(by_anidb.mode,Mode::Anidb);
        assert_eq!(by_anidb.key,"69");
        let by_name=validated(parse(r#"{"query":"One.Piece"}"#)).unwrap();
        assert_eq!(by_name.mode,Mode::Title);
        assert_eq!(by_name.key,"onepiece");
        assert!(validated(parse(r#"{"imdb_id":"tt0388629","query":"One Piece"}"#)).is_err());
        assert!(validated(parse(r#"{"imdb_id":"tt123,tt456"}"#)).is_err());
        assert!(validated(parse(r#"{"anidb_id":"69:4"}"#)).is_err());
        assert!(validated(parse(r#"{"query":"---"}"#)).is_err());
        assert!(validated(parse(r#"{"query":"test","limit":2001}"#)).is_err());
        assert!(validated(parse(r#"{"q":"One Piece","offset":1000001}"#)).is_err());
        assert!(validated(parse(r#"{"id_type":"kitsu"}"#)).is_err());
        assert!(validated(parse(r#"{"id":"987"}"#)).is_err());
        assert!(validated(parse(r#"{"kitsu_id":"987","query":"One Piece"}"#)).is_err());
        assert!(validated(parse(r#"{"kitsu_id":"987!"}"#)).is_err());
    }

    #[test]
    fn all_alias_modes_use_tantivy_include_unlinked_anidb_and_preserve_collisions(){
        let stamp=SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!(
            "matchlab-alias-{stamp}-{}.sqlite3",std::process::id()));
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(
            anidb_id TEXT,title TEXT,clean_title TEXT,title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(
            imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
              ('tt0388629','One Piece','onepiece',1999,'series','',1,0,'tvSeries'),
              ('tt0388629','Wan Pisu','wanpisu',1999,'series','',0,1,'tvSeries'),
              ('tt0997084','One Piece','onepiece',2000,'movie','',1,0,'movie'),
              ('tt1234567','One Piece Extended','onepieceextended',2020,'movie','',1,0,'movie');
            INSERT INTO anime_series_map VALUES
              ('tt0388629','69','81797',0),
              ('tt0388629','69','81797',1),
              ('tt0997084','1253','','0');
            INSERT INTO anime_ids VALUES
              ('tt0388629','kitsu','987'),
              ('tt0388629','anilist','101'),
              ('tt0388629','mal','21'),
              ('tt0388629','tvdb','81797'),
              ('tt0997084','kitsu','987'),
              ('tt0997084','mal','522');
            INSERT INTO anidb_titles VALUES
              ('69','One Piece','onepiece','main','en'),
              ('69','Wan Pīsu','wanpisu','official','ja'),
              ('1253','One Piece','onepiece','main','en'),
              ('777','Orphan Title','orphantitle','main','en');").unwrap();
        tantivy_search::warmup(&db).unwrap();
        let query=|json:&str|{
            let req=validated(serde_json::from_str(json).unwrap()).unwrap();
            lookup(&db,&req).unwrap()
        };
        let text=query(r#"{"query":"ONE.PIECE"}"#);
        assert_eq!(text["normalized"],"onepiece");
        assert_eq!(text["imdb_ids"],json!(["tt0388629","tt0997084"]));
        assert_eq!(text["anidb_ids"],json!(["1253","69"]));
        assert_eq!(text["total"],4); // Two IMDb names and two AniDB names
        assert_eq!(text["has_more"],false);
        assert_eq!(text["kitsu_ids"],json!(["987"]));
        assert_eq!(text["anilist_ids"],json!(["101"]));
        assert_eq!(text["mal_ids"],json!(["21","522"]));
        assert_eq!(text["tvdb_ids"],json!(["81797"]));
        assert_eq!(text["linked_ids_by_imdb"]["tt0388629"]["kitsu"],
            json!(["987"]));
        assert_eq!(text["linked_ids_by_imdb"]["tt0997084"]["mal"],
            json!(["522"]));
        let imdb=query(r#"{"imdb_id":"tt0388629"}"#);
        assert_eq!(imdb["imdb_ids"],json!(["tt0388629"]));
        assert_eq!(imdb["anidb_ids"],json!(["69"]));
        assert_eq!(imdb["total"],4); // Two IMDb and two AniDB names
        assert_eq!(imdb["kitsu_ids"],json!(["987"]));
        assert_eq!(imdb["tvdb_ids"],json!(["81797"]));
        let anidb=query(r#"{"anidb_id":"69"}"#);
        assert_eq!(anidb["total"],2);
        assert_eq!(anidb["imdb_ids"],json!(["tt0388629"]));
        let unlinked=query(r#"{"anidb_id":"777"}"#);
        assert_eq!(unlinked["imdb_ids"],json!([]));
        assert_eq!(unlinked["anidb_ids"],json!(["777"]));
        assert_eq!(unlinked["aliases"][0]["name"],"Orphan Title");
        let unlinked_name=query(r#"{"q":"orphan title"}"#);
        assert_eq!(unlinked_name["anidb_ids"],json!(["777"]));
        let kitsu=query(r#"{"kitsu_id":"987"}"#);
        assert_eq!(kitsu["imdb_ids"],json!(["tt0388629","tt0997084"]));
        assert_eq!(kitsu["total"],6);
        assert_eq!(kitsu["mode"],"kitsu_id");
        let kitsu_auto=query(r#"{"q":"kitsu:987"}"#);
        assert_eq!(kitsu_auto["imdb_ids"],kitsu["imdb_ids"]);
        let mal=query(r#"{"mal_id":"21"}"#);
        assert_eq!(mal["imdb_ids"],json!(["tt0388629"]));
        let anilist=query(r#"{"anilist_id":"101"}"#);
        assert_eq!(anilist["imdb_ids"],json!(["tt0388629"]));
        let tvdb=query(r#"{"tvdb_id":"81797"}"#);
        assert_eq!(tvdb["imdb_ids"],json!(["tt0388629"]));
        let typed=query(r#"{"id_type":"kitsu","id":"987"}"#);
        assert_eq!(typed["imdb_ids"],kitsu["imdb_ids"]);
        let paginated=query(r#"{"query":"One Piece","limit":1,"offset":1}"#);
        assert_eq!(paginated["total"],4);
        assert_eq!(paginated["aliases"].as_array().unwrap().len(),1);
        assert_eq!(paginated["imdb_ids"],json!(["tt0388629","tt0997084"]));
        assert_eq!(paginated["has_more"],true);
        let missing=query(r#"{"q":"No Such Alias"}"#);
        assert_eq!(missing["total"],0);
        assert_eq!(missing["aliases"],json!([]));
        drop(db);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }
}
