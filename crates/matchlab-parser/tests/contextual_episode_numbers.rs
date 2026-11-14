//! Contextual release numbers must not invent TV episodes.
use matchlab_parser::parse_title;
use serde_json::json;

fn parsed(name:&str)->serde_json::Map<String,serde_json::Value>{
    parse_title(name,false).unwrap_or_else(|error|panic!("{name}: {error}"))
}

#[test]
fn flat_film_collection_years_are_not_episodes(){
    for raw in [
        "Star.Wars.1977-1983.BluRay.1080p.DTSES6.1 2Audio.x264-CHD",
        "Star.Wars.1977–1983.BluRay.1080p.x264-CHD",
        "Harry.Potter.2001-2012.BluRay.1080p.x264-CHD",
    ]{
        let p=parsed(raw);
        assert_eq!(p["episodes"],json!([]),"{raw}: {p:?}");
        assert_eq!(p["seasons"],json!([]),"{raw}: {p:?}");
        assert!(p.get("year").is_none(),"{raw}: {p:?}");
        assert!(p["collection_year_range"].as_array().is_some(),"{raw}: {p:?}");
        assert!(!p["title"].as_str().unwrap_or("").contains("1983"),
            "{raw}: {p:?}");
    }
}

#[test]
fn a_real_single_film_year_still_parses(){
    let p=parsed("Star.Wars.1977.BluRay.1080p.x264-CHD");
    assert_eq!(p["year"],1977);
    assert_eq!(p["episodes"],json!([]));
    assert!(p.get("collection_year_range").is_none());
}

#[test]
fn spanish_four_digit_cap_is_a_tentative_grammar(){
    let p=parsed("La.Promesa.Cap.4123.720p.HDTV");
    let hint=&p["spanish_compact_episode_candidate"];
    assert_eq!(hint["season"],4,"{p:?}");
    assert_eq!(hint["episode"],123,"{p:?}");
    assert_eq!(hint["alternate_season"],41,"{p:?}");
    assert_eq!(hint["alternate_episode"],23,"{p:?}");
    assert_eq!(hint["verified"],false);

    let old=parsed("La.Promesa.Cap.1004.720p.HDTV");
    assert_eq!(old["seasons"],json!([10]));
    assert_eq!(old["episodes"],json!([4]));
    assert!(old.get("spanish_compact_episode_candidate").is_none());
}

#[test]
fn high_anime_episodes_remain_available_to_metadata_verification(){
    let p=parsed("One.Piece.E2000.1080p.WEB-DL");
    assert!(p.get("collection_year_range").is_none());
    assert_eq!(p["episodes"],json!([2000]),"{p:?}");
}
