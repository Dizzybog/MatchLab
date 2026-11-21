use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn x_of_y_in_a_single_video_file_is_that_episode() {
    for name in [
        "[AniDub]_Monster_[51_of_74]_[480x272_h264_aac]_[Suzaku]_[PSP].mp4",
        "1e3d86370e72c24b11e2049e73130503a12c1f95/0/[AniDub]_Monster_[51_of_74]_[480x272_h264_aac]_[Suzaku]_[PSP].mp4",
        "[AniDub] Monster (51 of 74) [480p].mkv",
    ] {
        let parsed = parse_title(name, false).unwrap();
        assert_eq!(parsed["title"], "Monster", "{name}");
        assert_eq!(parsed["episodes"], json!([51]), "{name}");
    }
}

#[test]
fn x_of_y_in_a_torrent_title_stays_a_pack() {
    let parsed = parse_title("[AniDub] Monster [51 of 74] [480p]", false).unwrap();
    let episodes = parsed["episodes"].as_array().unwrap();
    assert_eq!(episodes.len(), 51);
    assert_eq!(episodes[0], 1);
    assert_eq!(episodes[50], 51);
}
