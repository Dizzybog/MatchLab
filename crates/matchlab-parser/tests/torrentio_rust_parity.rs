//! Differential regression cases inspired by TheBeastLT/parse-torrent-title.
//! Only metadata explicitly supported by MatchLab's native Rust parser is
//! compared. No runtime dependency or source code is copied from Torrentio.
use matchlab_parser::parse_title;
use serde_json::{Value, json};

fn parsed(name: &str) -> serde_json::Map<String, Value> {
    parse_title(name, false).expect("valid release name")
}

#[test]
fn cyrillic_confusables_only_affect_release_coordinates() {
    for name in [
        "Example.Show.S01Е02.720р.WEB-DL.mkv",
        "Example.Show.S01е02.720р.WEB-DL.mkv",
    ] {
        let output = parsed(name);
        assert_eq!(output["seasons"], json!([1]), "{name}: {output:?}");
        assert_eq!(output["episodes"], json!([2]), "{name}: {output:?}");
        assert_eq!(output.get("resolution"), Some(&json!("720p")), "{name}: {output:?}");
    }
    let ordinary = parsed("Семья.2024.1080p.WEB-DL.mkv");
    assert!(ordinary["title"].as_str().is_some_and(|s| s.contains("Семья")),
        "{ordinary:?}");
}

#[test]
fn explicit_cjk_season_ordinals_are_distinct_from_anime_absolute_episodes() {
    for (name,season) in [
        ("Anime 第二季 第13話 [1080p].mkv", 2),
        ("Anime 第十季 第12話 [1080p].mkv", 10),
        ("Anime 第二十一季 第13話 [1080p].mkv", 21),
        ("Anime 第３季 第04話 [1080p].mkv", 3),
        ("Anime 第12季 第08話 [1080p].mkv", 12),
    ] {
        let output = parsed(name);
        assert_eq!(output["seasons"], json!([season]), "{name}: {output:?}");
        assert!(output.get("absolute_episode_candidate").is_some(), "{name}: {output:?}");
    }
    let invalid = parsed("Anime 第一百季 1080p WEB-DL.mkv");
    assert_ne!(invalid.get("seasons"), Some(&json!([100])), "{invalid:?}");
}

#[test]
fn russian_worded_season_and_episode_are_independent_of_release_quality() {
    let name = "«Эрнест и Селестина» 1 сезон 05 серия 720р.mkv";
    let output = parsed(name);
    assert_eq!(output["seasons"], json!([1]), "{output:?}");
    assert_eq!(output["episodes"], json!([5]), "{output:?}");
    assert_eq!(output.get("resolution"), Some(&json!("720p")), "{output:?}");
}

#[test]
fn fractional_anime_episodes_are_marked_without_fractional_coordinates() {
    for (name, episode, season) in [
        ("[Judas] Koisuru Asteroid - S01E06.5.mkv", 6, Some(1)),
        ("NanoCore S3 Episode 10.5 English Subbed.mp4", 10, Some(3)),
        ("[Doki] Hai to Gensou no Grimgar - 02.5 (1920x1080 HEVC BD FLAC) [FCDA1B13].mkv", 2, None),
        ("[SubsPlease] Baraou no Souretsu - 12.5v2 (720p) [7A329328].mkv", 12, None),
        ("[DBD-Raws][Gangsta.][09.5][1080P][BDRip][HEVC-10bit][FLAC].mkv", 9, None),
    ] {
        let output = parsed(name);
        assert_eq!(output["episodes"], json!([episode]), "{name}: {output:?}");
        assert_eq!(output.get("half_episode"), Some(&json!(true)), "{name}: {output:?}");
        if let Some(season) = season {
            assert_eq!(output["seasons"], json!([season]), "{name}: {output:?}");
        }
    }
}

#[test]
fn ratings_audio_sizes_and_episode_titles_never_count_as_half_episodes() {
    for name in [
        "Show.S01E05.1080p.WEB-DL.DDP5.1.mkv",
        "Show.S01E06.1080p.AAC2.0.H.265.mkv",
        "Show S01E07 1080p 5.5 GB",
        "Show - 07 [1080p] [v1.5].mkv",
        "Show Season 1.5 Complete",
        "Community S01E12 Comparative Religion IMDB 8.5.mkv",
        "Warrior.Nun.S01E01.Psalm.46.5.1080p.WEBRip.mkv",
        "AwesomenessTV.S02E05.5.Ways.to.Reject.a.Guy.480p.HDTV.mkv",
        "The.Pitt.S01E11.5.00.P.M.1080p.AMZN.WEB-DL.mkv",
        "[Group] 2.5 Dimensional Seduction - 01 [1080p].mkv",
    ] {
        let output = parsed(name);
        assert!(output.get("half_episode").is_none(), "{name}: {output:?}");
    }
}

#[test]
fn path_parent_half_episode_does_not_leak_into_other_file() {
    let p = parsed("[Group] Example - 12.5/Example.S01E13.1080p.mkv");
    assert_eq!(p["episodes"], json!([13]), "{p:?}");
    assert!(p.get("half_episode").is_none(), "{p:?}");
}
