//! Recognized collection years must not invent series coordinates.
//! Use release-name fixtures only; no downloaded content is involved.
use matchlab_parser::parse_title;
use serde_json::json;

fn parsed(raw:&str)->serde_json::Map<String,serde_json::Value>{
    parse_title(raw,false).unwrap_or_else(|e|panic!("{raw}: {e}"))
}

#[test]
fn flat_film_collections_protect_both_years_before_episode_matching(){
    for (raw,years) in [
        ("Star.Wars.1977-1983.BluRay.1080p.DTSES6.1 2Audio.x264-CHD",[1977,1983]),
        ("Star.Wars.1977–1983.BluRay.1080p.x264-CHD",[1977,1983]),
        ("Harry.Potter.2001-2012.BluRay.1080p.x264-CHD",[2001,2012]),
        ("Harry Potter (2001-2012) BluRay 1080p",[2001,2012]),
    ] {
        let p=parsed(raw);
        assert_eq!(p["collection_year_range"],json!(years),"{raw}: {p:?}");
        assert_eq!(p["seasons"],json!([]),"{raw}: {p:?}");
        assert_eq!(p["episodes"],json!([]),"{raw}: {p:?}");
        assert!(p.get("episode_start").is_none(),"{raw}: {p:?}");
        assert!(p.get("year").is_none(),"{raw}: {p:?}");
        assert!(!p["title"].as_str().unwrap_or("").contains(&years[1].to_string()),
            "{raw}: {p:?}");
    }
}

#[test]
fn normal_film_years_and_explicit_tv_episodes_remain_unchanged(){
    let film=parsed("Star.Wars.1977.BluRay.1080p.x264-CHD");
    assert_eq!(film["year"],1977);
    assert_eq!(film["episodes"],json!([]));
    assert!(film.get("collection_year_range").is_none());

    let tv=parsed("Example.Series.1977-1983.S01E02.1080p.WEB-DL");
    assert_eq!(tv["seasons"],json!([1]),"{tv:?}");
    assert_eq!(tv["episodes"],json!([2]),"{tv:?}");
}

#[test]
fn high_anime_episodes_and_spanish_cap_markers_are_not_capped(){
    let anime=parsed("One.Piece.E2000.1080p.WEB-DL");
    assert_eq!(anime["episodes"],json!([2000]),"{anime:?}");
    let spanish=parsed("La.Promesa.Cap.4123.720p.HDTV");
    assert!(spanish.get("collection_year_range").is_none(),"{spanish:?}");
}
