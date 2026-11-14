//! Synthetic forms of independently confirmed parser bugs. Production
//! history, original release titles and publisher names remain private.
use matchlab_parser::parse_title;
use serde_json::json;

#[test]
fn collection_year_ranges_preserve_independent_season_labels() {
    for (raw,seasons,years) in [
        ("Example.Show.(2011-2012).Complete.Season.1.S01.720p.WEB-DL",
            vec![1],vec![2011,2012]),
        ("Example.Show.[S01-06].(1985-1991).DVD",
            vec![1,2,3,4,5,6],vec![1985,1991]),
        ("Пример (07 сезон) (1999-2000) BDRip 1080",
            vec![7],vec![1999,2000]),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["seasons"],json!(seasons),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["collection_year_range"],json!(years),"{raw}: {parsed:?}");
        assert!(parsed.get("year").is_none(),"{raw}: {parsed:?}");
    }
    let films=parse_title("Example.Collection.1977-1983.BluRay.DTSES6.1",false).unwrap();
    assert_eq!(films["seasons"],json!([]));
    assert_eq!(films["episodes"],json!([]));
}

#[test]
fn title_word_it_is_not_an_itunes_release_tag() {
    for (raw,title) in [
        ("Example.Person.Crushing.It.2024.1080p.WEB.H264-GROUP",
            "Example Person Crushing It"),
        ("Example.Play.It.Forward.S02E04.1080p.WEB.h264-GROUP",
            "Example Play It Forward"),
        ("Example.Runs.Through.It.1992.1080p.WEB-GROUP",
            "Example Runs Through It"),
        ("【Publisher www.example.org】示例[无字片源].Example.Runs.Through.It.1992.1080p.MA.WEB-GROUP",
            "Example Runs Through It"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],title,"{raw}: {parsed:?}");
        assert!(parsed.get("network").is_none(),"{raw}: {parsed:?}");
    }
    for raw in [
        "Example.Show.2024.720p.iT.WEB-DL.H264-GROUP",
        "Example.Show.S01E02.1080p.iTunes.WEB-DL.H264-GROUP",
        "【Publisher www.example.org】Example.Show.2024.720p.iT.WEB-DL.H264-GROUP",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["title"],"Example Show","{raw}: {parsed:?}");
        assert_eq!(parsed["network"],"iTunes","{raw}: {parsed:?}");
    }
    let combined=parse_title(
        "【Publisher www.example.org】Example.Runs.Through.It.1992.720p.iT.WEB-DL.H264-GROUP",
        false).unwrap();
    assert_eq!(combined["title"],"Example Runs Through It","{combined:?}");
    assert_eq!(combined["network"],"iTunes","{combined:?}");
}

