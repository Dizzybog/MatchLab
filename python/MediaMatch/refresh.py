"""Refresh IMDb and anime metadata on the local machine, then atomically swap DB."""

from __future__ import annotations

import os
import sqlite3
import tempfile
from pathlib import Path

from .importers import update_anime, update_imdb
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
        anime_count = update_anime(index)
        # All connections are closed before the atomic replacement.
        os.replace(staging, target)
        return imdb_count, anime_count
    finally:
        staging.unlink(missing_ok=True)
