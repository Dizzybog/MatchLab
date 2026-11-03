"""Anime-only path must not consult IMDb title metadata."""

from types import SimpleNamespace

from MediaMatch import MetadataIndex
from MediaMatch import api
from MediaMatch.anime_only import resolve_anime_title


def _fixture(index):
    with index.connect() as db:
        db.executemany("INSERT INTO anime_series_map VALUES(?,?,?,?)", [
            ("tt7777777", "100", "tv1", 1),
            ("tt7777777", "200", "tv1", 2),
            ("tt7777777", "300", "tv1", 3),
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


def test_anime_only_identity_without_imdb_titles(tmp_path):
    index = MetadataIndex(tmp_path / "index.sqlite")
    _fixture(index)
    assert resolve_anime_title(index, "Example Anime Arc Two", 1, 4) == {
        "imdb_id": "tt7777777", "anidb_id": "200", "anidb_episode": 4,
        "season": 2, "episode": 4,
    }
    assert resolve_anime_title(index, "Example Anime Arc Two", 2, 4) == {
        "imdb_id": "tt7777777", "anidb_id": "300", "anidb_episode": 4,
        "season": 3, "episode": 4,
    }
    assert resolve_anime_title(index, "Example Anime Arc Two", 3, 4) is None


def test_api_skips_imdb_search_for_anime(tmp_path, monkeypatch):
    index = MetadataIndex(tmp_path / "index.sqlite")
    _fixture(index)

    def fake_parse(_title):
        return SimpleNamespace(
            parsed_title="Example Anime Arc Two", year=None,
            seasons=[1], episodes=[4],
            model_dump=lambda **_: {"title": "Example Anime Arc Two"},
        )
    monkeypatch.setattr(api, "parse", fake_parse)

    def imdb_search_forbidden(*args, **kwargs):
        raise AssertionError("IMDb titles table must not be searched for anime")
    monkeypatch.setattr(index, "search", imdb_search_forbidden)
    result = api.match_title(index, "Example.Anime.Arc.Two.S01E04")
    assert result["match_source"] == "anime_only"
    assert result["sid"] == "tt7777777:2:4"
    assert result["anime"]["asid"] == "200:4"


def test_ambiguous_anime_identity_rejected(tmp_path):
    index = MetadataIndex(tmp_path / "index.sqlite")
    _fixture(index)
    with index.connect() as db:
        db.execute("INSERT INTO anime_series_map VALUES(?,?,?,?)",
                   ("tt8888888", "200", "tv1", 0))
    assert resolve_anime_title(index, "Example Anime Arc Two", 1, 4) is None
