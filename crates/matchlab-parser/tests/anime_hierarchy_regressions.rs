//! User-reported anime release/file-path regressions, November 2026.
//! Parsing is intentionally metadata-free: ambiguous absolute numbers remain
//! candidates until the server verifies an exact AniDB-linked series title.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn bleach_leaf_episode_wins_over_parent_batch_and_season_labels() {
    let cases = [
        ("[Adosin] Bleach 190-366 (BD 1080p HEVC FLAC) [Dual Audio]/[Adosin] Bleach - 215 (BD 1080p HEVC FLAC) [Dual Audio].mkv", 215),
        ("bleach/Season 14/Bleach English Dubbed 720p Season(9-14)Episode(168-316) $M@nI$/Bleach Season 14 Ep(266-316)The Arrancar Part 6 Fall of the Arrancar (2010–2011)/Bleach - 295 - It's All a Trap... Engineered Bonds.mkv", 295),
        ("[Anime Time] Bleach Complete Series & Movies/Season 2 - The Entry/Bleach - 026 - Formation! The Worst Tag.mkv", 26),
    ];
    for (release, episode) in cases {
        let parsed = parse_title(release, false).expect(release);
        let candidate = &parsed["anime_episode_candidate"];
        assert_eq!(candidate["episode"], json!(episode), "{release}: {parsed:?}");
        assert_eq!(candidate["title"], json!("Bleach"), "{release}: {parsed:?}");
        assert_eq!(candidate["numbering"], json!("absolute"), "{release}");
    }
}

#[test]
fn bare_underscore_absolute_anime_number_is_a_verified_candidate_not_a_guess() {
    let parsed = parse_title("Bleach_345", false).unwrap();
    assert_eq!(parsed["anime_episode_candidate"]["title"], "Bleach");
    assert_eq!(parsed["anime_episode_candidate"]["episode"], 345);
    assert_eq!(parsed["anime_episode_candidate"]["numbering"], "absolute");
}

#[test]
fn season_tagged_absolute_anime_pack_removes_season_from_alias() {
    let parsed = parse_title(
        "[orion origin] Shinigami Bocchan to Kuro Maid S2 [13-24]  [WebRip] [1080p] [H265 AAC] [CHS＆JPN]",
        false,
    ).unwrap();
    let range = &parsed["anime_episode_range_candidate"];
    assert_eq!(range["title"], "Shinigami Bocchan to Kuro Maid", "{parsed:?}");
    assert_eq!(range["start"], 13, "{parsed:?}");
    assert_eq!(range["end"], 24, "{parsed:?}");
    assert_eq!(range["season_hint"], 2, "{parsed:?}");
    assert_eq!(range["numbering"], "absolute");
}

#[test]
fn underscore_delimited_absolute_anime_range_remains_tentative() {
    let raw = "Bleach_320-360";
    let parsed = parse_title(raw, false).unwrap();
    let hint = &parsed["anime_episode_range_candidate"];
    assert_eq!(hint["title"], "Bleach", "{parsed:?}");
    assert_eq!(hint["start"], 320, "{parsed:?}");
    assert_eq!(hint["end"], 360, "{parsed:?}");
    assert_eq!(hint["numbering"], "absolute", "{parsed:?}");
    assert_eq!(hint["verified"], false, "{parsed:?}");
    assert!(parsed.get("episode_start").is_none(),
        "without AniDB verification a bare numeric range is only a hint: {parsed:?}");
}

#[test]
fn season_only_and_title_only_releases_remain_distinct_from_single_episode() {
    let bleach = parse_title("Bleach_S02", false).unwrap();
    assert_eq!(bleach["seasons"], json!([2]));
    assert_eq!(bleach["episodes"], json!([]));
    let chaos = parse_title("[HKG&SumiSora&CASO][Chaos_Head][BDRip]", false).unwrap();
    assert_eq!(chaos["seasons"], json!([]));
    assert_eq!(chaos["episodes"], json!([]));
}
