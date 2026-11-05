//! Tantivy inverted index for normalized IMDb title prefixes and reversed suffixes.
//! A disk-backed index is opened or built for the current SQLite generation.
use std::{collections::HashMap,path::PathBuf,sync::{Mutex,OnceLock}};
use rusqlite::Connection;
use tantivy::{collector::DocSetCollector,doc,query::RegexQuery,schema::{Schema,STORED,STRING,Value},Index,IndexReader,ReloadPolicy};

#[derive(Clone)]
struct SearchIndex { reader:IndexReader, forward:tantivy::schema::Field, backward:tantivy::schema::Field, id:tantivy::schema::Field, anime:tantivy::schema::Field }
static CACHE:OnceLock<Mutex<HashMap<String,SearchIndex>>>=OnceLock::new();

fn build(db:&Connection,dir:&std::path::Path)->Result<SearchIndex,String>{
    let mut builder=Schema::builder();
    let forward=builder.add_text_field("forward",STRING|STORED);
    let backward=builder.add_text_field("backward",STRING);
    let id=builder.add_text_field("imdb_id",STRING|STORED);
    let anime=builder.add_text_field("anime_alias",STRING);
    let year=builder.add_i64_field("year",tantivy::schema::INDEXED|STORED);
    let schema=builder.build();
    // Only a completed generation may be opened. Build in a sibling staging
    // directory so crashes cannot leave a partially written published index.
    let marker=dir.join(".matchlab-complete");
    // A directory without a completion marker may belong to an interrupted
    // build. Never delete it blindly: another process may still be writing.
    // Fail closed instead of racing an active index builder.
    if dir.exists() && !marker.exists(){
        return Err(format!("incomplete Tantivy generation at {}",dir.display()));
    }
    if !marker.exists(){
        let parent=dir.parent().ok_or("index directory has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e|e.to_string())?;
        let stage=parent.join(format!(".tantivy-stage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map_err(|e|e.to_string())?.as_nanos()));
        let result=(|| -> Result<(),String>{
            std::fs::create_dir_all(&stage).map_err(|e|e.to_string())?;
            let staging_index=Index::create_in_dir(&stage,schema).map_err(|e|e.to_string())?;
            let mut writer=staging_index.writer(64_000_000).map_err(|e|e.to_string())?;
            let mut stmt=db.prepare("SELECT DISTINCT imdb_id,clean_title,year FROM titles WHERE year>0")
                .map_err(|e|e.to_string())?;
            let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))
                .map_err(|e|e.to_string())?;
            let mut count=0u64;
            for row in rows{
                let (imdb,clean,yr)=row.map_err(|e|e.to_string())?;
                let reversed:String=clean.chars().rev().collect();
                writer.add_document(doc!(forward=>clean,backward=>reversed,id=>imdb,year=>yr))
                    .map_err(|e|e.to_string())?;
                count+=1;
            }
            let has_alias_tables:i64=db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('anidb_titles','anime_series_map')",
                [],|r|r.get(0)).map_err(|e|e.to_string())?;
            if has_alias_tables==2 {
                let mut aliases=db.prepare("SELECT DISTINCT l.imdb_id,a.clean_title
                    FROM anidb_titles a JOIN anime_series_map l ON l.anidb_id=a.anidb_id
                    WHERE a.clean_title<>''").map_err(|e|e.to_string())?;
                let mapped=aliases.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))
                    .map_err(|e|e.to_string())?;
                for item in mapped {
                    let (imdb,alias)=item.map_err(|e|e.to_string())?;
                    writer.add_document(doc!(anime=>alias,id=>imdb)).map_err(|e|e.to_string())?;
                    count+=1;
                }
            }
            writer.commit().map_err(|e|e.to_string())?;
            drop(writer);
            drop(staging_index);
            let verify=Index::open_in_dir(&stage).map_err(|e|e.to_string())?;
            let reader=verify.reader().map_err(|e|e.to_string())?;
            if reader.searcher().num_docs()!=count{
                return Err("Tantivy index document count mismatch".into());
            }
            drop(reader);
            drop(verify);
            std::fs::write(stage.join(".matchlab-complete"),count.to_string())
                .map_err(|e|e.to_string())?;
            std::fs::rename(&stage,dir).map_err(|e|e.to_string())?;
            Ok(())
        })();
        if result.is_err(){let _=std::fs::remove_dir_all(&stage);}
        result?;
    }
    let index=Index::open_in_dir(dir).map_err(|e|e.to_string())?;
    let reader:IndexReader=index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()
        .map_err(|e:tantivy::TantivyError|e.to_string())?;
    // A completion marker alone is not sufficient after disk corruption or
    // interrupted external copies. Verify the committed document count.
    let expected=std::fs::read_to_string(&marker).map_err(|e|e.to_string())?
        .parse::<u64>().map_err(|e|format!("invalid Tantivy completion marker: {e}"))?;
    if reader.searcher().num_docs()!=expected{
        return Err(format!("Tantivy index corrupted: expected {expected} documents, found {}",reader.searcher().num_docs()));
    }
    Ok(SearchIndex{reader,forward,backward,id,anime})
}

