"""Bidirectional IMDb/TVDB <-> AniDB episode crosswalk.

Mirrors the direct offset and split Trakt-season rules from the SQL backup.
Only explicit crosswalk rows can translate absolute episode numbering.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .index import MetadataIndex


def _links(db, imdb_id: str):
    rows = db.execute(
        "SELECT imdb_id,anidb_id,tvdb_id,trakt_season "
        "FROM anime_series_map WHERE imdb_id=?", (imdb_id,),
    ).fetchall()
    if rows:
        return rows
    # Legacy imports without explicit series links: only a single AniDB ID is safe.
    ids = db.execute(
        "SELECT DISTINCT value FROM anime_ids WHERE group_id=? AND type='anidb'",
        (imdb_id,),
    ).fetchall()
    if len(ids) != 1:
        return []
    return [{"imdb_id": imdb_id, "anidb_id": ids[0][0],
             "tvdb_id": "", "trakt_season": 0}]


def _crosswalk(db, links):
    if not links:
        return []
    ids = sorted({r["anidb_id"] for r in links})
    slots = ",".join("?" for _ in ids)
    return db.execute(
        f"SELECT * FROM episode_map WHERE anidb_id IN ({slots}) "
        "AND anidb_season>0 AND tvdb_season>0",
        ids,
    ).fetchall()


def _split(db, imdb_id: str, link, mapping, links, mappings) -> bool:
    season = link["trakt_season"]
    if season <= 0:
        return False
    return any(
        other["anidb_id"] != link["anidb_id"]
        and other["trakt_season"] > 0
        and other["trakt_season"] != season
        and m["anidb_id"] == other["anidb_id"]
        and m["tvdb_id"] == mapping["tvdb_id"]
        and m["tvdb_season"] == mapping["tvdb_season"]
        for other in links for m in mappings
    )


def resolve_season_episode(index: MetadataIndex, imdb_id: str,
                           season: int, episode: int) -> dict | None:
    """Convert season-based IMDb/TVDB coordinates into AniDB coordinates."""
    if season <= 0 or episode <= 0:
        return None
    with index.connect() as db:
        links = _links(db, imdb_id)
        mappings = _crosswalk(db, links)
        candidates = []
        for link in links:
            for m in mappings:
                if m["anidb_id"] != link["anidb_id"]:
                    continue
                if link["tvdb_id"] and link["tvdb_id"] != m["tvdb_id"]:
                    continue
                split = _split(db, imdb_id, link, m, links, mappings)
                if split:
                    if link["trakt_season"] != season:
                        continue
                    ep = episode + max(m["start"], 1) - 1
                else:
                    if m["tvdb_season"] != season:
                        continue
                    ep = episode - m["offset"]
                if ep < max(m["start"], 1) or (m["end"] > 0 and ep > m["end"]):
                    continue
                candidates.append((m["offset"], m["start"], link["anidb_id"], ep))
        if not candidates:
            return None
        candidates.sort(key=lambda r: (-r[0], -r[1], r[2]))
        winner = candidates[0]
        # Do not silently pick between two equally ranked different episodes.
        if len(candidates) > 1 and candidates[1][:2] == winner[:2] and candidates[1][2:] != winner[2:]:
            return None
        return {"anidb_id": winner[2], "anidb_episode": winner[3]}


def resolve_absolute_episode(index: MetadataIndex, imdb_id: str,
                             episode: int) -> dict | None:
    """Convert an absolute AniDB episode to IMDb season/episode.

    Unlike a season-based title, the number is interpreted as an AniDB
    episode coordinate; a real crosswalk is mandatory to derive a season.
    """
    if episode <= 0:
        return None
    with index.connect() as db:
        links = _links(db, imdb_id)
        mappings = _crosswalk(db, links)
        candidates = []
        for link in links:
            for m in mappings:
                if m["anidb_id"] != link["anidb_id"]:
                    continue
                if link["tvdb_id"] and link["tvdb_id"] != m["tvdb_id"]:
                    continue
                if episode < max(m["start"], 1) or (m["end"] > 0 and episode > m["end"]):
                    continue
                split = _split(db, imdb_id, link, m, links, mappings)
                season = link["trakt_season"] if split else m["tvdb_season"]
                ep = episode - max(m["start"], 1) + 1 if split else episode + m["offset"]
                if season > 0 and ep > 0:
                    candidates.append((m["start"], m["offset"], link["anidb_id"], season, ep))
        if not candidates:
            return None
        candidates.sort(key=lambda r: (-r[0], -r[1], r[2]))
        winner = candidates[0]
        if len(candidates) > 1 and candidates[1][:2] == winner[:2] and candidates[1][2:] != winner[2:]:
            return None
        return {
            "imdb_id": imdb_id, "season": winner[3], "episode": winner[4],
            "anidb_id": winner[2], "anidb_episode": episode,
        }
