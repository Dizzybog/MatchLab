//! Measure a local MatchLab child using deterministic synthetic SQLite fixtures.
//!
//! No public data is downloaded. Every metadata source points to a reserved,
//! non-listening loopback port. Existing fixtures carry the current bundled
//! override marker and a recent full_refresh timestamp. This is an index
//! rebuild and process restart benchmark, not an uncached-disk or full-catalog
//! import test.
use matchlab_community::json::{dumps, Dump, Json, Object};
use matchlab_community::obj;
use matchlab_community::util::{sha256_hex, Result};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RELEASE: &str = "Benchmark.Aurora.2020.1080p.WEB-DL.x264.mkv";
pub const EXPECTED_SID: &str = "tt9000001";

const SCHEMA: &str = "
CREATE TABLE titles(imdb_id TEXT NOT NULL,title TEXT NOT NULL,clean_title TEXT NOT NULL,
    year INTEGER NOT NULL,category TEXT NOT NULL,region TEXT,
    original INTEGER NOT NULL DEFAULT 0,ordering INTEGER NOT NULL DEFAULT 0,
    type TEXT NOT NULL DEFAULT '',
    PRIMARY KEY(imdb_id,clean_title,year,region,ordering));
CREATE TABLE seasons(imdb_id TEXT PRIMARY KEY,max_season INTEGER NOT NULL);
CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER NOT NULL DEFAULT 0);
CREATE TABLE season_episodes(imdb_id TEXT NOT NULL,season INTEGER NOT NULL,
    episode_count INTEGER NOT NULL,max_episode INTEGER NOT NULL,PRIMARY KEY(imdb_id,season));
CREATE TABLE anime_ids(group_id TEXT NOT NULL,type TEXT NOT NULL,value TEXT NOT NULL,
    PRIMARY KEY(group_id,type,value));
