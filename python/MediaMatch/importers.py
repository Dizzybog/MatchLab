"""Streaming importers for IMDb's public TSV exports and anime mapping files."""

from __future__ import annotations

import csv
import gzip
import io
import json
import urllib.request
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import urlparse

from .index import MetadataIndex, clean_title

IMDB_BASE = "https://datasets.imdbws.com/"
_TYPES = {"movie", "tvMovie", "short", "tvShort", "tvSpecial", "tvSeries", "tvMiniSeries"}


def _open_tsv(source: str | Path):
    if str(source).startswith(("https://", "http://")):
        response = urllib.request.urlopen(str(source), timeout=120)
        return io.TextIOWrapper(gzip.GzipFile(fileobj=response), encoding="utf-8", newline="")
    path = Path(source)
    binary = gzip.open(path, "rb") if path.suffix == ".gz" else path.open("rb")
    return io.TextIOWrapper(binary, encoding="utf-8", newline="")


def import_imdb(index: MetadataIndex, basics: str | Path, akas: str | Path,
                episodes: str | Path | None = None) -> int:
    """Import title.basics, title.akas and optional title.episode in one transaction."""
    db = index.connect()
    count = 0
    try:
        db.execute("BEGIN IMMEDIATE")
        db.execute("DELETE FROM titles")
        db.execute("DELETE FROM seasons")
        db.execute("CREATE TEMP TABLE imdb_akas (tid TEXT, ordering INTEGER, title TEXT, clean_title TEXT, region TEXT, original INTEGER)")
        for row in csv.DictReader(_open_tsv(basics), delimiter="\t"):
            if row["titleType"] not in _TYPES or row["startYear"] == "\\N":
                continue
            imdb_id, title_type, title, year = row["tconst"], row["titleType"], row["primaryTitle"], int(row["startYear"])
            category = "series" if title_type in {"tvSeries", "tvMiniSeries"} else "movie"
            db.execute("INSERT OR IGNORE INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
                       (imdb_id, title, clean_title(title), year, category, None, 0, 0, title_type))
            original_title = row["originalTitle"]
            if original_title != "\\N" and clean_title(original_title) != clean_title(title):
                db.execute("INSERT OR IGNORE INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
                           (imdb_id, original_title, clean_title(original_title), year,
                            category, None, 1, 0, title_type))
            count += 1
        db.executemany("INSERT INTO imdb_akas VALUES(?,?,?,?,?,?)", (
            (row["titleId"], int(row["ordering"]), row["title"], clean_title(row["title"]),
             None if row["region"] == "\\N" else row["region"],
             int(row["isOriginalTitle"] == "1"))
            for row in csv.DictReader(_open_tsv(akas), delimiter="\t")
        ))
        alias_rows = db.execute("""INSERT OR IGNORE INTO titles
            SELECT b.imdb_id,a.title,a.clean_title,b.year,b.category,a.region,a.original,a.ordering,b.type
            FROM imdb_akas a JOIN (SELECT imdb_id,year,category,type FROM titles GROUP BY imdb_id) b
            ON b.imdb_id=a.tid""").rowcount
        count += max(alias_rows, 0)
        if episodes:
            seasons: dict[str, int] = {}
            for row in csv.DictReader(_open_tsv(episodes), delimiter="\t"):
                if row["seasonNumber"] != "\\N":
                    seasons[row["parentTconst"]] = max(seasons.get(row["parentTconst"], 0), int(row["seasonNumber"]))
            db.executemany("INSERT OR REPLACE INTO seasons VALUES(?,?)", seasons.items())
        db.execute("INSERT OR REPLACE INTO metadata_state VALUES(?,?,?)",
                   ("imdb", datetime.now(UTC).isoformat(), count))
        db.commit()
    except BaseException:
        db.rollback()
        raise
    finally:
        db.close()
    return count


def update_imdb(index: MetadataIndex) -> int:
    return import_imdb(index, *(IMDB_BASE + f"{name}.tsv.gz" for name in
                                ("title.basics", "title.akas", "title.episode")))


