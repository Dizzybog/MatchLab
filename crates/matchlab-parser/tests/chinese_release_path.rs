//! A real bilingual Chinese season/episode release that must be understood
//! as one hierarchical media path rather than as one mixed-script title.
use matchlab_parser::parse_title;
use serde_json::json;

const RELEASE: &str = "【高清剧集网发布 www.BPHDTV.com】少年白马醉春风2[第41集][国语配音+中文字幕].The.Young.Brewmasters.Adventure.S02.2024.1080p.WEB-DL.H264.AAC-ZeroTV/少年白马醉春风.The.Young.Brewmasters.Adventure.S02E41.2024.1080p.WEB-DL.H264.AAC-ZeroTV.mp4";

#[test]
fn chinese_english_release_path_keeps_english_title_and_leaf_episode() {
    let result = parse_title(RELEASE, false).expect("parse full bilingual torrent/file path");
    eprintln!("Chinese release parser result: {result:?}");
    assert_eq!(result["title"], json!("The Young Brewmasters Adventure"));
    assert_eq!(result["seasons"], json!([2]));
    assert_eq!(result["episodes"], json!([41]));
    assert_eq!(result["year"], json!(2024));
}