CREATE TABLE anime_series_map(imdb_id TEXT NOT NULL,anidb_id TEXT NOT NULL,
    tvdb_id TEXT NOT NULL DEFAULT '',trakt_season INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
CREATE TABLE episode_map(anidb_id TEXT NOT NULL,tvdb_id TEXT NOT NULL,
    anidb_season INTEGER NOT NULL,tvdb_season INTEGER NOT NULL,start INTEGER NOT NULL,
    end INTEGER NOT NULL,offset INTEGER NOT NULL DEFAULT 0,map TEXT NOT NULL DEFAULT '');
CREATE TABLE anidb_titles(anidb_id TEXT NOT NULL,title TEXT NOT NULL,clean_title TEXT NOT NULL,
    title_type TEXT NOT NULL,language TEXT NOT NULL,
    PRIMARY KEY(anidb_id,clean_title,title_type,language));
CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
CREATE INDEX anime_series_by_anidb ON anime_series_map(anidb_id);
CREATE INDEX episode_lookup_idx ON episode_map(anidb_id,anidb_season,start,end);
CREATE INDEX episode_by_tvdb ON episode_map(tvdb_id,tvdb_season,offset,start);
CREATE INDEX episode_by_anidb_order ON episode_map(anidb_id,tvdb_season,offset,start);
";

/// Salt the server appends to the bundled override (see
/// `bundled_animelists_override_marker`); a mismatch forces a refresh.
pub const OVERRIDE_SALT: &str = "generic-implicit-animelists-v1";

/// FNV-1a over the bundled anime-list override, as the server records it.
pub fn override_marker(root: &Path) -> Result<String> {
    let path = root.join("data/anime-list.override.xml");
    let mut bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    bytes.extend_from_slice(OVERRIDE_SALT.as_bytes());
    let mut value: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in bytes {
        value = (value ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    Ok(format!("animelists_override_{value:016x}"))
}

/// Bijective base-26 letters: 0 → a, 25 → z, 26 → aa.
pub fn letters(mut number: i64) -> String {
    let mut result = String::new();
    loop {
        result.insert(0, char::from(b'a' + (number % 26) as u8));
        number = number / 26 - 1;
        if number < 0 {
            return result;
        }
    }
}

pub fn create_fixture(root: &Path, directory: &Path, aliases: i64) -> Result<PathBuf> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let path = directory.join("metadata.sqlite3");
    let mut db = rusqlite::Connection::open(&path).map_err(|error| error.to_string())?;
    let sql = |error: rusqlite::Error| error.to_string();
    db.execute_batch(SCHEMA).map_err(sql)?;
    let transaction = db.transaction().map_err(sql)?;
    {
        let mut insert = transaction.prepare("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)").map_err(sql)?;
        for index in 0..aliases {
            let (title, clean, year) = if index == 0 {
                ("Benchmark Aurora".to_owned(), "benchmarkaurora".to_owned(), 2020)
            } else {
                (format!("Benchmark Catalog {}", letters(index)), format!("benchmarkcatalog{}", letters(index)), 1980 + index % 46)
            };
            insert
                .execute(rusqlite::params![format!("tt{:07}", 9_000_001 + index), title, clean, year, "movie", "", 1, 0, "movie"])
                .map_err(sql)?;
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string();
        let mut state = transaction.prepare("INSERT INTO metadata_state VALUES(?,?,?)").map_err(sql)?;
        for (source, updated, rows) in [
            ("imdb".to_owned(), stamp.clone(), aliases),
            ("full_refresh".to_owned(), stamp.clone(), aliases),
            (override_marker(root)?, stamp, 0),
            ("tantivy_generation".to_owned(), format!("v3-startup-benchmark-{aliases}"), 0),
        ] {
            state.execute(rusqlite::params![source, updated, rows]).map_err(sql)?;
        }
    }
    transaction.commit().map_err(sql)?;
    db.execute_batch("ANALYZE").map_err(sql)?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(sql)?;
    if integrity != "ok" {
        return Err("generated SQLite fixture failed integrity_check".into());
    }
    Ok(path)
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().no_proxy().build().expect("HTTP client")
}

/// One local request: status, duration and decoded body.
pub fn request(client: &reqwest::blocking::Client, base: &str, endpoint: &str, payload: Option<&Json>, timeout: f64) -> Result<Json> {
    let started = Instant::now();
    let url = format!("{base}{endpoint}");
    let builder = match payload {
        Some(payload) => client.post(url).header("Content-Type", "application/json").body(dumps(payload, Dump::DEFAULT)),
        None => client.get(url),
    };
    let response = builder.timeout(Duration::from_secs_f64(timeout)).send().map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let raw = response.bytes().map_err(|error| error.to_string())?;
    let body = Json::parse_bytes(&raw).unwrap_or_else(|_| Json::from(String::from_utf8_lossy(&raw).into_owned()));
    Ok(obj! {"status" => Json::Int(i128::from(status)), "duration_ms" => Json::Float(started.elapsed().as_secs_f64() * 1000.0), "body" => body})
}

fn ms(duration: Duration) -> Json {
    Json::Float(duration.as_secs_f64() * 1000.0)
}

pub struct Polling {
    pub timeout: f64,
    pub poll: f64,
}

fn wait_health(child: &Mutex<Child>, base: &str, endpoint: &str, started: Instant, polling: &Polling, stop: Option<&AtomicBool>) -> Json {
    let client = client();
    let deadline = started + Duration::from_secs_f64(polling.timeout);
    let mut last = Json::Null;
    while Instant::now() < deadline && !stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
        if let Ok(Some(status)) = child.lock().map(|mut child| child.try_wait().ok().flatten()) {
            let code = status.code().map_or("signal".into(), |code| code.to_string());
            return obj! {"error" => format!("child exited with status {code}"), "last" => last};
        }
        if let Ok(response) = request(&client, base, endpoint, None, 1.0) {
            let ready = response.field("status") == &Json::Int(200);
            last = response;
            if ready {
                return obj! {"spawn_to_ms" => ms(started.elapsed()), "response" => last};
            }
        }
        std::thread::sleep(Duration::from_secs_f64(polling.poll));
    }
    obj! {"spawn_to_ms" => Json::Null, "last" => last, "observation_seconds" => Json::Float(polling.timeout)}
}

pub fn rss_kib(pid: u32) -> Option<i128> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| line.strip_prefix("VmHWM:")).and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

fn median(ordered: &[f64]) -> f64 {
    let middle = ordered.len() / 2;
    if ordered.len() % 2 == 1 { ordered[middle] } else { (ordered[middle - 1] + ordered[middle]) / 2.0 }
}

pub fn distribution(values: &[f64]) -> Json {
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let n = ordered.len();
    let p95 = ordered[((95 * n).div_ceil(100)).saturating_sub(1)];
    obj! {"n" => n, "min_ms" => Json::Float(ordered[0]), "median_ms" => Json::Float(median(&ordered)),
          "p95_ms" => Json::Float(p95), "max_ms" => Json::Float(ordered[n - 1])}
}

fn files_under(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
}

pub fn index_state(metadata: &Path) -> Json {
    let root = PathBuf::from(format!("{}.tantivy", metadata.display()));
    let mut files = Vec::new();
    files_under(&root, &mut files);
    files.sort();
    let bytes: u64 = files.iter().filter_map(|path| path.metadata().ok()).map(|meta| meta.len()).sum();
    let markers: Object = files
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == ".matchlab-complete"))
        .map(|path| {
            let name = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().into_owned();
            (name, Json::from(std::fs::read_to_string(path).unwrap_or_default().trim().to_owned()))
        })
        .collect();
    obj! {"exists" => root.exists(), "bytes" => Json::Int(i128::from(bytes)), "complete_markers" => Json::Obj(markers)}
}

