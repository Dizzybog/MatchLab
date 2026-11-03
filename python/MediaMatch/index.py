"""SQLite metadata index and deterministic IMDb V4 style title matching."""

from __future__ import annotations

import re
import sqlite3
import unicodedata
from dataclasses import dataclass
from pathlib import Path

_SERIES = {"tvSeries", "tvMiniSeries"}
_MOVIES = {"movie", "tvMovie", "short", "tvShort", "tvSpecial"}
_MOVIE_TYPE_RANK = {"movie": 0, "tvMovie": 1, "short": 2,
                    "tvShort": 3, "tvSpecial": 4}


def clean_title(value: str) -> str:
    value = unicodedata.normalize("NFKD", value).encode("ascii", "ignore").decode()
    return re.sub(r"[^a-z0-9]+", "", value.lower())


@dataclass(frozen=True)
class Match:
    imdb_id: str
    title: str
    year: int
    category: str
    matched_title: str
    anime_ids: dict[str, str]
    media_type: str = ""


class MetadataIndex:
    """Persistent SQLite index. Pass a writable path; parent directories are created."""

    def __init__(self, path: str | Path = "metadata.sqlite3"):
        self.path = str(path)
        Path(self.path).parent.mkdir(parents=True, exist_ok=True)
        with self.connect() as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS titles (
                    imdb_id TEXT NOT NULL, title TEXT NOT NULL, clean_title TEXT NOT NULL,
                    year INTEGER NOT NULL, category TEXT NOT NULL, region TEXT,
                    original INTEGER NOT NULL DEFAULT 0, ordering INTEGER NOT NULL DEFAULT 0,
                    type TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (imdb_id, clean_title, year, region, ordering)
                );
                CREATE INDEX IF NOT EXISTS title_lookup ON titles(clean_title, year);
                CREATE INDEX IF NOT EXISTS title_suffix ON titles(clean_title);
                CREATE TABLE IF NOT EXISTS seasons (
                    imdb_id TEXT PRIMARY KEY, max_season INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS anime_ids (
                    group_id TEXT NOT NULL, type TEXT NOT NULL, value TEXT NOT NULL,
                    PRIMARY KEY (group_id, type, value)
                );
                CREATE TABLE IF NOT EXISTS episode_map (
                    anidb_id TEXT NOT NULL, tvdb_id TEXT NOT NULL, anidb_season INTEGER NOT NULL,
                    tvdb_season INTEGER NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL,
                    offset INTEGER NOT NULL DEFAULT 0, map TEXT NOT NULL DEFAULT ''
                );
                CREATE TABLE IF NOT EXISTS anidb_titles (
                    anidb_id TEXT NOT NULL, title TEXT NOT NULL,
                    clean_title TEXT NOT NULL, title_type TEXT NOT NULL,
                    language TEXT NOT NULL,
                    PRIMARY KEY(anidb_id, clean_title, title_type, language)
                );
                CREATE INDEX IF NOT EXISTS anidb_title_lookup ON anidb_titles(clean_title);
                CREATE TABLE IF NOT EXISTS anime_series_map (
                    imdb_id TEXT NOT NULL, anidb_id TEXT NOT NULL,
                    tvdb_id TEXT NOT NULL DEFAULT '', trakt_season INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (imdb_id, anidb_id, tvdb_id, trakt_season)
                );
                CREATE INDEX IF NOT EXISTS anime_series_by_anidb ON anime_series_map(anidb_id);
                CREATE INDEX IF NOT EXISTS anime_series_by_imdb ON anime_series_map(imdb_id);
                CREATE INDEX IF NOT EXISTS anime_lookup_idx ON anime_ids(type, value);
                CREATE INDEX IF NOT EXISTS episode_lookup_idx ON episode_map(anidb_id, anidb_season, start, end);
                CREATE TABLE IF NOT EXISTS metadata_state (
                    source TEXT PRIMARY KEY, updated_at TEXT NOT NULL, row_count INTEGER NOT NULL
                );
            """)

    def connect(self) -> sqlite3.Connection:
        db = sqlite3.connect(self.path)
        db.row_factory = sqlite3.Row
        return db

    def search(self, title: str, year: int | None = None, *, series: bool = False,
               required_max_season: int = 0, limit: int = 10) -> list[Match]:
        """Match cleaned IMDb titles with V4 duplicate, year and US/original tie breaks."""
        key = clean_title(title)
        if not key:
            return []
        allowed = _SERIES if series else _MOVIES
        with self.connect() as db:
            rows = db.execute("SELECT * FROM titles WHERE clean_title=? AND type IN ('movie','tvMovie','short','tvShort','tvSpecial','tvSeries','tvMiniSeries')", (key,)).fetchall()
            if not rows and len(key) >= 12:
                # Bounded longest-prefix fallback using the existing B-tree index.
                # Never use LIKE '%suffix': it scans the entire IMDb AKA table.
                for size in range(len(key) - 1, max(9, len(key) // 2) - 1, -1):
                    rows = db.execute(
                        "SELECT * FROM titles WHERE clean_title=? AND year>0",
                        (key[:size],),
                    ).fetchall()
                    if rows:
                        break
            if not rows and series:
                # StremThru AniDB main/official/synonyms for Japanese and
                # romanized aliases that IMDb AKAs may not contain.
                rows = db.execute(
                    "SELECT t.* FROM anidb_titles a "
                    "JOIN anime_series_map l ON l.anidb_id=a.anidb_id "
                    "JOIN titles t ON t.imdb_id=l.imdb_id "
                    "WHERE a.clean_title=? "
                    "AND t.type IN ('tvSeries','tvMiniSeries')",
                    (key,),
                ).fetchall()
            grouped: dict[str, list[sqlite3.Row]] = {}
            for row in rows:
                if row["type"] in allowed:
                    grouped.setdefault(row["imdb_id"], []).append(row)
            duplicate = len({r["imdb_id"] for r in db.execute(
                "SELECT DISTINCT imdb_id FROM titles WHERE clean_title=? AND year>0", (key,)
            )}) > 1
            matches = []
            for imdb_id, aliases in grouped.items():
                max_season = db.execute("SELECT max_season FROM seasons WHERE imdb_id=?", (imdb_id,)).fetchone()
                best = min(aliases, key=lambda r: (
                    abs(r["year"] - year) if year and r["year"] else 9999 if year else 0,
                    0 if not duplicate or r["original"] else 1,
                    r["year"] or 9999,
                    0 if not duplicate or r["region"] == "US" else 1,
                    r["ordering"] or 0,
                ))
                if series and required_max_season > 0 and (
                    max_season is None or max_season[0] < max(1, required_max_season - 1)
                ):
                    # SQL crosswalk may contain a season not present in IMDb's
                    # episode export (especially split anime seasons).
                    mapped = db.execute(
                        "SELECT 1 FROM anime_series_map l JOIN episode_map m "
                        "ON m.anidb_id=l.anidb_id "
                        "WHERE l.imdb_id=? AND (m.tvdb_season=? OR l.trakt_season=?) LIMIT 1",
                        (imdb_id, required_max_season, required_max_season),
                    ).fetchone()
                    if mapped is None:
                        continue
                types = {r["type"]: r["value"] for r in db.execute(
                    "SELECT type,value FROM anime_ids WHERE group_id=?", (imdb_id,)
                )}
                if "anidb" not in types:
                    link = db.execute(
                        "SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?",
                        (imdb_id,),
                    ).fetchall()
                    if len(link) == 1:
                        types["anidb"] = link[0][0]
                matches.append(Match(imdb_id, best["title"], best["year"], best["category"],
                                     best["title"], types, best["type"]))
            matches.sort(key=lambda m: (
                abs(m.year - year) if year and m.year else 9999 if year else 0,
                min((0 if not duplicate or r["original"] else 1) for r in grouped[m.imdb_id]),
                m.year or 9999,
                _MOVIE_TYPE_RANK.get(m.media_type, 0),
                min((0 if not duplicate or r["region"] == "US" else 1) for r in grouped[m.imdb_id]),
                -len(grouped[m.imdb_id]), m.imdb_id,
            ))
            return matches[:limit]

    def resolve_anime_episode(self, imdb_id: str, season: int, episode: int) -> tuple[str, int] | None:
        """IMDb/TVDB S/E -> AniDB episode, including split Trakt seasons."""
        from .anime_resolver import resolve_season_episode

        result = resolve_season_episode(self, imdb_id, season, episode)
        return (result["anidb_id"], result["anidb_episode"]) if result else None

    def resolve_anime_absolute(self, imdb_id: str, episode: int) -> dict | None:
        """Absolute AniDB episode -> IMDb/TVDB S/E."""
        from .anime_resolver import resolve_absolute_episode

        return resolve_absolute_episode(self, imdb_id, episode)

    def anime_lookup(self, source: str, value: str) -> dict[str, str] | None:
        with self.connect() as db:
            row = db.execute("SELECT group_id FROM anime_ids WHERE type=? AND value=? LIMIT 1", (source, value)).fetchone()
            if row is None:
                return None
            result = {r["type"]: r["value"] for r in db.execute(
                "SELECT type,value FROM anime_ids WHERE group_id=?", (row[0],))}
            result["imdb"] = row[0] if row[0].startswith("tt") else ""
            return result

    def map_anime_episode(self, anidb_id: str, season: int, episode: int) -> list[tuple[str, int, int]]:
        """Translate an AniDB season/episode into mapped TVDB season/episode coordinates."""
        with self.connect() as db:
            rows = db.execute("SELECT * FROM episode_map WHERE anidb_id=? AND anidb_season=? AND start<=? AND end>=?",
                              (anidb_id, season, episode, episode)).fetchall()
        return [(r["tvdb_id"], r["tvdb_season"], episode + r["offset"]) for r in rows]
