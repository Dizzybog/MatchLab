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
        ("911 Nashville 1X5 HDTV XviD Castellano",
         "911 Nashville", 1, vec![5]),
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
    let p = parsed("[Erai-raws] Spy x Family Season 3 - 03 [720p CR WEB-DL AVC AAC][MultiSub][3CA53AF2].mkv");
    assert_eq!(p["seasons"], json!([3]));
    assert_eq!(p["episodes"], json!([3]));
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

#[test]
fn spanish_cap_three_digit_season_episode_shorthand() {
    // CAP.NNN is a release convention: 1 season digit + 2 episode digits.
    // Unlike the unmarked anime numbers, these are explicit TV coordinates.
    for (raw, season, episode) in [
        ("La Promesa Cap.103 HDTV XviD Castellano", 1, 3),
        ("La Promesa.CAP_203.720p.HDTV", 2, 3),
        ("La Promesa Cap 312 WEB-DL", 3, 12),
        ("La Promesa cap-405 WEBRip", 4, 5),
        ("La Promesa.CAP.999.1080p", 9, 99),
    ] {
        let parsed = parsed(raw);
        assert_eq!(parsed["title"], "La Promesa", "{raw}: {parsed:?}");
        assert_eq!(parsed["seasons"], json!([season]), "{raw}: {parsed:?}");
        assert_eq!(parsed["episodes"], json!([episode]), "{raw}: {parsed:?}");
    }
    let nested = parsed("La Promesa/Temporada 1/La Promesa Cap.103.mkv");
    assert_eq!(nested["seasons"], json!([1]));
    assert_eq!(nested["episodes"], json!([3]));
}

#[test]
fn spanish_cap_notation_does_not_override_stronger_episode_coordinates() {
    let explicit = parsed("La Promesa S02E05 Cap.103 720p HDTV");
    assert_eq!(explicit["seasons"], json!([2]));
    assert_eq!(explicit["episodes"], json!([5]));

    for raw in ["La Promesa Capitulo 103", "La Promesa Cap.1034",
                "La Promesa Cap.100", "La Promesa cap.103p"] {
        let parsed = parsed(raw);
        assert_ne!((parsed["seasons"].clone(), parsed["episodes"].clone()),
            (json!([1]), json!([3])),
            "must not fabricate S01E03 from {raw}: {parsed:?}");
    }
}
