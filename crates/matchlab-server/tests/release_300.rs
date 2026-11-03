//! 300 release-name regression inputs: 100 movies, 100 TV, 100 anime.
//! These are parser smoke tests, not IMDb/AniDB identity-parity assertions.
use ptt_core::parse_title;

fn variants(title: &str, year: u16, season: u8, episode: u16, series: bool) -> Vec<String> {
    let dot = title.replace(' ', ".");
    let dash = title.replace(' ', "-");
    let suffixes = [
        "1080p.WEB-DL.x264-GROUP",
        "2160p.BluRay.REMUX.HEVC",
        "720p.WEBRip.AAC",
        "1080p.NF.WEB-DL.DDP5.1",
        "2160p.UHD.HDR.DV",
        "480p.DVDRip.XviD",
        "1080p.BluRay.x265.10bit",
        "720p.HDTV.x264",
        "1080p.AMZN.WEB-DL.H264",
        "2160p.WEB.H265.Atmos",
    ];
    suffixes.iter().enumerate().map(|(i, suffix)| {
        let prefix = if i % 2 == 0 { &dot } else { &dash };
        if series {
            match i % 4 {
                0 => format!("{prefix}.S{season:02}E{episode:02}.{suffix}"),
                1 => format!("{prefix} S{season:02}E{episode:02} {suffix}"),
                2 => format!("{prefix}.S{season:02}.E{episode:02}.{suffix}"),
                _ => format!("{prefix} - S{season:02}E{episode:02} - {suffix}"),
            }
        } else {
            match i % 3 {
                0 => format!("{prefix}.{year}.{suffix}"),
                1 => format!("{prefix} ({year}) {suffix}"),
                _ => format!("{prefix}-{year}-{suffix}"),
            }
        }
    }).collect()
}

#[test]
fn parse_300_movie_tv_anime_variants() {
    let movies = [
        ("Dune",2021), ("The Matrix",1999), ("Inception",2010),
        ("Interstellar",2014), ("Oppenheimer",2023), ("The Batman",2022),
        ("Parasite",2019), ("Arrival",2016), ("Gladiator",2000),
        ("The Lord of the Rings The Fellowship of the Ring",2001),
    ];
    let tv = [
        ("Breaking Bad",1,1), ("Better Call Saul",2,3), ("The Office",3,7),
        ("Game of Thrones",4,2), ("Stranger Things",2,4),
        ("The Last of Us",1,6), ("Severance",2,1),
        ("The Mandalorian",3,5), ("Chernobyl",1,2), ("Dark",2,6),
    ];
    let anime = [
        ("One Piece",1,10), ("Naruto Shippuden",2,3),
        ("Attack on Titan",3,4), ("Demon Slayer",2,5),
        ("Jujutsu Kaisen",2,8), ("Frieren Beyond Journeys End",1,12),
        ("Fullmetal Alchemist Brotherhood",1,4), ("Bleach",3,6),
        ("My Hero Academia",4,7), ("Spy x Family",2,9),
    ];
    let mut count = [0usize;3];
    for (title,year) in movies {
        for raw in variants(title,year,1,1,false) {
            let result = parse_title(&raw,false)
                .unwrap_or_else(|e|panic!("movie parser failed on {raw:?}: {e}"));
            assert!(!result.get("title").and_then(|v|v.as_str()).unwrap_or("").is_empty(),
                "movie title empty: {raw}");
            count[0] += 1;
        }
    }
    for (group, titles) in [
        (1usize, tv.as_slice()),
        (2usize, anime.as_slice()),
    ] {
        for &(title,season,episode) in titles {
            for raw in variants(title,2024,season,episode,true) {
                let result = parse_title(&raw,false)
                    .unwrap_or_else(|e|panic!("series parser failed on {raw:?}: {e}"));
                assert!(!result.get("title").and_then(|v|v.as_str()).unwrap_or("").is_empty(),
                    "series title empty: {raw}");
                count[group] += 1;
            }
        }
    }
    assert_eq!(count,[100,100,100]);
    println!("Parser smoke-tested {} movies, {} TV episodes and {} anime episodes",
        count[0],count[1],count[2]);
}
