//! Regressions from the first opt-in community-learning Issue #90.
//! Only parser fields with explicit raw markers are asserted here; a report's
//! IMDb identity and its unverified observations are never accepted as labels.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn community_s02e18_restores_zero_padded_explicit_season(){
    let cases=[
        ("CBC-TheWeekly.20190217.s02e18.SilentTreatment.YouveBeenTrolled.FakeFaces.1280x720.mp4",2,18),
        ("Example.Show.s01e09.WEB-DL",1,9),
        ("Example.Show.S12E08.1080p",12,8),
    ];
    for (raw,season,episode) in cases{
        let p=parse_title(raw,false).expect("parsing release");
        assert_eq!(p["seasons"],json!([season]),"{raw}: {p:?}");
        assert_eq!(p["episodes"],json!([episode]),"{raw}: {p:?}");
    }
}

#[test]
fn community_s2_anime_pack_keeps_season_but_not_fabricated_episodes(){
    for raw in [
        "[SubsPlease] Princess Connect! Re-Dive S2 (01-12) (480p) [Batch]",
        "[orion origin] Shinigami Bocchan to Kuro Maid S2 [13-24] [WebRip] [1080p]",
    ]{
        let p=parse_title(raw,false).expect("parsing anime pack");
        assert_eq!(p["seasons"],json!([2]),"{raw}: {p:?}");
        assert_eq!(p["episodes"],json!([]),"{raw}: {p:?}");
        assert_eq!(p["anime_episode_range_candidate"]["season_hint"],json!(2),"{raw}: {p:?}");
        assert_eq!(p["anime_episode_range_candidate"]["numbering"],json!("absolute"));
        assert_eq!(p["anime_episode_range_candidate"]["verified"],json!(false));
        assert!(p.get("episode_start").is_none(),"{raw}: {p:?}");
    }
}

#[test]
fn unmarked_batches_remain_tentative_and_seasonless_without_explicit_s_marker(){
    for raw in [
        "[SubsPlease] Spy x Family (01-25) (1080p) [Batch]",
        "Great Teacher Onizuka (01-43) [720p]",
    ]{
        let p=parse_title(raw,false).expect("parsing seasonless batch");
        assert_eq!(p["seasons"],json!([]),"{raw}: {p:?}");
        assert_eq!(p["episodes"],json!([]),"{raw}: {p:?}");
        assert!(p.get("episode_start").is_none(),"{raw}: {p:?}");
    }
}

#[test]
fn specials_season_zero_remains_valid_and_dimensions_are_not_coordinates(){
    // S00E18 is valid TV special-episode notation. Do not make the parser
    // mistake of rejecting specials when fixing positive season prefixes.
    let p=parse_title("Example.Show.S00E18.WEB-DL",false).unwrap();
    assert_eq!(p["seasons"],json!([0]),"{p:?}");
    assert_eq!(p["episodes"],json!([18]),"{p:?}");
    let technical=parse_title("Example.Show.1920x1080.WEB-DL",false).unwrap();
    assert_eq!(technical["seasons"],json!([]),"{technical:?}");
    assert_eq!(technical["episodes"],json!([]),"{technical:?}");
}

#[test]
fn existing_native_codec_patterns_do_not_need_duplicate_ml_regex(){
    for release in [
        "Example.Show.S01E02.H 264.WEB-DL.mkv",
        "Example.Show.S01E02.x 264.WEB-DL.mkv",
        "Example.Show.S01E02.H.264.BluRay.mp4",
    ]{
        let p=parse_title(release,false).unwrap();
        assert_eq!(p["codec"],json!("avc"),"{release}: {p:?}");
    }
}

#[test]
fn community_pandora_release_markers_do_not_pollute_series_title(){
    let raw="[ OxTorrent.cc ] Pandora.2019.S02E01.FASTSUB.VOSTFR.WEBRip.Xvid-EXTREME.avi";
    let p=parse_title(raw,false).expect("French TV release");
    assert_eq!(p["title"],json!("Pandora"),"{p:?}");
    assert_eq!(p["seasons"],json!([2]),"{p:?}");
    assert_eq!(p["episodes"],json!([1]),"{p:?}");
    assert_eq!(p["codec"],json!("xvid"),"{p:?}");
    assert_eq!(p["quality"],json!("WEBRip"),"{p:?}");
}

#[test]
fn community_known_opus_and_dts_channel_markers_are_not_reported_missing(){
    let opus=parse_title("Game of Thrones S06 1080p BluRay Multi AV1 Opus [AV1D]",false).unwrap();
    assert!(opus.get("audio").and_then(serde_json::Value::as_array).is_some_and(|a|a.contains(&json!("OPUS"))),"{opus:?}");
    assert_eq!(opus["codec"],json!("av1"),"{opus:?}");
    let dts=parse_title(
        "Homem.Formiga.e.a.Vespa.2018.REMUX.UHD.BluRay.2160p.HEVC.DTS-HD.MA7.1-GUR",
        false).unwrap();
    assert!(dts.get("audio").and_then(serde_json::Value::as_array).is_some_and(|a|a.contains(&json!("DTS Lossless"))),"{dts:?}");
    assert!(dts.get("channels").and_then(serde_json::Value::as_array).is_some_and(|a|a.contains(&json!("7.1"))),"{dts:?}");
}

#[test]
fn community_m4v_extension_and_natural_language_airdate(){
    let tv=parse_title("The Apprentice S20E01.m4v",false).unwrap();
    assert_eq!(tv.get("extension"),Some(&json!("m4v")),"{tv:?}");
    assert_eq!(tv["seasons"],json!([20]),"{tv:?}");
    assert_eq!(tv["episodes"],json!([1]),"{tv:?}");
    let dated=parse_title(
        "Exotic4k - Lorraine Mack (Latin Heat) NEW 08 November 2014 1080p",false).unwrap();
    assert_eq!(dated.get("date"),Some(&json!("2014-11-08")),"{dated:?}");
}

#[test]
fn community_existing_native_markers_remain_supported_without_duplicate_ml_rules(){
    for (title,field,value) in [
        ("Example.Show.S01E02.mkv","extension",json!("mkv")),
        ("Another.Movie.2024.WEB-DL","quality",json!("WEB-DL")),
        ("Another.Movie.2024.BluRay","quality",json!("BluRay")),
        ("Example.Show.S02E03.720p.HDTV","quality",json!("HDTV")),
    ]{
        let p=parse_title(title,false).unwrap();
        assert_eq!(p[field],value,"{title}: {p:?}");
    }
}

#[test]
fn path_fallback_validates_fields_independently_from_nearest_component(){
    // Coordinates from the leaf; other fields from the closest valid layer.
    // A numeric intermediate directory is neither an episode nor a title
    // unless metadata independently verifies a series of that name.
    let release="Cops Seasons 1-24 1989 720p BluRay DTS-HD MA 5.1/6/Season 6 WEB-DL/s6e10.avi";
    let parsed=parse_title(release,false).unwrap();
    assert_eq!(parsed["seasons"],json!([6]),"{parsed:?}");
    assert_eq!(parsed["episodes"],json!([10]),"{parsed:?}");
    assert_eq!(parsed["extension"],json!("avi"),"{parsed:?}");
    assert_eq!(parsed["year"],json!(1989),"{parsed:?}");
    assert_eq!(parsed["quality"],json!("WEB-DL"),"{parsed:?}");
    assert_eq!(parsed["resolution"],json!("720p"),"{parsed:?}");
    assert!(parsed["audio"].as_array().is_some_and(|v|
        v.contains(&json!("DTS Lossless"))),"{parsed:?}");
}
