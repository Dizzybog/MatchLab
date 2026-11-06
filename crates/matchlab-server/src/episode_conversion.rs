//! Canonical, verified episode crosswalk in both directions.
//! GET/POST /episode/resolve
use crate::{anime_resolver,open_db,AppState};
use axum::{extract::{Query,State},http::StatusCode,Json};
use serde::Deserialize;
use serde_json::{json,Value};

#[derive(Debug,Deserialize)]
pub struct ResolveRequest {
    imdb_id:Option<String>,
    anidb_id:Option<String>,
    season:Option<i64>,
    episode:i64,
}
#[derive(Debug)]
enum Request {
    Imdb {id:String,season:i64,episode:i64},
    Anidb {id:String,episode:i64},
}
fn validated(req:ResolveRequest)->Result<Request,String>{
    if !(1..=99_999).contains(&req.episode){
        return Err("episode must be between 1 and 99999".into());
    }
    match (req.imdb_id,req.anidb_id,req.season){
        (Some(id),None,Some(season))
            if crate::metadata_import::valid_imdb_id(&id) &&
                (1..=9_999).contains(&season)=>
            Ok(Request::Imdb{id,season,episode:req.episode}),
        (None,Some(id),None) if id.len()<=20 &&
            !id.is_empty() && id.bytes().all(|b|b.is_ascii_digit()) &&
            id.parse::<u64>().is_ok_and(|n|n>0)=>
            Ok(Request::Anidb{id:id.parse::<u64>()
                .map_err(|e|e.to_string())?.to_string(),episode:req.episode}),
        _=>Err("provide either imdb_id + season + episode, or anidb_id + episode".into()),
    }
}

fn resolve(db:&rusqlite::Connection,req:Request)->Result<Value,String>{
    let (direction,input,found)=match req{
        Request::Imdb{id,season,episode}=>{
            let result=anime_resolver::resolve_season(db,&id,season,episode)
                .map_err(|e|e.to_string())?;
            ("imdb_to_anidb",json!({
                "imdb_id":id,"season":season,"episode":episode,
            }),result.into_iter().collect::<Vec<_>>())
        },
        Request::Anidb{id,episode}=>{
            let results=anime_resolver::resolve_anidb(db,&id,episode)
                .map_err(|e|e.to_string())?;
            ("anidb_to_imdb",json!({
                "anidb_id":id,"episode":episode,
            }),results)
        }
    };
    // Preserve *all* verified outcomes. For multiple candidates the caller
    // must choose explicitly; there is no arbitrary first-result fallback.
    let multiple=found.len()>1;
    let matches=found.iter().map(|row|json!({
        "imdb_id":row.imdb_id,
        "season":row.season,
        "episode":row.episode,
        "anidb_id":row.anidb_id,
        "anidb_episode":row.anidb_episode,
        "sid":format!("{}:{}:{}",row.imdb_id,row.season,row.episode),
        "asid":format!("{}:{}",row.anidb_id,row.anidb_episode)
    })).collect::<Vec<_>>();
    Ok(json!({
        "direction":direction,"input":input,"matches":matches,
        "count":matches.len(),"found":!matches.is_empty(),"ambiguous":multiple,
        "selected":if matches.len()==1{matches.first().cloned()}else{None}
    }))
}
async fn respond(state:AppState,request:ResolveRequest)->(StatusCode,Json<Value>){
    let req=match validated(request){
        Ok(v)=>v,
        Err(error)=>return (StatusCode::BAD_REQUEST,Json(json!({"error":error}))),
    };
    let permit=match state.match_slots.clone().acquire_owned().await{
        Ok(v)=>v,
        Err(error)=>return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({
            "error":error.to_string()
        }))),
    };
    match tokio::task::spawn_blocking(move||{
        let _permit=permit;
        let db=open_db(&state.db).map_err(|e|e.to_string())?;
        resolve(&db,req)
    }).await{
        Ok(Ok(value))=>(StatusCode::OK,Json(value)),
        Ok(Err(error))=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":error}))),
        Err(error)=>(StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()}))),
    }
}
pub async fn get_resolve(State(state):State<AppState>,Query(req):Query<ResolveRequest>)
    ->(StatusCode,Json<Value>){respond(state,req).await}
pub async fn post_resolve(State(state):State<AppState>,Json(req):Json<ResolveRequest>)
    ->(StatusCode,Json<Value>){respond(state,req).await}

