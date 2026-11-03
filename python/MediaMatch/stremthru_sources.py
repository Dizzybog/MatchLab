"""Download anime ID and episode maps from the same public sources as StremThru.

Sources: nattadasu/animeApi v3 (animeapi.tsv), Anime-Lists/anime-list.xml.
No PostgreSQL or manually exported mappings are required.
"""

from __future__ import annotations

import csv
import io
import logging
import time
import urllib.request
import xml.etree.ElementTree as ET
from datetime import UTC, datetime

from .index import MetadataIndex

LOG = logging.getLogger(__name__)
ANIMEAPI_URL = "https://raw.githubusercontent.com/nattadasu/animeApi/refs/heads/v3/database/animeapi.tsv"
ANIMELISTS_URL = "https://raw.githubusercontent.com/Anime-Lists/anime-lists/refs/heads/master/anime-list.xml"
STREMTHRU_OVERRIDE_URL = "https://raw.githubusercontent.com/MunifTanjim/stremthru/main/internal/animelists/anime-list.override.xml"


def _download(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "torrent-parse-match/0.1"})
    LOG.info("metadata_http_request method=GET url=%s", url)
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=180) as response:
        payload = response.read()
        LOG.info("metadata_http_response url=%s status=%s bytes=%d elapsed_seconds=%.1f", url, response.status, len(payload), time.monotonic()-started)
        return payload


def _integer(value: str | None, default: int = 0) -> int:
    try:
        return int(value or default)
    except (ValueError, TypeError):
        return default


def import_animeapi(index: MetadataIndex, payload: bytes) -> int:
    """Build IMDb/AniDB/TVDB/Trakt identities from StremThru's animeApi feed."""
    rows = csv.DictReader(io.StringIO(payload.decode("utf-8-sig")), delimiter="\t")
    required = {"imdb", "anidb", "thetvdb", "trakt_season"}
    if not required.issubset(rows.fieldnames or []):
        raise ValueError("animeApi TSV is missing required headers")
    links = set()
    identities = set()
    count = 0
    for row in rows:
        imdb = (row.get("imdb") or "").strip()
        anidb = (row.get("anidb") or "").strip()
        if not imdb.startswith("tt") or not anidb.isdigit() or int(anidb) <= 0:
            continue
        tvdb = (row.get("thetvdb") or "").strip()
        if not tvdb.isdigit() or int(tvdb) <= 0:
            tvdb = ""
        trakt = _integer(row.get("trakt_season"))
        if (row.get("trakt_may_invalid") or "").lower() == "true":
            trakt = 0
        links.add((imdb, anidb, tvdb, trakt))
        identities.add((imdb, "anidb", anidb))
        for key, source in (("thetvdb", "tvdb"), ("myanimelist", "mal"),
                            ("anilist", "anilist"), ("kitsu", "kitsu")):
            value = (row.get(key) or "").strip()
            if value and value not in ("0", "unknown"):
                identities.add((imdb, source, value))
        count += 1
    if not links:
        raise ValueError("animeApi returned no IMDb/AniDB links")
    with index.connect() as db:
        db.execute("BEGIN IMMEDIATE")
        db.execute("DELETE FROM anime_series_map")
        db.executemany("INSERT OR IGNORE INTO anime_series_map VALUES(?,?,?,?)", links)
        db.executemany("INSERT OR IGNORE INTO anime_ids VALUES(?,?,?)", identities)
        db.execute("INSERT OR REPLACE INTO metadata_state VALUES(?,?,?)",
                   ("animeapi", datetime.now(UTC).isoformat(), count))
        db.commit()
    LOG.info("animeapi_import_success links=%d identities=%d", len(links), len(identities))
    return len(links)


def import_animelists(index: MetadataIndex, payload: bytes,
                     override: bytes | None = None) -> int:
    """Read Anime-Lists mapping runs, retaining direct/absolute episode offsets.

    This reproduces the core StremThru anime-lists semantics, but not its
    embedded override file or every special-case 'map' expression.
    """
    from .animelists import merged_items, prepare_episode_maps

    items = merged_items(payload, override)
    mappings = prepare_episode_maps(items)
    ids = set()
    for item in items:
        imdb = (item.get("imdbid") or "").strip()
        if imdb.startswith("tt"):
            ids.add((imdb, item.get("anidbid", ""), item.get("tvdbid", ""), 0))
    if not mappings:
        raise ValueError("Anime-Lists returned no episode mappings")
    with index.connect() as db:
        db.execute("BEGIN IMMEDIATE")
        db.execute("DELETE FROM episode_map")
        db.executemany("INSERT INTO episode_map VALUES(?,?,?,?,?,?,?,?)", mappings)
        db.executemany("INSERT OR IGNORE INTO anime_series_map VALUES(?,?,?,?)", ids)
        db.executemany("INSERT OR IGNORE INTO anime_ids VALUES(?,?,?)",
                       ((imdb, "anidb", anidb) for imdb, anidb, _, _ in ids))
        db.execute("INSERT OR REPLACE INTO metadata_state VALUES(?,?,?)",
                   ("animelists", datetime.now(UTC).isoformat(), len(mappings)))
        db.commit()
    LOG.info("animelists_import_success mappings=%d identities=%d", len(mappings), len(ids))
    return len(mappings)


def update_stremthru_anime(index: MetadataIndex) -> tuple[int, int]:
    # Download both before modifying either table.
    api_payload = _download(ANIMEAPI_URL)
    xml_payload = _download(ANIMELISTS_URL)
    override_payload = _download(STREMTHRU_OVERRIDE_URL)
    return (import_animeapi(index, api_payload),
            import_animelists(index, xml_payload, override_payload))
