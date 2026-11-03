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
    assert index.resolve_anime_episode("tt0213338", 1, 9) is None  # no crosswalk, no guess
    with index.connect() as db:
        db.execute("INSERT INTO anime_ids VALUES(?,?,?)",
                   ("tt0213338", "anidb", "24"))
    assert index.resolve_anime_episode("tt0213338", 1, 9) is None


def test_bidirectional_one_piece_style_numbering(tmp_path, monkeypatch):
    """Synthetic crosswalk: absolute 1067 and season 23 episode 2 refer to one episode."""
    from types import SimpleNamespace
    from MediaMatch import api
    index = MetadataIndex(tmp_path / "index.sqlite")
    with index.connect() as db:
        db.execute(
            "INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
            ("tt0388629", "One Piece", "onepiece", 1999, "series", "US", 1, 1, "tvSeries"),
        )
        db.execute("INSERT INTO anime_ids VALUES(?,?,?)",
                   ("tt0388629", "anidb", "69"))
        db.execute("INSERT INTO anime_series_map VALUES(?,?,?,?)",
                   ("tt0388629", "69", "81797", 0))
        # This is synthetic test data, not a claim about One Piece's real mapping.
        db.execute("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)",
                   ("69", "81797", 1, 23, 1066, 1080, -1065, ""))
        db.execute("INSERT INTO seasons VALUES(?,?)", ("tt0388629", 23))

    def fake_parse(title):
        import re
        season_match = re.search(r"S(\d+)E(\d+)", title, re.I)
        absolute_match = re.search(r"Episode\s+(\d+)", title, re.I)
        seasons = [int(season_match[1])] if season_match else []
        episodes = [int(season_match[2])] if season_match else (
            [int(absolute_match[1])] if absolute_match else []
        )
        return SimpleNamespace(
            parsed_title="One Piece", year=1999, seasons=seasons,
            episodes=episodes, model_dump=lambda **_: {"title": "One Piece"},
        )

    monkeypatch.setattr(api, "parse", fake_parse)
    absolute = api.match_title(index, "One Piece Episode 1067")
    seasonal = api.match_title(index, "One Piece S23E02")
    assert absolute is not None and seasonal is not None
    assert absolute["sid"] == seasonal["sid"] == "tt0388629:23:2"
    assert absolute["anime"]["asid"] == seasonal["anime"]["asid"] == "69:1067"


def test_split_season_reverse_mapping(tmp_path):
    index = MetadataIndex(tmp_path / "index.sqlite")
    with index.connect() as db:
        db.executemany("INSERT INTO anime_series_map VALUES(?,?,?,?)", [
            ("tt1234567", "100", "456", 1),
            ("tt1234567", "200", "456", 2),
        ])
        db.executemany("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)", [
            ("100", "456", 1, 1, 1, 12, 0, ""),
            ("200", "456", 1, 1, 13, 24, 0, ""),
        ])
    assert index.resolve_anime_episode("tt1234567", 2, 2) == ("200", 14)
    result = index.resolve_anime_absolute("tt1234567", 14)
    assert result is not None
    assert (result["season"], result["episode"]) == (2, 2)


def test_self_hosted_stremthru_source_import(tmp_path):
    from MediaMatch.stremthru_sources import import_animeapi, import_animelists
    index = MetadataIndex(tmp_path / "index.sqlite")
    tsv = (
        "imdb\\tanidb\\tthetvdb\\ttrakt_season\\ttrakt_may_invalid\\tmyanimelist\\n"
        "tt0388629\\t69\\t81797\\t23\\tfalse\\t21\\n"
    ).replace("\\t", "\t").replace("\\n", "\n").encode()
    xml = (b'<anime-list><anime anidbid="69" tvdbid="81797" '
           b'defaulttvdbseason="23" episodeoffset="-1065">'
           b'<mapping-list><mapping anidbseason="1" tvdbseason="23" '
           b'start="1066" end="1080" offset="-1065" /></mapping-list>'
           b'</anime></anime-list>')
    assert import_animeapi(index, tsv) == 1
    assert import_animelists(index, xml) >= 1
    assert index.resolve_anime_episode("tt0388629", 23, 2) == ("69", 1067)
    assert index.resolve_anime_absolute("tt0388629", 1067)["episode"] == 2


