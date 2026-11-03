"""SQLite metadata index and deterministic IMDb V4 style title matching."""

from __future__ import annotations

import logging
import re
import sqlite3
import unicodedata
from dataclasses import dataclass
from pathlib import Path

LOG = logging.getLogger(__name__)
_SERIES = {"tvSeries", "tvMiniSeries"}
_MOVIES = {"movie", "tvMovie", "short", "tvShort", "tvSpecial"}
_MOVIE_TYPE_RANK = {"movie": 0, "tvMovie": 1, "short": 2,
                    "tvShort": 3, "tvSpecial": 4}


def clean_title(value: str) -> str:
    value = value.casefold().replace("æ", "ae").replace("ø", "o").replace("œ", "oe").replace("ß", "ss")
    value = unicodedata.normalize("NFKD", value).encode("ascii", "ignore").decode()
    # PostgreSQL V4 removes the standalone word "and" before punctuation.
    value = re.sub(r"\\band\\b", "", value)
    return re.sub(r"[^a-z0-9]+", "", value)


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
                CREATE INDEX IF NOT EXISTS title_reverse_suffix ON titles(reverse(clean_title), type);
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
        db.create_function("reverse", 1, lambda text: text[::-1] if text else "", deterministic=True)
        return db

    def search(self, title: str, year: int | None = None, *, series: bool = False,
               required_max_season: int = 0, limit: int = 10,
               release_title: str | None = None, has_episode: bool = False) -> list[Match]:
        """Ordered IMDb V4 matching stages, with indexed prefix and reverse-suffix.

        The raw release name is optional for direct library users; API calls
        always supply it so a candidate must actually occur in the release.
        """
        key = clean_title(title)
        release = clean_title(release_title if release_title is not None else title)
        if not key:
            return []
        count_first = not year and not required_max_season and not has_episode
        with self.connect() as db:
            def select(where: str, params: tuple, allowed: set[str]) -> list[sqlite3.Row]:
                types = tuple(sorted(allowed))
                placeholders = ",".join("?" for _ in types)
                return db.execute(
                    f"SELECT * FROM titles WHERE {where} AND type IN ({placeholders}) AND year>0",
                    (*params, *types),
                ).fetchall()

            def eligible(row: sqlite3.Row) -> bool:
                if row["type"] in _SERIES and required_max_season:
                    # V4 accepts a one-season tolerance for missing IMDb metadata.
                    minimum = max(1, required_max_season - 1)
                    maximum = db.execute(
                        "SELECT max_season FROM seasons WHERE imdb_id=?",
                        (row["imdb_id"],),
                    ).fetchone()
                    if maximum is None or maximum[0] < minimum:
                        mapped = db.execute(
                            "SELECT 1 FROM anime_series_map l JOIN episode_map m "
                            "ON m.anidb_id=l.anidb_id "
                            "WHERE l.imdb_id=? AND (m.tvdb_season=? OR l.trakt_season=?) LIMIT 1",
                            (row["imdb_id"], required_max_season, required_max_season),
                        ).fetchone()
                        if mapped is None:
                            return False
                return True

            def present(row: sqlite3.Row) -> bool:
                return row["clean_title"] in release

            # Movie titles can fall back to series when the release has no year,
            # as in V4. Year-based movie fallback is intentionally conservative.
            allowed = _SERIES if series else (_MOVIES | _SERIES if not year else _MOVIES)

            def valid(rows: list[sqlite3.Row]) -> list[sqlite3.Row]:
                return [r for r in rows if eligible(r) and present(r)]

            def prefix_rows(prefix: str) -> list[sqlite3.Row]:
                return valid(select(
                    "clean_title>=? AND clean_title<?",
                    (prefix, prefix + "\uffff"), allowed,
                ))

            # 1. Exact/forward prefix: V4 prefers exact, then the longest
            # candidate title found in the original release.
            rows = prefix_rows(key)
            stage = "forward_prefix"
            if not rows:
                # 2. V4 can also match IMDb titles that are shorter than the
                # parsed title. Test progressively shorter exact prefixes.
                for size in range(len(key) - 1, max(2, len(key) // 2) - 1, -1):
                    rows = valid(select("clean_title=?", (key[:size],), allowed))
                    if rows:
                        stage = "shorter_prefix"
                        break
            if not rows and len(key) >= 12:
                # 3. Reverse suffix: IMDb AKA may have an extra leading phrase.
                # Indexed reversed clean titles avoid a full IMDb table scan.
                rev = key[::-1]
                rows = valid(select(
                    "reverse(clean_title)>=? AND reverse(clean_title)<? "
                    "AND length(clean_title)>?",
                    (rev, rev + "\uffff", len(key)), allowed,
                ))
                if rows:
                    stage = "reverse_suffix"
            if not rows and len(key) >= 6:
                # 4. Half-prefix fallback, with full release containment.
                half = key[:(len(key) + 1) // 2]
                rows = prefix_rows(half)
                if rows:
                    stage = "half_prefix"
            if not rows and series:
                rows = valid(db.execute(
                    "SELECT t.* FROM anidb_titles a "
                    "JOIN anime_series_map l ON l.anidb_id=a.anidb_id "
                    "JOIN titles t ON t.imdb_id=l.imdb_id "
                    "WHERE a.clean_title=? AND t.type IN ('tvSeries','tvMiniSeries')",
                    (key,),
                ).fetchall())
                if rows:
                    stage = "anime_alias"
            if not rows:
                LOG.debug("imdb_match stage=none key=%s year=%s series=%s release=%s",
                          key, year, series, release_title)
                return []

            # Exact match first, then longest IMDb AKA. Ranking follows the
            # original V4 year/original/US/type/AKA-count ordering.
            best_shape = min(
                (0 if r["clean_title"] == key else 1, -len(r["clean_title"]))
                for r in rows
            )
            if stage in {"forward_prefix", "half_prefix"}:
                rows = [r for r in rows if (
                    0 if r["clean_title"] == key else 1, -len(r["clean_title"])
                ) == best_shape]
            elif stage == "reverse_suffix":
                longest = max(len(r["clean_title"]) for r in rows)
                rows = [r for r in rows if len(r["clean_title"]) == longest]

            grouped: dict[str, list[sqlite3.Row]] = {}
            for row in rows:
                grouped.setdefault(row["imdb_id"], []).append(row)
            duplicate_titles = {r["clean_title"] for r in rows}
            duplicates = set()
            for cleaned in duplicate_titles:
                found = db.execute(
                    "SELECT COUNT(DISTINCT imdb_id) FROM titles WHERE clean_title=? AND year>0",
                    (cleaned,),
                ).fetchone()[0]
                if found > 1:
                    duplicates.add(cleaned)
            aka_counts: dict[str, int] = {}
            for imdb_id in grouped:
                aka_counts[imdb_id] = db.execute(
                    "SELECT COUNT(*) FROM titles WHERE imdb_id=?", (imdb_id,),
                ).fetchone()[0]

            def rank(row: sqlite3.Row) -> tuple:
                duplicate = row["clean_title"] in duplicates
                original = 0 if not duplicate or row["original"] else 1
                us = 0 if not duplicate or row["region"] == "US" else 1
                year_rank = (abs(row["year"] - year), original, row["year"]) if year else (
                    original, row["year"], 0
                )
                media_type = _MOVIE_TYPE_RANK.get(row["type"], 5 if row["type"] == "tvSeries" else 6)
                return (
                    -aka_counts[row["imdb_id"]] if count_first else 0,
                    year_rank, media_type if not series else 0, us,
                    -aka_counts[row["imdb_id"]] if not count_first else 0,
                    row["ordering"], row["imdb_id"],
                )

            matches = []
            for imdb_id, aliases in grouped.items():
                best = min(aliases, key=rank)
                anime_ids = {r["type"]: r["value"] for r in db.execute(
                    "SELECT type,value FROM anime_ids WHERE group_id=?", (imdb_id,),
                )}
                if "anidb" not in anime_ids:
                    linked = db.execute(
                        "SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?",
                        (imdb_id,),
                    ).fetchall()
                    if len(linked) == 1:
                        anime_ids["anidb"] = linked[0][0]
                matches.append((rank(best), Match(
                    imdb_id, best["title"], best["year"], best["category"],
                    best["title"], anime_ids, best["type"],
                )))
            matches.sort(key=lambda item: item[0])
            LOG.debug(
                "imdb_match stage=%s key=%s year=%s series=%s candidates=%d selected=%s",
                stage, key, year, series, len(matches),
                matches[0][1].imdb_id if matches else None,
            )
            return [match for _, match in matches[:limit]]

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
