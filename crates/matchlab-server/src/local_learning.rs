//! Offline, native-Rust field-learning worker.
//! It collects independent evidence, trains a bounded online FTRL model,
//! and stores all observations in a single transaction per history batch.
//! Model scores are *shadow-only*: they never authorize regex or SID/ASID.
use rusqlite::{params,Connection,OpenFlags,OptionalExtension};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{env,path::{Path,PathBuf},time::Duration,
    sync::atomic::{AtomicU64,Ordering}};

const POLL_SECONDS:u64=15;
const ACTIVE_BURST_PAUSE_MS:u64=200;
const BUSY_BACKOFF_MS:u64=250;
const MAX_BUSY_BACKOFF_MS:u64=15_000;
// Like StremThru's persistent job queue: consume a limited number of
// durable history IDs per transaction, preserving the committed cursor on
// failures and pacing bursts rather than monopolizing a CPU.
fn batch_size()->i64{
    crate::ml_config::get().training_batch_size
}
fn retry_delay(failures:u32)->Duration{
    Duration::from_millis(
        BUSY_BACKOFF_MS.saturating_mul(1u64<<failures.min(6))
            .min(MAX_BUSY_BACKOFF_MS)
    )
}

const MAX_PATTERNS:i64=2_000;
static JOBS_PROCESSED:AtomicU64=AtomicU64::new(0);
static SQLITE_BUSY_RETRIES:AtomicU64=AtomicU64::new(0);
static JOBS_AVAILABLE:AtomicU64=AtomicU64::new(0);
pub fn queue_status()->Value{
    serde_json::json!({
        "mode":"persistent_sqlite_queue",
        "batch_size":batch_size(),
        "processed_since_start":JOBS_PROCESSED.load(Ordering::Relaxed),
        "available_at_last_scan":JOBS_AVAILABLE.load(Ordering::Relaxed),
        "sqlite_busy_retries":SQLITE_BUSY_RETRIES.load(Ordering::Relaxed),
        "queue_retention_evictions":crate::dashboard_history::ml_queue_evicted(),
        "history_telemetry_dropped":crate::dashboard_history::history_telemetry_dropped(),
        "retry_policy":"exponential_backoff_at_least_once",
        "hot_path_blocking":false,
    })
}