def test_metadata_freshness_requires_complete_refresh(tmp_path):
    from datetime import UTC, datetime, timedelta
    from MediaMatch.service import metadata_due
    path = tmp_path / "metadata.sqlite3"
    assert metadata_due(path)
    index = MetadataIndex(path)
    assert metadata_due(path)
    now = datetime.now(UTC)
    with index.connect() as db:
        db.execute("INSERT INTO metadata_state VALUES(?,?,?)",
                   ("full_refresh", now.isoformat(), 10))
    assert not metadata_due(path, now=now)
    assert metadata_due(path, now=now + timedelta(days=7))
    assert not metadata_due(path, now=now + timedelta(days=6, hours=23))


def test_stremthru_override_merging_and_explicit_map():
    from MediaMatch.animelists import merged_items, prepare_episode_maps
    original = b'''<anime-list><anime anidbid="230" tvdbid="76703"
        defaulttvdbseason="a" episodeoffset="0">
        <mapping-list><mapping anidbseason="1" tvdbseason="1"
        start="1" end="12" offset="0" /></mapping-list>
        </anime></anime-list>'''
    override = b'''<anime-list><anime anidbid="230" tvdbid="76703"
        episodeoffset="5"><mapping-list><mapping anidbseason="1"
        tvdbseason="1" start="1" end="12" offset="2" /></mapping-list>
        </anime></anime-list>'''
    items = merged_items(original, override)
    maps = prepare_episode_maps(items)
    assert any(m[3] == -1 and m[6] == 5 for m in maps)
    assert any(m[3] == 1 and m[6] == 2 for m in maps)


def test_global_absolute_episodeoffset_for_multiple_anidb_parts(tmp_path):
    index = MetadataIndex(tmp_path / "metadata.sqlite3")
    with index.connect() as db:
        db.executemany("INSERT INTO anime_series_map VALUES(?,?,?,?)", [
            ("tt1234567", "6662", "114801", 0),
            ("tt1234567", "9980", "114801", 0),
        ])
        db.executemany("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)", [
            ("6662", "114801", 1, -1, 1, 175, 0, ""),
            ("6662", "114801", 1, 4, 151, 175, -150, ""),
            ("9980", "114801", 1, -1, 1, 102, 175, ""),
            ("9980", "114801", 1, 5, 1, 51, 0, ""),
        ])
    absolute = index.resolve_anime_absolute("tt1234567", 177)
    assert absolute is not None
    assert (absolute["season"], absolute["episode"]) == (5, 2)
    assert (absolute["anidb_id"], absolute["anidb_episode"]) == ("9980", 2)


def test_anidb_title_alias_import_and_index(tmp_path):
    import gzip
    from MediaMatch.anidb_titles import import_anidb_titles
    index = MetadataIndex(tmp_path / "metadata.sqlite3")
    titles = "".join(
        f'<anime aid="{i}"><title type="main" xml:lang="x-jat">'
        f'Sousou no Frieren {i}</title></anime>' for i in range(101)
    )
    assert import_anidb_titles(index, gzip.compress(
        ("<animetitles>" + titles + "</animetitles>").encode(),
    )) >= 100
    with index.connect() as db:
        row = db.execute(
            "SELECT anidb_id FROM anidb_titles WHERE clean_title=?",
            ("sousounofrieren1",),
        ).fetchone()
    assert row[0] == "1"


