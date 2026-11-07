//! Anime release conventions identified by reviewing lowlighter/rakun,
//! AIOStreams/release-parser and AIOStreams' file/folder merger.
//! Test the public parser surface, including non-anime false-positive guards.
use matchlab_parser::parse_title;
use serde_json::{Value, json};

fn parse(raw: &str) -> serde_json::Map<String, Value> {
    parse_title(raw, false).unwrap_or_else(|e| panic!("{raw:?}: {e}"))
}

#[test]
fn rakun_style_fansub_hash_episode_is_not_an_eight_character_crc() {
    for (raw, episode) in [
        ("[Moe-Raws] Kakegurui #05 (BS11 1280x720 x264 AAC).mp4", 5),
        ("[SubsPlease] Example Anime - #09v2 [1080p] [A3CB185F].mkv", 9),
        ("[Team] Example Anime - #012 [720p][Multiple Subtitle]", 12),
    ] {
        let result = parse(raw);
        assert_eq!(result["episodes"], json!([episode]), "{raw}");
        assert!(result["title"].as_str()
            .is_some_and(|title| title.contains("Kakegurui") || title.contains("Example Anime")),
            "title must exclude fansub episode and technical metadata: {raw}: {result:?}");
    }
}

#[test]
fn labelled_anime_ranges_preserve_endpoints_and_do_not_expand_a_batch() {
    for (raw, start, end) in [
        ("[WZF] Toriko - Episodio 93~94[HD][X264-AAC][1280x720]", 93, 94),
        ("[Raws] Example Anime - Eps 01~12 [1080p]", 1, 12),
        ("[Raws] Example Anime - EP01~EP07 [BD]", 1, 7),
        ("[Raws] Example Anime - Episode 11 & 12 [WEB]", 11, 12),
    ] {
        let result = parse(raw);
        assert_eq!(result["episode_start"], json!(start), "{raw}");
        assert_eq!(result["episode_end"], json!(end), "{raw}");
        assert_eq!(result["episode_count"], json!(end-start+1), "{raw}");
        assert_eq!(result["episode_range_numbering"], "absolute", "{raw}");
        assert_eq!(result["episodes"], json!([start]), "{raw}");
        assert!(result["title"].as_str().is_some_and(|title| !title.is_empty()), "{raw}");
    }

    let season_relative = parse("Example Show Season 2 Episode 03~04 1080p WEB-DL");
    assert_eq!(season_relative["episode_start"], 3);
    assert_eq!(season_relative["episode_end"], 4);
    assert_eq!(season_relative["episode_range_numbering"], "season");
    assert_eq!(season_relative["seasons"], json!([2]));
}

#[test]
fn east_asian_episode_markers_are_hints_not_unverified_sid_conversions() {
    for (raw, episode) in [
        ("[Group] アニメ第12話 [1080p].mkv", 12),
        ("[Group] 动画第41话 1080p", 41),
        ("[Group] 动画第41集 1080p", 41),
        ("[Fansub] 애니메이션 제7화 [WEB]", 7),
        ("[Fansub] 애니메이션 제13회 [WEB]", 13),
    ] {
        let result = parse(raw);
        assert_eq!(result["absolute_episode_candidate"], json!(episode), "{raw}");
        assert!(result.get("episode_range_numbering").is_none(), "{raw}");
    }
    let season = parse("アニメ 第2期 [1080p]");
    assert!(season.get("absolute_episode_candidate").is_none());
}

#[test]
fn a_child_episode_cannot_inherit_an_unrelated_parent_absolute_hint() {
    let matched = parse("动画第41集/Example.Anime.S02E41.WEB-DL.mkv");
    assert_eq!(matched["absolute_episode_candidate"], json!(41));
    assert_eq!(matched["episodes"], json!([41]));

    let unrelated = parse("动画第41集/Example.Anime.S02E05.WEB-DL.mkv");
    assert!(unrelated.get("absolute_episode_candidate").is_none());
    assert_eq!(unrelated["episodes"], json!([5]));

    let newer = parse("动画第41集/Example.Anime.第12話.WEB-DL.mkv");
    assert_eq!(newer["absolute_episode_candidate"], json!(12));
}

#[test]
fn ordinary_titles_and_structured_episode_coordinates_remain_unmodified() {
    let title_number = parse("[Erai-raws] 2-5 Jigen no Ririsa - 08 [1080p]");
    assert!(title_number.get("anime_episode_range_candidate").is_none(),
        "a numeric title prefix must not be treated as a release episode range");

    let movie = parse("Dragon Ball Z Movie - 09 - Bojack Unbound - 1080p");
    assert!(movie.get("episode_start").is_none());
    let volume = parse("Shinsekai Yori Vol.1-9 [BD]");
    assert!(volume.get("anime_episode_range_candidate").is_none());
    assert!(volume.get("episode_start").is_none());

    let explicit = parse("Example.Show.S02E03-S02E12.1080p");
    assert_eq!(explicit["episode_range_numbering"], "season");
    assert_eq!(explicit["episode_start"], 3);
    assert_eq!(explicit["episode_end"], 12);
    assert_eq!(explicit["seasons"], json!([2]));

    let absolute = parse("One.Piece.E768-E1064 [1080p]");
    assert_eq!(absolute["episode_range_numbering"], "absolute");
    assert_eq!(absolute["episode_start"], 768);
    assert_eq!(absolute["episode_end"], 1064);
}
