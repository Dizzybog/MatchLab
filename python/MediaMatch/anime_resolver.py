"""Bidirectional IMDb/TVDB <-> AniDB episode crosswalk.

Mirrors the direct offset and split Trakt-season rules from the SQL backup.
Only explicit crosswalk rows can translate absolute episode numbering.
"""

from __future__ import annotations

from typing import TYPE_CHECKING
import re

if TYPE_CHECKING:
    from .index import MetadataIndex


def _explicit_pairs(value: str) -> list[tuple[int, int]]:
    """Anime-Lists ';anidb-episode;tvdb-episode;' map expressions."""
    pairs = []
    for part in value.strip(";").split(";"):
        found = re.fullmatch(r"(\\d+)-(\\d+)", part.strip())
        if found:
            pairs.append((int(found[1]), int(found[2])))
    return pairs


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
                    explicit = _explicit_pairs(m["map"])
                    if explicit:
                        hits = [ani for ani, tv in explicit if tv == episode]
                        for ani in hits:
                            candidates.append((m["offset"], m["start"], link["anidb_id"], ani))
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
    """Translate a TVDB absolute episode to AniDB-local and IMDb S/E.

    Anime-Lists tvdb_season=-1 entries contain the cumulative
    `episodeoffset` for separate AniDB series (Fairy Tail, Bleach, etc.).
    """
    if episode <= 0:
        return None
    with index.connect() as db:
        links = _links(db, imdb_id)
        if not links:
            return None
        ids = sorted({link["anidb_id"] for link in links})
        placeholders = ",".join("?" for _ in ids)
        all_rows = db.execute(
            f"SELECT * FROM episode_map WHERE anidb_id IN ({placeholders}) "
            "AND anidb_season>0", ids,
        ).fetchall()
        candidates = []
        for link in links:
            associated = [m for m in all_rows
                          if m["anidb_id"] == link["anidb_id"]
                          and (not link["tvdb_id"] or m["tvdb_id"] == link["tvdb_id"])]
            absolutes = [m for m in associated if m["tvdb_season"] == -1]
            # A mapping without a global offset is local only when no
            # other AniDB part can claim the same IMDb title.
            if not absolutes and len(ids) > 1:
                continue
            offset = absolutes[0]["offset"] if len(absolutes) == 1 else 0
            local = episode - offset
            if local <= 0:
                continue
            if absolutes and absolutes[0]["end"] > 0 and local > absolutes[0]["end"]:
                continue
            for m in associated:
                if m["tvdb_season"] <= 0:
                    continue
                if local < max(m["start"], 1) or (m["end"] > 0 and local > m["end"]):
                    continue
                split = _split(db, imdb_id, link, m, links, all_rows)
                season = link["trakt_season"] if split else m["tvdb_season"]
                if split:
                    tv_episode = local - max(m["start"], 1) + 1
                else:
                    explicit = _explicit_pairs(m["map"])
                    if explicit:
                        hits = [tv for ani, tv in explicit if ani == local]
                        for tv in hits:
                            if season > 0 and tv > 0:
                                candidates.append((link["anidb_id"], local, season, tv))
                        continue
                    tv_episode = local + m["offset"]
                if season > 0 and tv_episode > 0:
                    candidates.append((link["anidb_id"], local, season, tv_episode))
        unique = set(candidates)
        if len(unique) != 1:
            return None
        anidb_id, local, season, tv_episode = unique.pop()
        return {
            "imdb_id": imdb_id, "season": season, "episode": tv_episode,
            "anidb_id": anidb_id, "anidb_episode": local,
        }
