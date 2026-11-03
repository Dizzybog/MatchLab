"""AniDB title aliases, using the public source consumed by StremThru."""

from __future__ import annotations

import gzip
import io
import logging
import time
import urllib.request
import xml.etree.ElementTree as ET
from datetime import UTC, datetime

from .index import MetadataIndex, clean_title

LOG = logging.getLogger(__name__)
ANIDB_TITLES_URL = "https://anidb.net/api/anime-titles.xml.gz"


def download_anidb_titles() -> bytes:
    request = urllib.request.Request(ANIDB_TITLES_URL, headers={
        "User-Agent": "Mozilla/5.0 (compatible; torrent-parse-match/0.1)",
    })
    LOG.info("metadata_http_request method=GET url=%s", ANIDB_TITLES_URL)
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=180) as response:
        payload = response.read()
        LOG.info("metadata_http_response url=%s status=%s bytes=%d elapsed_seconds=%.1f", ANIDB_TITLES_URL, response.status, len(payload), time.monotonic()-started)
        return payload


def import_anidb_titles(index: MetadataIndex, archive: bytes) -> int:
    """Import main, official and synonym names, including romaji variants."""
    count = 0
    with gzip.GzipFile(fileobj=io.BytesIO(archive)) as decompressed:
        with index.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute("DELETE FROM anidb_titles")
            batch = []
            for _event, element in ET.iterparse(decompressed, events=("end",)):
                if element.tag != "anime":
                    continue
                aid = element.get("aid", "")
                if not aid.isdigit():
                    element.clear()
                    continue
                for title in element.findall("title"):
                    kind = title.get("type", "")
                    if kind not in {"main", "official", "syn"}:
                        continue
                    value = (title.text or "").strip()
                    normalized = clean_title(value)
                    if not normalized:
                        continue
                    batch.append((aid, value, normalized, kind,
                                  title.get("{http://www.w3.org/XML/1998/namespace}lang",
                                            title.get("lang", ""))))
                    if len(batch) >= 1000:
                        db.executemany(
                            "INSERT OR IGNORE INTO anidb_titles VALUES(?,?,?,?,?)", batch,
                        )
                        count += len(batch)
                        batch.clear()
                element.clear()
            if batch:
                db.executemany("INSERT OR IGNORE INTO anidb_titles VALUES(?,?,?,?,?)", batch)
                count += len(batch)
            if count < 100:
                raise ValueError("AniDB titles import unexpectedly small")
            db.execute("INSERT OR REPLACE INTO metadata_state VALUES(?,?,?)",
                       ("anidb_titles", datetime.now(UTC).isoformat(), count))
            db.commit()
    LOG.info("anidb_titles_import_success aliases=%d", count)
    return count