def test_same_title_anime_beats_live_action_when_episode_crosswalk_exists(tmp_path, monkeypatch):
    from types import SimpleNamespace
    from MediaMatch import api
    index = MetadataIndex(tmp_path / "metadata.sqlite3")
    with index.connect() as db:
        db.executemany("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)", [
            ("tt0388629", "One Piece", "onepiece", 1999, "series", "US", 1, 1, "tvSeries"),
            ("tt11737520", "One Piece", "onepiece", 2023, "series", "US", 1, 1, "tvSeries"),
        ])
        db.executemany("INSERT INTO seasons VALUES(?,?)", [
            ("tt0388629", 23), ("tt11737520", 1),
        ])
        db.execute("INSERT INTO anime_ids VALUES(?,?,?)", ("tt0388629", "anidb", "69"))
        db.execute("INSERT INTO anime_series_map VALUES(?,?,?,?)",
                   ("tt0388629", "69", "81797", 0))
        db.execute("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)",
                   ("69", "81797", 1, 1, 1, 24, 0, ""))
    monkeypatch.setattr(api, "parse", lambda _: SimpleNamespace(
        parsed_title="One Piece", year=None, seasons=[1], episodes=[2],
        model_dump=lambda **__: {},
    ))
    result = api.match_title(index, "One Piece S01E02")
    assert result is not None
    assert result["imdb_id"] == "tt0388629"
    assert result["anime"]["asid"] == "69:2"


def test_anime_lists_explicit_one_to_many_map():
    from MediaMatch.anime_resolver import _explicit_pairs
    assert _explicit_pairs(";1-5+6;2-7;") == [(1, 5), (1, 6), (2, 7)]


def test_imdb_import_keeps_only_supported_types_and_related_aliases(tmp_path):
    basics = tmp_path / "types.basics.tsv.gz"
    akas = tmp_path / "types.akas.tsv.gz"
    episodes = tmp_path / "types.episodes.tsv.gz"
    base_headers = ["tconst", "titleType", "primaryTitle", "originalTitle",
                    "isAdult", "startYear", "endYear", "runtimeMinutes", "genres"]
    _tsv(basics, base_headers, [
        dict(tconst="tt0000001", titleType="movie", primaryTitle="Kept Film",
             originalTitle="Kept Film", startYear="2020"),
        dict(tconst="tt0000002", titleType="tvSeries", primaryTitle="Kept Show",
             originalTitle="Kept Show", startYear="\\N"),
        dict(tconst="tt0000003", titleType="tvEpisode", primaryTitle="Excluded Episode",
             originalTitle="Excluded Episode", startYear="2020"),
        dict(tconst="tt0000004", titleType="videoGame", primaryTitle="Excluded Game",
             originalTitle="Excluded Game", startYear="2020"),
        dict(tconst="tt0000005", titleType="tvPilot", primaryTitle="Excluded Pilot",
             originalTitle="Excluded Pilot", startYear="2020"),
    ])
    _tsv(akas, ["titleId", "ordering", "title", "region", "language",
                "types", "attributes", "isOriginalTitle"], [
        dict(titleId=f"tt{i:07d}", ordering="1", title=f"Alias {i}",
             region="US", isOriginalTitle="0") for i in range(1, 6)
    ])
    _tsv(episodes, ["tconst", "parentTconst", "seasonNumber", "episodeNumber"], [
        dict(tconst="tt0000006", parentTconst="tt0000002",
             seasonNumber="4", episodeNumber="1"),
        dict(tconst="tt0000007", parentTconst="tt0000003",
             seasonNumber="10", episodeNumber="1"),
    ])
    index = MetadataIndex(tmp_path / "types.sqlite3")
    import_imdb(index, basics, akas, episodes)
    with index.connect() as db:
        assert {r[0] for r in db.execute("SELECT DISTINCT imdb_id FROM titles")} == {
            "tt0000001", "tt0000002"
        }
        assert {r[0] for r in db.execute("SELECT DISTINCT imdb_id FROM seasons")} == {
            "tt0000002"
        }
        assert db.execute("SELECT year FROM titles WHERE imdb_id='tt0000002' LIMIT 1").fetchone()[0] == 0
        assert db.execute("SELECT COUNT(*) FROM titles WHERE type NOT IN "
                          "('movie','tvMovie','short','tvShort','tvSpecial',"
                          "'tvSeries','tvMiniSeries')").fetchone()[0] == 0
