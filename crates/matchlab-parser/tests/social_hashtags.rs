//! Social-media video titles end in glued hashtag lists. A season marker
//! inside a tag (`#showseason3`) is a keyword, not a release coordinate.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn trailing_hashtag_lists_are_not_release_fields() {
    for (raw, title) in [
        ("Example Show #fyp #anime #exampleshowseason3 #exseason3", "Example Show"),
        ("Drawing Example 🌊 || Example Show #fyp #fypシ#anime #art#drawing #showseason3", "Drawing Example 🌊 || Example Show"),
        ("Example Show #edit#exampleshow #exampleshowseason2 ", "Example Show"),
    ] {
        let parsed = parse_title(raw, false).unwrap();
        assert_eq!(parsed["title"], title, "{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"], json!([]), "{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"], json!([]), "{raw}: {parsed:?}");
    }
}

#[test]
fn numbers_and_single_tags_keep_their_meaning() {
    // A numeric "#" is an episode/issue number, and one tag is not a list.
    for raw in ["Example Show #12", "Example Show #1 #2", "Example #Alive 2020", "#fyp #anime"] {
        let parsed = parse_title(raw, false).unwrap();
        assert!(parsed["title"].as_str().unwrap().contains('#'), "{raw}: {parsed:?}");
    }
    let season = parse_title("Example Show Season 3 #fyp #anime", false).unwrap();
    assert_eq!(season["seasons"], json!([3]), "{season:?}");
}
