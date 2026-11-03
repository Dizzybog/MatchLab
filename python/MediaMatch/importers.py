"""Streaming importers for IMDb's public TSV exports and anime mapping files."""

from __future__ import annotations

import csv
import gzip
import io
import json
import logging
import time
import urllib.request
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import urlparse

from .index import MetadataIndex, clean_title

LOG = logging.getLogger(__name__)
IMDB_BASE = "https://datasets.imdbws.com/"
_TYPES = {"movie", "tvMovie", "short", "tvShort", "tvSpecial", "tvSeries", "tvMiniSeries"}


def _open_tsv(source: str | Path):
    if str(source).startswith(("https://", "http://")):
        LOG.info("metadata_http_request method=GET url=%s", source)
        started = time.monotonic()
        response = urllib.request.urlopen(str(source), timeout=120)
        LOG.info("metadata_http_response url=%s status=%s content_length=%s elapsed_seconds=%.1f", source, response.status, response.headers.get("Content-Length", "unknown"), time.monotonic()-started)
        return io.TextIOWrapper(gzip.GzipFile(fileobj=response), encoding="utf-8", newline="")
    path = Path(source)
    binary = gzip.open(path, "rb") if path.suffix == ".gz" else path.open("rb")
    return io.TextIOWrapper(binary, encoding="utf-8", newline="")


def import_imdb(index: MetadataIndex, basics: str | Path, akas: str | Path,
                episodes: str | Path | None = None) -> int:
    """Import only categorized movies/series and their aliases in bounded batches.

    IMDb title.akas includes aliases for episodes, games and many other
    unsupported types. Never stage all of those rows on disk.
    """
    LOG.info("imdb_import_start basics=%s akas=%s episodes=%s", basics, akas, episodes)
    db = index.connect()
    count = 0
    try:
        db.execute("BEGIN IMMEDIATE")
        db.execute("DELETE FROM titles")
        db.execute("DELETE FROM seasons")
        # IMDb type is authoritative. Unknown year does not imply unknown type.
        with _open_tsv(basics) as stream:
            for row in csv.DictReader(stream, delimiter="\t"):
                title_type = row.get("titleType", "")
                if title_type not in _TYPES:
                    continue
                imdb_id = row["tconst"]
                title = row.get("primaryTitle", "")
                year_text = row.get("startYear", "")
                year = int(year_text) if year_text.isdigit() else 0
                category = "series" if title_type in {"tvSeries", "tvMiniSeries"} else "movie"
                normalized = clean_title(title)
                if not normalized:
                    continue
                db.execute("INSERT OR IGNORE INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
                           (imdb_id, title, normalized, year, category,
                            "", 0, 0, title_type))
                original = row.get("originalTitle", "")
                original_key = clean_title(original)
                if original_key and original_key != normalized:
                    db.execute("INSERT OR IGNORE INTO titles VALUES(?,?,?,?,?,?,?,?,?)",
                               (imdb_id, original, original_key, year, category,
                                "", 1, 0, title_type))
                count += 1

        # The temporary table is capped to a batch, not the full IMDb AKAs.
        db.execute("""CREATE TEMP TABLE imdb_akas (
            tid TEXT, ordering INTEGER, title TEXT, clean_title TEXT,
            region TEXT, original INTEGER
        )""")
        batch = []
        def flush_aliases() -> None:
            nonlocal count
            if not batch:
                return
            db.executemany("INSERT INTO imdb_akas VALUES(?,?,?,?,?,?)", batch)
            inserted = db.execute("""INSERT OR IGNORE INTO titles
                SELECT b.imdb_id,a.title,a.clean_title,b.year,b.category,
                       a.region,a.original,a.ordering,b.type
                FROM imdb_akas a
                JOIN titles b ON b.imdb_id=a.tid
                WHERE b.ordering=0""").rowcount
            count += max(inserted, 0)
            db.execute("DELETE FROM imdb_akas")
            batch.clear()

        LOG.info("imdb_import_progress stage=basics count=%d", count)
        with _open_tsv(akas) as stream:
            for row in csv.DictReader(stream, delimiter="\t"):
                title = row.get("title", "")
                key = clean_title(title)
                if not key:
                    continue
                ordering = row.get("ordering", "")
                batch.append((
                    row["titleId"], int(ordering) if ordering.isdigit() else 0,
                    title, key, "" if row.get("region") in (None, "\\N") else row["region"],
                    int(row.get("isOriginalTitle") == "1"),
                ))
                if len(batch) >= 20000:
                    flush_aliases()
        flush_aliases()
        LOG.info("imdb_import_progress stage=akas count=%d", count)
        db.execute("DROP TABLE imdb_akas")

        if episodes:
            # Only keep max-season metadata for categorized TV series.
            db.execute("""CREATE TEMP TABLE series_seasons (
                imdb_id TEXT PRIMARY KEY, max_season INTEGER NOT NULL
            )""")
            with _open_tsv(episodes) as stream:
                for row in csv.DictReader(stream, delimiter="\t"):
                    season = row.get("seasonNumber", "")
                    if not season.isdigit() or int(season) <= 0:
                        continue
                    db.execute("""INSERT INTO series_seasons VALUES(?,?)
                        ON CONFLICT(imdb_id) DO UPDATE SET
                        max_season=max(max_season,excluded.max_season)""",
                        (row["parentTconst"], int(season)))
            db.execute("""INSERT INTO seasons
                SELECT s.imdb_id,s.max_season FROM series_seasons s
                WHERE EXISTS(SELECT 1 FROM titles t
                             WHERE t.imdb_id=s.imdb_id
                               AND t.type IN ('tvSeries','tvMiniSeries'))""")
            db.execute("DROP TABLE series_seasons")
        db.execute("INSERT OR REPLACE INTO metadata_state VALUES(?,?,?)",
                   ("imdb", datetime.now(UTC).isoformat(), count))
        db.commit()
        LOG.info("imdb_import_success count=%d", count)
    except BaseException:
        LOG.exception("imdb_import_failed")
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
