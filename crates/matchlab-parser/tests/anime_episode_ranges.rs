//! AnimeTosho-style episode batches and false-positive guards.
//! Release names are examples; no torrent data or copyrighted files are used.
use matchlab_parser::parse_title;
use serde_json::{json,Value};

fn parse(input:&str)->serde_json::Map<String,Value>{
    parse_title(input,false).unwrap_or_else(|e|panic!("{input:?}: {e}"))
}

#[test]
fn unmarked_anime_batches_are_tentative_ranges_not_expanded_lists(){
    for (raw,start,end) in [
        ("[WasteOfBlindness] Ganbare Genki 01-07 Batch",1,7),
        ("[SubsPlease] Spy x Family (01-25) (1080p) [Batch]",1,25),
        ("[New-raws] Busamen Gachi Fighter - 01~12 [1080p]",1,12),
        ("[AG] Turn A Gundam 26 - 50 [BD]",26,50),
        ("Great Teacher Onizuka (01-43) [720p]",1,43),
        ("Anime Example 1-7",1,7),
        ("Anime Example 01 – 12",1,12),
        ("Anime Example 01～12 [WEB]",1,12),
        ("Anime Example (001-007)",1,7),
        ("Anime Example 01-12 + OVA",1,12),
    ] {
        let parsed=parse(raw);
        let hint=&parsed["anime_episode_range_candidate"];
        assert_eq!(hint["start"],json!(start),"{raw}");
        assert_eq!(hint["end"],json!(end),"{raw}");
        assert_eq!(hint["verified"],json!(false),"{raw}");
        assert_eq!(hint["numbering"],json!("absolute"),"{raw}");
        assert!(hint["title"].as_str().is_some_and(|s|!s.is_empty()),"{raw}");
        assert!(parsed.get("episode_start").is_none(),
            "unmarked episodes must not become canonical before anime verification: {raw}");
    }
}

#[test]
fn unmarked_range_guards_reject_titles_volumes_years_and_resolutions(){
    for raw in [
        "009-1",
        "Anime Example Season 1-7 [1080p]",
        "Anime Example Vol. 1-7",
        "Anime Example Part 1-7 [BD]",
        "Anime Example Chapter 1-7",
        "Anime Example 2015-2024",
        "Anime Example 720-1080",
        "Anime Example 1920x1080",
        "Anime Example 01-99999",
        "Anime Example 1-1002",
        "Anime Example 1-7 Days of Summer",
        "1-7",
    ] {
        let parsed=parse(raw);
        assert!(parsed.get("anime_episode_range_candidate").is_none(),
            "false unmarked anime interval: {raw}");
    }
}

#[test]
fn explicit_season_episode_ranges_keep_the_season(){
    for raw in [
        "[Gecko] Koala's Diary - S01E01-E30 [1080p]",
        "Example.Show.S01E01-S01E12.1080p",
        "Example Show S1E01-12",
        "Example Show S01 E01 ~ E12",
    ] {
        let p=parse(raw);
        assert_eq!(p["episode_range_numbering"],"season","{raw}");
        assert_eq!(p["episode_start"],1,"{raw}");
        let end=if raw.contains("E30"){30}else{12};
        assert_eq!(p["episode_end"],end,"{raw}");
        assert_eq!(p["seasons"],json!([1]),"{raw}");
        assert_eq!(p["episodes"],json!([1]),"{raw}");
        assert!(p.get("anime_episode_range_candidate").is_none(),"{raw}");
    }
}

#[test]
fn absolute_marked_ranges_and_cross_season_guards(){
    let p=parse("One.Piece.E768-E1064 [1080p]");
    assert_eq!(p["episode_range_numbering"],"absolute");
    assert_eq!(p["episode_start"],768);
    assert_eq!(p["episode_end"],1064);
    assert_eq!(p["episode_count"],297);

    let p=parse("Example Show S01E01-S02E12");
    assert_ne!(p.get("episode_range_numbering"),Some(&json!("season")),
        "a range crossing different seasons must never be flattened");
}
