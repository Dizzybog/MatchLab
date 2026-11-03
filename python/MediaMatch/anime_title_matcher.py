"""Anime title-specific matching from the older torrent_stream anime resolver.

Two paths are deliberately distinct:
* Generic/base anime title + SxxExx -> global TVDB season, offset mapping.
* Specific part/subtitle + SxxExx -> local season relative to that AniDB part.

Never return an unverified AniDB episode for an explicit global season.
"""

from __future__ import annotations

import logging

from .index import clean_title

LOG = logging.getLogger(__name__)


def _related(db, imdb_id: str) -> list[dict]:
    rows = db.execute(
        "SELECT DISTINCT imdb_id,anidb_id,tvdb_id,trakt_season "
        "FROM anime_series_map WHERE imdb_id=? AND anidb_id<>''",
        (imdb_id,),
    ).fetchall()
    return [dict(r) for r in rows]


def _alias_candidates(db, links: list[dict], title: str) -> list[tuple]:
    key = clean_title(title)
    if len(key) < 4:
        return []
    ids = sorted({link["anidb_id"] for link in links})
    if not ids:
        return []
    marks = ",".join("?" for _ in ids)
    # AniDB titles include localized and alternate part names. The longest
    # valid prefix is the most specific part; no fuzzy arbitrary substitutions.
    rows = db.execute(
        f"SELECT anidb_id,clean_title FROM anidb_titles "
        f"WHERE anidb_id IN ({marks}) AND "
        "(clean_title=? OR (length(clean_title)>=4 AND ? LIKE clean_title||'%'))",
        (*ids, key, key),
    ).fetchall()
    scored = set()
    for row in rows:
        alias = row["clean_title"]
        if alias == key:
            priority = 0
        else:
            priority = 1
        scored.add((priority, -len(alias), row["anidb_id"], alias))
    # Anime-Planet title slugs from animeApi are also useful when present.
    for row in db.execute(
        f"SELECT value,group_id FROM anime_ids WHERE type='animeplanet' "
        f"AND group_id=?",
        (links[0]["imdb_id"],),
    ):
        alias = clean_title(row["value"].replace("-", " "))
        if len(alias) >= 4 and (alias == key or key.startswith(alias)):
            for link in links:
                scored.add((0 if alias == key else 1, -len(alias),
                            link["anidb_id"], alias))
    return sorted(scored)


def resolve_titled_anime(index, imdb_id: str, parsed_title: str,
                          season: int, episode: int) -> dict | None:
    """Resolve part-specific anime episodes without treating local S01 as TVDB S01."""
    if season <= 0 or episode <= 0:
        return None
    with index.connect() as db:
        links = _related(db, imdb_id)
        if not links:
            return None
        aliases = _alias_candidates(db, links, parsed_title)
        if not aliases:
            LOG.debug("anime_title stage=no_alias imdb=%s title=%r", imdb_id, parsed_title)
            return None
        # An equally strong alias for different AniDB parts is ambiguous.
        best = aliases[0]
        tied = {r[2] for r in aliases if r[:2] == best[:2]}
        if len(tied) != 1:
            LOG.warning("anime_title stage=ambiguous imdb=%s title=%r ids=%s",
                        imdb_id, parsed_title, sorted(tied))
            return None
        selected = best[2]
        mappings = db.execute(
            "SELECT * FROM episode_map WHERE anidb_id=? "
            "AND anidb_season>0 AND tvdb_season>0 "
            "ORDER BY tvdb_season,offset,start",
            (selected,),
        ).fetchall()
        if not mappings:
            return None
        # Base AniDB segment is the earliest mapped part in the TVDB family.
        family = mappings[0]["tvdb_id"]
        base = db.execute(
            "SELECT anidb_id FROM episode_map WHERE tvdb_id=? "
            "AND anidb_season>0 AND tvdb_season>0 "
            "ORDER BY tvdb_season,offset,start,"
            "CASE WHEN anidb_id GLOB '[0-9]*' THEN CAST(anidb_id AS INTEGER) ELSE 2147483647 END "
            "LIMIT 1",
            (family,),
        ).fetchone()
        if base and selected == base[0]:
            # The base title uses global TVDB season numbering.
            LOG.debug("anime_title stage=global imdb=%s anidb=%s season=%d episode=%d",
                      imdb_id, selected, season, episode)
            return index.resolve_anime_episode(imdb_id, season, episode)

        # Specific subtitle/part uses a relative season offset among siblings
        # of the same IMDb ID (as in match_torrent_stream_anime_resolve).
        siblings = db.execute(
            "SELECT DISTINCT m.anidb_id,MIN(m.tvdb_season) AS tvdb_season,"
            "MIN(m.offset) AS offset,MIN(m.start) AS start "
            "FROM episode_map m JOIN anime_series_map l ON l.anidb_id=m.anidb_id "
            "WHERE l.imdb_id=? AND m.tvdb_id=? AND "
            "m.anidb_season>0 AND m.tvdb_season>0 "
            "GROUP BY m.anidb_id ORDER BY tvdb_season,offset,start,anidb_id",
            (imdb_id, family),
        ).fetchall()
        parts = [r["anidb_id"] for r in siblings]
        if selected not in parts:
            return None
        target_position = parts.index(selected) + season - 1
        if target_position >= len(parts):
            LOG.info("anime_title stage=relative_out_of_range imdb=%s selected=%s season=%d",
                     imdb_id, selected, season)
            return None
        target = parts[target_position]
        rows = db.execute(
            "SELECT * FROM episode_map WHERE anidb_id=? "
            "AND anidb_season>0 AND tvdb_season>0 ORDER BY tvdb_season,offset,start",
            (target,),
        ).fetchall()
        # Relative part E01 means AniDB episode 1; a part with start>1
        # means E01 starts at that offset (split Trakt seasons).
        candidates = []
        for m in rows:
            local = episode + max(m["start"], 1) - 1 if m["start"] > 1 else episode
            if local < max(m["start"], 1):
                continue
            if m["end"] > 0 and local > m["end"]:
                continue
            candidates.append(local)
        if len(set(candidates)) != 1:
            LOG.warning("anime_title stage=relative_ambiguous imdb=%s anidb=%s episode=%d candidates=%s",
                        imdb_id, target, episode, candidates)
            return None
        local = candidates[0]
        LOG.info("anime_title stage=relative imdb=%s title=%r selected=%s target=%s "
                 "relative_season=%d anidb_episode=%d",
                 imdb_id, parsed_title, selected, target, season, local)
        return target, local
