//! Download source archives into a temporary workspace, then publish only after
//! the complete native importer validates the new SQLite database.
use std::{error::Error,fs::{self,File},io::{Read,Write},path::{Path,PathBuf},time::{SystemTime,UNIX_EPOCH,Instant}};
const MAX_DOWNLOAD_BYTES:u64=2_000_000_000;
const DEFAULT_METADATA_URLS:[&str;7]=[
    "https://datasets.imdbws.com/title.basics.tsv.gz",
    "https://datasets.imdbws.com/title.akas.tsv.gz",
    "https://datasets.imdbws.com/title.episode.tsv.gz",
    "https://datasets.imdbws.com/title.ratings.tsv.gz",
    "https://raw.githubusercontent.com/nattadasu/animeApi/refs/heads/v3/database/animeapi.tsv",
    "https://raw.githubusercontent.com/Anime-Lists/anime-lists/refs/heads/master/anime-list.xml",
    "https://anidb.net/api/anime-titles.xml.gz",
];

pub fn configured_urls(override_urls:Option<&str>)->Result<Vec<String>,Box<dyn Error>>{
    let mut urls:Vec<String>=match override_urls {
        Some(value)=>value.split(',').map(|s|s.trim().to_owned()).collect(),
        None=>DEFAULT_METADATA_URLS.iter().map(|s|(*s).to_owned()).collect(),
    };
    // Preserve existing six-URL overrides; insert the official ratings feed.
    if urls.len()==6{urls.insert(3,DEFAULT_METADATA_URLS[3].to_string());}
    if urls.len()!=7 || urls.iter().any(|url|url.is_empty()) {
        return Err("MATCHLAB_METADATA_URLS requires seven comma-separated HTTPS URLs".into());
    }
    for url in &urls {
        if reqwest::Url::parse(url)?.scheme()!="https" {
            return Err("metadata source must use HTTPS".into());
        }
    }
    Ok(urls)
}

