"""Refresh IMDb and anime metadata on the local machine, then atomically swap DB."""

from __future__ import annotations

import os
import sqlite3
import tempfile
from datetime import UTC, datetime
from pathlib import Path

from .importers import update_imdb
from .stremthru_sources import update_stremthru_anime
from .index import MetadataIndex


def refresh_local(path: str | Path) -> tuple[int, int]:
    """Keep the current database available if a download or import fails.

    Requires enough free disk space for a second copy of the SQLite index.
    Call under an external flock to prevent concurrent refreshes.
    """
    target = Path(path).expanduser().resolve()
    target.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=".media-match-", suffix=".sqlite3", dir=target.parent)
    os.close(fd)
    staging = Path(name)
    try:
        if target.exists():
            with sqlite3.connect(target) as source, sqlite3.connect(staging) as dest:
                source.backup(dest)
        index = MetadataIndex(staging)
        imdb_count = update_imdb(index)
        anime_links, anime_episodes = update_stremthru_anime(index)
        anime_count = anime_links + anime_episodes
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
        os.replace(staging, target)
        return imdb_count, anime_count
    finally:
        staging.unlink(missing_ok=True)
