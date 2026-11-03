"""Run 100 real built-in parser + SQLite matcher calls against a deterministic fixture.

This is an offline integration simulation, not a benchmark against the live
IMDb corpus. Every case and its single-result output is printed to CI logs.
"""

from __future__ import annotations

import json

from MediaMatch import MetadataIndex
from MediaMatch.api import match_title

MOVIES = [
    ("Dune", 2021), ("Inception", 2010), ("Interstellar", 2014),
    ("The Matrix", 1999), ("Titanic", 1997), ("Avatar", 2009),
    ("Gladiator", 2000), ("Oppenheimer", 2023), ("Arrival", 2016),
    ("The Prestige", 2006),
]
SERIES = [
    ("Breaking Bad", 2008), ("Better Call Saul", 2015),
    ("Stranger Things", 2016), ("The Office", 2005), ("Lost", 2004),
    ("Dark", 2017), ("The Wire", 2002), ("Fargo", 2014),
    ("Severance", 2022), ("The Last of Us", 2023),
]
MOVIE_STYLES = [
    "{title}.{year}.1080p.BluRay.x264-GROUP",
    "{title}.{year}.2160p.WEB-DL.DDP5.1.H265",
    "{title} ({year}) 720p WEBRip",
    "{title}.{year}.REMUX.HEVC.HDR",
    "{title}.{year}.1080p.NF.WEB-DL",
]
SERIES_STYLES = [
    "{title}.S01E01.1080p.WEB-DL.x264-GROUP",
    "{title}.S02E03.2160p.WEB.H265",
    "{title}.S03E05.720p.HDTV.x264",
    "{title}.S01E08.1080p.BluRay.REMUX",
    "{title}.S02E10.1080p.WEBRip",
]


def test_one_hundred_torrent_titles(tmp_path):
    index = MetadataIndex(tmp_path / "hundred.sqlite3")
    cases = []
    with index.connect() as db:
        for n, (title, year) in enumerate(MOVIES + SERIES, 1):
            series = n > len(MOVIES)
            imdb_id = f"tt{n:07d}"
            from MediaMatch.index import clean_title
            db.execute("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
                       (imdb_id, title, clean_title(title), year,
                        "series" if series else "movie", "US", 1, 1,
                        "tvSeries" if series else "movie"))
            if series:
                db.execute("INSERT INTO seasons VALUES(?,?)", (imdb_id, 10))
            styles = SERIES_STYLES if series else MOVIE_STYLES
            for template in styles:
                raw = template.format(title=title.replace(" ", "."), year=year)
                cases.append((raw, imdb_id, series))
    assert len(cases) == 100
    correct = wrong = unmatched = 0
    for number, (raw, expected, is_series) in enumerate(cases, 1):
        try:
            result = match_title(index, raw)
            actual = result["imdb_id"] if result else None
            status = "MATCH" if actual == expected else ("NULL" if actual is None else "WRONG")
            if status == "MATCH":
                correct += 1
            elif status == "NULL":
                unmatched += 1
            else:
                wrong += 1
            print("SIM100 " + json.dumps({
                "n": number, "t_title": raw, "expected": expected,
                "actual": actual, "sid": result["sid"] if result else None,
                "status": status,
            }, ensure_ascii=False))
        except Exception as exc:
            wrong += 1
            print("SIM100 " + json.dumps({
                "n": number, "t_title": raw, "expected": expected,
                "status": "EXCEPTION", "error": repr(exc),
            }))
    print(f"SIM100 SUMMARY total=100 matched={correct} null={unmatched} wrong_or_exception={wrong}")
    assert wrong == 0, "Incorrect IDs or exceptions in 100-title simulation"