fn download(client:&reqwest::blocking::Client,url:&str,path:&Path)->Result<(),Box<dyn Error>>{
    let parsed=reqwest::Url::parse(url)?;
    if parsed.scheme()!="https"{return Err("metadata source must use HTTPS".into());}
    let response=client.get(parsed).send()?.error_for_status()?;
    if response.url().scheme()!="https"{return Err("metadata redirect must remain HTTPS".into());}
    if response.content_length().is_some_and(|n|n>MAX_DOWNLOAD_BYTES){
        return Err("metadata archive exceeds download limit".into());
    }
    let total=response.content_length();
    let started=Instant::now();
    let mut file=File::create(path)?;
    let mut stream=response.take(MAX_DOWNLOAD_BYTES+1);
    let mut copied=0u64;
    let mut next_log=25_000_000u64;
    let mut buffer=[0u8;64*1024];
    tracing::info!(file=%path.display(),total_bytes=?total,"metadata download started");
    loop {
        let read=stream.read(&mut buffer)?;
        if read==0{break;}
        file.write_all(&buffer[..read])?;
        copied+=read as u64;
        if copied>MAX_DOWNLOAD_BYTES{return Err("empty or oversized metadata archive".into());}
        if copied>=next_log{
            let elapsed=started.elapsed().as_secs_f64().max(0.001);
            tracing::info!(file=%path.display(),downloaded_mb=copied/1_000_000,
                total_mb=total.map(|n|n/1_000_000),mb_per_second=(copied as f64/1_000_000.0/elapsed),
                "metadata download progress");
            next_log=((copied/25_000_000)+1)*25_000_000;
        }
    }
    if copied==0{return Err("empty or oversized metadata archive".into());}
    tracing::info!(file=%path.display(),downloaded_mb=copied/1_000_000,
        elapsed_seconds=started.elapsed().as_secs(),"metadata download completed");
    file.flush()?;
    file.sync_all()?;
    Ok(())
}
struct Workspace(PathBuf);
impl Drop for Workspace{fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn metadata_client()->Result<reqwest::blocking::Client,reqwest::Error>{
    reqwest::blocking::Client::builder()
        // Enforce TLS before every network request, including redirects.
        // Checking only the final response URL permits a plaintext request
        // to be sent before the download's scheme check can reject it.
        .https_only(true)
        .timeout(std::time::Duration::from_secs(900))
        .connect_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
}
/// Seven HTTPS URLs in order: IMDb basics, akas, episodes, ratings, animeApi,
/// Anime-Lists, AniDB titles. Sources are explicit to avoid silent feed changes.
pub fn refresh_from_urls(target:&Path,urls:&[String])->Result<(u64,u64,u64,u64),Box<dyn Error>>{
    if urls.len()!=7{return Err("expected exactly seven metadata URLs".into());}
    // Reject every invalid source before starting network I/O or creating a workspace.
    for url in urls {
        if reqwest::Url::parse(url)?.scheme()!="https" {
            return Err("metadata source must use HTTPS".into());
        }
    }
    let parent=target.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let nonce=SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir=parent.join(format!(".matchlab-download-{}-{nonce}",std::process::id()));
    fs::create_dir(&dir)?;
    let _cleanup=Workspace(dir.clone());
    let client=metadata_client()?;
    let names=["basics.tsv.gz","akas.tsv.gz","episodes.tsv.gz","ratings.tsv.gz","animeapi.tsv","animelists.xml","anidb.xml.gz"];
    let paths:Vec<PathBuf>=names.iter().map(|name|dir.join(name)).collect();
    for (url,path) in urls.iter().zip(paths.iter()){download(&client,url,path)?;}
    // AniBridge refines the Anime-Lists episode mappings. It is optional:
    // a failed download keeps the refresh on Anime-Lists alone.
    let anibridge=anibridge_url()?.and_then(|url|{
        let path=dir.join("anibridge.json");
        match download(&client,&url,&path) {
            Ok(())=>Some(path),
            Err(error)=>{tracing::warn!(%error,"AniBridge download failed; using Anime-Lists only");None},
        }
    });
    super::metadata_import::refresh_from_files_with_ratings(target,&paths[0],&paths[1],&paths[2],
        &paths[4],&paths[5],&paths[6],Some(&paths[3]),anibridge.as_deref())
}

/// Daily AniBridge release (https://github.com/anibridge/anibridge-mappings).
/// MATCHLAB_ANIBRIDGE_URL overrides it; "off" disables it.
const DEFAULT_ANIBRIDGE_URL:&str=
    "https://github.com/anibridge/anibridge-mappings/releases/download/v3/mappings.min.json";
pub fn anibridge_url()->Result<Option<String>,Box<dyn Error>>{
    parse_anibridge_url(std::env::var("MATCHLAB_ANIBRIDGE_URL").ok().as_deref())
}
fn parse_anibridge_url(raw:Option<&str>)->Result<Option<String>,Box<dyn Error>>{
    let url=match raw.map(str::trim) {
        None|Some("")=>DEFAULT_ANIBRIDGE_URL,
        Some(value) if value.eq_ignore_ascii_case("off")=>return Ok(None),
        Some(value)=>value,
    };
    if reqwest::Url::parse(url)?.scheme()!="https" {
        return Err("MATCHLAB_ANIBRIDGE_URL must use HTTPS".into());
    }
    Ok(Some(url.to_owned()))
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn anibridge_source_defaults_on_and_can_be_disabled(){
        assert_eq!(parse_anibridge_url(None).unwrap().as_deref(),Some(DEFAULT_ANIBRIDGE_URL));
        assert_eq!(parse_anibridge_url(Some("off")).unwrap(),None);
        assert!(parse_anibridge_url(Some("http://example.org/m.json")).is_err());
        assert_eq!(parse_anibridge_url(Some("https://example.org/m.json")).unwrap().as_deref(),
            Some("https://example.org/m.json"));
    }
    #[test]
    fn defaults_enable_zero_config_startup(){
        let urls=configured_urls(None).unwrap();
        assert_eq!(urls.len(),7);
        assert!(urls.iter().all(|url|url.starts_with("https://")));
        assert!(urls[0].ends_with("title.basics.tsv.gz"));
        assert!(urls[3].ends_with("title.ratings.tsv.gz"));
        assert!(urls[6].ends_with("anime-titles.xml.gz"));
    }
    #[test]
    fn overrides_are_validated(){
        assert!(configured_urls(Some("")).is_err());
        assert!(configured_urls(Some("https://example.org/only-one")).is_err());
        let urls=configured_urls(None).unwrap().join(",");
        assert_eq!(configured_urls(Some(&urls)).unwrap().len(),7);
        assert!(configured_urls(Some(&urls.replacen("https://","http://",1))).is_err());
    }
    #[test]
    fn legacy_sources_insert_ratings_without_reordering_other_feeds(){
        let mut legacy=configured_urls(None).unwrap();
        legacy.remove(3);
        assert_eq!(configured_urls(Some(&legacy.join(","))).unwrap(),
            configured_urls(None).unwrap());
    }
    #[test]
    fn transport_rejects_plaintext_before_attempting_a_connection(){
        // This exercises the client policy underneath download()'s URL
        // validation, so a redirect cannot bypass the HTTPS requirement.
        let error=metadata_client().unwrap()
            .get("http://127.0.0.1:1/plaintext")
            .timeout(std::time::Duration::from_millis(100))
            .send().unwrap_err();
        assert!(error.is_builder(),"reject the scheme before transport: {error}");
    }
    #[test]
    fn reject_insecure_url_set_before_creating_workspace(){
        let folder=std::env::temp_dir().join(format!("matchlab-url-validation-{}-{}",
            std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let target=folder.join("live.sqlite3");
        let urls=vec!["https://example.org/file".to_string(),
            "http://example.org/insecure".to_string(),
            "https://example.org/file".to_string(),
            "https://example.org/file".to_string(),
            "https://example.org/file".to_string(),
            "https://example.org/file".to_string(),
            "https://example.org/file".to_string()];
        assert!(refresh_from_urls(&target,&urls).is_err());
        assert!(!folder.exists(),"invalid URL set must not create a workspace");
    }
    #[test]
    fn reject_non_https_sources_without_creating_a_file(){
        let path=std::env::temp_dir().join(format!("matchlab-insecure-{}",std::process::id()));
        assert!(download(&reqwest::blocking::Client::new(),"http://example.org/test",&path).is_err());
        assert!(!path.exists());
    }
}
