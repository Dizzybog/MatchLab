//! Synthetic controls for title-word collisions found in a private audit.
//! Production release strings and publisher names are not retained here.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn network_names_inside_titles_do_not_hide_the_actual_source() {
    for raw in [
        "Example.&.Nick.&.Nick.&.Other.2026.2160p.HULU.WEB-DL.H265-GROUP.mkv",
        "Example.&.Nick.&.Nick.&.Other.S01E02.2160p.HULU.WEB-DL.H265-GROUP.mkv",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example & Nick & Nick & Other","{raw}: {parsed:?}");
        assert_eq!(parsed["network"],"Hulu","{raw}: {parsed:?}");
    }
    let repeated=parse_title("Example.And.Nick.2024.NICK.WEB.mkv",false).unwrap();
    assert_eq!(repeated["title"],"Example And Nick","{repeated:?}");
    assert_eq!(repeated["network"],"Nickelodeon","{repeated:?}");
    let leading_year=parse_title("2024.Example.Title.NICK.WEB.mkv",false).unwrap();
    assert_eq!(leading_year["network"],"Nickelodeon","{leading_year:?}");
}

#[test]
fn ambiguous_language_words_belong_to_the_title_before_identity_markers() {
    for (raw,title) in [
        ("Example.And.Mara.2024.1080p.WEB.mkv","Example And Mara"),
        ("Example.Doctor.Pol.S02E03.1080p.WEB.mkv","Example Doctor Pol"),
        ("El.ejemplo.se.equivoca.1989.DVDRip.mkv","El ejemplo se equivoca"),
        ("Example.Go.To.Del.Mar.2021.1080p.WEB.mkv","Example Go To Del Mar"),
        ("Example.del.Mar.3D.HOU.BluRay.1080p.mkv","Example del Mar"),
        ("Example.Hides.in.Russian.(Digital).mkv","Example Hides in Russian"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert!(parsed.get("languages").is_none_or(|languages|languages==&json!([])),
            "{raw}: {parsed:?}");
    }
    for (word,code) in [("Mara","mr"),("Pol","pl"),("se","sv"),("Russian","ru")] {
        let raw=format!("Example.in.{word}.2024.{word}.1080p.WEB.mkv");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["title"],format!("Example in {word}"),"{raw}: {parsed:?}");
        assert_eq!(parsed["languages"],json!([code]),"{raw}: {parsed:?}");
    }
    let leading_year=parse_title("2024.Example.Title.Mara.1080p.WEB.mkv",false).unwrap();
    assert_eq!(leading_year["languages"],json!(["mr"]),"{leading_year:?}");
    for raw in [
        "Example.Movie.2024.1080p.WEB-DL.in.Russian.mkv",
        "Example.Movie.2024.1080p.WEB-DL.[in Russian].mkv",
        "Example.Movie.1080p.WEB-DL.in.Russian.mkv",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Movie","{raw}: {parsed:?}");
        assert_eq!(parsed["languages"],json!(["ru"]),"{raw}: {parsed:?}");
    }
    let both=parse_title(
        "Example.Hides.in.Russian.2024.1080p.WEB-DL.in.Russian.mkv",false).unwrap();
    assert_eq!(both["title"],"Example Hides in Russian","{both:?}");
    assert_eq!(both["languages"],json!(["ru"]),"{both:?}");
}

#[test]
fn true_diamond_and_mono_title_words_keep_coordinates_and_specific_tags() {
    for (raw,title) in [
        ("Example.The.True.Story.1973.1080p.WEB.mkv","Example The True Story"),
        ("Example.True.Crime.S01E01.720p.WEB.mkv","Example True Crime"),
        ("Example.of.the.Diamond.E16.720p.WEB.mkv","Example of the Diamond"),
        ("The.Diamond.Example.1969.720p.WEB.mkv","The Diamond Example"),
        ("[Fansub] Example wa Mono ga Osuki - 04 (480p).mkv","Example wa Mono ga Osuki"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert!(parsed.get("audio").is_none(),"{raw}: {parsed:?}");
        assert!(parsed.get("channels").is_none(),"{raw}: {parsed:?}");
        assert!(parsed.get("edition").is_none(),"{raw}: {parsed:?}");
        if raw.contains("E16") {assert_eq!(parsed["episodes"],json!([16]));}
    }
    for raw in ["Example.The.True.Story.2024.TRUE.mkv",
        "Example.The.True.Story.2024.True.mkv",
        "Example.The.True.Story.S01E02.TRUE.mkv",
        "Example.The.True.Story.2024.TrueHD7.1.mkv"] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example The True Story","{raw}: {parsed:?}");
        assert_eq!(parsed["audio"],json!(["TrueHD"]),"{raw}: {parsed:?}");
    }
    let leading_year=parse_title("2024.Example.Title.TRUE.mkv",false).unwrap();
    assert_eq!(leading_year["audio"],json!(["TrueHD"]),"{leading_year:?}");
    let mono=parse_title("Example.Mono.Series.S01E02.MONO.mkv",false).unwrap();
    assert_eq!(mono["title"],"Example Mono Series","{mono:?}");
    assert_eq!(mono["channels"],json!(["mono"]),"{mono:?}");
    let diamond=parse_title("Example.Diamond.Series.2024.Diamond.1080p.WEB.mkv",false).unwrap();
    assert_eq!(diamond["title"],"Example Diamond Series","{diamond:?}");
    assert_eq!(diamond["edition"],"Diamond Edition","{diamond:?}");
}

