//! Tantivy inverted index for normalized IMDb title prefixes and reversed suffixes.
//! A disk-backed index is opened or built for the current SQLite generation.
use std::{collections::HashMap,path::PathBuf,sync::{Mutex,OnceLock}};
use rusqlite::Connection;
use tantivy::{collector::TopDocs,doc,query::RegexQuery,schema::{Schema,STORED,STRING,Value},Index,IndexReader,ReloadPolicy};

struct SearchIndex { reader:IndexReader, forward:tantivy::schema::Field, backward:tantivy::schema::Field, id:tantivy::schema::Field, year:tantivy::schema::Field }
static CACHE:OnceLock<Mutex<HashMap<String,SearchIndex>>>=OnceLock::new();

fn build(db:&Connection,dir:&std::path::Path)->Result<SearchIndex,String>{
    let mut builder=Schema::builder();
    let forward=builder.add_text_field("forward",STRING|STORED);
    let backward=builder.add_text_field("backward",STRING);
    let id=builder.add_text_field("imdb_id",STRING|STORED);
    let year=builder.add_i64_field("year",tantivy::schema::INDEXED|STORED);
    let schema=builder.build();
    let index=if dir.join("meta.json").exists(){
        Index::open_in_dir(dir).map_err(|e|e.to_string())?
    }else{
        std::fs::create_dir_all(dir).map_err(|e|e.to_string())?;
        Index::create_in_dir(dir,schema).map_err(|e|e.to_string())?
    };
    if index.searchable_segment_ids().map_err(|e|e.to_string())?.is_empty(){
    let mut writer=index.writer(64_000_000).map_err(|e|e.to_string())?;
    let mut stmt=db.prepare("SELECT DISTINCT imdb_id,clean_title,year FROM titles WHERE year>0")
        .map_err(|e|e.to_string())?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))
        .map_err(|e|e.to_string())?;
    for row in rows{
        let (imdb,clean,yr)=row.map_err(|e|e.to_string())?;
        let reversed:String=clean.chars().rev().collect();
        writer.add_document(doc!(forward=>clean,backward=>reversed,id=>imdb,year=>yr))
            .map_err(|e|e.to_string())?;
    }
    writer.commit().map_err(|e|e.to_string())?;
    }
    let reader=index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()
        .map_err(|e:tantivy::TantivyError|e.to_string())?;
    Ok(SearchIndex{reader,forward,backward,id,year})
}

/// Returns candidate IMDb IDs; the deterministic IMDb V4 ranker remains authoritative.
/// The current capped retrieval is retained pending V4 parity validation.
pub fn candidates(db:&Connection,key:&str,year:Option<i32>,suffix:bool,limit:usize)->Result<Vec<String>,String>{
    if key.is_empty()||limit==0{return Ok(Vec::new());}
    let cache=CACHE.get_or_init(||Mutex::new(HashMap::new()));
    let mut guard=cache.lock().map_err(|e|e.to_string())?;
    let source=db.path().unwrap_or(":memory:");
    let version=std::fs::metadata(source).ok().and_then(|m|m.modified().ok());
    let path=format!("{source}:{version:?}");
    guard.retain(|key,_|!key.starts_with(&format!("{source}:"))||key==&path);
    if !guard.contains_key(&path){
        let directory=if source==":memory:" {std::env::temp_dir().join(format!("matchlab-tantivy-{}",std::process::id()))} else {PathBuf::from(format!("{source}.tantivy"))};
        // Version each snapshot by SQLite modification time, so metadata replacement
        // cannot silently reuse an index built from an older database.
        let generation=format!("{:?}",version).replace(|c:char|!c.is_ascii_alphanumeric(),"_");
        let directory=directory.join(generation);
        guard.insert(path.clone(),build(db,&directory)?);
    }
    let search=guard.get(&path).ok_or("index missing")?;
    let field=if suffix{search.backward}else{search.forward};
    let term=if suffix{key.chars().rev().collect::<String>()}else{key.to_string()};
    let pattern=format!("{}.*",term);
    let query=RegexQuery::from_pattern(&pattern,field).map_err(|e|e.to_string())?;
    let reader=search.reader.searcher();
    let mut ids=Vec::new();
    let mut seen=std::collections::HashSet::new();
    let hits=reader.search(&query,&TopDocs::with_limit(limit.saturating_mul(8).min(10000)))
        .map_err(|e|e.to_string())?;
    for (_,addr) in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        let yr=doc.get_first(search.year).and_then(|v|v.as_i64()).unwrap_or(0);
        if year.is_some_and(|y|(yr-i64::from(y)).abs()>1){continue;}
        if let Some(imdb)=doc.get_first(search.id).and_then(|v|v.as_str()){
            if seen.insert(imdb.to_string()){ids.push(imdb.to_string());}
        }
        if ids.len()>=limit{break;}
    }
    Ok(ids)
}


// Prepare the index before serving requests. A full implementation should invoke
// this from the metadata publication pipeline, not from the request path.
pub fn warmup(db:&Connection)->Result<(),String>{
    let _=candidates(db,"a",None,false,1)?;
    Ok(())
}