pub fn summarize(runs: &[Json]) -> Vec<Json> {
    let mut groups: std::collections::BTreeSet<(String, i128)> = std::collections::BTreeSet::new();
    for run in runs {
        groups.insert((run.field("scenario").as_str().unwrap_or_default().to_owned(), run.field("imdb_alias_rows").as_int().unwrap_or(0)));
    }
    let mut result = Vec::new();
    for (scenario, aliases) in groups {
        let group: Vec<&Json> = runs
            .iter()
            .filter(|run| run.field("scenario").str_eq(&scenario) && run.field("imdb_alias_rows") == &Json::Int(aliases))
            .collect();
        let good: Vec<&&Json> = group.iter().filter(|run| run.get("error").is_none()).collect();
        let mut summary = obj! {"scenario" => scenario.as_str(), "imdb_alias_rows" => Json::Int(aliases),
                                "runs" => group.len(), "successful_runs" => good.len()};
        for (label, outer, inner) in [
            ("spawn_to_live", "live", "spawn_to_ms"),
            ("spawn_to_ready", "ready", "spawn_to_ms"),
            ("first_parse", "first_parse", "duration_ms"),
            ("first_match", "first_match", "duration_ms"),
            ("warm_parse_process_medians", "warm_parse", "median_ms"),
            ("warm_match_process_medians", "warm_match", "median_ms"),
        ] {
            let values: Vec<f64> = good.iter().filter_map(|run| run.field(outer).field(inner).as_number()).collect();
            if !values.is_empty()
                && let Json::Obj(map) = &mut summary {
                    map.insert(label.into(), distribution(&values));
                }
        }
        let mut peaks: Vec<f64> = good.iter().filter_map(|run| run.field("peak_rss_kib").as_number()).collect();
        if !peaks.is_empty() {
            peaks.sort_by(f64::total_cmp);
            let number = |value: f64| if value.fract() == 0.0 { Json::Int(value as i128) } else { Json::Float(value) };
            if let Json::Obj(map) = &mut summary {
                map.insert("peak_rss_kib".into(), obj! {"min" => number(peaks[0]), "median" => number(median(&peaks)),
                                                         "max" => number(peaks[peaks.len() - 1])});
            }
        }
        result.push(summary);
    }
    result
}

