//! Synthetic coordinate controls from a private, independently collected
//! discrepancy cohort. No production release strings are retained here.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn repeated_numeric_airdate_titles_do_not_invent_season_episode_pairs() {
    for title in ["20-20","12-12"] {
        let raw=format!("{title}.2026.04.03.Example.Story.1080p.WEB.h264-GROUP.mkv");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert_eq!(parsed["date"],"2026-04-03","{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
    }
}

#[test]
fn explicit_and_ordinary_numeric_coordinates_remain_eligible() {
    for (raw,season,episode) in [
        ("20-20.S02E03.2026.04.03.1080p.WEB.mkv",2,3),
        ("03-12.2026.04.03.Example.1080p.WEB.mkv",3,12),
        ("20-20.mkv",20,20),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!([season]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([episode]),"{raw}: {parsed:?}");
    }
}

#[test]
fn anime_family_movie_ordinals_are_not_television_episodes() {
    for (raw,title) in [
        ("[Fansub] One Example Piece Movie 14 - Example Subtitle (BDRip MP4).mkv",
            "One Example Piece Movie 14 - Example Subtitle"),
        ("[Fansub] One Example Piece Film 3 - Named Subtitle (BDRip MP4).mkv",
            "One Example Piece Film 3 - Named Subtitle"),
        ("One Example Piece Movie 14.2025.1080p.BluRay.mkv",
            "One Example Piece Movie 14"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"],json!([]),"{raw}: {parsed:?}");
    }
    let episode=parse_title("[Fansub] One Example Piece - 14 [1080p].mkv",false).unwrap();
    assert_eq!(episode["episodes"],json!([14]));
    let explicit=parse_title("One Example Piece Movie 14 S02E03.1080p.WEB.mkv",false).unwrap();
    assert_eq!(explicit["seasons"],json!([2]));
    assert_eq!(explicit["episodes"],json!([3]));
}