const MIN_EXAMPLES:i64=5;
fn max_examples()->i64{
    // Disk quota is enforced independently using measured SQLite pages.
    // Do not silently shrink a 500k corpus to 200k because of GiB settings.
    crate::ml_config::get().max_examples
}
pub(crate) fn active()->bool{
    crate::ml_config::get().enabled
}
fn digest(raw:&str)->String{format!("{:x}",Sha256::digest(raw.as_bytes()))}
fn normalized(title:&str)->String{
    title.chars().flat_map(char::to_lowercase)
        .map(|c|if c.is_alphanumeric(){c}else{' '})
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}
fn connect(path:&Path)->Result<Connection,String>{
    if let Some(dir)=path.parent(){
        std::fs::create_dir_all(dir).map_err(|e|e.to_string())?;
    }
    let db=Connection::open(path).map_err(|e|e.to_string())?;
    db.busy_timeout(Duration::from_secs(3)).map_err(|e|e.to_string())?;
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS settings(k TEXT PRIMARY KEY,v TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS patterns(
            key TEXT PRIMARY KEY,field_code TEXT NOT NULL,
            distinct_examples INTEGER NOT NULL DEFAULT 0,
            model_score REAL,first_seen INTEGER NOT NULL,last_seen INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS samples(
            key TEXT PRIMARY KEY,pattern_key TEXT NOT NULL,observed_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS pattern_samples_idx ON samples(pattern_key);")
        .map_err(|e|e.to_string())?;
    let model=crate::online_model::OnlineModel::load(&db)?;
    model.publish_status();
    crate::candidate_discovery::publish(&db)?;
    crate::title_learning::init(&db)?;
    crate::title_learning::publish(&db)?;
    Ok(db)
}
fn setting(db:&Connection,key:&str)->Result<Option<String>,String>{
    db.query_row("SELECT v FROM settings WHERE k=?1",[key],|r|r.get(0))
        .optional().map_err(|e|e.to_string())
}
fn put(db:&Connection,key:&str,value:&str)->Result<(),String>{
    db.execute("INSERT INTO settings(k,v) VALUES(?1,?2)
        ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        params![key,value]).map_err(|e|e.to_string())?;
    Ok(())
}
fn count(db:&Connection,table:&str)->Result<i64,String>{
    let sql=match table {
        "patterns"=>"SELECT count(*) FROM patterns",
        "samples"=>"SELECT count(*) FROM samples",
        _=>return Err("unknown statistics table".into())
    };
    db.query_row(sql,[],|r|r.get(0)).map_err(|e|e.to_string())
}
fn pattern(flag:&Value,parsed:&Value,selected:&Value)->Option<(String,String)>{
    let code=flag["code"].as_str()?;
    if !code.starts_with("field_"){return None;}
    let feature=if code=="field_mismatch_year"{
        let (p,s)=(parsed["year"].as_i64()?,selected["year"].as_i64()?);
        if !(1880..=2100).contains(&p)||!(1880..=2100).contains(&s){return None;}
        format!("year_delta:{}",p-s)
    }else if code.starts_with("field_missing_")
        ||code.starts_with("field_overextracted_")
        ||code.starts_with("field_underextracted_")
        ||code.starts_with("field_misassigned_")
        ||code.starts_with("field_incorrect_")
        ||matches!(code,"field_mismatch_codec"|"field_mismatch_quality"|
            "field_mismatch_resolution"){
        // Stable, bounded anomaly kind only. Never store a private fragment
        // of the original title as a rule label.
        flag["signal"].as_str().unwrap_or("unknown").to_owned()
    }else{
        let imdb=selected["imdb_id"].as_str()?;
        if !(9..=10).contains(&imdb.len())||!imdb.starts_with("tt")
            ||!imdb[2..].bytes().all(|b|b.is_ascii_digit()){return None;}
        imdb.to_owned()
    };
    Some((code.to_owned(),digest(&format!("{code}:{feature}"))))
}
/// Dashboard writer maintains an exact O(1) queue count via SQLite
/// triggers. When the learner is behind, count only already acknowledged
/// rows, usually zero or the small bounded cleanup tail, rather than scanning
/// millions of pending jobs every 200ms during catch-up.
fn pending_training_jobs(db:&Connection,after:i64)->Result<i64,String>{
    let total=db.query_row(
        "SELECT total FROM ml_queue_metrics WHERE only_row=1",[],
        |row|row.get::<_,i64>(0));
    match total {
        Ok(total)=>{
            let acknowledged:i64=db.query_row(
                "SELECT COUNT(*) FROM ml_training_queue WHERE id<=?1",
                [after],|row|row.get(0)).map_err(|e|e.to_string())?;
            Ok(total.saturating_sub(acknowledged).max(0))
        },
        // Support old history files before the queue-metrics migration.
        Err(_)=>db.query_row(
            "SELECT COUNT(*) FROM ml_training_queue WHERE id>?1",
            [after],|row|row.get(0)).map_err(|e|e.to_string()),
    }
}
fn scan(history:&Path,metadata:&Path,local:&mut Connection,
    revalidate:bool)->Result<usize,String>{
    // Revalidation is necessary at startup and during scheduled maintenance,
    // not every 15 seconds when nothing in history changed.
    if revalidate||crate::field_learning::revision()==0{
        crate::field_learning::reconcile(local)?;
        crate::tv_learning::reconcile(local)?;
        crate::corrective_learning::reconcile(local)?;
        crate::general_learning::reconcile(local)?;
    }
    let history_db=Connection::open_with_flags(history,OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e|e.to_string())?;
    history_db.busy_timeout(Duration::from_secs(2)).map_err(|e|e.to_string())?;
    let newest:i64=history_db.query_row("SELECT COALESCE(MAX(id),0)
        FROM ml_training_queue",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    let cursor=setting(local,"after_id")?;
    // ml_training_queue only contains requests after ML opt-in. Unlike
    // dashboard history, these are deliberately queued training jobs, so
    // no first-run event may silently vanish while the worker initializes.
    let earliest:i64=history_db.query_row(
        "SELECT COALESCE(MIN(id),0) FROM ml_training_queue",[],
        |r|r.get(0)).map_err(|e|e.to_string())?;
    let mut after=cursor.as_deref().and_then(|v|v.parse::<i64>().ok())
        .unwrap_or_else(||earliest.saturating_sub(1));
    if cursor.is_none(){put(local,"after_id",&after.to_string())?;}
    // Exact queued row count, not an ID difference (IDs have gaps when
    // duplicate release names are coalesced or jobs are acknowledged).
    let queued=pending_training_jobs(&history_db,after)?;
    JOBS_AVAILABLE.store(queued.max(0) as u64,Ordering::Relaxed);
    if newest<after{put(local,"after_id",&newest.to_string())?;return Ok(0);}
    if newest==after{
        if revalidate {
            if let Ok(meta)=Connection::open_with_flags(
                metadata,OpenFlags::SQLITE_OPEN_READ_ONLY){
                crate::learned_regex::reconcile(local,&meta)?;
            }
        }
        return Ok(0);
    }
    let mut stmt=history_db.prepare("SELECT id,title,details
        FROM ml_training_queue
        WHERE id>?1 ORDER BY id ASC LIMIT ?2").map_err(|e|e.to_string())?;
    let rows=stmt.query_map(params![after,batch_size()],|r|Ok((r.get::<_,i64>(0)?,
        r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
        .map_err(|e|e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e|e.to_string())?;
    let processed=rows.len();
    let meta=Connection::open_with_flags(metadata,OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
    // Avoid revalidating every active regex twice on each small batch.
    // Validation runs after the committed batch and on scheduled refresh.
    // Load a committed snapshot every batch. If a batch fails, all evidence,
    // model weights, and the history cursor roll back together.
    let mut model=crate::online_model::OnlineModel::load(local)?;
    let tx=local.transaction().map_err(|e|e.to_string())?;
    let mut evidence=crate::field_learning::count_evidence(&tx)?;
    let mut tv_evidence=crate::tv_learning::count(&tx)?;
    let mut corrective_evidence=crate::corrective_learning::count(&tx)?;
    let mut general_evidence=crate::general_learning::count(&tx)?;
    let mut discovery=crate::candidate_discovery::budget(&tx)?;
    let mut title_evidence=crate::title_learning::budget(&tx)?;
    let mut new_title_observations=0usize;
    let mut samples=count(&tx,"samples")?;
    let mut patterns=count(&tx,"patterns")?;
    let now=chrono::Utc::now().timestamp();
    for (id,title,raw) in rows{
        after=id;
        let Ok(details)=serde_json::from_str::<Value>(&raw)else{continue};
        // Reparse once from raw history using the current trusted Rust
        // parser. Never use completed, potentially obsolete ML responses
        // as the label source; share this immutable base between learners.
        let Ok(canonical_context)=matchlab_parser::parse_title_canonical_context(&title,false)else{
            // A malformed historical release is not allowed to poison the
            // learning cursor and indefinitely replay the entire batch.
            // Never print private torrent names to error logs.
            tracing::debug!("skipping unparseable local learning history row");
            continue;
        };
        let base=Value::Object(canonical_context.result.clone());
        // Flag missing, over- and underextraction *before* treating
        // canonical parser values as model training positives.
        let mut flags=crate::field_audit::audit(
            &title,&base,&details["selected"]);
        let title_signal=crate::title_learning::observe(
            &tx,&title,&base,&details["selected"],&mut title_evidence)?;
        if let Some(signal)=title_signal{
            flags.push(crate::title_learning::review_flag(signal));
            new_title_observations+=1;
        }
        let clean=crate::field_audit::canonical_for_training(&base,&flags);
        // Metamorphic source-handler learning treats the complete canonical
        // parser result as its reference. A proven title-boundary anomaly
        // invalidates that premise, so this example must not become a
        // positive general-handler training label.
        if title_signal.is_none(){
            crate::general_learning::observe(&tx,&title,&canonical_context,
                &mut general_evidence)?;
        }
        // Unlike the advisory ranker, corrective evidence must compare
        // independent raw syntax with the untouched canonical parser.
        crate::corrective_learning::observe(&tx,&title,&base,
            &mut corrective_evidence)?;
        // One title normalization / holdout hash shared by all ML observers.
        let features=crate::online_model::TitleFeatureContext::new(&title);
        model.observe_canonical_fields_with_context(&tx,&title,&clean,&features)?;
        if let Some(signal)=title_signal{
            model.observe_with_context(&tx,&title,"title",signal,&features)?;
        }
        for (field,signal) in crate::tv_learning::observe(
            &tx,&title,&clean,&mut tv_evidence)? {
            model.observe_with_context(&tx,&title,&field,&signal,&features)?;
        }
        if let Some(ref metadata)=meta{
            crate::learned_regex::observe(&tx,metadata,&title,&details,&base)?;
        }
        for (field,signal) in crate::field_learning::observe_with_budget(
            &tx,&title,&details,&clean,&mut evidence)?{
            model.observe_with_context(&tx,&title,&field,&signal,&features)?;
        }
        crate::candidate_discovery::observe_with_context(&tx,&title,&clean,
            &model,&mut discovery,&features)?;
        // Several field errors from the same torrent share one normalized
        // source identity. Calculate it lazily and only once per title.
        let mut normalized_source=None::<String>;
        for flag in flags{
            let Some((code,group))=pattern(&flag,&base,&details["selected"])else{continue};
            if samples>=max_examples(){continue;}
            let source=normalized_source.get_or_insert_with(||normalized(&title));
            let sample=digest(&format!("{group}:{source}"));
            // The PRIMARY KEY is the dedup check. No preliminary SELECT
            // and no per-observation transactions.
            let known:i64=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM patterns WHERE key=?1)",
                [&group],|r|r.get(0)).map_err(|e|e.to_string())?;
            if known==0&&patterns>=MAX_PATTERNS{continue;}
            let inserted=tx.execute(
                "INSERT OR IGNORE INTO samples(key,pattern_key,observed_at)
                 VALUES(?1,?2,?3)",params![sample,group,now])
                .map_err(|e|e.to_string())?;
            if inserted==0{continue;}
            let n:i64=tx.query_row(
                "INSERT INTO patterns(key,field_code,distinct_examples,first_seen,last_seen)
                 VALUES(?1,?2,1,?3,?3)
                 ON CONFLICT(key) DO UPDATE SET
                   distinct_examples=distinct_examples+1,last_seen=excluded.last_seen
                 RETURNING distinct_examples",
                params![group,code,now],|r|r.get(0)).map_err(|e|e.to_string())?;
            samples+=1;
            if known==0{patterns+=1;}
            if n>=MIN_EXAMPLES{
                let field=flag["field"].as_str().unwrap_or("");
                let signal=flag["signal"].as_str().unwrap_or("");
                if let Some(score)=model.score(field,signal,&title){
                    tx.execute("UPDATE patterns SET model_score=?1 WHERE key=?2",
                        params![score,group]).map_err(|e|e.to_string())?;
                }
            }
        }
    }
    // Persist both trained weights and holdout-only evaluation counters
    // atomically with the consumed history cursor.
    model.save(&tx)?;
    // The cursor and learned model are committed in the same transaction.
    put(&tx,"after_id",&after.to_string())?;
    tx.commit().map_err(|e|e.to_string())?;
    JOBS_PROCESSED.fetch_add(processed as u64,Ordering::Relaxed);
    JOBS_AVAILABLE.store(queued.saturating_sub(processed as i64).max(0) as u64,
        Ordering::Relaxed);
    model.publish_status();
    crate::candidate_discovery::publish(local)?;
    if new_title_observations>0{
        crate::title_learning::publish(local)?;
    }
    if let Some(ref metadata)=meta{
        crate::learned_regex::reconcile(local,metadata)?;
    }
    crate::field_learning::reconcile(local)?;
    crate::tv_learning::reconcile(local)?;
    crate::corrective_learning::reconcile(local)?;
    crate::general_learning::reconcile(local)?;
    Ok(processed)
}
/// Acknowledgment is the model's committed cursor; deleting durable
/// jobs is only disk cleanup. If this DELETE is busy or interrupted, jobs
/// remain on disk but cannot be applied twice to the trained model.
fn cleanup_committed_jobs(history:&Path,local:&Connection)->Result<usize,String>{
    let Some(cursor)=setting(local,"after_id")?else{return Ok(0);};
    let after=cursor.parse::<i64>().unwrap_or(0);
    if after<=0{return Ok(0);}
    let db=Connection::open(history).map_err(|e|e.to_string())?;
    db.busy_timeout(Duration::from_millis(50))
        .map_err(|e|e.to_string())?;
    db.execute("DELETE FROM ml_training_queue WHERE id IN (
        SELECT id FROM ml_training_queue
        WHERE id<=?1 ORDER BY id ASC LIMIT 512
    )",params![after]).map_err(|e|e.to_string())
}
pub fn verify_native_model()->Result<(),String>{
    let db=Connection::open_in_memory().map_err(|e|e.to_string())?;
    let mut model=crate::online_model::OnlineModel::load(&db)?;
    for n in 0..64{
        model.observe(&db,&format!("Test{n}.2160p.WEB-DL"),"resolution","2160p")?;
    }
    let yes=model.score("resolution","2160p","Other.2160p.WEB-DL");
    let no=model.score("codec","2160p","Other.2160p.WEB-DL");
    match (yes,no) {
        (Some(a),Some(b)) if a>b=>Ok(()),
        _=>Err("native FTRL self-test failed".into())
    }
}
/// Explicit, offline synthetic throughput probe. Never run automatically
/// during serving; actual production decisions require representative real
/// releases and matching-accuracy regression measurements separately.
pub fn benchmark_native_model()->Result<Value,String>{
    let db=Connection::open_in_memory().map_err(|e|e.to_string())?;
    let mut model=crate::online_model::OnlineModel::load(&db)?;
    const N:usize=10_000;
    let start=std::time::Instant::now();
    // Mirror actual worker's batched transaction rather than benchmarking
    // 10,000 standalone SQLite fsyncs as if they were model training.
    let tx=db.unchecked_transaction().map_err(|e|e.to_string())?;
    for n in 0..N{
        let (field,signal)=if n%2==0{("resolution","2160p")}
            else{("codec","x265")};
        let title=format!("Benchmark.Work.{n}.2160p.x265.WEB-DL");
        model.observe(&tx,&title,field,signal)?;
    }
    tx.commit().map_err(|e|e.to_string())?;
    let training_ms=start.elapsed().as_secs_f64()*1000.0;
    let start=std::time::Instant::now();
    for n in 0..N{
        let title=format!("Probe.Work.{n}.2160p.x265.WEB-DL");
        let _=model.score("resolution","2160p",&title);
    }
    let inference_ms=start.elapsed().as_secs_f64()*1000.0;
    let names=[
        "Film.Name.2024.2160p.HEVC.WEB-DL",
        "TV.Series.S02E07.1080p.x264.WEBRip",
        "Anime.Name.EPIS300.720p.AAC",
        "Series.Name.Season.3.Episode.9.1080p",
    ];
    let start=std::time::Instant::now();
    for n in 0..N{
        matchlab_parser::parse_title(names[n%names.len()],false)
            .map_err(|e|e.to_string())?;
    }
    let parse_ms=start.elapsed().as_secs_f64()*1000.0;
    // Measure the ML-only full-parser verification path as well. The first
    // replay compiles a candidate generation; subsequent replays must reuse
    // that exact immutable generation without changing any parsed fields.
    let replay_titles=names.iter().map(|name|(*name).to_owned()).collect::<Vec<_>>();
    let cold_start=std::time::Instant::now();
    let expected=matchlab_parser::replay_handler_rules(&[],&[],&[],&replay_titles)
        .map_err(|e|e.to_string())?;
    let replay_cold_ms=cold_start.elapsed().as_secs_f64()*1000.0;
    const REPLAY_ITERATIONS:usize=16;
    let warm_start=std::time::Instant::now();
    for _ in 0..REPLAY_ITERATIONS{
        let actual=matchlab_parser::replay_handler_rules(&[],&[],&[],&replay_titles)
            .map_err(|e|e.to_string())?;
        if actual!=expected{return Err("compiled ML replay changed parser output".into());}
    }
    let replay_warm_average_ms=warm_start.elapsed().as_secs_f64()*1000.0
        /REPLAY_ITERATIONS as f64;
    Ok(serde_json::json!({
        "benchmark":"synthetic_offline_single_thread",
        "warning":"not a representative dataset or a parser-accuracy claim",
        "samples_per_stage":N,
        "ftrl_train_with_sqlite_dedup_ms":training_ms,
        "ftrl_inference_ms":inference_ms,
        "canonical_parse_ms":parse_ms,
        "regex_replay_first_ms":replay_cold_ms,
        "regex_replay_cached_average_ms":replay_warm_average_ms,
        "compiled_replay_cache":matchlab_parser::replay_cache_status(),
        "ftrl_parameter_bytes":128*1024,
        "requires_real_arm64_corpus_validation":true
    }))
}
pub fn start(history:PathBuf,metadata:PathBuf){
    if !active(){return;}
    let state=PathBuf::from(env::var("MATCHLAB_ML_STATE_FILE")
        .unwrap_or_else(|_|"/data/local-learning.sqlite3".into()));
    let worker=std::thread::Builder::new()
        .name("matchlab-local-learning".into()).spawn(move||{
            let mut maintained=std::time::Instant::now()
                .checked_sub(Duration::from_secs(300))
                .unwrap_or_else(std::time::Instant::now);
            // Reuse the SQLite writer connection: repeated open/WAL setup
            // and schema creation every polling interval waste host CPU.
            let mut connection:Option<Connection>=None;
            let mut legacy_captured=false;
            let mut rule_observer=crate::learning_rule_log::Observer::default();
            let mut consecutive_busy=0u32;
            let mut last_queue_cleanup=std::time::Instant::now()
                .checked_sub(Duration::from_secs(15))
                .unwrap_or_else(std::time::Instant::now);
            loop{
                if connection.is_none(){
                    match connect(&state){
                        Ok(db)=>{connection=Some(db);legacy_captured=false;},
                        Err(error)=>{
                            if error.contains("database is locked"){
                                SQLITE_BUSY_RETRIES.fetch_add(1,Ordering::Relaxed);
                                consecutive_busy=consecutive_busy.saturating_add(1);
                                if consecutive_busy==1||consecutive_busy%8==0{
                                    tracing::debug!(failures=consecutive_busy,
                                        "local learning database busy; queued work deferred");
                                }
                                std::thread::sleep(retry_delay(consecutive_busy));
                            }else{
                                tracing::debug!(%error,"local learning database unavailable");
                                std::thread::sleep(Duration::from_secs(POLL_SECONDS));
                            }
                            continue;
                        }
                    }
                }
                let mut processed=0usize;
                if let Some(ref mut db)=connection{
                    if !legacy_captured{
                        match crate::community_rule_archive::snapshot_legacy(db){
                            Ok(_)=>legacy_captured=true,
                            Err(error)=>tracing::debug!(%error,
                                "historical regex migration skipped"),
                        }
                    }
                    let refresh=maintained.elapsed()>=Duration::from_secs(300);
                    let result=(||->Result<usize,String>{
                        if refresh{
                            // The cache actor owns a different SQLite file;
                            // this call only schedules background maintenance.
                            crate::learning_cache::maintain(db,&metadata)?;
                            maintained=std::time::Instant::now();
                        }
                        let n=scan(&history,&metadata,db,refresh)?;
                        if refresh{
                            let meta=Connection::open_with_flags(
                                &metadata,OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
                            crate::community_rule_archive::snapshot(db,meta.as_ref())?;
                            // Local evidence and durable cache have separate
                            // quotas and writers. Archive rules before any
                            // obsolete-evidence eviction under disk pressure.
                            crate::learning_cache::maintain_learning(db)?;
                        }
                        crate::learning_rule_log::after_cycle(db,&mut rule_observer)?;
                        Ok(n)
                    })();
                    match result{
                        Ok(n)=>{
                            processed=n;
                            consecutive_busy=0;
                        },
                        Err(error) if error.contains("database is locked")=>{
                            SQLITE_BUSY_RETRIES.fetch_add(1,Ordering::Relaxed);
                            consecutive_busy=consecutive_busy.saturating_add(1);
                            if consecutive_busy==1||consecutive_busy%8==0{
                                tracing::debug!(failures=consecutive_busy,
                                    "local learning database busy; queued work will retry");
                            }
                            // Never advance the committed queue cursor on
                            // SQLITE_BUSY: the batch is retried idempotently.
                            std::thread::sleep(retry_delay(consecutive_busy));
                            continue;
                        },
                        Err(error)=>{
                            tracing::debug!(%error,"local learning cycle skipped");
                            connection=None;
                            std::thread::sleep(Duration::from_secs(POLL_SECONDS));
                            continue;
                        }
                    }
                }
                if last_queue_cleanup.elapsed()>=Duration::from_secs(10){
                    if let Some(db)=connection.as_ref(){
                        // The history writer is independent. Never delay a
                        // parser request (or fail the learner) on cleanup.
                        let _=cleanup_committed_jobs(&history,db);
                    }
                    last_queue_cleanup=std::time::Instant::now();
                }
                // Short bursts when the persistent queue has more work;
                // otherwise idle until the next normal polling interval.
                let pause=if processed>=batch_size() as usize{
                    Duration::from_millis(ACTIVE_BURST_PAUSE_MS)
                }else{Duration::from_secs(POLL_SECONDS)};
                std::thread::sleep(pause);
            }
        });
    if let Err(error)=worker{tracing::debug!(%error,"failed to start local learning");}
}
#[cfg(test)]
mod tests{
    use super::*;
    use serde_json::json;
    #[test]
    fn pending_jobs_use_trigger_count_and_exclude_acked_rows(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE ml_training_queue(
            id INTEGER PRIMARY KEY,title_hash TEXT UNIQUE,title TEXT,details TEXT);
            CREATE TABLE ml_queue_metrics(only_row INTEGER PRIMARY KEY,total INTEGER);
            INSERT INTO ml_queue_metrics VALUES(1,0);
            CREATE TRIGGER ml_queue_count_insert AFTER INSERT ON ml_training_queue BEGIN
                UPDATE ml_queue_metrics SET total=total+1 WHERE only_row=1;
            END;
            CREATE TRIGGER ml_queue_count_delete AFTER DELETE ON ml_training_queue BEGIN
                UPDATE ml_queue_metrics SET total=total-1 WHERE only_row=1;
            END;").unwrap();
        for id in 1..=100{
            db.execute("INSERT INTO ml_training_queue VALUES(?1,?2,'a','{}')",
                params![id,format!("{id}")]).unwrap();
        }
        assert_eq!(pending_training_jobs(&db,0).unwrap(),100);
        assert_eq!(pending_training_jobs(&db,73).unwrap(),27);
        db.execute("DELETE FROM ml_training_queue WHERE id<=73",[]).unwrap();
        assert_eq!(pending_training_jobs(&db,73).unwrap(),27);
        db.execute("DELETE FROM ml_training_queue WHERE id>90",[]).unwrap();
        assert_eq!(pending_training_jobs(&db,73).unwrap(),17);
    }
    #[test]
    fn stremthru_style_queue_pacing_is_bounded(){
        assert_eq!(retry_delay(0),Duration::from_millis(250));
        assert_eq!(retry_delay(1),Duration::from_millis(500));
        assert!(retry_delay(100)<=Duration::from_secs(15));
        assert!(ACTIVE_BURST_PAUSE_MS<POLL_SECONDS*1000);
    }
    #[test]
    fn year_offset_patterns_are_hashed_not_saved_as_films(){
        let d=json!({"parsed":{"year":2024},"selected":{"year":2020}});
        let a=json!({"code":"field_mismatch_year"});
        let (code,key)=pattern(&a,&d["parsed"],&d["selected"]).unwrap();
        assert_eq!(code,"field_mismatch_year");
        assert_eq!(key.len(),64);
    }
    #[test]
    fn episode_gap_patterns_separate_distinct_imdb_works(){
        let a=json!({"code":"field_unresolved_episode"});
        let d=json!({"selected":{"imdb_id":"tt1234567"}});
        let e=json!({"selected":{"imdb_id":"tt7654321"}});
        assert_ne!(pattern(&a,&d["parsed"],&d["selected"]).unwrap().1,
            pattern(&a,&e["parsed"],&e["selected"]).unwrap().1);
    }
    #[test]
    fn extraction_review_patterns_do_not_require_a_matched_imdb(){
        let unmatched=Value::Null;
        let parsed=json!({"title":"Show S02E07","episodes":[7]});
        for (code,signal) in [
            ("field_overextracted_title","release_marker_in_title"),
            ("field_underextracted_title","unparsed_title_prefix"),
            ("field_underextracted_episodes","explicit_episode_range_incomplete"),
            ("field_overextracted_episodes","single_episode_multiple_values"),
            ("field_mismatch_codec","explicit_codec_disagrees"),
            ("field_mismatch_quality","explicit_quality_disagrees"),
            ("field_mismatch_resolution","explicit_resolution_disagrees"),
            ("field_misassigned_season_episode","explicit_season_episode_swapped"),
            ("field_misassigned_season","episode_marker_used_as_season"),
            ("field_misassigned_episode","season_marker_used_as_episode"),
            ("field_incorrect_season","explicit_season_disagrees"),
            ("field_incorrect_episode","explicit_episode_disagrees"),
        ] {
            let diagnostic=json!({"code":code,"signal":signal,"field":"title"});
            let found=pattern(&diagnostic,&parsed,&unmatched).unwrap();
            assert_eq!(found.0,code);
            assert_eq!(found.1.len(),64);
        }
    }
    #[test]
    fn native_inference_is_finite_and_requires_training(){
        verify_native_model().unwrap();
    }
}
