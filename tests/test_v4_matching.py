"""Regression cases derived from IMDb V4's prefix/suffix and ranking stages."""

from MediaMatch import MetadataIndex


def _put(index, rows, seasons=()):
    with index.connect() as db:
        db.executemany("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)", rows)
        db.executemany("INSERT INTO seasons VALUES(?,?)", seasons)


def test_v4_forward_prefix_longer_imdb_title(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite")
    _put(index, [("tt1234567", "Dune Part Two", "duneparttwo", 2024,
                  "movie", "US", 1, 1, "movie")])
    results = index.search("Dune", 2024, release_title="Dune.Part.Two.2024.1080p")
    assert results[0].imdb_id == "tt1234567"


def test_v4_shorter_prefix_title_in_release(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite")
    _put(index, [("tt1234567", "Mission Impossible", "missionimpossible", 1996,
                  "movie", "US", 1, 1, "movie")])
    assert index.search("Mission Impossible Fallout", 1996,
                        release_title="Mission.Impossible.Fallout.1996")[0].imdb_id == "tt1234567"


def test_v4_reverse_suffix_with_full_release_containment(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite")
    _put(index, [("tt1234567", "Lord Of The Rings The Hobbit Unexpected",
                  "lordoftheringsthehobbitunexpected", 2012, "series", "US", 1, 1, "tvSeries")],
         [("tt1234567", 3)])
    results = index.search("The Hobbit Unexpected", 2012, series=True, required_max_season=1,
                           release_title="Lord.Of.The.Rings.The.Hobbit.Unexpected.S01E01")
    assert results[0].imdb_id == "tt1234567"
    assert not index.search("The Hobbit Unexpected", 2012, series=True,
                            release_title="The.Hobbit.Unexpected.S01E01")


def test_v4_original_title_tiebreak(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite")
    _put(index, [
        ("tt0000001", "Dune", "dune", 1984, "movie", "US", 0, 1, "movie"),
        ("tt0000002", "Dune", "dune", 1984, "movie", "FR", 1, 1, "movie"),
    ])
    assert index.search("Dune", 1984)[0].imdb_id == "tt0000002"


def test_v4_season_tolerance(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite")
    _put(index, [("tt1234567", "Example Series", "exampleseries", 2020,
                  "series", "US", 1, 1, "tvSeries")], [("tt1234567", 4)])
    assert index.search("Example Series", series=True, required_max_season=5)
    assert not index.search("Example Series", series=True, required_max_season=6)


def test_v4_suffix_index_exists(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite")
    with index.connect() as db:
        indexes = {row[1] for row in db.execute("PRAGMA index_list(titles)")}
    assert "title_reverse_suffix" in indexes