#[test]
fn removing_a_broadcast_date_preserves_adjacent_metadata_boundaries() {
    for (raw,date,quality) in [
        ("Example.Show.2020.03.06.HDTV.x264-GROUP","2020-03-06","HDTV"),
        ("Example.Show.2025.09.08.HDTV.720p.x264.mp4","2025-09-08","HDTV"),
        ("Example.Show.2018.02.13.WEB-DL.x264.mkv","2018-02-13","WEB-DL"),
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["date"],date,"{raw}: {parsed:?}");
        assert_eq!(parsed["title"],"Example Show","{raw}: {parsed:?}");
        assert_eq!(parsed["quality"],quality,"{raw}: {parsed:?}");
        assert_eq!(parsed["codec"],"avc","{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert!(parsed.get("year").is_none(),"{raw}: {parsed:?}");
    }
    let words=parse_title("Example.Title.2024.01.02.Real.Episode.Words.1080p",false).unwrap();
    assert_eq!(words["title"],"Example Title");
    assert_eq!(words["date"],"2024-01-02");
    assert!(words.get("quality").is_none());
    let leading=parse_title(
        "[Published at www.example.org] - 2019.12.30 - Example Performer - Named Segment (1080p).mp4",
        false).unwrap();
    assert_eq!(leading["title"],"Example Performer - Named Segment","{leading:?}");
    assert_eq!(leading["date"],"2019-12-30");
    let branded=parse_title(
        "Example NBC 2022 06 17 Guest Name After the Event 480p",false).unwrap();
    assert_eq!(branded["title"],"Example NBC","{branded:?}");
    assert_eq!(branded["date"],"2022-06-17");
    assert!(branded.get("network").is_none());
    let network=parse_title("Example.Show.2024.01.02.NBC.HDTV.x264",false).unwrap();
    assert_eq!(network["title"],"Example Show","{network:?}");
    assert_eq!(network["network"],"NBC","{network:?}");
    assert_eq!(network["quality"],"HDTV");
    let dated_episode=parse_title(
        "www.example.org - Example Show S02E03 Tuesday September 22 2026 1080i AMZN WEB-DL H264-GROUP",
        false).unwrap();
    assert_eq!(dated_episode["title"],"Example Show","{dated_episode:?}");
    assert_eq!(dated_episode["network"],"Amazon","{dated_episode:?}");
    assert_eq!(dated_episode["date"],"2026-09-22");
    assert_eq!(dated_episode["seasons"],json!([2]));
    assert_eq!(dated_episode["episodes"],json!([3]));
    for raw in [
        "[NBC] Example.Show.2024.01.02.HDTV",
        "(NBC) Example.Show.2024.01.02.HDTV",
        "【NBC】 Example.Show.2024.01.02.HDTV",
        "（NBC） Example.Show.2024.01.02.HDTV",
    ] {
        let parsed=parse_title(raw,false).unwrap();
        assert_eq!(parsed["network"],"NBC","{raw}: {parsed:?}");
        assert_eq!(parsed["date"],"2024-01-02","{raw}: {parsed:?}");
    }
}

#[test]
fn plural_bit_depth_tags_use_the_native_conversion_without_fake_episodes() {
    for (tag,expected) in [("8Bits","8bit"),("10Bits","10bit"),("12bits","12bit"),
        ("10.bit","10bit"),("10-bit","10bit"),("10bit","10bit")] {
        let raw=format!("Example.Show.S03.1080p.x265.{tag}.GROUP");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["bit_depth"],expected,"{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"],json!([3]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([]),"{raw}: {parsed:?}");
        assert_eq!(parsed["codec"],"hevc","{raw}: {parsed:?}");
    }
}

#[test]
fn joined_truehd_channels_preserve_audio_and_channel_values() {
    for (audio,channels) in [("TrueHD7.1","7.1"),("TrueHD.7.1","7.1"),
        ("TrueHD5.1","5.1"),("TrueHD2.0","2.0")] {
        let raw=format!("Example.Show.S01E02.2024.BluRay.1080p.{audio}.x265.10bit");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["audio"],json!(["TrueHD"]),"{raw}: {parsed:?}");
        assert_eq!(parsed["channels"],json!([channels]),"{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"],json!([2]),"{raw}: {parsed:?}");
    }
}

#[test]
fn a_dts_release_group_prefix_is_not_a_lossless_audio_marker() {
    let parsed=parse_title("Example.Movie.2013.1080p.BluRay.DTS-HDMasterTeam",false).unwrap();
    assert_eq!(parsed["audio"],json!(["DTS Lossy"]));
    assert_eq!(parsed["group"],"HDMasterTeam");
    for (tag,audio) in [("DTS-HD.MA.7.1-GROUP","DTS Lossless"),
        ("DTS-HD-Master.7.1-GROUP","DTS Lossless"),
        ("DTS-X.7.1-GROUP","DTS Lossless"),
        ("DTS-HD.HR.7.1-GROUP","DTS Lossy")] {
        let raw=format!("Example.Movie.2013.1080p.BluRay.{tag}");
        let parsed=parse_title(&raw,false).unwrap();
        assert_eq!(parsed["audio"],json!([audio]),"{raw}: {parsed:?}");
        assert_eq!(parsed["channels"],json!(["7.1"]),"{raw}: {parsed:?}");
    }
}
