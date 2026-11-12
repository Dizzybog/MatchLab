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
    assert_eq!(result["absolute_episode_candidate"], json!(41));
}

#[test]
fn chinese_marker_is_only_a_hint_and_does_not_rewrite_explicit_coordinates() {
    let source = "少年白马醉春风[第41集]/The.Young.Brewmasters.Adventure.S02E41.2024.mp4";
    let result = parse_title(source, false).unwrap();
    assert_eq!(result["seasons"], json!([2]));
    assert_eq!(result["episodes"], json!([41]));
    assert_eq!(result["absolute_episode_candidate"], json!(41));

    let plain = parse_title("The.Young.Brewmasters.Adventure.S02E41.2024.mp4", false).unwrap();
    assert!(plain.get("absolute_episode_candidate").is_none());
}


const LORD_OF_ALL_LORDS_RELEASE:&str =
    "【高清剧集网发布 www.DDHDTV.com】圣祖[全26集][国语配音+中文字幕].Lord.of.All.Lords.S01.2024.2160p.WEB-DL.DDP2.0.H265-ParkTV/圣祖.Lord.of.All.Lords.S01E02.2024.2160p.WEB-DL.DDP2.0.H265-ParkTV.mkv";

const DEMON_HUNTER_RELEASE:&str =
    "【高清剧集网发布 www.BBEGGE.com】沧元图[第62集][国语配音+中文字幕].The.Demon.Hunter.S01.2023.2160p.WEB-DL.H265.AAC-ColorTV/【更多高清剧集下载请访问 www.BPHDTV.com】【更多剧集打包下载请访问 www.BPHDTV.com】.mkv";

#[test]
fn lord_of_all_lords_chinese_complete_pack_yields_actual_leaf_episode(){
    let p=parse_title(LORD_OF_ALL_LORDS_RELEASE,false).expect("parse exact Lord of All Lords path");
    assert_eq!(p["title"],json!("Lord of All Lords"),"full parse: {p:?}");
    assert_eq!(p["year"],json!(2024),"{p:?}");
    assert_eq!(p["seasons"],json!([1]),"{p:?}");
    assert_eq!(p["episodes"],json!([2]),"{p:?}");
    assert!(p.get("episode_start").is_none(),
        "parent [全26集] must not produce an episode interval: {p:?}");
}

#[test]
fn demon_hunter_advertising_only_leaf_inherits_parent_title_and_absolute_hint(){
    let p=parse_title(DEMON_HUNTER_RELEASE,false).expect("parse exact Demon Hunter path");
    assert_eq!(p["title"],json!("The Demon Hunter"),"full parse: {p:?}");
    assert_eq!(p["year"],json!(2023),"{p:?}");
    assert_eq!(p["seasons"],json!([1]),"{p:?}");
    assert_eq!(p["absolute_episode_candidate"],json!(62),
        "Chinese 第62集 is an unverified absolute hint, not S01E62: {p:?}");
    assert_eq!(p["episodes"],json!([]),
        "must not invent a verified season-local episode: {p:?}");
}

#[test]
fn url_banner_cleanup_is_generic_and_does_not_erase_release_metadata(){
    // Not Chinese-specific: source websites can advertise themselves in
    // bracket blocks of any script. Preserve fields from the original name
    // while removing only untrusted publisher words from title identity.
    let raw="[Published at www.scene.example]Example.Show.S02E04.2024.1080p.WEB-DL/【下载 www.other.example】【镜像 https://mirror.example】.mkv";
    let p=parse_title(raw,false).unwrap();
    assert_eq!(p["title"],json!("Example Show"),"{p:?}");
    assert_eq!(p["year"],json!(2024),"{p:?}");
    assert_eq!(p["seasons"],json!([2]),"{p:?}");
    assert_eq!(p["episodes"],json!([4]),"{p:?}");
    assert_eq!(p["resolution"],json!("1080p"),"{p:?}");
    assert_eq!(p["quality"],json!("WEB-DL"),"{p:?}");
    assert_eq!(p["extension"],json!("mkv"),"{p:?}");
    let layers=p["path_layers"].as_array().unwrap();
    assert_eq!(layers.len(),2);
    assert_eq!(layers[0]["parsed"]["title"],"Example Show");
    assert!(layers[1]["parsed"].get("title").is_none(),
        "ad-only leaf cannot become a show title: {layers:?}");
}
