import csv
import gzip

from MediaMatch import MetadataIndex
from MediaMatch.api import match_title
from MediaMatch.importers import import_anime_mappings, import_imdb


def _tsv(path, headers, rows):
    with gzip.open(path, "wt", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=headers, delimiter="\t")
        writer.writeheader()
        writer.writerows(rows)


def test_imdb_import_and_v4_duplicate_year_ranking(tmp_path):
    basics = tmp_path / "basics.tsv.gz"
    akas = tmp_path / "akas.tsv.gz"
    eps = tmp_path / "episodes.tsv.gz"
    _tsv(basics, ["tconst", "titleType", "primaryTitle", "originalTitle", "isAdult", "startYear", "endYear", "runtimeMinutes", "genres"], [
        dict(tconst="tt0000001", titleType="movie", primaryTitle="Dune", originalTitle="Dune", isAdult="0", startYear="1984"),
        dict(tconst="tt0000002", titleType="movie", primaryTitle="Dune", originalTitle="Dune", isAdult="0", startYear="2021"),
        dict(tconst="tt0000003", titleType="tvSeries", primaryTitle="Dune", originalTitle="Dune", isAdult="0", startYear="2021"),
    ])
    _tsv(akas, ["titleId", "ordering", "title", "region", "language", "types", "attributes", "isOriginalTitle"], [
        dict(titleId="tt0000001", ordering="1", title="Dune", region="GB", language="en", types="original", attributes="", isOriginalTitle="1"),
        dict(titleId="tt0000002", ordering="1", title="Dune", region="US", language="en", types="", attributes="", isOriginalTitle="0"),
        dict(titleId="tt0000003", ordering="1", title="Dune", region="US", language="en", types="", attributes="", isOriginalTitle="0"),
    ])
    _tsv(eps, ["tconst", "parentTconst", "seasonNumber", "episodeNumber"], [
        dict(tconst="tt1000001", parentTconst="tt0000003", seasonNumber="3", episodeNumber="1")
    ])
    index = MetadataIndex(tmp_path / "index.sqlite")
    assert import_imdb(index, basics, akas, eps) > 0
    assert [m.imdb_id for m in index.search("Dune", 2021)] == ["tt0000002", "tt0000001"]
    assert index.search("Dune", series=True)[0].imdb_id == "tt0000003"
    assert index.search("Dune", series=True, required_max_season=5) == []


def test_anime_ids_and_api_use_existing_parser(tmp_path):
    file = tmp_path / "anime.tsv"
    file.write_text("imdb\tanidb\tmal\ttvdb\nanime\t123\t456\t789\n", encoding="utf-8")
    index = MetadataIndex(tmp_path / "index.sqlite")
    import_anime_mappings(index, file)
    assert index.anime_lookup("anidb", "123")["mal"] == "456"
    assert match_title(index, "Cowboy Bebop 1998") is None


def test_single_result_and_ambiguity(tmp_path):
    index = MetadataIndex(tmp_path / "index.sqlite")
    with index.connect() as db:
        db.executemany(
            "INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
            [
                ("tt0000001", "Dune", "dune", 1984, "movie", "US", 1, 1, "movie"),
                ("tt0000002", "Dune", "dune", 2021, "movie", "US", 1, 1, "movie"),
            ],
        )
    assert index.search("Dune", 2021)[0].imdb_id == "tt0000002"
    assert index.search("Dune", 1984)[0].imdb_id == "tt0000001"


def test_anime_url_sources_and_preserve_episode_maps(tmp_path):
    import json
    index = MetadataIndex(tmp_path / "index.sqlite")
    with index.connect() as db:
        db.execute("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)",
                   ("123", "789", 1, 1, 1, 12, 0, ""))
    file = tmp_path / "anime.json"
    file.write_text(json.dumps({"data": [{
        "sources": [
            "https://anidb.net/anime/123",
            "https://myanimelist.net/anime/456/Cowboy_Bebop",
            "https://www.imdb.com/title/tt0213338/",
        ],
    }]}), encoding="utf-8")
    assert import_anime_mappings(index, file) >= 3
    assert index.anime_lookup("anidb", "123")["imdb"] == "tt0213338"
    assert index.map_anime_episode("123", 1, 2) == [("789", 1, 2)]


def test_anime_episode_crosswalk_uses_sql_offset_priority(tmp_path):
    index = MetadataIndex(tmp_path / "index.sqlite")
    with index.connect() as db:
        db.execute(
            "INSERT INTO anime_ids VALUES(?,?,?)", ("tt2250192", "anidb", "15146"),
        )
        db.executemany(
            "INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)",
            [
                ("15146", "259640", 1, 4, 1, 20, 0, ""),
                ("15146", "259640", 1, 4, 1, 12, 12, ""),
            ],
        )
    assert index.resolve_anime_episode("tt2250192", 4, 15) == ("15146", 3)
    assert index.resolve_anime_episode("tt2250192", 8, 15) is None


def test_anime_episode_without_crosswalk_requires_unique_anidb(tmp_path):
    index = MetadataIndex(tmp_path / "index.sqlite")
    with index.connect() as db:
        db.execute("INSERT INTO anime_ids VALUES(?,?,?)",
                   ("tt0213338", "anidb", "23"))
    assert index.resolve_anime_episode("tt0213338", 1, 9) == ("23", 9)
    with index.connect() as db:
        db.execute("INSERT INTO anime_ids VALUES(?,?,?)",
                   ("tt0213338", "anidb", "24"))
    assert index.resolve_anime_episode("tt0213338", 1, 9) is None
