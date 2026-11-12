//! Historical user-supplied MatchLab torrents, retained verbatim as a
//! production-facing parser regression catalog (November 2026).
//! A passing parse does NOT prove an IMDb/AniDB identity. Those require
//! verified relational metadata and are tested by the server integration suite.
//! When an absolute episode is ambiguous, assert its *candidate* marker only;
//! the parser must never invent a verified SID or ASID.
use matchlab_parser::parse_title;
use serde_json::{json,Map,Value};

fn parse(raw:&str)->Map<String,Value>{
    parse_title(raw,false).unwrap_or_else(|error|panic!("{raw}: {error}"))
}

#[test]
fn historical_explicit_season_episode_titles(){
    for (raw,season,episode) in [
        ("Dragon.Ball.Super.S01E03.DUBBED",1,3),
        ("JoJos Bizarre Adventure S02E37 DUBBED XviD-AFG",2,37),
        ("Bleach S16E19",16,19),
        ("Solo Leveling S02E03",2,3),
        ("bleach.S17E01",17,1),
        ("bleach.S17.E01",17,1),
        ("Bleach.S01E01.1080p.WEB",1,1),
        ("Monster.the.jeffrey.dahmer.story.s01.e01",1,1),
        ("The.Office.S07E25+E26.Search.Committee",7,25),
        ("Formula1.S2025E86.Italy.Grand.Prix",2025,86),
        ("Cops Seasons 1-24/s6e10.avi",6,10),
        ("Cops Seasons 1-24/6/s6e10.avi",6,10),
        ("The Simpsons Season 9/Season 9/S09E24 Lost Our Lisa.mkv",9,24),
        ("The Simpsons Season 9/S09E24 Lost Our Lisa.mkv",9,24),
        ("The.Show.2005.S01.2160p/Season.2.720p/The.Show.E06.1080p.mkv",2,6),
    ]{
        let p=parse(raw);
        assert_eq!(p["seasons"],json!([season]),"season: {raw}: {p:?}");
        assert!(p["episodes"].as_array().is_some_and(|a|a.contains(&json!(episode))),
            "episode {episode} missing from {raw}: {p:?}");
    }
}

#[test]
fn historical_season_only_and_specials_do_not_invent_episode(){
    for (raw,season) in [
        ("Bleach S01",1),("Bleach_S02",2),("Bleach.S07.MULTi.1080p.WEB.H264-UwU",7),
        ("Bleach S17",17),("Supernatural S02",2),
    ]{
        let p=parse(raw);
        assert_eq!(p["seasons"],json!([season]),"{raw}: {p:?}");
        assert_eq!(p["episodes"],json!([]),"{raw}: {p:?}");
    }
    for raw in [
        "One Piece Film Red",
        "District 9",
        "The 100",
    ]{
        let p=parse(raw);
        assert!(p["title"].as_str().is_some_and(|t|!t.is_empty()),"{raw}: {p:?}");
        assert_eq!(p["episodes"],json!([]),"unverified movie-number episode: {raw}");
    }
}

#[test]
fn historical_absolute_anime_numbers_stay_tentative(){
    for (raw,base,episode) in [
        ("Bleach_300","Bleach",300),
        ("Bleach.300","Bleach",300),
        ("Bleach_320","Bleach",320),
        ("Bleach_338","Bleach",338),
        ("Bleach 326","Bleach",326),
        ("Bleach 400","Bleach",400),
        ("Bleach_345","Bleach",345),
        ("Naruto Shippuuden 291","Naruto Shippuuden",291),
    ]{
        let p=parse(raw);
        let candidate=&p["anime_episode_candidate"];
        assert_eq!(candidate["episode"],episode,"{raw}: {p:?}");
        assert_eq!(candidate["title"],base,"{raw}: {p:?}");
    }
    let ranged=parse("Bleach_320-360");
    assert_eq!(ranged["anime_episode_range_candidate"]["start"],320);
    assert_eq!(ranged["anime_episode_range_candidate"]["end"],360);
    assert!(ranged.get("episode_start").is_none(),
        "parser cannot verify an absolute anime interval without metadata");
    let overwide=parse("Bleach_320 - 400");
    assert!(overwide.get("episode_start").is_none(),
        "overwide anime pack must not publish a verified range: {overwide:?}");
}

#[test]
fn historical_date_based_releases_keep_iso_date(){
    for (raw,date) in [
        ("The Daily Show 2018 02 13 Ladies Night News Making Women 1080p WEB x264-TBS[N1C]",
            "2018-02-13"),
        ("The.Daily.Show.2022.01.20.1080p.WEB-DL","2022-01-20"),
        ("The.Daily.Show.2022-01-20.1080p.WEB-DL","2022-01-20"),
    ]{
        let p=parse(raw);
        assert_eq!(p["date"],date,"{raw}: {p:?}");
    }
}

#[test]
fn historical_multilingual_and_batch_markers(){
    let p=parse("Born.with.Luck.2026.EP01-24.HD1080P.X264.AAC.Mandarin.CHS.XLYS");
    assert_eq!(p["episode_start"],1,"{p:?}");
    assert_eq!(p["episode_end"],24,"{p:?}");
    let office=parse("The.Office.S07E25+E26.Search.Committee");
    assert!(office["episodes"].as_array()
        .is_some_and(|a|a.contains(&json!(25))&&a.contains(&json!(26))),
        "dual episode marker lost: {office:?}");
    let russian=parse("Леди Баг – Сезон 3, Эпизод 21 – Кукловод 2");
    assert_eq!(russian["seasons"],json!([3]),"{russian:?}");
    assert_eq!(russian["episodes"],json!([21]),"{russian:?}");
}

#[test]
fn historically_reported_releases_are_accepted_without_panics(){
    // These are smoke inputs. A nonempty parsed title is not proof of a
    // correct identity or AniDB coordinate; don't smuggle inferred labels
    // into authoritative matcher tests.
    for raw in [
        "[ACX] El Cazador - 19 - A Man Who Protects",
        "BBC Indian Ocean 5of6 Sri Lanka to Bangladesh",
        "Jujutsu Kaisen - 10",
        "Jujutsu Kaisen S2 - 07",
        "One Piece Film Red",
        "Naruto Shippuuden 291",
        "Jojo's bizarre adventure stone ocean - 27",
        "Jojo's bizarre adventure stone ocean 27",
        "Jojo's bizarre adventure stone ocean 27.mkv",
        "[Erai-raws] Jihanki 3 - 08 [1080p CR WEBRip HEVC AAC][MultiSub][C869EEDC].mkv",
        "Cops Seasons 1-24/Unknown Folder/6/s6e10.avi",
        "The Daily Show 2018 02 13 Ladies Night News Making Women 1080p WEB x264-TBS[N1C]",
    ]{
        let p=parse(raw);
        assert!(p["title"].as_str().is_some_and(|t|!t.is_empty()),"{raw}: {p:?}");
    }
}
