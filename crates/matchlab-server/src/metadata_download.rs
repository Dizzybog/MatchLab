//! Download source archives into a temporary workspace, then publish only after
//! the complete native importer validates the new SQLite database.
use std::{error::Error,fs::{self,File},io::{Read,Write},path::{Path,PathBuf},time::{SystemTime,UNIX_EPOCH,Instant}};
const MAX_DOWNLOAD_BYTES:u64=2_000_000_000;
const DEFAULT_METADATA_URLS:[&str;6]=[
    "https://datasets.imdbws.com/title.basics.tsv.gz",
    "https://datasets.imdbws.com/title.akas.tsv.gz",
    "https://datasets.imdbws.com/title.episode.tsv.gz",
    "https://raw.githubusercontent.com/nattadasu/animeApi/refs/heads/v3/database/animeapi.tsv",
    "https://raw.githubusercontent.com/Anime-Lists/anime-lists/refs/heads/master/anime-list.xml",
    "https://anidb.net/api/anime-titles.xml.gz",
];

pub fn configured_urls(override_urls:Option<&str>)->Result<Vec<String>,Box<dyn Error>>{
    let urls:Vec<String>=match override_urls {
        Some(value)=>value.split(',').map(|s|s.trim().to_owned()).collect(),
        None=>DEFAULT_METADATA_URLS.iter().map(|s|(*s).to_owned()).collect(),
    };
    if urls.len()!=6 || urls.iter().any(|url|url.is_empty()) {
        return Err("MATCHLAB_METADATA_URLS requires six comma-separated HTTPS URLs".into());
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
/// Six HTTPS URLs in order: IMDb basics, akas, episodes, animeApi,
/// Anime-Lists, AniDB titles. Sources are explicit to avoid silent feed changes.
pub fn refresh_from_urls(target:&Path,urls:&[String])->Result<(u64,u64,u64,u64),Box<dyn Error>>{
    if urls.len()!=6{return Err("expected exactly six metadata URLs".into());}
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
    let client=reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(900))
        .connect_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let names=["basics.tsv.gz","akas.tsv.gz","episodes.tsv.gz","animeapi.tsv","animelists.xml","anidb.xml.gz"];
    let paths:Vec<PathBuf>=names.iter().map(|name|dir.join(name)).collect();
    for (url,path) in urls.iter().zip(paths.iter()){download(&client,url,path)?;}
    super::metadata_import::refresh_from_files(target,&paths[0],&paths[1],&paths[2],&paths[3],&paths[4],&paths[5])
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn defaults_enable_zero_config_startup(){
        let urls=configured_urls(None).unwrap();
        assert_eq!(urls.len(),6);
        assert!(urls.iter().all(|url|url.starts_with("https://")));
        assert!(urls[0].ends_with("title.basics.tsv.gz"));
        assert!(urls[5].ends_with("anime-titles.xml.gz"));
    }
    #[test]
    fn overrides_are_validated(){
        assert!(configured_urls(Some("")).is_err());
        assert!(configured_urls(Some("https://example.org/only-one")).is_err());
        let urls=configured_urls(None).unwrap().join(",");
        assert_eq!(configured_urls(Some(&urls)).unwrap().len(),6);
        assert!(configured_urls(Some(&urls.replacen("https://","http://",1))).is_err());
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
