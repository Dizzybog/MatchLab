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
fn invalid_zero_season_and_technical_dimensions_cannot_be_tv_coordinates(){
    let p=parse_title("Example.Show.S00E18.WEB-DL",false).unwrap();
    assert_ne!(p["seasons"],json!([0]),"{p:?}");
    let technical=parse_title("Example.Show.1920x1080.WEB-DL",false).unwrap();
    assert_eq!(technical["seasons"],json!([]),"{technical:?}");
    assert_eq!(technical["episodes"],json!([]),"{technical:?}");
}
