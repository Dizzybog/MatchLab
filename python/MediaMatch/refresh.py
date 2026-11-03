"""Refresh IMDb and anime metadata on the local machine, then atomically swap DB."""

from __future__ import annotations

import logging
import os
import time
import tempfile
from datetime import UTC, datetime
from pathlib import Path

from .importers import update_imdb
from .anidb_titles import download_anidb_titles, import_anidb_titles
from .stremthru_sources import update_stremthru_anime
from .index import MetadataIndex

LOG = logging.getLogger(__name__)


def refresh_local(path: str | Path) -> tuple[int, int]:
    """Keep the current database available if a download or import fails.

    Requires enough free disk space for a second copy of the SQLite index.
    Call under an external flock to prevent concurrent refreshes.
    """
    started = time.monotonic()
    target = Path(path).expanduser().resolve()
    LOG.info("metadata_refresh_start target=%s", target)
    target.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=".media-match-", suffix=".sqlite3", dir=target.parent)
    os.close(fd)
    staging = Path(name)
    try:
        # Rebuild from an empty staging database. Copying the previous
        # SQLite file would retain unused pages and stale imported aliases.
        # The old database remains untouched until the new import succeeds.
        index = MetadataIndex(staging)
        LOG.info("metadata_stage source=imdb action=download_and_import")
        imdb_count = update_imdb(index)
        LOG.info("metadata_stage source=imdb status=complete rows=%d", imdb_count)
        LOG.info("metadata_stage source=animeapi_animelists action=download_and_import")
        anime_links, anime_episodes = update_stremthru_anime(index)
        LOG.info("metadata_stage source=animeapi_animelists status=complete links=%d episodes=%d", anime_links, anime_episodes)
        LOG.info("metadata_stage source=anidb_titles action=download_and_import")
        alias_count = import_anidb_titles(index, download_anidb_titles())
        LOG.info("metadata_stage source=anidb_titles status=complete aliases=%d", alias_count)
        anime_count = anime_links + anime_episodes + alias_count
        if imdb_count <= 0 or anime_links <= 0 or anime_episodes <= 0:
            raise ValueError("Metadata import incomplete; refusing to replace live database")
        with index.connect() as db:
            db.execute(
                "INSERT OR REPLACE INTO metadata_state(source,updated_at,row_count) "
                "VALUES(?,?,?)",
                ("full_refresh", datetime.now(UTC).isoformat(), imdb_count + anime_count),
            )
            db.commit()
        # All connections are closed before the atomic replacement.
        # A new staging DB is already compact. Avoid VACUUM here:
        # it would temporarily require another full database copy.
        os.replace(staging, target)
        LOG.info("metadata_refresh_success target=%s imdb=%d anime=%d elapsed_seconds=%.1f", target, imdb_count, anime_count, time.monotonic()-started)
        return imdb_count, anime_count
    except Exception:
        LOG.exception("metadata_refresh_failed target=%s elapsed_seconds=%.1f previous_db_retained=true", target, time.monotonic()-started)
        raise
    finally:
        staging.unlink(missing_ok=True)