fn open_search(db:&Connection)->Result<(SearchIndex,Option<PathBuf>),String>{
    let source=db.path().filter(|path|!path.is_empty() && *path!=":memory:").unwrap_or(":memory:");
    // SQLite metadata may be replaced atomically with identical timestamps.
    // Bind the Tantivy generation to the file identity and size as well as mtime.
    let metadata=std::fs::metadata(source).ok();
    let version=metadata.as_ref().and_then(|m|m.modified().ok());
    #[cfg(unix)]
    let fingerprint=metadata.as_ref().map(|m|format!("{}-{}-{}-{}",std::os::unix::fs::MetadataExt::ino(m),std::os::unix::fs::MetadataExt::dev(m),m.len(),std::os::unix::fs::MetadataExt::ctime_nsec(m)));
    #[cfg(not(unix))]
    let fingerprint=metadata.as_ref().map(|m|format!("{}",m.len()));
    // In-memory SQLite connections have no filesystem generation and must
    // never share a cached Tantivy index across independent test databases.
    // Each connection gets a fresh temporary generation for this query.
    let memory_dir=if source==":memory:" {
        Some(std::env::temp_dir().join(format!("matchlab-tantivy-memory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map_err(|e|e.to_string())?.as_nanos())))
    }else{None};
    let path=format!("anime-v2:{source}:{version:?}:{fingerprint:?}");
    // Do not hold the global cache mutex while running regex queries.
    // A concurrent metadata refresh may still build a generation under the
    // mutex; queries on already-open generations can proceed independently.
    let search=if let Some(ref directory)=memory_dir {
        build(db,directory)?
    }else{
        let cache=CACHE.get_or_init(||Mutex::new(HashMap::new()));
        let mut guard=cache.lock().map_err(|e|e.to_string())?;
        guard.retain(|key,_|!key.starts_with(&format!("anime-v2:{source}:"))||key==&path);
        if !guard.contains_key(&path){
            let directory=PathBuf::from(format!("{source}.tantivy"));
            let generation=format!("anime-v2-{:?}-{:?}",version,fingerprint).replace(|c:char|!c.is_ascii_alphanumeric(),"_");
            let directory=directory.join(generation);
            guard.insert(path.clone(),build(db,&directory)?);
        }
        guard.get(&path).ok_or("index missing")?.clone()
    };
    Ok((search,memory_dir))
}

/// Returns candidate IMDb IDs; the deterministic IMDb V4 ranker remains authoritative.
/// Uses exhaustive document collection before ID deduplication.
/// Year is intentionally not a hard filter: V4 ranks year proximity and
/// permits matches outside a one-year window.
/// The limit parameter is retained for API compatibility; V4 ranking must
/// receive the complete matching set, so it does not truncate this result.
pub fn candidates(db:&Connection,key:&str,_year:Option<i32>,suffix:bool,limit:usize)->Result<Vec<String>,String>{
    candidates_internal(db,key,_year,suffix,limit,false)
}
pub fn anime_candidates(db:&Connection,key:&str)->Result<Vec<String>,String>{
    candidates_internal(db,key,None,false,1,true)
}
fn candidates_internal(db:&Connection,key:&str,_year:Option<i32>,suffix:bool,limit:usize,anime_only:bool)->Result<Vec<String>,String>{
    if key.is_empty()||limit==0{return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let field=if anime_only{search.anime}else if suffix{search.backward}else{search.forward};
    let term=if suffix{key.chars().rev().collect::<String>()}else{key.to_string()};
    // Tantivy RegexQuery consumes a regex, not a literal string. Escape
    // metacharacters so aliases never change the intended search semantics.
    let mut literal=String::new();
    for ch in term.chars() {
        if ".+*?()[]{}|^$\\".contains(ch) {
            literal.push('\\');
        }
        literal.push(ch);
    }
    let pattern=if anime_only{literal}else{format!("{}.*",literal)};
    let query=RegexQuery::from_pattern(&pattern,field).map_err(|e|e.to_string())?;
    let reader=search.reader.searcher();
    let mut ids=Vec::new();
    let mut seen=std::collections::HashSet::new();
    // Exhaustive candidate collection prevents arbitrary TopDocs truncation
    // from discarding a valid year or longer alias before V4 ranking.
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(imdb)=doc.get_first(search.id).and_then(|v|v.as_str()){
            if seen.insert(imdb.to_string()){ids.push(imdb.to_string());}
        }
        // Do not truncate candidates before deterministic V4 ranking.
    }
    if let Some(directory)=memory_dir {
        drop(search);
        let _=std::fs::remove_dir_all(directory);
    }
    Ok(ids)
}


// Prepare the index before serving requests. A full implementation should invoke
// this from the metadata publication pipeline, not from the request path.
pub fn warmup(db:&Connection)->Result<(),String>{
    let (search,memory_dir)=open_search(db)?;
    drop(search);
    if let Some(directory)=memory_dir {let _=std::fs::remove_dir_all(directory);}
    Ok(())
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn independent_in_memory_databases_never_share_titles() {
        let first=Connection::open_in_memory().unwrap();
        let second=Connection::open_in_memory().unwrap();
        for db in [&first,&second] {
            db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER);").unwrap();
        }
        first.execute("INSERT INTO titles VALUES('tt1111111','dune',1984)",[]).unwrap();
        second.execute("INSERT INTO titles VALUES('tt2222222','dune',2021)",[]).unwrap();
        assert_eq!(candidates(&first,"dune",None,false,1).unwrap(),vec!["tt1111111"]);
        assert_eq!(candidates(&second,"dune",None,false,1).unwrap(),vec!["tt2222222"]);
    }

    #[test]
    fn corrupted_completion_marker_is_rejected() {
        let root=std::env::temp_dir().join(format!(
            "matchlab-tantivy-corrupt-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER);
            INSERT INTO titles VALUES('tt1234567','example',2020);").unwrap();
        let index_path=root.join("index");
        drop(build(&db,&index_path).unwrap());
        std::fs::write(index_path.join(".matchlab-complete"),"999").unwrap();
        assert!(build(&db,&index_path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn anime_alias_discovery_uses_tantivy_exact_match() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles VALUES('tt1234567','official title',2020);
            INSERT INTO anidb_titles VALUES('123','alternate anime');
            INSERT INTO anime_series_map VALUES('123','tt1234567');").unwrap();
        assert_eq!(anime_candidates(&db,"alternate anime").unwrap(),vec!["tt1234567"]);
        assert!(anime_candidates(&db,"alternate").unwrap().is_empty());
        assert!(candidates(&db,"alternate",None,false,1).unwrap().is_empty());
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER);
            INSERT INTO titles VALUES('tt1234567','a.b',2020);
            INSERT INTO titles VALUES('tt7654321','axb',2020);").unwrap();
        assert_eq!(candidates(&db,"a.b",None,false,1).unwrap(),vec!["tt1234567"]);
    }

    #[test]
    fn disk_index_reopens_without_rebuilding() {
        let root=std::env::temp_dir().join(format!(
            "matchlab-tantivy-persistence-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let database=root.join("metadata.sqlite3");
        let conn=Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER);
            INSERT INTO titles VALUES('tt1234567','supernatural',2005);").unwrap();
        let index_path=root.join("index");
        let first=build(&conn,&index_path).unwrap();
        assert_eq!(first.reader.searcher().num_docs(),1);
        drop(first);
        // Reopen the same persisted generation with no SQLite changes.
        let reopened=build(&conn,&index_path).unwrap();
        assert_eq!(reopened.reader.searcher().num_docs(),1);
        drop(reopened);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }
}
