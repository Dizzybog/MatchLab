//! Check CLI dispatch without downloading metadata: insecure source URLs must
//! reach source validation, and must never create a database or workspace.
use std::{path::PathBuf,process::Command,time::{SystemTime,UNIX_EPOCH}};

fn check_source_count(count:usize){
    let folder:PathBuf=std::env::temp_dir().join(format!(
        "matchlab-cli-sources-{}-{}-{}",std::process::id(),count,
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    let output=Command::new(env!("CARGO_BIN_EXE_matchlab-server"))
        .arg("refresh-metadata-urls")
        .arg(folder.join("metadata.sqlite3"))
        .args(vec!["http://127.0.0.1:1/source";count])
        .output().expect("run metadata refresh CLI");
    assert!(!output.status.success());
    let error=String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("metadata source must use HTTPS"),
        "{count} source URLs must reach source validation: {error}");
    assert!(!folder.exists(),"invalid source URLs must not create a workspace");
}

#[test]
fn metadata_refresh_accepts_current_seven_source_argument_list(){
    check_source_count(7);
}

#[test]
fn metadata_refresh_accepts_legacy_six_source_argument_list(){
    check_source_count(6);
}