pub struct Options {
    pub root: PathBuf,
    pub binary: PathBuf,
    pub samples: usize,
    pub first_request: String,
    pub timeout: f64,
    pub poll_seconds: f64,
    pub missing_observe_seconds: f64,
}

fn set(row: &mut Json, key: &str, value: Json) {
    if let Json::Obj(map) = row {
        map.insert(key.to_owned(), value);
    }
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    Ok(listener.local_addr().map_err(|error| error.to_string())?.port())
}

fn probe_parser(client: &reqwest::blocking::Client, base: &str, samples: usize, started: Instant, result: &mut Json) -> Result<()> {
    let order = |result: &mut Json, step: String| {
        if let Json::Obj(map) = result
            && let Some(Json::Arr(items)) = map.get_mut("application_request_order") {
                items.push(Json::from(step));
            }
    };
    order(result, "first_parse".into());
    let payload = obj! {"title" => RELEASE};
    let mut first = request(client, base, "/parse", Some(&payload), 10.0)?;
    if first.field("status") != &Json::Int(200) {
        return Err(format!("parser unavailable: {}", dumps(&first, Dump::DEFAULT)));
    }
    set(&mut first, "spawn_to_complete_ms", ms(started.elapsed()));
    let body = first.field("body").clone();
    set(result, "first_parse", first);
    order(result, format!("warm_parse_{samples}"));
    let mut timings = Vec::new();
    for _ in 0..samples {
        let sample = request(client, base, "/parse", Some(&payload), 10.0)?;
        if sample.field("status") != &Json::Int(200) || sample.field("body") != &body {
            return Err("a warm synthetic parse changed fields or status".into());
        }
        timings.push(sample.field("duration_ms").as_number().unwrap_or(0.0));
    }
    set(result, "warm_parse", distribution(&timings));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run_child(
    options: &Options,
    metadata: &Path,
    run_dir: &Path,
    dead_port: u16,
    scenario: &str,
    aliases: i64,
    iteration: usize,
) -> Result<Json> {
    std::fs::create_dir_all(run_dir).map_err(|error| error.to_string())?;
    let port = free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    let log_path = run_dir.join("server.log");
    let mut result = obj! {
        "scenario" => scenario, "imdb_alias_rows" => Json::from(aliases), "iteration" => iteration,
        "metadata" => metadata.display().to_string(),
        "metadata_bytes" => Json::Int(metadata.metadata().map_or(0, |meta| i128::from(meta.len()))),
        "index_before" => index_state(metadata), "log" => log_path.display().to_string(),
        "first_request" => options.first_request.as_str(), "application_request_order" => Json::Arr(vec![]),
        "first_match_has_prior_parse_request" => options.first_request == "parse",
    };
    let mut command = Command::new(&options.binary);
    command.current_dir(&options.root).env_clear();
    for (key, value) in std::env::vars_os() {
        if !key.to_string_lossy().starts_with("MATCHLAB_") {
            command.env(key, value);
        }
    }
    let urls: Vec<String> = (0..7).map(|number| format!("https://127.0.0.1:{dead_port}/{number}")).collect();
    command
        .env("MATCHLAB_DB", metadata)
        .env("MATCHLAB_HISTORY_DB", run_dir.join("history.sqlite3"))
        .env("MATCHLAB_HOST", format!("127.0.0.1:{port}"))
        .env("MATCHLAB_ML_SIZE_GB", "0")
        .env("MATCHLAB_STRICT_MODE", "true")
        .env("MATCHLAB_MAX_CONCURRENT_MATCHES", "2")
        .env("RUST_LOG", "matchlab_server=info")
        .env("MATCHLAB_METADATA_URLS", urls.join(","));
    for variable in ["NO_PROXY", "no_proxy"] {
        let inherited = std::env::var(variable).unwrap_or_default();
        let parts: Vec<&str> = [inherited.as_str(), "127.0.0.1", "localhost"].into_iter().filter(|part| !part.is_empty()).collect();
        command.env(variable, parts.join(","));
    }
    let log = std::fs::File::create(&log_path).map_err(|error| error.to_string())?;
    let log_err = log.try_clone().map_err(|error| error.to_string())?;
    let started = Instant::now();
    let child = command.stdin(Stdio::null()).stdout(log).stderr(log_err).spawn().map_err(|error| format!("spawn: {error}"))?;
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(Mutex::new(Vec::<i128>::new()));
    let ready_timeout = if aliases == 0 { options.missing_observe_seconds } else { options.timeout };
    let ready_thread = {
        let (child, stop, base) = (Arc::clone(&child), Arc::clone(&stop), base.clone());
        let polling = Polling { timeout: ready_timeout, poll: options.poll_seconds };
        std::thread::spawn(move || wait_health(&child, &base, "/health/ready", started, &polling, Some(&stop)))
    };
    let memory_thread = {
        let (stop, peak) = (Arc::clone(&stop), Arc::clone(&peak));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(value) = rss_kib(pid) {
                    peak.lock().map(|mut peak| peak.push(value)).ok();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let client = client();
    let outcome = (|| -> Result<()> {
        let live = wait_health(&child, &base, "/health/live", started, &Polling { timeout: options.timeout, poll: options.poll_seconds }, None);
        let alive = !live.field("spawn_to_ms").is_null() && live.get("spawn_to_ms").is_some();
        set(&mut result, "live", live.clone());
        if !alive {
            return Err(format!("local child did not become live: {}", dumps(&live, Dump::DEFAULT)));
        }
        if options.first_request == "parse" {
            probe_parser(&client, &base, options.samples, started, &mut result)?;
        }
        let ready = ready_thread.join().unwrap_or(Json::Null);
        set(&mut result, "ready", ready.clone());
        if aliases > 0 && ready.field("spawn_to_ms").is_null() {
            return Err(format!("local child did not become ready: {}", dumps(&ready, Dump::DEFAULT)));
        }
        if let Json::Obj(map) = &mut result
            && let Some(Json::Arr(items)) = map.get_mut("application_request_order") {
                items.push(Json::from("first_match"));
            }
        let payload = obj! {"title" => RELEASE};
        let mut first = request(&client, &base, "/match", Some(&payload), 10.0)?;
        set(&mut first, "spawn_to_complete_ms", ms(started.elapsed()));
        set(&mut result, "first_match", first.clone());
        if aliases > 0 {
            if first.field("status") != &Json::Int(200) || !first.field("body").field("sid_start").str_eq(EXPECTED_SID) {
                return Err(format!("synthetic known match failed: {}", dumps(&first, Dump::DEFAULT)));
            }
            if let Json::Obj(map) = &mut result
                && let Some(Json::Arr(items)) = map.get_mut("application_request_order") {
                    items.push(Json::from(format!("warm_match_{}", options.samples)));
                }
            let mut timings = Vec::new();
            for _ in 0..options.samples {
                let sample = request(&client, &base, "/match", Some(&payload), 10.0)?;
                if sample.field("status") != &Json::Int(200) || !sample.field("body").field("sid_start").str_eq(EXPECTED_SID) {
                    return Err("a warm synthetic match changed identity or status".into());
                }
                timings.push(sample.field("duration_ms").as_number().unwrap_or(0.0));
            }
            set(&mut result, "warm_match", distribution(&timings));
        } else if first.field("status") != &Json::Int(503) {
            return Err("missing metadata unexpectedly accepted matching".into());
        }
        if options.first_request == "match" {
            // Missing metadata returns 503 before entering the matcher;
            // only a successful match exercises a process-cold parser.
            if aliases == 0 {
                set(&mut result, "note", Json::from("The initial match returned 503 without initializing the parser."));
            }
            probe_parser(&client, &base, options.samples, started, &mut result)?;
        }
        // Let independently scheduled startup stages flush their log lines.
        std::thread::sleep(Duration::from_millis(25));
        Ok(())
    })();
    if let Err(error) = outcome {
        set(&mut result, "error", Json::from(error));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = memory_thread.join();
    if let Some(value) = rss_kib(pid) {
        peak.lock().map(|mut peak| peak.push(value)).ok();
    }
    if let Ok(mut child) = child.lock() {
        terminate(&mut child);
    }
    let peak_value = peak.lock().ok().and_then(|peak| peak.iter().max().copied());
    set(&mut result, "peak_rss_kib", peak_value.map_or(Json::Null, Json::Int));
    set(&mut result, "index_after", index_state(metadata));
    let log_text = String::from_utf8_lossy(&std::fs::read(&log_path).unwrap_or_default()).into_owned();
    let lines: Vec<String> = log_text.lines().map(str::to_owned).collect();
    if aliases > 0
        && lines.iter().any(|line| line.contains("scheduled metadata refresh failed") || line.contains("metadata download started"))
    {
        set(&mut result, "error", Json::from("fixture unexpectedly attempted a metadata refresh; benchmark invalid"));
    }
    set(&mut result, "startup_log", Json::from_strs(lines));
    Ok(result)
}

fn terminate(child: &mut Child) {
    // SIGTERM first so the server can flush; then SIGKILL after three seconds.
    let _ = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub struct Plan {
    pub output_dir: PathBuf,
    pub sizes: Vec<i64>,
    pub restarts: usize,
    pub fixtures_only: bool,
}

pub fn run(options: &Options, plan: &Plan) -> Result<(PathBuf, bool)> {
    std::fs::create_dir_all(&plan.output_dir).map_err(|error| error.to_string())?;
    let output = plan.output_dir.canonicalize().map_err(|error| error.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let work = output.join(format!("startup-{stamp:x}"));
    std::fs::create_dir_all(&work).map_err(|error| error.to_string())?;
    let mut fixtures = Vec::new();
    for size in &plan.sizes {
        fixtures.push((*size, create_fixture(&options.root, &work.join(format!("catalog-{size}")), *size)?));
    }
    let binary_sha = std::fs::read(&options.binary).ok().map(sha256_hex);
    let limitations = vec![
        "Synthetic uniformly distributed movie aliases; zero AniDB/episode rows.".to_owned(),
        "OS page cache is not dropped; no-index means missing Tantivy index, not cold disk.".to_owned(),
        "No public network import; missing-metadata sources fail at a loopback refused port.".to_owned(),
        "Readiness is polled concurrently; discovery precision is limited by poll and request scheduling.".to_owned(),
        "Peak RSS is Linux VmHWM sampled every 5ms through the request probes.".to_owned(),
        "Same-title warm requests exercise caches, not distinct-title throughput.".to_owned(),
        if options.first_request == "parse" {
            "The first match runs after parser probes, so its parser cache is already warm.".to_owned()
        } else {
            "Successful first matches run before parser probes; a missing-metadata 503 does not initialize the parser.".to_owned()
        },
    ];
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let cpus = std::thread::available_parallelism().map_or(0, usize::from);
    let mut report = obj! {
        "format_version" => Json::Int(1),
        "created_utc" => chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "binary" => options.binary.display().to_string(),
        "binary_sha256" => binary_sha.map_or(Json::Null, Json::from),
        "platform" => platform, "cpu_count" => cpus, "work_directory" => work.display().to_string(),
        "limitations" => Json::from_strs(limitations),
        "fixtures" => Json::Arr(fixtures.iter().map(|(size, path)| obj! {
            "imdb_alias_rows" => Json::from(*size), "unique_imdb_works" => Json::from(*size), "anidb_alias_rows" => Json::Int(0),
            "metadata" => path.display().to_string(),
            "metadata_bytes" => Json::Int(path.metadata().map_or(0, |meta| i128::from(meta.len()))),
        }).collect()),
        "settings" => obj! {
            "restarts" => plan.restarts, "samples" => options.samples, "poll_seconds" => Json::Float(options.poll_seconds),
            "first_request" => options.first_request.as_str(), "ml_enabled" => false, "strict" => true,
            "max_concurrent_matches" => Json::Int(2), "override_marker" => override_marker(&options.root)?,
        },
    };
    let mut runs = Vec::new();
    if !plan.fixtures_only {
        // Keeping this port bound without listen() prevents another process
        // from turning a fail-fast source into a live endpoint.
        let refused = bind_without_listen()?;
        let dead_port = refused.1;
        runs.push(run_child(options, &work.join("missing/metadata.sqlite3"), &work.join("runs/missing"), dead_port, "missing_metadata", 0, 0)?);
        for (size, metadata) in &fixtures {
            for iteration in 0..=plan.restarts {
                let scenario = if iteration == 0 { "no_index" } else { "disk_index_restart" };
                let run = run_child(options, metadata, &work.join(format!("runs/{size}-{iteration}")), dead_port, scenario, *size, iteration)?;
                println!(
                    "{scenario} aliases={size} iteration={iteration} ready_ms={} error={}",
                    dumps(run.field("ready").field("spawn_to_ms"), Dump::DEFAULT),
                    dumps(run.field("error"), Dump::DEFAULT)
                );
                let failed = run.get("error").is_some();
                runs.push(run);
                if failed {
                    break;
                }
            }
        }
        drop(refused);
    }
    let failed = runs.iter().any(|run| run.get("error").is_some());
    let summary = summarize(&runs);
    set(&mut report, "runs", Json::Arr(runs));
    set(&mut report, "summary", Json::Arr(summary));
    let destination = output.join("report.json");
    std::fs::write(&destination, dumps(&report, Dump::PRETTY) + "\n").map_err(|error| error.to_string())?;
    Ok((destination, failed))
}

/// A bound, never-listening loopback socket (connections are refused).
fn bind_without_listen() -> Result<(TcpStream, u16)> {
    // std cannot bind without listening; use a socket that connects to a
    // listener we immediately close, keeping its local port reserved.
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let address: SocketAddr = listener.local_addr().map_err(|error| error.to_string())?;
    let stream = TcpStream::connect(address).map_err(|error| error.to_string())?;
    let port = stream.local_addr().map_err(|error| error.to_string())?.port();
    drop(listener);
    Ok((stream, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_bijective_base_26() {
        assert_eq!(letters(0), "a");
        assert_eq!(letters(25), "z");
        assert_eq!(letters(26), "aa");
        assert_eq!(letters(27), "ab");
        assert_eq!(letters(702), "aaa");
    }

    #[test]
    fn override_marker_tracks_the_server_definition() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let server = std::fs::read_to_string(root.join("crates/matchlab-server/src/metadata_import.rs")).unwrap();
        let start = server.find("fn bundled_animelists_override_marker").expect("server marker function");
        let body = &server[start..start + 600];
        assert!(body.contains(&format!("b\"{OVERRIDE_SALT}\"")), "server override salt changed: {body}");
        assert!(override_marker(&root).unwrap().starts_with("animelists_override_"));
    }

    #[test]
    fn distribution_matches_statistics_module() {
        let values = [5.0, 1.0, 3.0, 2.0];
        let summary = distribution(&values);
        assert_eq!(summary.field("median_ms"), &Json::Float(2.5));
        assert_eq!(summary.field("p95_ms"), &Json::Float(5.0));
        assert_eq!(summary.field("min_ms"), &Json::Float(1.0));
    }
}
