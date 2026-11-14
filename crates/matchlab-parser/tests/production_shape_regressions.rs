//! Minimal synthetic release shapes preserving bugs found in retained debug
//! history. No private feed records, paths, publisher names or URLs are kept.
use matchlab_parser::parse_title;
use serde_json::{Value,json};

#[test]
fn transport_stream_extension_does_not_override_broadcast_quality() {
    for raw in [
        "Example.Broadcast.2021.04.Event.Practice3.HDTV.1080i.ts",
        "Example.Documentary.2019.1080i.HDTV.x264.ts",
        "Example.Documentary.2019.1080i.HDTV.x264.TS",
        "Example.Documentary.2019.HDTV/Example.Documentary.1080i.x264.ts",
        "Example.Broadcast.2024.Channel.1.HD.ts",
        "Пример 2024 - Второй этап  Example Broadcast 2024 - Stage 2  Channel 1 HD.ts",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["extension"],"ts","{raw}: {parsed:?}");
        assert_eq!(parsed["quality"],"HDTV","{raw}: {parsed:?}");
        assert!(parsed.get("trash").is_none(),"{raw}: {parsed:?}");
    }
    let unlabelled=parse_title("Example.Movie.2024.1080p.ts",false).unwrap();
    assert_eq!(unlabelled["extension"],"ts");
    assert!(unlabelled.get("quality").is_none());
    assert!(unlabelled.get("trash").is_none());
    for raw in [
        "Example.Movie.2024.TS.720p.mkv",
        "Example.Movie.2024.HDTS.720p.mkv",
        "Example.Movie.2024.HD.TS.mkv",
    ] {
        let telesync=parse_title(raw,false).unwrap();
        assert_eq!(telesync["quality"],"TeleSync","{raw}: {telesync:?}");
        assert_eq!(telesync["trash"],true,"{raw}: {telesync:?}");
    }
}

#[test]
fn domain_shaped_words_inside_a_work_title_are_not_publisher_sites() {
    for raw in [
        "Example.And.Co.S01.720p",
        "Example.And.Co.S01E02.1080p.WEB-DL.mkv",
        "Example.And.Co.2024.1080p.WEB-DL.mkv",
        "Example.And.Co.S01/Example.And.Co.S01E02.mkv",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example And Co","{raw}: {parsed:?}");
        assert!(parsed.get("site").is_none(),"{raw}: {parsed:?}");
    }
}

#[test]
fn explicit_publisher_context_still_allows_domain_extraction() {
    for raw in [
        "Example Show - publisher.tv [WEBRip 1080p]",
        "[publisher.tv] Example.Show.S01E02.1080p.WEB-DL",
        "Example.Show.S01E02.1080p.WEB-DL.publisher.tv",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Show","{raw}: {parsed:?}");
        assert_eq!(parsed["site"],"publisher.tv","{raw}: {parsed:?}");
    }
}

#[test]
fn written_season_title_recovery_cannot_restore_a_url_banner() {
    for raw in [
        "【发布网站信息 www.example.org】示例故事.第二季.2024.1080p.WEB-DL",
        "[Published at www.example.org]示例故事.第二季.2024.1080p.WEB-DL",
        "【发布网站信息 www.example.org】示例故事.第二季[杜比视界版本][全08集][简繁英字幕].2024.2160p.WEB-DL",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"示例故事","{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"],json!([2]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["year"],2024,"{raw}: {parsed:?}");
        assert_eq!(parsed["quality"],"WEB-DL","{raw}: {parsed:?}");
        assert_eq!(parsed["site"],"www.example.org","{raw}: {parsed:?}");
    }
}

#[test]
fn complete_bare_domain_banners_do_not_become_movie_titles() {
    for raw in [
        "【发布网站信息：mirror.example.com】示例电影.2015.1080p.WEB-DL",
        "[Publisher: example.org]示例电影.2015.1080p.WEB-DL",
        "示例电影.2015.WEB-DL/【发布网站信息：mirror.example.com】.mkv",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"示例电影","{raw}: {parsed:?}");
        assert_eq!(parsed["year"],2015,"{raw}: {parsed:?}");
        assert_eq!(parsed["quality"],"WEB-DL","{raw}: {parsed:?}");
    }
    let fansub=parse_title("[Fansub Group] Example.Show.S01E02.1080p.mkv",false).unwrap();
    assert_eq!(fansub["title"],"Example Show");
    assert_eq!(fansub["group"],"Fansub Group");
}

#[test]
fn digits_inside_a_release_group_do_not_become_a_production_year() {
    for group in ["TEAM1980","TEAM2024","GROUP1999","TEAM_1980","TEAM-1980","TEAM.1980"] {
        let raw=format!("Example.Show.S02E11.720p.WEB-DL-[{group}].mp4");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Show","{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"],json!([2]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([11]),"{raw}: {parsed:?}");
        assert!(parsed.get("year").is_none(),"{raw}: {parsed:?}");
    }
    let labelled=parse_title("Example.Movie.2015.1080p.WEB-DL-[TEAM1980].mkv",false).unwrap();
    assert_eq!(labelled["year"],2015);
    let old=parse_title("Example.Movie.(1980).1080p.WEB-DL.mkv",false).unwrap();
    assert_eq!(old["year"],1980);
    for raw in [
        "Example.Movie.[2024].1080p.WEB-DL.mkv",
        "Example.Movie-[2024].1080p.WEB-DL.mkv",
        "Example.Movie.[HD_2024].1080p.WEB-DL.mkv",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["year"],2024,"{raw}: {parsed:?}");
    }
    // The release-group fix must not redefine existing ambiguous attached
    // title/year forms elsewhere in the source string.
    for (raw,year) in [
        ("Movie2020.1080p",None),
        ("TMDB2020.1080p",Some(2020)),
        ("Film2020.1080p",Some(2020)),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed.get("year").and_then(Value::as_i64),year,
            "{raw}: {parsed:?}");
    }
}
