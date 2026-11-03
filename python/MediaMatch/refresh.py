"""Refresh IMDb and anime metadata on the local machine, then atomically swap DB."""

from __future__ import annotations

import os
import tempfile
from datetime import UTC, datetime
from pathlib import Path

from .importers import update_imdb
from .anidb_titles import download_anidb_titles, import_anidb_titles
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
        # Rebuild from an empty staging database. Copying the previous
        # SQLite file would retain unused pages and stale imported aliases.
        # The old database remains untouched until the new import succeeds.
        index = MetadataIndex(staging)
        imdb_count = update_imdb(index)
        anime_links, anime_episodes = update_stremthru_anime(index)
        alias_count = import_anidb_titles(index, download_anidb_titles())
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
        with index.connect() as db:
            db.execute("PRAGMA journal_mode=DELETE")
            db.execute("VACUUM")
        os.replace(staging, target)
        return imdb_count, anime_count
    finally:
        staging.unlink(missing_ok=True)
