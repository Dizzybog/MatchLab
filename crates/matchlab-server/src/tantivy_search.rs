//! Tantivy inverted index for normalized IMDb title prefixes and reversed suffixes.
//! A disk-backed index is opened or built for the current SQLite generation.
use std::{collections::HashMap,fs::File,path::{Path,PathBuf},sync::{Mutex,OnceLock},time::{Duration,SystemTime,UNIX_EPOCH}};
use rusqlite::{Connection,OptionalExtension};
use tantivy::{collector::DocSetCollector,doc,query::{RegexQuery,TermQuery},schema::{Schema,STORED,STRING,Value,IndexRecordOption},Index,IndexReader,ReloadPolicy,Term};

#[derive(Clone)]
struct SearchIndex { reader:IndexReader, forward:tantivy::schema::Field, backward:tantivy::schema::Field, id:tantivy::schema::Field, anime:tantivy::schema::Field, anime_id:tantivy::schema::Field, title:tantivy::schema::Field,
    category:tantivy::schema::Field, region:tantivy::schema::Field,
    original:tantivy::schema::Field, ordering:tantivy::schema::Field,
    media_type:tantivy::schema::Field, year:tantivy::schema::Field }
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct AnimeAlias {pub anidb_id:String,pub clean_title:String,pub imdb_id:String}

#[derive(Debug,Clone)]
pub struct TitleAlias {
    pub imdb_id:String,pub title:String,pub clean:String,pub year:i32,
    pub category:String,pub region:Option<String>,pub original:bool,
    pub ordering:i32,pub media_type:String,
}

static CACHE:OnceLock<Mutex<HashMap<String,SearchIndex>>>=OnceLock::new();

