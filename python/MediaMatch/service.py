"""Autonomous metadata refresh: startup check, then a check every 24 hours."""

from __future__ import annotations

import logging
import os
import sqlite3
import threading
import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

from .index import MetadataIndex
from .refresh import refresh_local

LOG = logging.getLogger(__name__)
WEEK = timedelta(days=7)
DAY_SECONDS = 24 * 60 * 60
RETRY_SECONDS = 60 * 60


def metadata_due(path: str | Path, now: datetime | None = None) -> bool:
    """A complete successful refresh timestamp, not a file mtime, is authoritative."""
    now = now or datetime.now(UTC)
    if not Path(path).is_file():
        return True
    try:
        with sqlite3.connect(path) as db:
            row = db.execute(
                "SELECT updated_at FROM metadata_state WHERE source='full_refresh'"
            ).fetchone()
        if not row:
            return True
        updated = datetime.fromisoformat(row[0])
        if updated.tzinfo is None:
            return True
        return updated > now or now - updated >= WEEK
    except (sqlite3.Error, ValueError, OSError):
        return True


def run_refresh_if_due(path: str | Path) -> bool:
    if not metadata_due(path):
        return False
    LOG.info("Metadata is missing or older than 7 days; starting local refresh")
    refresh_local(path)
    LOG.info("Local metadata refresh completed")
    return True


def run_service(path: str | Path, host: str, port: int) -> None:
    """Serve existing metadata while a due refresh runs in a background thread.

    On a new installation the API is available but matches are not served
    until the initial metadata build completes.
    """
    from .api import serve

    index = MetadataIndex(path)
    stop = threading.Event()

    def updater() -> None:
        while not stop.is_set():
            try:
                updated = run_refresh_if_due(path)
                wait = DAY_SECONDS
                if updated:
                    LOG.info("Next freshness check in 24 hours")
            except Exception:
                LOG.exception("Metadata refresh failed; retaining previous database")
                wait = RETRY_SECONDS
            stop.wait(wait)

    thread = threading.Thread(target=updater, name="metadata-updater", daemon=True)
    thread.start()
    try:
        serve(index, host, port)
    finally:
        stop.set()
        thread.join(timeout=5)