#[cfg(test)]
mod tests{
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn validates_both_id_directions_and_coordinates(){
        let check=|input:&str|validated(serde_json::from_str(input).unwrap());
        assert!(matches!(check(r#"{"imdb_id":"tt0388629","season":21,"episode":1}"#),
            Ok(Request::Imdb{..})));
        assert!(matches!(check(r#"{"anidb_id":"00069","episode":892}"#),
            Ok(Request::Anidb{id,..}) if id=="69"));
        assert!(check(r#"{"imdb_id":"tt0388629","episode":892}"#).is_err());
        assert!(check(r#"{"anidb_id":"69","season":1,"episode":1}"#).is_err());
        assert!(check(r#"{"anidb_id":"69","imdb_id":"tt0388629","episode":4}"#).is_err());
        assert!(check(r#"{"anidb_id":"69","episode":0}"#).is_err());
        assert!(check(r#"{"imdb_id":"tt0388629","season":-1,"episode":4}"#).is_err());
        assert!(check(r#"{"anidb_id":"69:4","episode":4}"#).is_err());
    }

    #[test]
    fn bidirectional_one_piece_spans_tvdb_seasons_without_guessing(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
             imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
             CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
             CREATE TABLE episode_map(
             anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
             start INTEGER,end INTEGER,offset INTEGER,map TEXT);
             INSERT INTO anime_series_map VALUES('tt0388629','69','81797',1);
             INSERT INTO episode_map VALUES('69','81797',1,-1,1,0,0,'');
             INSERT INTO episode_map VALUES('69','81797',1,21,892,1085,-891,'');
             INSERT INTO episode_map VALUES('69','81797',1,22,1086,1155,-1085,'');").unwrap();
        let forward=resolve(&db,Request::Imdb{
            id:"tt0388629".into(),season:21,episode:1}).unwrap();
        assert_eq!(forward["selected"]["asid"],"69:892");
        let reverse=resolve(&db,Request::Anidb{
            id:"69".into(),episode:892}).unwrap();
        assert_eq!(reverse["selected"]["sid"],"tt0388629:21:1");
        let reverse_next=resolve(&db,Request::Anidb{
            id:"69".into(),episode:1089}).unwrap();
        assert_eq!(reverse_next["selected"]["sid"],"tt0388629:22:4");
        assert_eq!(reverse["ambiguous"],false);
        let missing=resolve(&db,Request::Anidb{
            id:"69".into(),episode:1156}).unwrap();
        assert_eq!(missing["count"],0);
        assert_eq!(missing["selected"],Value::Null);
    }

    #[test]
    fn explicit_crosswalk_pairs_convert_both_ways(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
             imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
             CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
             CREATE TABLE episode_map(
             anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
             start INTEGER,end INTEGER,offset INTEGER,map TEXT);
             INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
             INSERT INTO episode_map VALUES('123','456',1,2,1,20,0,'10-3;11-4');").unwrap();
        let forward=resolve(&db,Request::Imdb{
            id:"tt1234567".into(),season:2,episode:3}).unwrap();
        assert_eq!(forward["selected"]["asid"],"123:10");
        let back=resolve(&db,Request::Anidb{
            id:"123".into(),episode:10}).unwrap();
        assert_eq!(back["selected"]["sid"],"tt1234567:2:3");
        assert_eq!(back["count"],1);
        let absent=resolve(&db,Request::Anidb{
            id:"123".into(),episode:12}).unwrap();
        assert_eq!(absent["count"],0);
    }

    #[test]
    fn split_anidb_parts_use_verifiable_local_episode_offsets(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
             imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
             CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
             CREATE TABLE episode_map(
             anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
             start INTEGER,end INTEGER,offset INTEGER,map TEXT);
             INSERT INTO anime_series_map VALUES('tt1234567','101','456',1);
             INSERT INTO anime_series_map VALUES('tt1234567','102','456',2);
             INSERT INTO episode_map VALUES('101','456',1,3,1,12,0,'');
             INSERT INTO episode_map VALUES('102','456',1,3,13,24,-12,'');").unwrap();
        let reverse=resolve(&db,Request::Anidb{
            id:"102".into(),episode:15}).unwrap();
        assert_eq!(reverse["selected"]["sid"],"tt1234567:2:3");
        let forward=resolve(&db,Request::Imdb{
            id:"tt1234567".into(),season:2,episode:3}).unwrap();
        assert_eq!(forward["selected"]["asid"],"102:15");
        let out_of_range=resolve(&db,Request::Anidb{
            id:"102".into(),episode:25}).unwrap();
        assert_eq!(out_of_range["count"],0);
    }

    #[test]
    fn one_anidb_episode_with_two_explicit_tvdb_coordinates_is_ambiguous(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
             imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
             CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
             CREATE TABLE episode_map(
             anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
             start INTEGER,end INTEGER,offset INTEGER,map TEXT);
             INSERT INTO anime_series_map VALUES('tt1234567','123','456',0);
             INSERT INTO episode_map VALUES('123','456',1,2,1,20,0,'10-3+4');").unwrap();
        let back=resolve(&db,Request::Anidb{
            id:"123".into(),episode:10}).unwrap();
        assert_eq!(back["count"],2);
        assert_eq!(back["selected"],Value::Null);
        assert_eq!(back["ambiguous"],true);
        assert_eq!(back["matches"][0]["sid"],"tt1234567:2:3");
        assert_eq!(back["matches"][1]["sid"],"tt1234567:2:4");
    }

    #[test]
    fn ambiguous_anidb_to_imdb_never_picks_first_series(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_series_map(
             imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
             CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
             CREATE TABLE episode_map(
             anidb_id TEXT,tvdb_id TEXT,anidb_season INTEGER,tvdb_season INTEGER,
             start INTEGER,end INTEGER,offset INTEGER,map TEXT);
             INSERT INTO anime_series_map VALUES('tt1111111','123','456',0);
             INSERT INTO anime_series_map VALUES('tt2222222','123','456',0);
             INSERT INTO episode_map VALUES('123','456',1,1,1,12,0,'');").unwrap();
        let result=resolve(&db,Request::Anidb{
            id:"123".into(),episode:4}).unwrap();
        assert_eq!(result["count"],2);
        assert_eq!(result["ambiguous"],true);
        assert_eq!(result["selected"],Value::Null);
        assert_eq!(result["matches"][0]["sid"],"tt1111111:1:4");
        assert_eq!(result["matches"][1]["sid"],"tt2222222:1:4");
    }
}