#[test]
fn tag_only_brackets_and_bracketed_title_prose_are_distinguished() {
    for raw in ["[NICK] Example.Movie.2024.WEB.mkv",
        "(NICK) Example.Movie.2024.WEB.mkv",
        "【NICK】 Example.Movie.2024.WEB.mkv",
        "（NICK） Example.Movie.2024.WEB.mkv"] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["network"],"Nickelodeon","{raw}: {parsed:?}");
    }
    for (raw,title) in [
        ("Example.(Nick.And.Other).2024.WEB.mkv","Example (Nick And Other)"),
        ("Example.(The.True.Story).2024.WEB.mkv","Example (The True Story)"),
        ("Example.(in.Russian).2024.WEB.mkv","Example (in Russian)"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert!(parsed.get("network").is_none(),"{raw}: {parsed:?}");
        assert!(parsed.get("audio").is_none(),"{raw}: {parsed:?}");
        assert!(parsed.get("languages").is_none_or(|languages|languages==&json!([])),
            "{raw}: {parsed:?}");
    }
    for (tag,field,value) in [("TRUE","audio",json!(["TrueHD"])),
        ("Mara","languages",json!(["mr"])),("MONO","channels",json!(["mono"]))] {
        let raw=format!("[{tag}] Example.Movie.2024.WEB.mkv");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed[field],value,"{raw}: {parsed:?}");
    }
}

#[test]
fn year_terminated_hyphenated_titles_do_not_become_release_groups() {
    for (raw,title) in [("the-example-1930","the-example"),
        ("league-of-example-men-1937","league-of-example-men"),
        ("example-of-a-courtesan-1972","example-of-a-courtesan")] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert!(parsed.get("group").is_none(),"{raw}: {parsed:?}");
    }
    for raw in ["Example.Movie.2024.1080p.WEB-GROUP.mkv",
        "Example.Movie.2024-GROUP.mkv","Example.Title-GROUP.mkv",
        "Example.Movie.2024-GROUP","Example.Title-GROUP",
        "2024.Example.Title-GROUP","2024.Example.Title-GROUP.mkv",
        "[GROUP] Example.Movie.2024.WEB.mkv"] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["group"],"GROUP","{raw}: {parsed:?}");
    }
}

#[test]
fn captured_publisher_domain_suffixes_do_not_restore_release_metadata_to_title() {
    for suffix in ["cc","pl","cz","nz","my","im","ws","ph","eu","pw",
        "be","vc","red","info","lol","pe","xyz","re","site","to","biz",
        "onl","vg","ps","me","io"] {
        let raw=format!("[ publisher.{suffix} ] Example.Movie.2022.FRENCH.1080p.WEB.x264-GROUP.mkv");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Movie","{raw}: {parsed:?}");
        assert_eq!(parsed["group"],"GROUP","{raw}: {parsed:?}");
        assert_eq!(parsed["languages"],json!(["fr"]),"{raw}: {parsed:?}");
    }
    let series=parse_title("[publisher.pl] Example.Series.S01E10.PL.SKY.WEB-DL.H264.DDP5.1-GROUP",false).unwrap();
    assert_eq!(series["title"],"Example Series","{series:?}");
    assert_eq!(series["episodes"],json!([10]));
    assert_eq!(series["languages"],json!(["pl"]),"{series:?}");
    let annotation=parse_title("[publisher.cz FRENCH] Example.Movie.2024.1080p.WEB.mkv",false).unwrap();
    assert_eq!(annotation["languages"],json!(["fr"]),"{annotation:?}");
    let fansub=parse_title("[Example-Raws] Named.Series - 04 [720p].mkv",false).unwrap();
    assert_eq!(fansub["group"],"Example-Raws","{fansub:?}");
    assert_eq!(fansub["title"],"Named Series","{fansub:?}");
    for separator in ["-",".","_"," - "] {
        let raw=format!("[publisher.pl]{separator}[mirror.pl].Example.Movie.2024.WEB-GROUP.mkv");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Movie","{raw}: {parsed:?}");
    }
    let named=parse_title("[publisher.pl]-[Fansub] Named.Series - 04 [720p].mkv",false).unwrap();
    assert_eq!(named["title"],"Named Series","{named:?}");
}

#[test]
fn digital_format_before_a_publisher_is_not_part_of_the_work_title() {
    let parsed=parse_title("Example Hides in Russian (Digital) (Publisher7)",false).unwrap();
    assert_eq!(parsed["title"],"Example Hides in Russian","{parsed:?}");
    assert_eq!(parsed["group"],"Publisher7","{parsed:?}");
    assert!(parsed.get("languages").is_none_or(|languages|languages==&json!([])));
    for (raw,title) in [("Digital.2024.WEB.mkv","Digital"),
        ("Example.(Digital.World).2024.WEB.mkv","Example (Digital World)")] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
    }
}
