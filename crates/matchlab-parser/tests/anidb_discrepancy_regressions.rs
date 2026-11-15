//! Synthetic forms of parser/matcher disagreements found among AniDB-linked
//! matches in the protected debug feed. Production names remain private.
use matchlab_parser::parse_title;
use serde_json::json;

fn candidate(raw:&str)->(String,i64){
    let parsed=parse_title(raw,false).unwrap();
    let hint=&parsed["anime_episode_candidate"];
    assert_eq!(hint["verified"],false,"{raw}: {parsed:?}");
    assert_eq!(hint["numbering"],"absolute","{raw}: {parsed:?}");
    (hint["title"].as_str().unwrap_or_else(||panic!("{raw}: {parsed:?}")).to_owned(),
        hint["episode"].as_i64().unwrap())
}

#[test]
fn undashed_fansub_episode_before_a_tag_is_an_unverified_candidate() {
    for (raw,title,episode) in [
        ("[Group] Example no Asukara 12 [3EB3C732].mkv","Example no Asukara",12),
        ("Example Ball Kai_07_(HD.720p)","Example Ball Kai",7),
        ("[Group]_Example_Actors_09_[VOSTFR][HD_1280x720].mp4","Example Actors",9),
        ("[Group] Example Kindergarten   03 [480p] mkv","Example Kindergarten",3),
        ("[Group] Example Demo Koi ga Shitai!-05.mp4","Example Demo Koi ga Shitai!",5),
        ("[Group] Example Monogatari 04 VOSTFR","Example Monogatari",4),
        ("Example Kamisama 03 480p Group TGx","Example Kamisama",3),
        ("[Group] Example 12v2 [720p].mkv","Example",12),
        // A named arc after " - " belongs to the exact anime alias.
        ("[Group]Example_Art_Online_-_Alicization_20_[1280x720][Hi10p].mkv",
            "Example Art Online - Alicization",20),
    ] {
        assert_eq!(candidate(raw),(title.to_owned(),episode),"{raw}");
        // The canonical episode stays empty until the matcher verifies it.
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"],json!([]),"{raw}: {parsed:?}");
    }
}

#[test]
fn sequel_numbers_years_and_product_codes_are_not_tagged_episodes() {
    for raw in [
        "Example Title 2 [1080p]",          // unpadded sequel number
        "Example 13 (1995) [1080p]",        // numbered film followed by its year
        "Example Runner 2049 [1080p]",      // year-like title number
        "EXMP-985",                         // product code
        "Example 12 BluRay 1080p",          // no bracket or fansub tag
        "example.show.0202.720p-grp.mkv",   // dotted scene name
        "Example Go 2019 06 10 720p HDTV",  // dated release
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert!(parsed.get("anime_episode_candidate").is_none(),"{raw}: {parsed:?}");
    }
}

#[test]
fn padded_and_subtitled_batches_are_range_candidates_not_episode_lists() {
    for (raw,title,start,end) in [
        ("Example Piece - 0575-0746 - Example Saga - 172 episodes [1080p] [Group]",
            "Example Piece",575,746),
        ("Example Dunk 1-101 dvdrip (japanese audio english sub)","Example Dunk",1,101),
        ("Example-Gi-Oh!_GX_1-26_DVDRip_(Finnish)","Example-Gi-Oh! GX",1,26),
        // The whole release title, not the prefix-only parse, names the anime.
        ("[Group] Example-Gi-Oh! 050-097 [480p]","Example-Gi-Oh!",50,97),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        let hint=&parsed["anime_episode_range_candidate"];
        assert_eq!(hint["title"],title,"{raw}: {parsed:?}");
        assert_eq!(hint["start"],start,"{raw}: {parsed:?}");
        assert_eq!(hint["end"],end,"{raw}: {parsed:?}");
        assert_eq!(hint["verified"],false,"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
    }
}
