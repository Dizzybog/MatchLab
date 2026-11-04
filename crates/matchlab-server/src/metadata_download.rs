//! Download source archives into a temporary workspace, then publish only after
//! the complete native importer validates the new SQLite database.
use std::{error::Error,fs::{self,File},io::{self,Write},path::{Path,PathBuf},time::{SystemTime,UNIX_EPOCH}};
const MAX_DOWNLOAD_BYTES:u64=2_000_000_000;
fn download(client:&reqwest::blocking::Client,url:&str,path:&Path)->Result<(),Box<dyn Error>>{
    let parsed=reqwest::Url::parse(url)?;
    if parsed.scheme()!="https"{return Err("metadata source must use HTTPS".into());}
    let mut response=client.get(parsed).send()?.error_for_status()?;
    if response.content_length().is_some_and(|n|n>MAX_DOWNLOAD_BYTES){
        return Err("metadata archive exceeds download limit".into());
    }
    let mut file=File::create(path)?;
    let copied=io::copy(&mut response.take(MAX_DOWNLOAD_BYTES+1),&mut file)?;
    if copied==0||copied>MAX_DOWNLOAD_BYTES{return Err("empty or oversized metadata archive".into());}
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
    fn reject_non_https_sources_without_creating_a_file(){
        let path=std::env::temp_dir().join(format!("matchlab-insecure-{}",std::process::id()));
        assert!(download(&reqwest::blocking::Client::new(),"http://example.org/test",&path).is_err());
        assert!(!path.exists());
    }
}
