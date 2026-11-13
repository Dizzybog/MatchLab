//! MatchLab's persistent Tantivy search engine for IMDb and AniDB titles.
//! Exact/prefix/suffix lookups share immutable metadata index generations.
use std::{collections::{HashMap,BTreeSet},fs::File,path::{Path,PathBuf},sync::{RwLock,OnceLock,atomic::{AtomicU64,Ordering}},time::{Duration,SystemTime,UNIX_EPOCH}};
use crate::query_cache::QueryCache;
use rusqlite::{Connection,OptionalExtension};
use tantivy::{collector::DocSetCollector,doc,query::{RegexQuery,TermQuery,TermSetQuery},schema::{Schema,STORED,STRING,Value,IndexRecordOption},Index,IndexReader,ReloadPolicy,Term};

#[derive(Clone)]
struct SearchIndex {
    reader: IndexReader,
    forward: tantivy::schema::Field,
    backward: tantivy::schema::Field,
    id: tantivy::schema::Field,
    anime: tantivy::schema::Field,
    anime_id: tantivy::schema::Field,
    anime_title: tantivy::schema::Field,
    anime_type: tantivy::schema::Field,
    anime_language: tantivy::schema::Field,
    external_key: tantivy::schema::Field,
    external_type: tantivy::schema::Field,
    external_value: tantivy::schema::Field,
    title: tantivy::schema::Field,
    category: tantivy::schema::Field,
    region: tantivy::schema::Field,
    original: tantivy::schema::Field,
    ordering: tantivy::schema::Field,
    media_type: tantivy::schema::Field,
    year: tantivy::schema::Field,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct AnimeAlias {pub anidb_id:String,pub clean_title:String,pub imdb_id:String}

#[derive(Debug,Clone)]
pub struct TitleAlias {
    pub imdb_id:String,pub title:String,pub clean:String,pub year:i32,
    pub category:String,pub region:Option<String>,pub original:bool,
    pub ordering:i32,pub media_type:String,
}

// SearchIndex generations are immutable after publication. Concurrent
// queries only need a shared read lock; index opening uses the write path.
static CACHE:OnceLock<RwLock<HashMap<String,SearchIndex>>>=OnceLock::new();
static MEMORY_INDEX_NONCE:AtomicU64=AtomicU64::new(0);

// Reuse identical immutable Tantivy lookups across files belonging to the
// same torrent (and across unrelated torrents with the same title). Never
// cache a final SID/ASID: per-file episode, anime part and year still apply.
const LOOKUP_TTL:Duration=Duration::from_secs(600);
const LOOKUP_CAP:usize=512;
const LOOKUP_MAX_ALIASES:usize=256;
static TITLE_LOOKUPS:OnceLock<QueryCache<Vec<TitleAlias>>>=OnceLock::new();
static ANIME_LOOKUPS:OnceLock<QueryCache<Vec<AnimeAlias>>>=OnceLock::new();
static DUPLICATE_LOOKUPS:OnceLock<QueryCache<bool>>=OnceLock::new();
static ANIME_ID_LOOKUPS:OnceLock<QueryCache<Vec<String>>>=OnceLock::new();
static ANIDB_EXACT_LOOKUPS:OnceLock<QueryCache<BTreeSet<String>>>=OnceLock::new();

fn lookup_cache_key(db:&Connection,kind:&str,key:&str)->Option<String>{
    // In-memory SQLite connections may share the same ':memory:' label but
    // contain unrelated metadata: bypass the process-wide cache completely.
    let source=db.path().filter(|p|!p.is_empty()&&*p!=":memory:")?;
    // Generation is read from this *connection* (possibly an old snapshot)
    // rather than the latest published database path. All query caches
    // automatically miss after an atomic metadata generation change.
    let version=generation(db).ok()?;
    Some(format!("{source:?}|{version:?}|{kind:?}|{key:?}"))
}


fn build(db:&Connection,dir:&std::path::Path)->Result<SearchIndex,String>{
    let mut builder=Schema::builder();
    let forward=builder.add_text_field("forward",STRING|STORED);
    let backward=builder.add_text_field("backward",STRING);
    let id=builder.add_text_field("imdb_id",STRING|STORED);
    let anime=builder.add_text_field("anime_alias",STRING|STORED);
    let anime_id=builder.add_text_field("anime_anidb_id",STRING|STORED);
    let anime_title=builder.add_text_field("anime_original_title",STORED);
    let anime_type=builder.add_text_field("anime_title_type",STORED);
    let anime_language=builder.add_text_field("anime_language",STORED);
    let external_key=builder.add_text_field("external_key",STRING);
    let external_type=builder.add_text_field("external_type",STORED);
    let external_value=builder.add_text_field("external_value",STORED);
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
        // Two in-memory Tantivy builds may start within one clock tick.
        // A process-wide nonce prevents them sharing a staging directory
        // (and colliding on its Tantivy writer lock or meta.json).
        let stage=parent.join(format!(".tantivy-stage-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map_err(|e|e.to_string())?.as_nanos(),
            MEMORY_INDEX_NONCE.fetch_add(1,Ordering::Relaxed)));
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
            // Index *all* AniDB title aliases, including titles without an
            // IMDb link. Linked aliases retain their individual IMDb IDs, so
            // one exact title can correctly expose several separate works.
            let has_titles:i64=db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='anidb_titles'",
                [],|r|r.get(0)).map_err(|e|e.to_string())?;
            let has_links:i64=db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='anime_series_map'",
                [],|r|r.get(0)).map_err(|e|e.to_string())?;
            if has_titles>0 {
                // Historical unit-test fixtures only have anidb_id/clean_title.
                // Production imports also contain original title, type and
                // language. Keep both fixtures and existing data usable.
                let mut info=db.prepare("PRAGMA table_info(anidb_titles)")
                    .map_err(|e|e.to_string())?;
                let columns:BTreeSet<String>=info.query_map([],|r|r.get::<_,String>(1))
                    .map_err(|e|e.to_string())?
                    .collect::<Result<_,_>>().map_err(|e|e.to_string())?;
                let column=|name:&str,fallback:&str| {
                    if columns.contains(name){format!("a.{name}")}else{fallback.to_owned()}
                };
                let sql=format!(
                    "SELECT DISTINCT a.anidb_id,{},a.clean_title,{},{},{} \
                     FROM anidb_titles a {} WHERE a.clean_title<>''",
                    column("title","a.clean_title"),
                    column("title_type","''"),
                    column("language","''"),
                    if has_links>0{"l.imdb_id"}else{"NULL"},
                    if has_links>0{
                        "LEFT JOIN anime_series_map l ON l.anidb_id=a.anidb_id"
                    }else{""}
                );
                let mut stmt=db.prepare(&sql).map_err(|e|e.to_string())?;
                let mapped=stmt.query_map([],|r|Ok((
                    r.get::<_,String>(0)?,r.get::<_,String>(1)?,
                    r.get::<_,String>(2)?,r.get::<_,String>(3)?,
                    r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?
                ))).map_err(|e|e.to_string())?;
                for item in mapped {
                    let (anidb,raw,clean,kind,language,imdb)=
                        item.map_err(|e|e.to_string())?;
                    // Invalid historical comma-joined IMDb IDs are never
                    // exposed, but their AniDB titles remain discoverable.
                    let document=if let Some(imdb)=imdb
                        .filter(|id|crate::metadata_import::valid_imdb_id(id)){
                        doc!(anime=>clean,anime_id=>anidb,anime_title=>raw,
                            anime_type=>kind,anime_language=>language,id=>imdb)
                    }else{
                        doc!(anime=>clean,anime_id=>anidb,anime_title=>raw,
                            anime_type=>kind,anime_language=>language)
                    };
                    writer.add_document(document).map_err(|e|e.to_string())?;
                    count+=1;
                }
            }
            // ID bridge entries come from animeApi and Anime-Lists' relational
            // tables. They have no title fields, so cannot contaminate title
            // lookup or canonical IMDb ranking. The external_key term is
            // indexed for reverse Kitsu/AniList/MAL/TVDB lookup.
            let mut ids=BTreeSet::<(String,String,String)>::new();
            let has_ids:i64=db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='anime_ids'",
                [],|r|r.get(0)).map_err(|e|e.to_string())?;
            if has_ids>0{
                let mut stmt=db.prepare("SELECT group_id,type,value FROM anime_ids")
                    .map_err(|e|e.to_string())?;
                let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,
                    r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
                    .map_err(|e|e.to_string())?;
                for row in rows{
                    let (imdb,kind,value)=row.map_err(|e|e.to_string())?;
                    if crate::metadata_import::valid_imdb_id(&imdb)
                        && !kind.is_empty() && !value.is_empty(){
                        ids.insert((imdb,kind,value));
                    }
                }
            }
            if has_links>0{
                let mut info=db.prepare("PRAGMA table_info(anime_series_map)")
                    .map_err(|e|e.to_string())?;
                let columns:BTreeSet<String>=info.query_map([],|r|r.get::<_,String>(1))
                    .map_err(|e|e.to_string())?
                    .collect::<Result<_,_>>().map_err(|e|e.to_string())?;
                if columns.contains("imdb_id")&&columns.contains("anidb_id"){
                    let tvdb=if columns.contains("tvdb_id"){"tvdb_id"}else{"''"};
                    let sql=format!(
                        "SELECT imdb_id,anidb_id,{tvdb} FROM anime_series_map");
                    let mut stmt=db.prepare(&sql).map_err(|e|e.to_string())?;
                    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,
                        r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
                        .map_err(|e|e.to_string())?;
                    for row in rows{
                        let (imdb,anidb,tvdb)=row.map_err(|e|e.to_string())?;
                        if !crate::metadata_import::valid_imdb_id(&imdb){continue;}
                        if !anidb.is_empty(){ids.insert((imdb.clone(),
                            "anidb".into(),anidb));}
                        if !tvdb.is_empty(){ids.insert((imdb,
                            "tvdb".into(),tvdb));}
                    }
                }
            }
            for (imdb,kind,value) in ids{
                writer.add_document(doc!(
                    id=>imdb,external_key=>format!("{kind}:{value}"),
                    external_type=>kind,external_value=>value
                )).map_err(|e|e.to_string())?;
                count+=1;
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
    Ok(SearchIndex{reader,forward,backward,id,anime,anime_id,anime_title,anime_type,anime_language,external_key,external_type,external_value,title,category,region,
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
    PathBuf::from(format!("{}.tantivy",source.display())).join(format!("schema7-{generation_id}"))
}

/// Cheap readiness probe: never start a full index build from /health.
pub fn index_ready(db:&Connection)->bool{
    let Some(source)=db.path().filter(|path|!path.is_empty()&&*path!=":memory:")else{
        return false;
    };
    let Ok(generation_id)=generation(db)else{return false;};
    index_dir(Path::new(source),&generation_id)
        .join(".matchlab-complete").is_file()
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
    let active=format!("schema7-{}",generation(db)?);
    let root=PathBuf::from(format!("{}.tantivy",target.display()));
    if !root.exists(){return Ok(());}
    let mut old=Vec::new();
    for entry in std::fs::read_dir(&root).map_err(|e|e.to_string())?{
        let entry=entry.map_err(|e|e.to_string())?;
        let name=entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("schema7-v3-")||name==active||!entry.path().is_dir()
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
        Some(std::env::temp_dir().join(format!("matchlab-tantivy-memory-{}-{}-{}",
            std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH)
                .map_err(|e|e.to_string())?.as_nanos(),
            MEMORY_INDEX_NONCE.fetch_add(1,Ordering::Relaxed))))
    }else{None};
    let generation_id=generation(db)?;
    let path=format!("{source}:{generation_id}");
    let search=if let Some(ref directory)=memory_dir {
        build(db,directory)?
    }else{
        let cache=CACHE.get_or_init(||RwLock::new(HashMap::new()));
        // No exclusive process-wide lock on the ordinary warmed-index path.
        let cached={
            let guard=cache.read().map_err(|e|e.to_string())?;
            guard.get(&path).cloned()
        };
        if let Some(cached)=cached{
            cached
        }else{
            let mut guard=cache.write().map_err(|e|e.to_string())?;
            // Double-check after acquiring the exclusive lock: a concurrent
            // request may have loaded this same generation already.
            if !guard.contains_key(&path) {
                let location=index_dir(Path::new(source),&generation_id);
                // Never index millions of titles on an HTTP request.
                if !location.join(".matchlab-complete").is_file(){
                    return Err(format!("Tantivy generation not ready: {}",location.display()));
                }
                let opened=build(db,&location)?;
                // Preserve immutable generations for old SQLite snapshots.
                const MAX_GENERATIONS:usize=12;
                if guard.len()>=MAX_GENERATIONS {
                    if let Some(victim)=guard.keys().find(|k|k.as_str()!=path).cloned() {
                        guard.remove(&victim);
                    }
                }
                guard.insert(path.clone(),opened);
            }
            guard.get(&path).ok_or("index missing")?.clone()
        }
    };
    Ok((search,memory_dir))
}


/// A full unranked alias record. An IMDb alias and an AniDB alias are
/// distinct sources even when the normalized text is identical. Duplicate
/// normalized spellings, languages, and linked identities are preserved.
#[derive(Debug,Clone,PartialEq,Eq,PartialOrd,Ord,serde::Serialize)]
pub struct AliasRecord {
    pub source: &'static str,
    pub name: String,
    pub normalized: String,
    #[serde(skip_serializing_if="Option::is_none")]
    pub imdb_id: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub anidb_id: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub year: Option<i64>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub title_type: Option<String>,
}
#[derive(Debug,Clone,Copy)]
pub enum AliasLookupKind { ImdbId, AnidbId, ExactTitle }

// Rehydrate records once for both single-ID and multi-ID index queries.
// One TermSetQuery replaces O(N) reader acquisitions for external aliases.
fn alias_records_from_hits(
    search:&SearchIndex,
    reader:&tantivy::Searcher,
    hits:impl IntoIterator<Item=tantivy::DocAddress>,
)->Result<Vec<AliasRecord>,String>{
    let mut records=BTreeSet::new();
    for addr in hits {
        let document:tantivy::TantivyDocument=reader.doc(addr)
            .map_err(|e|e.to_string())?;
        let read=|field|document.get_first(field)
            .and_then(|value|value.as_str()).map(str::to_owned);
        let imdb_id=read(search.id).filter(|id|
            crate::metadata_import::valid_imdb_id(id));
        if let Some(normalized)=read(search.anime){
            if let (Some(anidb_id),Some(name))=(
                read(search.anime_id),read(search.anime_title)){
                records.insert(AliasRecord{
                    source:"anidb",name,normalized,imdb_id,
                    anidb_id:Some(anidb_id),year:None,
                    language:read(search.anime_language).filter(|v|!v.is_empty()),
                    title_type:read(search.anime_type).filter(|v|!v.is_empty()),
                });
            }
        }else if let (Some(imdb_id),Some(name),Some(normalized))=(
            imdb_id,read(search.title),read(search.forward)){
            records.insert(AliasRecord{
                source:"imdb",name,normalized,imdb_id:Some(imdb_id),
                anidb_id:None,
                year:document.get_first(search.year).and_then(|v|v.as_i64()),
                language:None,title_type:None,
            });
        }
    }
    Ok(records.into_iter().collect())
}

/// All identity lookups and exact normalized title lookups use Tantivy term
/// postings. No SQLite title scans, prefix expansion, ranker, or TopDocs cap.
pub fn lookup_aliases(
    db:&Connection,kind:AliasLookupKind,key:&str,
)->Result<Vec<AliasRecord>,String>{
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let hits=match kind{
        AliasLookupKind::ImdbId=>{
            let q=TermQuery::new(Term::from_field_text(search.id,key),
                IndexRecordOption::Basic);
            reader.search(&q,&DocSetCollector)
        },
        AliasLookupKind::AnidbId=>{
            let q=TermQuery::new(Term::from_field_text(search.anime_id,key),
                IndexRecordOption::Basic);
            reader.search(&q,&DocSetCollector)
        },
        AliasLookupKind::ExactTitle=>{
            let q=TermSetQuery::new([
                Term::from_field_text(search.forward,key),
                Term::from_field_text(search.anime,key),
            ]);
            reader.search(&q,&DocSetCollector)
        },
    }.map_err(|e|e.to_string())?;
    let records=alias_records_from_hits(&search,&reader,hits)?;
    drop(reader);
    drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(records)
}


/// All source IDs linked to each IMDb identity, retaining the group boundary
/// so an unrelated AniDB part is never described as the direct ID of another.
pub fn external_ids_by_imdb(
    db:&Connection,ids:&BTreeSet<String>,
)->Result<std::collections::BTreeMap<String,std::collections::BTreeMap<String,BTreeSet<String>>>,String>{
    let mut out=std::collections::BTreeMap::new();
    if ids.is_empty(){return Ok(out);}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let query=TermSetQuery::new(ids.iter().map(|id|
        Term::from_field_text(search.id,id)));
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    for addr in hits{
        let d:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        let get=|f|d.get_first(f).and_then(|v|v.as_str());
        if let (Some(imdb),Some(kind),Some(value))=(
            get(search.id),get(search.external_type),get(search.external_value)){
            if ids.contains(imdb){
                out.entry(imdb.to_owned())
                    .or_insert_with(std::collections::BTreeMap::new)
                    .entry(kind.to_owned())
                    .or_insert_with(BTreeSet::new)
                    .insert(value.to_owned());
            }
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(out)
}

/// Exact external identity (Kitsu/MAL/AniList/TVDB/etc.) -> every IMDb link.
/// No best-match ranking: multiple works with this ID remain separate.
pub fn imdb_ids_for_external_id(
    db:&Connection,kind:&str,value:&str,
)->Result<BTreeSet<String>,String>{
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let term=Term::from_field_text(search.external_key,&format!("{kind}:{value}"));
    let query=TermQuery::new(term,IndexRecordOption::Basic);
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut ids=BTreeSet::new();
    for addr in hits{
        let d:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(id)=d.get_first(search.id).and_then(|v|v.as_str())
            && crate::metadata_import::valid_imdb_id(id){
            ids.insert(id.to_owned());
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(ids)
}

/// Fetch all titles for an explicit set of IMDb IDs with one Tantivy union.
/// Used after alternate-ID lookup, not for fuzzy title discovery.
/// Rehydrate only the returned documents; no title scans or per-ID searches.
pub fn aliases_for_imdb_ids(
    db:&Connection,ids:&BTreeSet<String>,
)->Result<Vec<AliasRecord>,String>{
    if ids.is_empty(){return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let query=TermSetQuery::new(ids.iter()
        .map(|id|Term::from_field_text(search.id,id)));
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let records=alias_records_from_hits(&search,&reader,hits)?;
    drop(reader);
    drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(records)
}

/// IMDb-independent AniDB identity lookup. Collect only AniDB ID postings
/// (including works without an IMDb link) instead of rehydrating full title
/// aliases. The cache key is tied to the immutable Tantivy generation.
fn anidb_ids_exact_uncached(db:&Connection,key:&str)->Result<BTreeSet<String>,String>{
    if key.len()<4{return Ok(BTreeSet::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let query=TermQuery::new(
        Term::from_field_text(search.anime,key),IndexRecordOption::Basic);
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut result=BTreeSet::new();
    for addr in hits{
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(id)=doc.get_first(search.anime_id).and_then(|v|v.as_str()){
            result.insert(id.to_owned());
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(result)
}

pub fn anidb_ids_exact(db:&Connection,key:&str)->Result<BTreeSet<String>,String>{
    if let Some(cache_key)=lookup_cache_key(db,"anidb_exact",key){
        ANIDB_EXACT_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(cache_key,||anidb_ids_exact_uncached(db,key),
                |ids|ids.len()<=LOOKUP_MAX_ALIASES)
    }else{anidb_ids_exact_uncached(db,key)}
}

/// A very short anime alias must not establish identity for a different
/// release title beginning with that word ("Monster: The Jeffrey Dahmer
/// Story" is not the 2004 anime "Monster"). Compare the BEST / longest
/// alias with the COMPLETE normalized title. Keep nested shorter aliases
/// available only when a sufficiently specific longer alias also matches.
fn anime_alias_covers_title(key:&str,longest_alias_len:usize)->bool{
    longest_alias_len==key.len()
        ||(longest_alias_len.saturating_mul(2)>=key.len()
            &&key.len().saturating_sub(longest_alias_len)<=16)
}
/// Resolve AniDB aliases through exact Tantivy term lookups, including aliases
/// that are prefixes of a release. SQLite is only used later for mapping data.
fn anime_aliases_uncached(db:&Connection,key:&str,min_prefix:usize)->Result<Vec<AnimeAlias>,String>{
    if key.len()<4{return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    // Search all valid alias prefixes in one union query. The previous
    // implementation submitted one Tantivy query per character position.
    // Leading and trailing AniDB aliases share one Tantivy term-union query.
    // No per-character searches, SQLite scans or index-format changes.
    let terms=(4..=key.len())
        .filter(|&len|(len>=min_prefix||len==key.len())
            && key.is_char_boundary(len)
            && key.is_char_boundary(key.len()-len))
        .flat_map(|len|[
            Term::from_field_text(search.anime,&key[..len]),
            Term::from_field_text(search.anime,&key[key.len()-len..]),
        ]);
    let query=TermSetQuery::new(terms);
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut matches=Vec::with_capacity(hits.len());
    let mut seen=std::collections::HashSet::new();
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        let Some(imdb)=doc.get_first(search.id).and_then(|v|v.as_str())else{continue};
        // Old metadata snapshots may still contain comma-joined Anime-Lists
        // IDs until their next refresh. Do not treat them as valid titles.
        if !crate::metadata_import::valid_imdb_id(imdb){continue;}
        let Some(anidb)=doc.get_first(search.anime_id).and_then(|v|v.as_str())else{continue};
        let Some(alias)=doc.get_first(search.anime).and_then(|v|v.as_str())else{continue};
        let result=AnimeAlias{anidb_id:anidb.to_owned(),
            clean_title:alias.to_owned(),imdb_id:imdb.to_owned()};
        if seen.insert((result.anidb_id.clone(),result.clean_title.clone(),result.imdb_id.clone())){
            matches.push(result);
        }
    }
    drop(reader);
    drop(search);
    if let Some(dir)=memory_dir {let _=std::fs::remove_dir_all(dir);}
    matches.sort_by(|a,b|b.clean_title.len().cmp(&a.clean_title.len())
        .then_with(||key.starts_with(&b.clean_title).cmp(&key.starts_with(&a.clean_title)))
        .then(a.anidb_id.cmp(&b.anidb_id)).then(a.imdb_id.cmp(&b.imdb_id)));
    // This single gate protects IMDb's anime fallback, anime-priority
    // season/episode mapping, and title-only anime identity together.
    // It leaves exact matches untouched and retains nested valid prefixes.
    if matches.first().is_some_and(|best|
        !anime_alias_covers_title(key,best.clean_title.len())){
        matches.clear();
    }
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

/// Check the exact normalized alias first, avoiding broad prefix scans for
/// common titles (including short titles such as "Up" and "It").
fn title_aliases_exact_uncached(db:&Connection,key:&str)->Result<Vec<TitleAlias>,String>{
    if key.is_empty(){return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let query=TermQuery::new(Term::from_field_text(search.forward,key),IndexRecordOption::Basic);
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut aliases=Vec::with_capacity(hits.len());
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(alias)=read_title(&doc,&search){aliases.push(alias);}
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

/// Recover complete AND incomplete IMDb titles extending the original
/// release's word-boundary prefix after parsing stopped early. Every supplied
/// key extends the first, so one indexed prefix query discovers all candidate
/// completions. The matcher independently verifies the complete raw words,
/// minimum coverage and the release's technical-metadata boundary.
/// Never search title substrings or scan SQLite for title candidates.
fn title_aliases_at_release_prefixes_uncached(
    db:&Connection,keys:&[String],
)->Result<Vec<TitleAlias>,String>{
    let Some(shortest)=keys.first().filter(|key|!key.is_empty())else{
        return Ok(Vec::new());
    };
    let (search,memory_dir)=open_search(db)?;
    // All keys are made from compact normalized title words, but escape
    // regex operators defensively to avoid altering the lookup semantics.
    let mut literal=String::new();
    for ch in shortest.chars(){
        if ".+*?()[]{}|^$\\".contains(ch){literal.push('\\');}
        literal.push(ch);
    }
    let query=RegexQuery::from_pattern(&format!("{literal}.*"),search.forward)
        .map_err(|e|e.to_string())?;
    let reader=search.reader.searcher();
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut aliases=Vec::with_capacity(hits.len());
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(alias)=read_title(&doc,&search){aliases.push(alias);}
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

/// Recover longer IMDb titles anchored at a later whole-word boundary in
/// the original release, ending at the first season/episode/quality marker.
/// One bounded indexed prefix query replaces N per-word searches. The
/// matcher rejects all unsafe partial words/coverage after retrieval.
fn title_aliases_at_release_suffixes_uncached(
    db:&Connection,keys:&[String],
)->Result<Vec<TitleAlias>,String>{
    if keys.is_empty(){return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let mut alternatives=Vec::with_capacity(keys.len());
    for key in keys {
        if key.is_empty(){continue;}
        let mut escaped=String::new();
        for ch in key.chars(){
            if ".+*?()[]{}|^$\\".contains(ch){escaped.push('\\');}
            escaped.push(ch);
        }
        alternatives.push(escaped);
    }
    if alternatives.is_empty(){return Ok(Vec::new());}
    let pattern=format!("({}).*",alternatives.join("|"));
    let query=RegexQuery::from_pattern(&pattern,search.forward)
        .map_err(|e|e.to_string())?;
    let reader=search.reader.searcher();
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut aliases=Vec::with_capacity(hits.len());
    for addr in hits{
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(alias)=read_title(&doc,&search){aliases.push(alias);}
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

/// Memoize source-derived release prefix/suffix unions just like the
/// existing alias lookups. These indexed candidate sets are independent of
/// file episode coordinates and are shared across sibling paths; full-title
/// Unicode/word-boundary validation still runs on every request.
pub fn title_aliases_at_release_prefixes(
    db:&Connection,keys:&[String],
)->Result<Vec<TitleAlias>,String>{
    if keys.is_empty(){return Ok(Vec::new());}
    let joined=keys.join("\u{1f}");
    if let Some(id)=lookup_cache_key(db,"release_prefixes",&joined){
        TITLE_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,
                ||title_aliases_at_release_prefixes_uncached(db,keys),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{title_aliases_at_release_prefixes_uncached(db,keys)}
}
pub fn title_aliases_at_release_suffixes(
    db:&Connection,keys:&[String],
)->Result<Vec<TitleAlias>,String>{
    if keys.is_empty(){return Ok(Vec::new());}
    let joined=keys.join("\u{1f}");
    if let Some(id)=lookup_cache_key(db,"release_suffixes",&joined){
        TITLE_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,
                ||title_aliases_at_release_suffixes_uncached(db,keys),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{title_aliases_at_release_suffixes_uncached(db,keys)}
}

/// Find complete IMDb alias prefixes AND suffixes inside the parsed title.
/// One Tantivy term-union query discovers the candidates; a separate word-
/// boundary-aware ranker rejects fragments of longer words.
fn title_aliases_inside_uncached(db:&Connection,key:&str)->Result<Vec<TitleAlias>,String>{
    if key.len()<6{return Ok(Vec::new());}
    let min_len=4usize.max((key.len()+1)/2).max(key.len().saturating_sub(16));
    if min_len>=key.len(){return Ok(Vec::new());}
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    let terms=(min_len..key.len())
        .filter(|&len|key.is_char_boundary(len)
            && key.is_char_boundary(key.len()-len))
        .flat_map(|len|[
            Term::from_field_text(search.forward,&key[..len]),
            Term::from_field_text(search.forward,&key[key.len()-len..]),
        ]);
    let query=TermSetQuery::new(terms);
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut aliases=Vec::with_capacity(hits.len());
    for addr in hits{
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(alias)=read_title(&doc,&search)
            && alias.clean.len()>=min_len
            && (key.starts_with(&alias.clean)||key.ends_with(&alias.clean)){
            aliases.push(alias);
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

/// Complete prefix/suffix search. All candidates are retained until the IMDb
/// ranker applies its deterministic rules; no top-N cutoff is introduced.
fn title_aliases_uncached(db:&Connection,key:&str,suffix:bool)->Result<Vec<TitleAlias>,String>{
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
    // A union of exact IMDb ID postings replaces one query per ID. This
    // includes every title alias for every ID, without an arbitrary TopDocs
    // cutoff or a new title ranking policy.
    let query=TermSetQuery::new(ids.iter()
        .map(|id|Term::from_field_text(search.id,id)));
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    let mut aliases=Vec::with_capacity(hits.len());
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(title)=read_title(&doc,&search){aliases.push(title);}
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(aliases)
}

fn has_duplicate_title_uncached(db:&Connection,key:&str)->Result<bool,String>{
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

/// Returns candidate IMDb IDs; the deterministic MatchLab ranker remains authoritative.
/// Uses exhaustive document collection before ID deduplication.
/// Year is intentionally not a hard filter: IMDb ranks year proximity and
/// permits matches outside a one-year window.
/// The limit parameter is retained for API compatibility; IMDb ranking must
/// receive the complete matching set, so it does not truncate this result.
#[cfg(test)]
pub fn candidates(db:&Connection,key:&str,_year:Option<i32>,suffix:bool,limit:usize)->Result<Vec<String>,String>{
    candidates_internal(db,key,_year,suffix,limit,false)
}
fn anime_candidates_uncached(db:&Connection,key:&str)->Result<Vec<String>,String>{
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
    // from discarding a valid year or longer alias before IMDb ranking.
    let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
    for addr in hits {
        let doc:tantivy::TantivyDocument=reader.doc(addr).map_err(|e|e.to_string())?;
        if let Some(imdb)=doc.get_first(search.id).and_then(|v|v.as_str()){
            // Old AniDB alias indexes may contain a comma-separated IMDb
            // field. Never expose that string as a candidate ID.
            if anime_only && !crate::metadata_import::valid_imdb_id(imdb){continue;}
            if seen.insert(imdb.to_string()){ids.push(imdb.to_string());}
        }
        // Do not truncate candidates before deterministic IMDb ranking.
    }
    if let Some(directory)=memory_dir {
        drop(search);
        let _=std::fs::remove_dir_all(directory);
    }
    Ok(ids)
}


// Open the already-published index at startup or after a refresh.
pub fn warmup(db:&Connection)->Result<(),String>{
    // Startup / refresh workers may recover a genuinely missing index.
    // No API request is allowed to trigger this expensive operation.
    if let Some(source)=db.path().filter(|p|!p.is_empty()&&*p!=":memory:"){
        let location=index_dir(Path::new(source),&generation(db)?);
        if !location.join(".matchlab-complete").is_file(){
            drop(build(db,&location)?);
        }
    }
    let (search,memory_dir)=open_search(db)?;
    drop(search);
    if let Some(directory)=memory_dir {let _=std::fs::remove_dir_all(directory);}
    Ok(())
}


/// Single-flight, generation-aware memoization of immutable Tantivy results.
/// Exact-title, prefix and suffix queries remain exhaustive. Oversized
/// candidate sets are deliberately not cached to keep memory bounded.
pub fn title_aliases_exact(db:&Connection,key:&str)->Result<Vec<TitleAlias>,String>{
    if let Some(id)=lookup_cache_key(db,"exact_title",key){
        TITLE_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,||title_aliases_exact_uncached(db,key),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{title_aliases_exact_uncached(db,key)}
}

pub fn title_aliases(db:&Connection,key:&str,suffix:bool)->Result<Vec<TitleAlias>,String>{
    let kind=if suffix{"suffix_title"}else{"prefix_title"};
    if let Some(id)=lookup_cache_key(db,kind,key){
        TITLE_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,||title_aliases_uncached(db,key,suffix),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{title_aliases_uncached(db,key,suffix)}
}

pub fn title_aliases_inside(db:&Connection,key:&str)->Result<Vec<TitleAlias>,String>{
    if let Some(id)=lookup_cache_key(db,"inside_title",key){
        TITLE_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,||title_aliases_inside_uncached(db,key),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{title_aliases_inside_uncached(db,key)}
}

/// Strict matching never enumerates anime prefixes/suffixes. Reuse the
/// existing exact-title Tantivy TermSetQuery, which checks the IMDb and AniDB
/// index fields in one lookup. The prefix-oriented anime cache is not touched.
pub fn anime_aliases_exact(db:&Connection,raw_title:&str)->Result<Vec<AnimeAlias>,String>{
    let key=crate::title_normalization::clean_title(raw_title);
    if key.len()<4{return Ok(Vec::new());}
    Ok(lookup_aliases(db,AliasLookupKind::ExactTitle,&key)?
        .into_iter()
        .filter(|alias|alias.source=="anidb" &&alias.normalized==key)
        .filter_map(|alias|Some(AnimeAlias{
            anidb_id:alias.anidb_id?,
            imdb_id:alias.imdb_id?,
            clean_title:alias.normalized
        }))
        .collect())
}

/// Cache the indexed candidates independent of the source title's word
/// boundaries. Different raw titles can normalize to the same key but have
/// different legal suffixes (e.g. "Beyond Dune" vs "BeyondDune").
fn indexed_anime_aliases(db:&Connection,key:&str,min_prefix:usize)->Result<Vec<AnimeAlias>,String>{
    if let Some(id)=lookup_cache_key(db,&format!("anime_aliases_{min_prefix}"),key){
        ANIME_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,||anime_aliases_uncached(db,key,min_prefix),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{anime_aliases_uncached(db,key,min_prefix)}
}

/// For callers with only the compact normalized key, suffix alignment cannot
/// be verified. Allow exact and leading aliases only; never accept a suffix
/// that may begin midway through a longer source word.
pub fn anime_aliases(db:&Connection,key:&str,min_prefix:usize)->Result<Vec<AnimeAlias>,String>{
    Ok(indexed_anime_aliases(db,key,min_prefix)?.into_iter()
        .filter(|alias|key.starts_with(&alias.clean_title)).collect())
}

/// Discover trailing aliases only at real *source-title* word boundaries.
/// Indexed compact aliases alone cannot distinguish "Beyond Dune" from
/// "BeyondDune", so test suffixes of normalized whole source words.
/// Prefix/exact lookup behavior and the Tantivy cache are unchanged.
pub fn anime_aliases_for_title(
    db:&Connection,raw_title:&str,min_prefix:usize,
)->Result<Vec<AnimeAlias>,String>{
    let key=crate::title_normalization::clean_title(raw_title);
    if key.len()<4{return Ok(Vec::new());}
    let mut results=indexed_anime_aliases(db,&key,min_prefix)?;
    if results.iter().any(|alias|!key.starts_with(&alias.clean_title)){
        let words=crate::title_normalization::clean_words(raw_title);
        let mut boundary_suffixes=std::collections::HashSet::new();
        let mut suffix=String::new();
        for word in words.iter().rev(){
            suffix.insert_str(0,word);
            boundary_suffixes.insert(suffix.clone());
        }
        results.retain(|alias|key.starts_with(&alias.clean_title)
            ||(key.ends_with(&alias.clean_title)
                &&boundary_suffixes.contains(&alias.clean_title)));
    }
    // Re-check coverage on accepted candidates. The indexed best alias may
    // have been an illegal suffix and must not authorize a weak prefix.
    if results.first().is_some_and(|best|
        !anime_alias_covers_title(&key,best.clean_title.len())){
        results.clear();
    }
    Ok(results)
}

/// Check all title-identity collisions in one exact postings query. A large
/// IMDb shortlist previously ran a Tantivy query per distinct normalized
/// alias; this stays exhaustive and applies the same positive-year filter
/// as has_duplicate_title_uncached. Small sets keep their single-flight
/// per-key cache, avoiding a multi-term query for common exact matches.
pub fn duplicate_titles(
    db:&Connection,keys:&BTreeSet<String>,
)->Result<std::collections::HashSet<String>,String>{
    let mut duplicates=std::collections::HashSet::new();
    if keys.len()<=4 {
        for key in keys {
            if has_duplicate_title(db,key)?{duplicates.insert(key.clone());}
        }
        return Ok(duplicates);
    }
    let (search,memory_dir)=open_search(db)?;
    let reader=search.reader.searcher();
    for batch in keys.iter().collect::<Vec<_>>().chunks(128){
        let query=TermSetQuery::new(batch.iter()
            .map(|key|Term::from_field_text(search.forward,key)));
        let hits=reader.search(&query,&DocSetCollector).map_err(|e|e.to_string())?;
        let mut first=HashMap::<String,String>::new();
        for addr in hits {
            let doc:tantivy::TantivyDocument=reader.doc(addr)
                .map_err(|e|e.to_string())?;
            if doc.get_first(search.year).and_then(|value|value.as_i64())
                .unwrap_or(0)<=0 {continue;}
            let (Some(title),Some(id))=(
                doc.get_first(search.forward).and_then(|value|value.as_str()),
                doc.get_first(search.id).and_then(|value|value.as_str()))
                else{continue};
            if let Some(previous)=first.get(title){
                if previous.as_str()!=id{duplicates.insert(title.to_owned());}
            }else{first.insert(title.to_owned(),id.to_owned());}
        }
    }
    drop(reader);drop(search);
    if let Some(dir)=memory_dir{let _=std::fs::remove_dir_all(dir);}
    Ok(duplicates)
}

pub fn has_duplicate_title(db:&Connection,key:&str)->Result<bool,String>{
    if let Some(id)=lookup_cache_key(db,"duplicate_title",key){
        DUPLICATE_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,||has_duplicate_title_uncached(db,key),|_|true)
    }else{has_duplicate_title_uncached(db,key)}
}

pub fn anime_candidates(db:&Connection,key:&str)->Result<Vec<String>,String>{
    if let Some(id)=lookup_cache_key(db,"anime_candidate_ids",key){
        ANIME_ID_LOOKUPS.get_or_init(||QueryCache::new(LOOKUP_CAP,LOOKUP_TTL))
            .get_or_compute(id,||anime_candidates_uncached(db,key),
                |rows|rows.len()<=LOOKUP_MAX_ALIASES)
    }else{anime_candidates_uncached(db,key)}
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn warmed_index_handles_simultaneous_readers(){
        let path=std::env::temp_dir().join(format!(
            "matchlab-concurrent-tantivy-{}-{:?}.sqlite3",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_file(&path);
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,
                updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-parallel-readers',0);
            INSERT INTO titles VALUES
                ('tt0000400','Parallel Show','parallelshow',2026,'series','',1,0,'tvSeries');")
            .unwrap();
        warmup(&db).unwrap();
        drop(db);
        let mut workers=Vec::new();
        for _ in 0..8 {
            let path=path.clone();
            workers.push(std::thread::spawn(move||{
                let db=Connection::open(&path).unwrap();
                for _ in 0..16 {
                    let hits=title_aliases_exact(&db,"parallelshow").unwrap();
                    assert_eq!(hits.len(),1);
                    assert_eq!(hits[0].imdb_id,"tt0000400");
                }
            }));
        }
        for worker in workers {worker.join().unwrap();}
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(format!("{}.tantivy",path.display())).unwrap();
    }

    #[test]
    fn title_id_union_preserves_all_aliases_and_deduplicates_repeated_ids(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            INSERT INTO titles VALUES
                ('tt0000001','First Show','firstshow',2020,'series','',1,0,'tvSeries'),
                ('tt0000001','Show One','showone',2020,'series','US',0,1,'tvSeries'),
                ('tt0000002','Second Show','secondshow',2021,'series','',1,0,'tvSeries'),
                ('tt0000003','Other Show','othershow',2022,'series','',1,0,'tvSeries');").unwrap();
        let ids=vec!["tt0000001".into(),"tt0000002".into(),
                     "tt0000001".into(),"tt9999999".into()];
        let found=titles_by_ids(&db,&ids).unwrap();
        let expected=ids.iter().collect::<std::collections::BTreeSet<_>>()
            .into_iter().map(|id|titles_by_ids(&db,&[(*id).clone()]).unwrap())
            .flatten().map(|r|(r.imdb_id,r.title,r.clean,r.year,r.category,
                r.region,r.original,r.ordering,r.media_type))
            .collect::<std::collections::BTreeSet<_>>();
        let actual=found.into_iter().map(|r|(r.imdb_id,r.title,r.clean,r.year,
            r.category,r.region,r.original,r.ordering,r.media_type))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual,expected);
        assert_eq!(actual.len(),3);
        assert!(titles_by_ids(&db,&[]).unwrap().is_empty());
    }

    #[test]
    fn batch_duplicate_aliases_match_single_title_checks(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            INSERT INTO titles VALUES
            ('tt0000001','Shared Title','sharedtitle',2020,'movie','',1,0,'movie'),
            ('tt0000002','Shared Title','sharedtitle',2021,'movie','',1,0,'movie'),
            ('tt0000003','Single Title','singletitle',2019,'movie','',1,0,'movie'),
            ('tt0000003','Single Title','singletitle',2019,'movie','US',0,1,'movie'),
            ('tt0000004','Other Title','othertitle',2018,'movie','',1,0,'movie'),
            ('tt0000005','Unreleased','unreleased',0,'movie','',1,0,'movie'),
            ('tt0000006','Unreleased','unreleased',0,'movie','US',0,1,'movie'),
            ('tt0000007','First Extras','firstextras',2015,'movie','',1,0,'movie'),
            ('tt0000008','Second Extras','secondextras',2015,'movie','',1,0,'movie');")
            .unwrap();
        let keys=["sharedtitle","singletitle","othertitle","unreleased",
                  "firstextras","secondextras","absent"]
            .into_iter().map(str::to_owned).collect::<BTreeSet<_>>();
        let batched=duplicate_titles(&db,&keys).unwrap();
        let expected=keys.iter().filter(|key|
            has_duplicate_title(&db,key).unwrap()).cloned()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(batched,expected);
        assert_eq!(batched,
            std::collections::HashSet::from(["sharedtitle".to_owned()]));
        assert!(duplicate_titles(&db,&BTreeSet::new()).unwrap().is_empty());
    }

    #[test]
    fn multi_imdb_lookup_matches_union_of_individual_index_queries(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,title TEXT,clean_title TEXT,
                title_type TEXT,language TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles VALUES
                ('tt0000001','First Show','firstshow',2020,'series','',1,0,'tvSeries'),
                ('tt0000001','Show One','showone',2020,'series','US',0,1,'tvSeries'),
                ('tt0000002','Second Show','secondshow',2021,'series','',1,0,'tvSeries'),
                ('tt0000003','Other Show','othershow',2022,'series','',1,0,'tvSeries');
            INSERT INTO anidb_titles VALUES
                ('123','Anime Alternate','animealternate','main','en');
            INSERT INTO anime_series_map VALUES('123','tt0000002');").unwrap();
        let ids=BTreeSet::from(["tt0000001".to_owned(),"tt0000002".to_owned()]);
        let mut expected=BTreeSet::new();
        for id in &ids {
            expected.extend(lookup_aliases(&db,AliasLookupKind::ImdbId,id).unwrap());
        }
        let actual=aliases_for_imdb_ids(&db,&ids).unwrap();
        assert_eq!(actual,expected.into_iter().collect::<Vec<_>>());
        assert!(actual.iter().all(|item|
            item.imdb_id.as_deref()!=Some("tt0000003")));
        assert!(actual.iter().any(|item|item.anidb_id.as_deref()==Some("123")));
        assert!(aliases_for_imdb_ids(&db,&BTreeSet::new()).unwrap().is_empty());
    }

    #[test]
    fn disk_cache_key_changes_with_metadata_generation(){
        let root=std::env::temp_dir().join(format!(
            "matchlab-query-generation-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file=root.join("metadata.sqlite3");
        let db=Connection::open(&file).unwrap();
        db.execute_batch("CREATE TABLE metadata_state(source TEXT PRIMARY KEY,
            updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-one',0);").unwrap();
        let one=lookup_cache_key(&db,"exact_title","doctorwho").unwrap();
        assert_eq!(one,lookup_cache_key(&db,"exact_title","doctorwho").unwrap());
        assert_ne!(one,lookup_cache_key(&db,"exact_title","onepiece").unwrap());
        db.execute("UPDATE metadata_state SET updated_at='v3-two'
            WHERE source='tantivy_generation'",[]).unwrap();
        let two=lookup_cache_key(&db,"exact_title","doctorwho").unwrap();
        assert_ne!(one,two);
        assert_ne!(two,lookup_cache_key(&db,"anime_aliases_6","doctorwho").unwrap());
        assert!(lookup_cache_key(&Connection::open_in_memory().unwrap(),
            "exact_title","doctorwho").is_none());
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_does_not_change_tantivy_matching_results(){
        let path=std::env::temp_dir().join(format!(
            "matchlab-search-cache-{}-{:?}.sqlite3",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_file(&path);
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,
                updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-cache-test',0);
            INSERT INTO titles VALUES('tt0056751','Doctor Who','doctorwho',1963,
                'series','',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt0436992','Doctor Who','doctorwho',2005,
                'series','',1,0,'tvSeries');").unwrap();
        warmup(&db).unwrap();
        let first=title_aliases_exact(&db,"doctorwho").unwrap();
        let again=title_aliases_exact(&db,"doctorwho").unwrap();
        assert_eq!(first.len(),2);
        assert_eq!(again.len(),2);
        assert_eq!(first.iter().map(|r|(&r.imdb_id,r.year)).collect::<std::collections::BTreeSet<_>>(),
            again.iter().map(|r|(&r.imdb_id,r.year)).collect::<std::collections::BTreeSet<_>>());
        assert!(has_duplicate_title(&db,"doctorwho").unwrap());
        assert!(has_duplicate_title(&db,"doctorwho").unwrap());
        // Query variants have separate keys; title and prefix searches can
        // cache independently without changing their returned documents.
        assert_eq!(title_aliases(&db,"doctorwho",false).unwrap().len(),2);
        assert_eq!(title_aliases(&db,"doctorwho",false).unwrap().len(),2);
        drop(db);
        let _=std::fs::remove_file(&path);
        let _=std::fs::remove_dir_all(format!("{}.tantivy",path.display()));
    }

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
    fn stale_multi_imdb_aliases_are_not_returned_as_valid_candidates(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1663145','valid title',2020);
            INSERT INTO anidb_titles VALUES('123','shared anime');
            INSERT INTO anime_series_map VALUES('123','tt1663145,tt1679583');").unwrap();
        assert!(anime_aliases(&db,"shared anime",6).unwrap().is_empty(),
            "legacy CSV identifier must not enter anime priority");
        assert!(anime_candidates(&db,"shared anime").unwrap().is_empty(),
            "legacy CSV identifier must not enter IMDb candidate selection");
    }
    #[test]
    fn multi_imdb_aliases_preserve_ambiguity_instead_of_picking_first(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1663145','valid title',2020);
            INSERT INTO anidb_titles VALUES('123','shared anime');
            INSERT INTO anime_series_map VALUES('123','tt1663145');
            INSERT INTO anime_series_map VALUES('123','tt1679583');").unwrap();
        let aliases=anime_aliases(&db,"shared anime",6).unwrap();
        assert_eq!(aliases.len(),2);
        assert_eq!(aliases[0].imdb_id,"tt1663145");
        assert_eq!(aliases[1].imdb_id,"tt1679583");
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
    fn short_anime_alias_cannot_hijack_a_longer_unrelated_title(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year)
                VALUES('tt0434706','monster',2004);
            INSERT INTO anidb_titles VALUES('100','monster');
            INSERT INTO anime_series_map VALUES('100','tt0434706');").unwrap();
        let long=crate::title_normalization::clean_title(
            "Monster The Jeffrey Dahmer Story");
        assert!(anime_aliases(&db,&long,6).unwrap().is_empty(),
            "2004 anime must not match a series with a much longer title");
        let exact=anime_aliases(&db,"monster",6).unwrap();
        assert_eq!(exact.len(),1,"the original exact anime title must still work");
        assert_eq!(exact[0].clean_title,"monster");
        assert!(anime_alias_covers_title("bleachtybw",6),
            "reasonable partial aliases remain eligible");
        assert!(!anime_alias_covers_title(&long,7));
    }
    #[test]
    fn suffix_anime_aliases_are_discovered_without_index_rebuild(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year)
                VALUES('tt0000001','bluearchive',2024);
            INSERT INTO anidb_titles VALUES('100','bluearchive');
            INSERT INTO anime_series_map VALUES('100','tt0000001');").unwrap();
        // Only a real word boundary may expose a trailing AniDB alias.
        let matches=anime_aliases_for_title(&db,"My Favorite BlueArchive",6).unwrap();
        assert_eq!(matches.len(),1);
        assert_eq!(matches[0].clean_title,"bluearchive");
        assert!(anime_aliases_for_title(&db,"MyFavoriteBlueArchive",6)
            .unwrap().is_empty());
        assert!(anime_aliases(&db,"myfavoritebluearchive",6)
            .unwrap().is_empty(),"compact keys cannot prove suffix boundaries");
        assert!(anime_aliases(&db,"monster the jeffrey dahmer story",6)
            .unwrap().is_empty());
    }
    #[test]
    fn anidb_suffix_requires_complete_source_word_boundary(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year)
                VALUES('tt0000001','bluearchive',2024);
            INSERT INTO anidb_titles VALUES('100','bluearchive');
            INSERT INTO anime_series_map VALUES('100','tt0000001');").unwrap();

        for title in ["My Favorite BlueArchive","My.Favorite.BlueArchive",
                      "My_Favorite_BlueArchive"] {
            let rows=anime_aliases_for_title(&db,title,6).unwrap();
            assert_eq!(rows.len(),1,"whole-word suffix: {title}");
        }
        for title in ["MyFavoriteBlueArchive","MyFavoritebluearchive",
                      "FavoriteBlueArchive"] {
            assert!(anime_aliases_for_title(&db,title,6).unwrap().is_empty(),
                "partial-word suffix must be rejected: {title}");
        }
        assert_eq!(anime_aliases_for_title(&db,"BlueArchive",6).unwrap().len(),1,
            "exact anime alias is unaffected");
        assert_eq!(anime_aliases_for_title(&db,"BlueArchive Part Two",6)
            .unwrap().len(),1,"prefix anime alias is unaffected");
        // The same compact key is cached for both forms. Boundaries must be
        // checked AFTER cache lookup and must never leak between requests.
        let accepted=anime_aliases_for_title(&db,"My Favorite BlueArchive",6).unwrap();
        assert_eq!(accepted.len(),1);
        assert!(anime_aliases_for_title(&db,"MyFavoriteBlueArchive",6)
            .unwrap().is_empty());
    }
    #[test]
    fn imdb_aliases_inside_title_are_retrieved_as_full_terms(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Beyond Dune','beyonddune',2020,'movie','',1,0,'movie'),
                ('tt2222222','Mystery','mystery',2020,'movie','',1,0,'movie');")
            .unwrap();
        let hits=title_aliases_inside(&db,"mysterybeyonddune").unwrap();
        assert_eq!(hits.len(),1);
        assert_eq!(hits[0].imdb_id,"tt1111111");
    }

    #[test]
    fn anime_aliases_collect_nested_prefixes_in_one_query() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            CREATE TABLE anime_series_map(anidb_id TEXT,imdb_id TEXT);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt0000001','onepiece',2000);
            INSERT INTO anidb_titles VALUES('100','onepiece');
            INSERT INTO anidb_titles VALUES('200','onepiecealabasta');
            INSERT INTO anidb_titles VALUES('300','ones');
            INSERT INTO anime_series_map VALUES('100','tt0000001');
            INSERT INTO anime_series_map VALUES('200','tt0000001');
            INSERT INTO anime_series_map VALUES('300','tt0000001');").unwrap();
        let results=anime_aliases(&db,"onepiecealabastaremastered",6).unwrap();
        assert_eq!(results.len(),2);
        assert_eq!(results[0].clean_title,"onepiecealabasta");
        assert_eq!(results[1].clean_title,"onepiece");
        assert_eq!(anime_aliases(&db,"ones",6).unwrap()[0].clean_title,"ones");
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
        assert_eq!(title_aliases_exact(&db,"dune").unwrap().len(),2);
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
        assert!(!index_ready(&db),"staging SQLite is not published yet");
        drop(db);
        std::fs::rename(staging,&target).unwrap();
        let published=Connection::open(&target).unwrap();
        warmup(&published).unwrap();
        assert!(index_ready(&published));
        assert_eq!(candidates(&published,"example",None,false,1).unwrap(),vec!["tt1234567"]);
        assert!(completed.is_file());
        drop(published);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_sqlite_readers_keep_their_generation_after_publication() {
        let root=std::env::temp_dir().join(format!("matchlab-generation-overlap-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let target=root.join("metadata.sqlite3");
        let stage=root.join("stage.sqlite3");
        let schema="CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
            title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
            region TEXT DEFAULT '',original INTEGER DEFAULT 1,
            ordering INTEGER DEFAULT 0,type TEXT DEFAULT 'movie');
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);";
        let old=Connection::open(&target).unwrap();
        old.execute_batch(schema).unwrap();
        old.execute_batch("INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt1111111','legacy',2000);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-oldreader',0);").unwrap();
        prepare(&old,&target).unwrap();
        assert_eq!(candidates(&old,"legacy",None,false,1).unwrap(),vec!["tt1111111"]);
        let next=Connection::open(&stage).unwrap();
        next.execute_batch(schema).unwrap();
        next.execute_batch("INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt2222222','modern',2020);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-newreader',0);").unwrap();
        prepare(&next,&target).unwrap();
        drop(next);
        std::fs::rename(&stage,&target).unwrap();
        let current=Connection::open(&target).unwrap();
        for _ in 0..3 {
            assert_eq!(candidates(&old,"legacy",None,false,1).unwrap(),vec!["tt1111111"]);
            assert_eq!(candidates(&current,"modern",None,false,1).unwrap(),vec!["tt2222222"]);
        }
        drop(current);drop(old);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_published_index_is_never_built_during_a_search(){
        let root=std::env::temp_dir().join(format!(
            "matchlab-no-request-rebuild-{}-{:?}",
            std::process::id(),std::thread::current().id()));
        let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db_path=root.join("metadata.sqlite3");
        let db=Connection::open(&db_path).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,clean_title TEXT,year INTEGER,
                title TEXT NOT NULL DEFAULT '',category TEXT NOT NULL DEFAULT 'movie',
                region TEXT NOT NULL DEFAULT '',original INTEGER NOT NULL DEFAULT 1,
                ordering INTEGER NOT NULL DEFAULT 0,type TEXT NOT NULL DEFAULT 'movie');
            CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT,row_count INTEGER);
            INSERT INTO titles(imdb_id,clean_title,year) VALUES('tt4567890','example',2020);
            INSERT INTO metadata_state VALUES('tantivy_generation','v3-no-request-build',0);").unwrap();
        let dir=index_dir(&db_path,"v3-no-request-build");
        assert!(candidates(&db,"example",None,false,1).is_err());
        assert!(!dir.exists(),"HTTP lookups must not start index construction");
        warmup(&db).unwrap();
        assert_eq!(candidates(&db,"example",None,false,1).unwrap(),vec!["tt4567890"]);
        drop(db);
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