def import_anime_mappings(index: MetadataIndex, path: str | Path) -> int:
    """Import schema-shaped TSV or manami-project anime-offline-database JSON."""
    path = Path(path)
    if path.suffix.lower() == ".json":
        payload = json.loads(path.read_text(encoding="utf-8"))
        rows = []
        source_names = {"anidb": "ANIDB", "anilist": "ANILIST", "animeplanet": "ANIME-PLANET",
                        "anisearch": "ANISEARCH", "imdb": "IMDB", "kitsu": "KITSU",
                        "mal": "MAL", "livechart": "LIVECHART", "tmdb": "TMDB",
                        "tvdb": "TVDB", "trakt": "TRAKT"}
        for entry in payload.get("data", []):
            row = {k: "" for k in source_names}
            for item in entry.get("sources", []):
                # anime-offline-database uses URL strings, not {source, id} objects.
                if isinstance(item, str):
                    parsed = urlparse(item)
                    host = (parsed.hostname or "").lower()
                    ident = parsed.path.rstrip("/").split("/")[-1]
                    if host in {"anidb.net", "www.anidb.net"}:
                        row["anidb"] = ident.removeprefix("anime/")
                    elif host in {"myanimelist.net", "www.myanimelist.net"}:
                        row["mal"] = ident if ident.isdigit() else parsed.path.strip("/").split("/")[1] if len(parsed.path.strip("/").split("/")) > 1 else ""
                    elif host in {"anilist.co", "www.anilist.co"}:
                        row["anilist"] = parsed.path.strip("/").split("/")[1] if len(parsed.path.strip("/").split("/")) > 1 else ""
                    elif host in {"kitsu.app", "kitsu.io"}:
                        row["kitsu"] = ident
                    elif host in {"imdb.com", "www.imdb.com"}:
                        row["imdb"] = next((part for part in parsed.path.split("/") if part.startswith("tt") and part[2:].isdigit()), "")
                    elif host in {"thetvdb.com", "www.thetvdb.com"}:
                        row["tvdb"] = ident
                elif isinstance(item, dict):
                    for key, source in source_names.items():
                        if str(item.get("source", "")).upper() == source:
                            row[key] = str(item.get("id", ""))
            rows.append(row)
    else:
        rows = csv.DictReader(path.open(encoding="utf-8-sig", newline=""), delimiter="\t")
    mappings = []
    episode_maps = []
    known = {"imdb", "anidb", "anilist", "animeplanet", "anisearch", "kitsu", "mal",
             "livechart", "notifymoe", "tmdb", "tvdb", "lboxd", "trakt"}
    db = index.connect()
    try:
        db.execute("BEGIN IMMEDIATE")
        # Preserve manually imported crosswalks and episode mappings on source refresh.
        # Automatic imports only add known IDs; they must not erase other sources.
        for row in rows:
            imdb_id = row.get("imdb", "")
            key = imdb_id or row.get("anidb", "")
            if not key:
                continue
            for source in known:
                value = (row.get(source) or "").strip()
                if value and value != "\\N":
                    mappings.append((key, source, value))
            if row.get("anidb_season") and row.get("tvdb_season"):
                episode_maps.append((row["anidb"], row["tvdb"], int(row["anidb_season"]),
                    int(row["tvdb_season"]), int(row.get("start", 1)), int(row.get("end", 9999)),
                    int(row.get("offset", 0)), row.get("map", "")))
        db.executemany("INSERT OR IGNORE INTO anime_ids VALUES(?,?,?)", mappings)
        db.executemany("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)", episode_maps)
        db.execute("INSERT OR REPLACE INTO metadata_state VALUES(?,?,?)",
                   ("anime", datetime.now(UTC).isoformat(), len(mappings)))
        db.commit()
    except BaseException:
        db.rollback()
        raise
    finally:
        db.close()
    return len(mappings)


def update_anime(index: MetadataIndex) -> int:
    """Refresh ID crosswalks from the maintained anime-offline-database export."""
    url = "https://raw.githubusercontent.com/manami-project/anime-offline-database/master/anime-offline-database.json"
    with urllib.request.urlopen(url, timeout=120) as response:
        payload = response.read()
    import tempfile
    with tempfile.NamedTemporaryFile(suffix=".json") as temporary:
        temporary.write(payload)
        temporary.flush()
        return import_anime_mappings(index, temporary.name)


def import_sql_crosswalk(index: MetadataIndex, series_tsv: str | Path,
                         episodes_tsv: str | Path) -> tuple[int, int]:
    """Import the two SQL backup tables exported to TSV.

    Accepts anime_id_map columns imdb/anidb/tvdb/trakt_season and
    anidb_tvdb_episode_map columns anidb_id/tvdb_id/anidb_season/
    tvdb_season/start/end/offset/map. This replaces only crosswalk rows;
    automatic anime metadata refresh does not delete them.
    """
    with open(series_tsv, encoding="utf-8-sig", newline="") as stream:
        links = []
        for row in csv.DictReader(stream, delimiter="\t"):
            imdb = (row.get("imdb") or "").strip()
            anidb = (row.get("anidb") or "").strip()
            if not imdb.startswith("tt") or not anidb:
                continue
            links.append((imdb, anidb, (row.get("tvdb") or "").strip(),
                          int(row.get("trakt_season") or 0)))
    with open(episodes_tsv, encoding="utf-8-sig", newline="") as stream:
        maps = []
        for row in csv.DictReader(stream, delimiter="\t"):
            if not row.get("anidb_id") or not row.get("tvdb_id"):
                continue
            maps.append((
                row["anidb_id"].strip(), row["tvdb_id"].strip(),
                int(row["anidb_season"]), int(row["tvdb_season"]),
                int(row["start"]), int(row["end"]), int(row["offset"]),
                row.get("map") or "",
            ))
    with index.connect() as db:
        db.execute("BEGIN IMMEDIATE")
        db.execute("DELETE FROM anime_series_map")
        db.execute("DELETE FROM episode_map")
        db.executemany("INSERT OR IGNORE INTO anime_series_map VALUES(?,?,?,?)", links)
        db.executemany("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)", maps)
        db.executemany(
            "INSERT OR IGNORE INTO anime_ids VALUES(?,?,?)",
            ((imdb, "anidb", anidb) for imdb, anidb, _, _ in links),
        )
        db.commit()
    return len(links), len(maps)
