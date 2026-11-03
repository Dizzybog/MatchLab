"""Dashboard diagnostics use production matching and expose both pools."""

from types import SimpleNamespace

from MediaMatch import MetadataIndex
from MediaMatch import api
from MediaMatch.dashboard import HTML, diagnose


def test_dashboard_contains_form_and_result_panels():
    assert 'id="form"' in HTML
    assert 'id="imdbcandidates"' in HTML
    assert 'id="animecandidate"' in HTML
    assert "/debug/match" in HTML
    assert "innerHTML=d." not in HTML


def test_diagnostics_no_match_without_imdb_metadata(tmp_path, monkeypatch):
    index = MetadataIndex(tmp_path / "meta.sqlite")
    def fake_parse(_title):
        return SimpleNamespace(
            parsed_title="Missing Movie", year=2020,
            seasons=[], episodes=[],
            model_dump=lambda **_: {"title": "Missing Movie", "year": 2020},
        )
    monkeypatch.setattr(api, "parse", fake_parse)
    data = diagnose(index, "Missing.Movie.2020")
    assert data["selected"] is None
    assert data["imdb_candidates"] == []
    assert data["anime_candidate"] is None
    assert data["interpreted"]["title"] == "Missing Movie"
    assert data["duration_ms"] >= 0


def test_diagnostics_show_imdb_candidates(tmp_path, monkeypatch):
    index = MetadataIndex(tmp_path / "meta.sqlite")
    with index.connect() as db:
        db.execute("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
                   ("tt1234567", "Dune", "dune", 2021, "movie", "US", 1, 1, "movie"))
    def fake_parse(_title):
        return SimpleNamespace(
            parsed_title="Dune", year=2021,
            seasons=[], episodes=[],
            model_dump=lambda **_: {"title": "Dune", "year": 2021},
        )
    monkeypatch.setattr(api, "parse", fake_parse)
    data = diagnose(index, "Dune.2021")
    assert data["imdb_candidates"][0]["imdb_id"] == "tt1234567"
    assert data["selected"]["imdb_id"] == "tt1234567"
