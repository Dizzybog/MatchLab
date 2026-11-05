//! Cases discovered in the November 2026 request-history sample.
//! Raw release names are intentionally kept here, not the full private history.
use matchlab_parser::parse_title;
use serde_json::{json, Value};

fn parsed(raw: &str) -> serde_json::Map<String, Value> {
    parse_title(raw, false).unwrap_or_else(|e| panic!("parsing {raw:?}: {e}"))
}

#[test]
fn spanish_uppercase_x_episode_and_chains() {
    for (raw, title, season, episodes) in [
        ("Suenos de libertad 1X124 HDTV XviD Castellano",
         "Suenos de libertad", 1, vec![124]),
        ("Chicago PD 9X21 HDTV XviD Castellano",
         "Chicago PD", 9, vec![21]),
        ("Navy Investigacion Criminal 22X17X18 HDTV XviD Castellano",
         "Navy Investigacion Criminal", 22, vec![17, 18]),
    ] {
        let p = parsed(raw);
        assert_eq!(p["title"], title, "{raw}");
        assert_eq!(p["seasons"], json!([season]), "{raw}");
        assert_eq!(p["episodes"], json!(episodes), "{raw}");
    }
}

#[test]
fn explicit_season_word_not_an_episode() {
    for (raw, title, season) in [
        ("Mr. Robot - Season - 04 [2019] 1080p AMZN WebRip x265 DDP 5.1 Kira [SEV]",
         "Mr. Robot", 4),
        ("Night.On.Earth.Season-1.2020.UHD.2160p.HDR.NF.WEBRip.DDP.Atmos.5.1.HEVC-DDR",
         "Night On Earth", 1),
        ("Airwolf Season 1[1984]", "Airwolf", 1),
    ] {
        let p = parsed(raw);
        assert_eq!(p["title"], title, "{raw}");
        assert_eq!(p["seasons"], json!([season]), "{raw}");
        assert_eq!(p["episodes"], json!([]), "{raw}");
    }
}

#[test]
fn explicit_sxe_not_lost_with_airdate_prefix() {
    let p = parsed("60Minutes.20181216.s51e12");
    assert_eq!(p["title"], "60Minutes");
    assert_eq!(p["seasons"], json!([51]));
    assert_eq!(p["episodes"], json!([12]));
}

#[test]
fn anime_named_final_season_keeps_episode() {
    let p = parsed("[Dough-Fansubs] Beastars Final Season - 05 [1080p NF WEB-DL AVC EAC3][English].mkv");
    assert_eq!(p["title"], "Beastars Final Season");
    assert_eq!(p["seasons"], json!([]));
    assert_eq!(p["episodes"], json!([5]));

    let p = parsed("[Erai-raws] Monogatari Series - Off and Monster Season - 02 [480p][Multiple Subtitle][15AFFBF9].mkv");
    assert_eq!(p["seasons"], json!([]));
    assert_eq!(p["episodes"], json!([2]));
    assert_eq!(p["title"], "Monogatari Series - Off and Monster Season");
}

#[test]
fn resolution_dimensions_are_not_season_episode() {
    let p = parsed("[GFearJ] Jie Mo Ren (WEB 1920x1080 x264 AAC)");
    assert_eq!(p["seasons"], json!([]));
    assert_eq!(p["episodes"], json!([]));
}
