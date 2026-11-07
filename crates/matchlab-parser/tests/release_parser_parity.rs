//! Differential cases adapted as ideas (not copied implementation) from
//! AIOStreams/release-parser and lowlighter/rakun (MIT). The parser remains
//! native Rust and does not change its authoritative identity/crosswalk rules.
use matchlab_parser::parse_title;
use serde_json::{Value, json};

fn parsed(title: &str) -> serde_json::Map<String, Value> {
    parse_title(title, false).unwrap_or_else(|e| panic!("{title:?}: {e}"))
}

#[test]
fn aiostreams_episode_titles_for_regular_tv_and_anime() {
    for (release, expected) in [
        (
            "The.100.S01E01.Pilot.1080p.AVC.DTS-HD.MA.5.1.REMUX-FraMeSToR.mkv",
            "Pilot",
        ),
        (
            "1923 S02E01 The Killing Season 1080p AMZN WEB-DL DDP5 1 H 264-FLUX",
            "The Killing Season",
        ),
        (
            "Friends.S07E20.The.One.With.Rachel's.Big.Kiss.720p.BluRay.2CH.x265.mkv",
            "The One With Rachel's Big Kiss",
        ),
        (
            "One.Piece.S01E1116.Lets.Go.Get.It!.Buggys.Big.Declaration.2160p.WEB-DL.mkv",
            "Lets Go Get It! Buggys Big Declaration",
        ),
        (
            "Futurama.S08E03.How.the.West.Was.1010001.1080p.HULU.WEB-DL.mkv",
            "How the West Was 1010001",
        ),
        (
            "[bonkai77].RahXephon.Episode.08.Bitterly.Cold.Holy.Night.[BD.1080p].mkv",
            "Bitterly Cold Holy Night",
        ),
        (
            "Show.S01E01.Spider-Man.Returns.1080p.WEB-DL-GRP",
            "Spider-Man Returns",
        ),
    ] {
        let p = parsed(release);
        assert_eq!(p.get("episode_title"), Some(&json!(expected)), "{release}");
    }
}

#[test]
fn episode_title_does_not_capture_release_metadata() {
    for release in [
        "Show.Name.2012.S01E01.1080p.BluRay-GRP",
        "Borgen.S04E01.NORDiC.1080p.WEB-DL.H.264.DD5.1-TWA",
        "Show.S01E01.German.1080p.WEB-DL-GRP",
        "One.Piece.S01E01.JPN.AAC2.0.1080p.WEB-DL",
        "Gotham.S03E17.XviD-AFG",
        "The.Mandalorian.S01E06.4K.HDR.2160p",
        "Some.Show.S01E01.PREAiR.HDTV.XviD-GRP",
        "[SubsPlease] One Piece - 1111 (480p) [2E05E658].mkv",
        "COMPASS 2.0 ANIMATION PROJECT",
        "Movie.2024.1080p.BluRay.x264",
    ] {
        let p = parsed(release);
        assert!(p.get("episode_title").is_none(), "false episode title for {release}: {p:?}");
    }
}

#[test]
fn rakun_fansub_markers_and_final_episodes_are_conservative() {
    for (release, episode) in [
        ("[Moe-Raws] Kakegurui #05 (BS11 1280x720 x264 AAC).mp4", 5),
        ("[Ohys-Raws] Overlord III - 13 END (AT-X 1280x720 x264 AAC).mp4", 13),
    ] {
        let p = parsed(release);
        assert_eq!(p["episodes"], json!([episode]), "{release}");
        assert!(p["title"].as_str().is_some_and(|v| !v.is_empty()), "{release}");
    }

    for release in [
        "The #1 Movie (2021) 1080p BluRay",
        "The 100 2014 S01E01 1080p",
        "One Piece 1111 (480p) [2E05E658]",
        "[Team] A Show - 2015-2024 BluRay",
    ] {
        let p = parsed(release);
        if release.contains("#1 Movie") {
            assert!(p["episodes"].as_array().is_none_or(Vec::is_empty), "{release}");
        }
    }
}

#[test]
fn explicit_episode_title_does_not_leak_through_path_inheritance() {
    let p = parsed("Example Show S01E01 Pilot/Example.Show.S01E02.1080p.WEB-DL.mkv");
    assert_eq!(p["episodes"], json!([2]));
    assert!(p.get("episode_title").is_none(), "{p:?}");
}

#[test]
fn movie_and_video_metadata_still_parse_without_tv_coordinates() {
    let p = parsed("The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1");
    assert_eq!(p["title"], json!("The Matrix"));
    assert_eq!(p["year"], json!(1999));
    assert_eq!(p["resolution"], json!("1080p"));
    assert!(p.get("episode_title").is_none());
    assert!(p["episodes"].as_array().is_none_or(Vec::is_empty));
}


#[test]
fn additional_aiostreams_episode_title_formats() {
    for (raw, title) in [
        ("The.Office.S07E25+E26.Search.Committee.mp4", "Search Committee"),
        ("Game of Thrones - S02E07 - A Man Without Honor [2160p] [HDR].mkv", "A Man Without Honor"),
        ("Show.Name.2012.1x01.Episode.Title.1080p.BluRay-GRP", "Episode Title"),
        ("Community.S01E25.English.as.a.Second.Language.720p.WEB-DL", "English as a Second Language"),
        ("Formula.1.2024x05.Chinese.Grand.Prix.Sprint.1080p.F1TV.WEB-DL", "Chinese Grand Prix Sprint"),
        ("Supernatural-S01E01-Strange.Relations.1080p.WEB-DL", "Strange Relations"),
    ] {
        let output = parsed(raw);
        assert_eq!(output.get("episode_title"), Some(&json!(title)), "{raw}: {output:?}");
    }
}

#[test]
fn multilingual_metadata_is_not_an_episode_title() {
    for raw in [
        "Show.S01E01.VOSTFR.1080p.WEB-DL",
        "Show.S01E01.dublado.1080p.WEB-DL",
        "Show.S01E01.legendado.1080p.WEB-DL",
        "Show.S01E01.deutsch.1080p.WEB-DL",
        "Show.S01E01.francais.1080p.WEB-DL",
        "Show.S01E01.2160p.HDR10+.REMUX",
    ] {
        let output = parsed(raw);
        assert!(output.get("episode_title").is_none(), "{raw}: {output:?}");
    }
}

#[test]
fn a_single_torrent_path_does_not_mix_parent_and_child_episode_titles() {
    let p = parsed("Example Show S01E01 Pilot/Example.Show.S01E02.The.Finale.1080p.mkv");
    assert_eq!(p["episodes"], json!([2]));
    assert_eq!(p["episode_title"], json!("The Finale"));

    let p = parsed("Example Show S01E01 Pilot/Example.Show.S01E02.1080p.WEB-DL.mkv");
    assert_eq!(p["episodes"], json!([2]));
    assert!(p.get("episode_title").is_none(), "{p:?}");
}
