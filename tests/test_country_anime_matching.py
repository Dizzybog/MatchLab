"""Country-qualified IMDb aliases and title-relative anime part regression tests."""

from MediaMatch import MetadataIndex
from MediaMatch.anime_title_matcher import resolve_titled_anime
from MediaMatch.country_aliases import country_title_candidates


def test_country_alias_variants():
    candidates = country_title_candidates("The Office UK S01E01 1080p")
    assert "theofficeuk" in candidates
    assert "theofficeunitedkingdom" in candidates
    assert country_title_candidates("The Office S01E01 1080p") == ()


def test_country_edition_beats_generic_prefix(tmp_path):
    index = MetadataIndex(tmp_path / "meta.sqlite")
    with index.connect() as db:
        db.executemany("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)", [
            ("tt0000001", "The Office", "theoffice", 2005, "series", "US", 1, 1, "tvSeries"),
            ("tt0000002", "The Office United Kingdom", "theofficeunitedkingdom",
             2001, "series", "GB", 1, 1, "tvSeries"),
        ])
    matches = index.search("The Office UK", series=True,
                           release_title="The.Office.UK.S01E01.1080p")
    assert matches[0].imdb_id == "tt0000002"


def test_anime_specific_title_relative_season(tmp_path):
    index = MetadataIndex(tmp_path / "meta.sqlite")
    with index.connect() as db:
        db.executemany("INSERT INTO anime_series_map VALUES(?,?,?,?)", [
            ("tt9999999", "100", "tv1", 1),
            ("tt9999999", "200", "tv1", 2),
            ("tt9999999", "300", "tv1", 3),
        ])
        db.executemany("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)", [
            ("100", "tv1", 1, 1, 1, 12, 0, ""),
            ("200", "tv1", 1, 2, 1, 12, 0, ""),
            ("300", "tv1", 1, 3, 1, 12, 0, ""),
        ])
        db.executemany("INSERT INTO anidb_titles VALUES(?,?,?,?,?)", [
            ("100", "Example Anime", "exampleanime", "main", "en"),
            ("200", "Example Anime Arc Two", "exampleanimearctwo", "main", "en"),
            ("300", "Example Anime Arc Three", "exampleanimearcthree", "main", "en"),
        ])
    assert resolve_titled_anime(index, "tt9999999", "Example Anime", 3, 4) == dict(anidb_id="300", anidb_episode=4, season=3, episode=4)
    assert resolve_titled_anime(index, "tt9999999", "Example Anime Arc Two", 1, 4) == dict(anidb_id="200", anidb_episode=4, season=2, episode=4)
    assert resolve_titled_anime(index, "tt9999999", "Example Anime Arc Two", 2, 4) == ("300", 4)
    assert resolve_titled_anime(index, "tt9999999", "Example Anime Arc Two", 3, 4) is None


def test_anime_missing_episode_never_invents_mapping(tmp_path):
    index = MetadataIndex(tmp_path / "meta.sqlite")
    with index.connect() as db:
        db.execute("INSERT INTO anime_series_map VALUES(?,?,?,?)",
                   ("tt9999999", "100", "tv1", 1))
        db.execute("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)",
                   ("100", "tv1", 1, 1, 1, 12, 0, ""))
        db.execute("INSERT INTO anidb_titles VALUES(?,?,?,?,?)",
                   ("100", "Example Anime", "exampleanime", "main", "en"))
    assert resolve_titled_anime(index, "tt9999999", "Example Anime", 20, 5) is None