fn build(db:&Connection,dir:&std::path::Path)->Result<SearchIndex,String>{
    let mut builder=Schema::builder();
    let forward=builder.add_text_field("forward",STRING|STORED);
    let backward=builder.add_text_field("backward",STRING);
    let id=builder.add_text_field("imdb_id",STRING|STORED);
    let anime=builder.add_text_field("anime_alias",STRING|STORED);
    let anime_id=builder.add_text_field("anime_anidb_id",STORED);
    let year=builder.add_i64_field("year",STORED);
    let title=builder.add_text_field("title",STORED);
    let category=builder.add_text_field("category",STORED);
    let region=builder.add_text_field("region",STORED);
    let original=builder.add_i64_field("original",STORED);
    let ordering=builder.add_i64_field("ordering",STORED);
    let media_type=builder.add_text_field("media_type",STORED);
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
            let mut stmt=db.prepare("SELECT imdb_id,title,clean_title,year,category,region,original,ordering,type
                FROM titles WHERE year>0")
                .map_err(|e|e.to_string())?;
            let rows=stmt.query_map([],|r|Ok((
                r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,
                r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,
                r.get::<_,i64>(6)?,r.get::<_,i64>(7)?,r.get::<_,String>(8)?)))
                .map_err(|e|e.to_string())?;
            let mut count=0u64;
            for row in rows{
                let (imdb,title_text,clean,yr,category_text,region_text,original_value,ordering_value,kind)=
                    row.map_err(|e|e.to_string())?;
                let reversed:String=clean.chars().rev().collect();
                writer.add_document(doc!(forward=>clean,backward=>reversed,id=>imdb,year=>yr,
                    title=>title_text,category=>category_text,region=>region_text.unwrap_or_default(),
                    original=>original_value,ordering=>ordering_value,media_type=>kind))
                    .map_err(|e|e.to_string())?;
                count+=1;
            }
            let has_alias_tables:i64=db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('anidb_titles','anime_series_map')",
                [],|r|r.get(0)).map_err(|e|e.to_string())?;
            if has_alias_tables==2 {
                let mut aliases=db.prepare("SELECT DISTINCT l.imdb_id,a.anidb_id,a.clean_title
                    FROM anidb_titles a JOIN anime_series_map l ON l.anidb_id=a.anidb_id
                    WHERE a.clean_title<>''").map_err(|e|e.to_string())?;
                let mapped=aliases.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
                    .map_err(|e|e.to_string())?;
                for item in mapped {
                    let (imdb,anidb,alias)=item.map_err(|e|e.to_string())?;
                    writer.add_document(doc!(anime=>alias,anime_id=>anidb,id=>imdb)).map_err(|e|e.to_string())?;
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
            // A second process may already have published this immutable generation.
            if let Err(error)=std::fs::rename(&stage,dir) {
                if !dir.join(".matchlab-complete").exists() { return Err(error.to_string()); }
            }
            // Persist directory entries before metadata can point to this index.
            File::open(dir).and_then(|f|f.sync_all()).map_err(|e|e.to_string())?;
            File::open(parent).and_then(|f|f.sync_all()).map_err(|e|e.to_string())?;
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
    Ok(SearchIndex{reader,forward,backward,id,anime,anime_id,title,category,region,
        original,ordering,media_type,year})
}

/// Each staged SQLite image contains a unique index-generation identifier.
/// Legacy databases continue to work using a filesystem fingerprint until
/// their next full refresh.
fn generation(db:&Connection)->Result<String,String>{
    let stamped=db.query_row(
        "SELECT updated_at FROM metadata_state WHERE source='tantivy_generation'",
        [],|row|row.get::<_,String>(0)).optional();
    match stamped {
        Ok(Some(id)) if id.starts_with("v3-") &&
            id.chars().all(|c|c.is_ascii_alphanumeric()||c=='-') => return Ok(id),
        Ok(Some(_)) => return Err("invalid Tantivy generation identifier".into()),
        Ok(None) => {},
        Err(e) if e.to_string().contains("no such table: metadata_state") => {},
        Err(e) => return Err(e.to_string()),
    }
    let source=db.path().filter(|p|!p.is_empty()&&*p!=":memory:").unwrap_or(":memory:");
    let metadata=std::fs::metadata(source).ok();
    let modified=metadata.as_ref().and_then(|m|m.modified().ok());
    #[cfg(unix)]
    let fingerprint=metadata.as_ref().map(|m|format!("{}-{}-{}-{}",
        std::os::unix::fs::MetadataExt::ino(m),
        std::os::unix::fs::MetadataExt::dev(m),m.len(),
        std::os::unix::fs::MetadataExt::ctime_nsec(m)));
    #[cfg(not(unix))]
    let fingerprint=metadata.as_ref().map(|m|format!("{}",m.len()));
    Ok(format!("legacy-v3-{modified:?}-{fingerprint:?}")
        .replace(|c:char|!c.is_ascii_alphanumeric(),"_"))
}

fn index_dir(source:&Path,generation_id:&str)->PathBuf{
    PathBuf::from(format!("{}.tantivy",source.display())).join(format!("schema5-{generation_id}"))
}

/// Build and verify the *final* Tantivy generation before publishing SQLite.
/// No second index build or request-time warmup is required after publication.
pub fn prepare(db:&Connection,target:&Path)->Result<(),String>{
    let dest=index_dir(target,&generation(db)?);
    drop(build(db,&dest)?);
    Ok(())
}

/// Reclaim old published generations conservatively. Never remove a recent
/// generation or the one currently referenced by the published database.
pub fn prune(db:&Connection,target:&Path)->Result<(),String>{
    let active=format!("schema5-{}",generation(db)?);
    let root=PathBuf::from(format!("{}.tantivy",target.display()));
    if !root.exists(){return Ok(());}
    let mut old=Vec::new();
    for entry in std::fs::read_dir(&root).map_err(|e|e.to_string())?{
        let entry=entry.map_err(|e|e.to_string())?;
        let name=entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("schema5-v3-")||name==active||!entry.path().is_dir()
            ||!entry.path().join(".matchlab-complete").is_file(){continue;}
        let modified=entry.metadata().and_then(|m|m.modified()).map_err(|e|e.to_string())?;
        old.push((modified,entry.path()));
    }
    old.sort_by_key(|(when,_)|std::cmp::Reverse(*when));
    for (when,path) in old.into_iter().skip(2) {
        if SystemTime::now().duration_since(when).unwrap_or_default()>=Duration::from_secs(7*24*3600){
            std::fs::remove_dir_all(path).map_err(|e|e.to_string())?;
        }
    }
    Ok(())
}

fn open_search(db:&Connection)->Result<(SearchIndex,Option<PathBuf>),String>{
    let source=db.path().filter(|p|!p.is_empty()&&*p!=":memory:").unwrap_or(":memory:");
    let temporary=source==":memory:";
    let memory_dir=if temporary {
        Some(std::env::temp_dir().join(format!("matchlab-tantivy-memory-{}-{}",
            std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH)
                .map_err(|e|e.to_string())?.as_nanos())))
    }else{None};
    let generation_id=generation(db)?;
    let path=format!("{source}:{generation_id}");
    let search=if let Some(ref directory)=memory_dir {
        build(db,directory)?
    }else{
        let cache=CACHE.get_or_init(||Mutex::new(HashMap::new()));
        let mut guard=cache.lock().map_err(|e|e.to_string())?;
        guard.retain(|key,_|!key.starts_with(&format!("{source}:"))||key==&path);
        if !guard.contains_key(&path) {
            guard.insert(path.clone(),build(db,&index_dir(Path::new(source),&generation_id))?);
        }
        guard.get(&path).ok_or("index missing")?.clone()
    };
    Ok((search,memory_dir))
}

/// Resolve AniDB aliases through exact Tantivy term lookups, including aliases
/// that are prefixes of a release. SQLite is only used later for mapping data.
pub fn anime_aliases(db:&Connection,key:&str,min_prefix:usize)->Result<Vec<AnimeAlias>,String>{
    if key.len()<4{return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let mut matches=Vec::new();
    let mut seen=std::collections::HashSet::new();
    for len in (4..=key.len()).rev(){
        if !key.is_char_boundary(len)||len<min_prefix&&len!=key.len(){continue;}
        let token=&key[..len];
        let query=TermQuery::new(Term::from_field_text(search.anime,token),IndexRecordOption::Basic);
        let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
        for addr in hits {
            let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
            let Some(imdb)=doc.get_first(search.id).and_then(|v|v.as_str()) else {continue};
            let Some(anidb)=doc.get_first(search.anime_id).and_then(|v|v.as_str()) else {continue};
            let result=AnimeAlias{anidb_id:anidb.to_string(),
                clean_title:token.to_owned(),imdb_id:imdb.to_owned()};
            if seen.insert((result.anidb_id.clone(),result.clean_title.clone(),result.imdb_id.clone())){
                matches.push(result);
            }
        }
    }
    drop(reader);
    drop(search);
    if let Some(dir)=memory_dir {let _=std::fs::remove_dir_all(dir);}
    matches.sort_by(|a,b|b.clean_title.len().cmp(&a.clean_title.len())
        .then(a.anidb_id.cmp(&b.anidb_id)).then(a.imdb_id.cmp(&b.imdb_id)));
    Ok(matches)
}

/// Rehydrate a matching IMDb title document directly from Tantivy; no SQLite
/// title-table lookup is needed during candidate selection or ranking.
fn read_title(doc:&tantivy::TantivyDocument,s:&SearchIndex)->Option<TitleAlias>{
    let get=|field|doc.get_first(field).and_then(|value|value.as_str()).map(str::to_owned);
    let imdb_id=get(s.id)?;
    let title=get(s.title)?;
    let clean=get(s.forward)?;
    let year=i32::try_from(doc.get_first(s.year)?.as_i64()?).ok()?;
    let category=get(s.category)?;
    let region=get(s.region).filter(|v|!v.is_empty());
    let original=doc.get_first(s.original)?.as_i64()?!=0;
    let ordering=i32::try_from(doc.get_first(s.ordering)?.as_i64()?).ok()?;
    let media_type=get(s.media_type)?;
    Some(TitleAlias{imdb_id,title,clean,year,category,region,original,ordering,media_type})
}

/// Complete prefix/suffix search. All candidates are retained until the V4
/// ranker applies its deterministic rules; no top-N cutoff is introduced.
pub fn title_aliases(db:&Connection,key:&str,suffix:bool)->Result<Vec<TitleAlias>,String>{
    if key.is_empty(){return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let field=if suffix{search.backward}else{search.forward};
    let term=if suffix{key.chars().rev().collect::<String>()}else{key.to_string()};
    let mut escaped=String::new();
    for ch in term.chars(){
        if ".+*?()[]{}|^$\\".contains(ch){escaped.push('\\');}
        escaped.push(ch);
    }
    let query=RegexQuery::from_pattern(&format!("{escaped}.*"),field).map_err(|e|e.to_string())?;
    let reader=search.reader.searcher();
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut aliases=Vec::with_capacity(hits.len());
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(title)=read_title(&doc,&search){
            if (suffix&&title.clean.len()>key.len()&&title.clean.ends_with(key))
                || (!suffix&&title.clean.starts_with(key)){aliases.push(title);}
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

/// Indexed ID equality is reserved for resolving already-discovered anime
/// IMDb IDs. It does not scan the SQLite title catalog.
pub fn titles_by_ids(db:&Connection,ids:&[String])->Result<Vec<TitleAlias>,String>{
    if ids.is_empty(){return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let mut aliases=Vec::new();
    let unique:std::collections::HashSet<_>=ids.iter().collect();
    for imdb in unique {
        let query=TermQuery::new(Term::from_field_text(search.id,imdb),IndexRecordOption::Basic);
        let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
        for addr in hits {
            let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
            if let Some(title)=read_title(&doc,&search){aliases.push(title);}
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

pub fn has_duplicate_title(db:&Connection,key:&str)->Result<bool,String>{
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let query=TermQuery::new(Term::from_field_text(search.forward,key),IndexRecordOption::Basic);
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut first:Option<String>=None;
    let mut duplicate=false;
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if doc.get_first(search.year).and_then(|v|v.as_i64()).unwrap_or(0)<=0{continue;}
        let Some(id)=doc.get_first(search.id).and_then(|v|v.as_str())else{continue;};
        if first.as_deref().is_some_and(|prev|prev!=id){duplicate=true;break;}
        first=Some(id.to_owned());
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(duplicate)
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


// Open the already-published index at startup or after a refresh.
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
            db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');").unwrap();
        }
        first.execute("INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1111111','dune',1984)",[]).unwrap();
        second.execute("INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt2222222','dune',2021)",[]).unwrap();
        assert_eq!(candidates(&first,"dune",None,false,1).unwrap(),vec!["tt1111111"]);
        assert_eq!(candidates(&second,"dune",None,false,1).unwrap(),vec!["tt2222222"]);
    }

    #[test]
    fn incomplete_generation_is_not_deleted() {
        let root=std::env::temp_dir().join(format!(
            "matchlab-tantivy-incomplete-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db=Connection::open_in_memory().unwrap();
        let dir=root.join("index");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sentinel"),"in progress").unwrap();
        assert!(build(&db,&dir).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("sentinel")).unwrap(),"in progress");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupted_completion_marker_is_rejected() {
        let root=std::env::temp_dir().join(format!(
            "matchlab-tantivy-corrupt-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1234567','example',2020);").unwrap();
        let index_path=root.join("index");
        drop(build(&db,&index_path).unwrap());
        std::fs::write(index_path.join(".matchlab-complete"),"999").unwrap();
        assert!(build(&db,&index_path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn anime_alias_discovery_uses_tantivy_exact_match() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1234567','official title',2020);
            INSERT INTO anidb_titles VALUES('123','alternate anime');
            INSERT INTO anime_series_map VALUES('123','tt1234567');").unwrap();
        assert_eq!(anime_candidates(&db,"alternate anime").unwrap(),vec!["tt1234567"]);
        assert_eq!(anime_aliases(&db,"alternate anime extra",6).unwrap(),vec![AnimeAlias{
            anidb_id:"123".into(),clean_title:"alternate anime".into(),imdb_id:"tt1234567".into()}]);
        assert!(anime_candidates(&db,"alternate").unwrap().is_empty());
        assert!(candidates(&db,"alternate",None,false,1).unwrap().is_empty());
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1234567','a.b',2020);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt7654321','axb',2020);").unwrap();
        assert_eq!(candidates(&db,"a.b",None,false,1).unwrap(),vec!["tt1234567"]);
    }

    #[test]
    fn stored_title_aliases_are_selected_without_sqlite_title_scans(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            INSERT INTO titles VALUES('tt101','Dune','dune',1984,'movie','US',1,0,'movie');
            INSERT INTO titles VALUES('tt202','Dune','dune',2021,'movie','US',1,0,'movie');
            INSERT INTO titles VALUES('tt303','Duneland','duneland',2022,'movie','US',0,1,'movie');").unwrap();
        let matches=title_aliases(&db,"dune",false).unwrap();
        assert_eq!(matches.len(),3);
        assert!(matches.iter().any(|m|m.imdb_id=="tt202"&&m.title=="Dune"&&m.year==2021));
        assert!(has_duplicate_title(&db,"dune").unwrap());
        assert!(!has_duplicate_title(&db,"duneland").unwrap());
        assert_eq!(titles_by_ids(&db,&["tt303".into()]).unwrap().len(),1);
    }

    #[test]
    fn prebuilt_generation_survives_atomic_sqlite_publication() {
        let root=std::env::temp_dir().join(format!("matchlab-tantivy-publish-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let target=root.join("metadata.sqlite3");
        let staging=root.join("staging.sqlite3");
        let db=Connection::open(&staging).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1234567','example',2020);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-testpublish',0);").unwrap();
        prepare(&db,&target).unwrap();
        let completed=index_dir(&target,"v3-testpublish").join(".matchlab-complete");
        assert!(completed.is_file());
        drop(db);
        std::fs::rename(staging,&target).unwrap();
        let published=Connection::open(&target).unwrap();
        warmup(&published).unwrap();
        assert_eq!(candidates(&published,"example",None,false,1).unwrap(),vec!["tt1234567"]);
        assert!(completed.is_file());
        drop(published);
        std::fs::remove_dir_all(root).unwrap();
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
        conn.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1234567','supernatural',2005);").unwrap();
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
