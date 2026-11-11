//! Special release qualifiers follow TheTVDB's S00 convention. Do not
//! confuse standalone bonus episodes with a serialised OVA/ONA or a show
//! whose ordinary title happens to contain "special".
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn isolated_special_release_keywords_set_season_zero(){
    for (raw,kind) in [
        ("Example Anime OVA 1 [1080p].mkv","ova"),
        ("Example Anime.OAV.E02.1080p.mkv","oav"),
        ("Example Anime OAD 01.mkv","oad"),
        ("Example Show Special E03 1080p WEB-DL","special"),
        ("Example Show Specials E01.mkv","specials"),
        ("Example Show Bonus Episode 04.mkv","bonus episode"),
        ("Example Show Extra Episode 02.mkv","extra episode"),
        ("Example Show Unaired Episode 01.mkv","unaired episode"),
        ("Example Show Unaired Pilot.mkv","unaired pilot"),
        ("Example Show Pilot Episode 01.mkv","pilot episode"),
        ("Example Show Omake 01.mkv","omake"),
        ("Example Show Picture Drama 02.mkv","picture drama"),
        ("Example Show Webisode 01.mkv","webisode"),
        ("Example Anime SP01.mkv","sp01"),
        ("Example Show [SP] E01.mkv","sp"),
        ("Example Show Recap Special 01.mkv","recap special"),
        ("Example Show Behind the Scenes E01.mkv","behind the scenes"),
        ("Example Show ONA Special E01.mkv","ona special"),
        ("Example Show One Shot 01.mkv","one shot"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!([0]),"{raw}: {parsed:?}");
        assert_eq!(parsed["special_episode"],json!(true),"{raw}: {parsed:?}");
        assert_eq!(parsed["special_type"],json!(kind),"{raw}: {parsed:?}");
    }
}

#[test]
fn specials_folder_is_inherited_but_explicit_child_season_wins(){
    let parsed=parse_title("Example Show/Specials/E03.1080p.mkv",false).unwrap();
    assert_eq!(parsed["seasons"],json!([0]),"{parsed:?}");
    assert_eq!(parsed["episodes"],json!([3]),"{parsed:?}");
    assert_eq!(parsed["special_episode"],json!(true),"{parsed:?}");

    let ova=parse_title("Example Show/OVA/E03.1080p.mkv",false).unwrap();
    assert_eq!(ova["seasons"],json!([0]),"{ova:?}");
    assert_eq!(ova["title"],"Example Show","{ova:?}");
    assert_eq!(ova["special_type"],"ova","{ova:?}");

    let ordinary=parse_title("Example Show/Specials/Example.Show.S01E03.mkv",false).unwrap();
    assert_eq!(ordinary["seasons"],json!([1]),"{ordinary:?}");
    assert!(ordinary.get("special_episode").is_none(),"{ordinary:?}");
    assert!(ordinary.get("special_type").is_none(),"{ordinary:?}");
}

#[test]
fn formats_and_titles_that_are_not_individual_specials_do_not_set_s00(){
    for raw in [
        "Example Anime ONA E01.mkv",
        "Example Anime OVA Complete",
        "Example Anime OVA 01-12.mkv",
        "Example Anime OVA E01-E12.mkv",
        "Example Anime OAD Batch 01",
        "Special Ops Lioness S01E03 1080p",
        "Special Edition 2025 1080p BluRay",
        "Example Show Season 2 OVA Episode 01.mkv",
        "Example Show S02E03 Bonus Episode.mkv",
        "Example Show Episode 03 1080p WEB-DL",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_ne!(parsed["seasons"],json!([0]),"{raw}: {parsed:?}");
        assert!(parsed.get("special_episode").is_none(),"{raw}: {parsed:?}");
    }
}

#[test]
fn special_tag_does_not_erase_known_episode_number_or_invent_one(){
    let release=parse_title("Example.Show.Special.E03.1080p.mkv",false).unwrap();
    assert_eq!(release["seasons"],json!([0]),"{release:?}");
    assert_eq!(release["episodes"],json!([3]),"{release:?}");

    let unnumbered=parse_title("Example.Show.OVA.1080p.mkv",false).unwrap();
    assert_eq!(unnumbered["seasons"],json!([0]),"{unnumbered:?}");
    assert_eq!(unnumbered["episodes"],json!([]),"{unnumbered:?}");
}
