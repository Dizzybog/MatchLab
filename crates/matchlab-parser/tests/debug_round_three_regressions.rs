//! Synthetic forms of parser defects confirmed in the third protected
//! debug-feed review. Production release names remain private.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn year_glued_to_the_title_keeps_the_last_title_letter() {
    for (raw,title,year) in [
        ("Example(1989)","Example",1989),
        ("The Example Return(1990)[DVDRip]","The Example Return",1990),
        ("Some Person_Born To Win(1971)[DVDRip]","Some Person Born To Win",1971),
        ("Escape to Examplea1979_BDRip","Escape to Examplea",1979),
        // The preserved letter is no longer mistaken for a language tag.
        ("Rush(1989)","Rush",1989),
        // Existing separated and leading forms are unchanged.
        ("Example (1989)","Example",1989),
        ("Example.1989.1080p","Example",1989),
        ("(1989) Example","Example",1989),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert_eq!(parsed["year"],year,"{raw}: {parsed:?}");
        assert!(parsed.get("languages").is_none(),"{raw}: {parsed:?}");
    }
    // S/E-prefixed four digit numbers are still not production years.
    let coordinates=parse_title("Example.S2019E01",false).unwrap();
    assert!(coordinates.get("year").is_none(),"{coordinates:?}");
    assert_eq!(coordinates["seasons"],json!([2019]));
}

#[test]
fn release_group_digits_are_not_a_french_t_season() {
    for raw in [
        "Example 1983 1080p BluRay x264-samplet0",
        "Road.To.Example.2014.720p.BRRip.x264-Examplet99",
        "Example.Nation.2015.BRRip.XviD.EX4MPL3T69",
        "Example.2018.720p.BluRay.x264-XT601",
        "Example.Too.1993.BDRip.X264.CHT-xedit110",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
    }
    // A standalone French tome/season marker is preserved.
    for (raw,seasons,episodes) in [
        ("Exemple.T01",vec![1],vec![]),
        ("Exemple T3E05",vec![3],vec![5]),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!(seasons),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!(episodes),"{raw}: {parsed:?}");
    }
}

#[test]
fn season_pack_does_not_borrow_an_episode_from_a_later_technical_tag() {
    for raw in [
        "Example.S02.2020.WEB-DL.3Audio.MiniSD-GRP",
        "Example.S02.WEB-DL.2Audio.x264",
        "Example.Show.S01.2025.1080p.WEB-DL.H264.AAC",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"].as_array().unwrap().len(),1,"{raw}: {parsed:?}");
    }
    // Every adjacent season/episode spelling still parses.
    for raw in ["Show.S01.05.720p","Show S01 - 05 720p","Show.S01E05",
        "Show.S01.E05","Show S01 E05","Show.s01x05","Show.S01.Ep.05"] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!([1]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([5]),"{raw}: {parsed:?}");
    }
}

#[test]
fn qhd_inside_a_site_name_is_not_a_resolution() {
    let parsed=parse_title(
        "【发布 www.EXQHDTV.com】Example.Show.S01.2025.1080p.WEB-DL.H264.AAC",false).unwrap();
    assert_eq!(parsed["resolution"],"1080p","{parsed:?}");
    assert_eq!(parsed["seasons"],json!([1]),"{parsed:?}");
    assert_eq!(parsed["episodes"],json!([]),"{parsed:?}");
    assert_eq!(parsed["title"],"Example Show","{parsed:?}");
    for raw in ["Example.QHD.S01E01","Example.2020.WQHD.WEB-DL"] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["resolution"],"1440p","{raw}: {parsed:?}");
    }
}

#[test]
fn degenerate_bracketed_batch_is_one_unverified_episode_candidate() {
    for raw in [
        "[Group] Example Anime (24-24) (1080p) [ABCD1234].mkv",
        "Example Anime (24-24)",
        "[Group] Example Anime [24-24] [1080p]",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Anime","{raw}: {parsed:?}");
        let candidate=&parsed["anime_episode_range_candidate"];
        assert_eq!(candidate["start"],24,"{raw}: {parsed:?}");
        assert_eq!(candidate["end"],24,"{raw}: {parsed:?}");
        assert_eq!(candidate["verified"],false,"{raw}: {parsed:?}");
        // Never an invented season or a canonical episode without metadata.
        assert_eq!(parsed["seasons"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
    }
    // An unbracketed score-like number pair stays in an ordinary title.
    let film=parse_title("Example 2-2 (2010)",false).unwrap();
    assert_eq!(film["title"],"Example 2-2","{film:?}");
    // Ordinary batches are unchanged.
    let batch=parse_title("Example Anime (01-12) [1080p]",false).unwrap();
    assert_eq!(batch["anime_episode_range_candidate"]["start"],1);
    assert_eq!(batch["anime_episode_range_candidate"]["end"],12);
}

#[test]
fn sample_files_inherit_the_release_title() {
    for (raw,title,year) in [
        ("Example.But.You.2023.1080p.WEBRip.h264-GRP/Sample/sample.mkv",
            "Example But You",2023),
        ("The.Last.Example.2010.DVDRip.XviD.AC3-GRP/Sample/Sample.avi",
            "The Last Example",2010),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert_eq!(parsed["year"],year,"{raw}: {parsed:?}");
    }
    // A release that is itself named Sample keeps its title.
    let own=parse_title("Sample.2019.1080p.WEB",false).unwrap();
    assert_eq!(own["title"],"Sample","{own:?}");
}

#[test]
fn technical_numbers_of_a_dated_release_are_not_coordinates() {
    for raw in [
        // A repeated or second year.
        "Example Reunion (2012) [DvdRip 2012] avi",
        "Example Pentalogy (2003 to 2017)[1080p - BDRip]",
        // A file size in GB and the picture format.
        "Example.1958.dvdrip_[1.46]_[grp].avi",
        "Example (1995) [dvd] [ntsc] [16x9 proper]",
        // A bare codec number.
        "example.-1969.1080p.-web-dl.-aac-2.0.-h.-264-grp",
        "The.Example.Christmas.2012 DVDrip 264 AAC-GRP",
        "example.-1974.-720p.-blu-ray.-hq.x-265.10bit-grp",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert!(parsed.get("year").is_some(),"{raw}: {parsed:?}");
    }
    // Labelled or undated coordinates of the same shapes are preserved.
    for (raw,seasons,episodes) in [
        ("[group][EXAMPLE][264][1280x720]",vec![],vec![264]),
        ("The Example [2.14] The Carpet.avi",vec![2],vec![14]),
        ("Example.E2004.Title.480p",vec![],vec![2004]),
        ("Example 2019 E1999 1080p",vec![],vec![1999]),
        ("Example 16x09 720p",vec![16],vec![9]),
        ("Example.2x265.720p",vec![2],vec![265]),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!(seasons),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!(episodes),"{raw}: {parsed:?}");
    }
}
