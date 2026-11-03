"""Anime-only identity and episode resolution.

No SELECT from IMDb titles, AKAs or seasons. IMDb IDs are read solely from
anime_series_map (animeApi / Anime-Lists) and anime_ids.
"""

from __future__ import annotations

import logging

from .index import clean_title
from .anime_resolver import resolve_season_episode, resolve_absolute_episode
from .anime_title_matcher import resolve_titled_anime, has_specific_anime_alias

LOG = logging.getLogger(__name__)


def resolve_anime_title(index, parsed_title: str, season: int | None,
                        episode: int | None) -> dict | None:
    """Resolve one unambiguous anime using only anime metadata.

    A specific AniDB alias may represent a separate part: title-relative
    seasons are mapped to canonical TVDB/Trakt coordinates. An unresolved
    episode is never fabricated.
    """
    key = clean_title(parsed_title)
    if len(key) < 4:
        return None
    with index.connect() as db:
        # Restrict matches to AniDB titles with a verified anime -> IMDb link.
        # Exact alias wins over longer/shorter prefix. For release titles,
        # allow a trailing arc/part token only when the alias is a prefix.
        rows = db.execute(
            "SELECT DISTINCT a.anidb_id,a.clean_title,l.imdb_id "
            "FROM anidb_titles a "
            "JOIN anime_series_map l ON l.anidb_id=a.anidb_id "
            "WHERE a.clean_title=? OR "
            "(length(a.clean_title)>=6 AND ? LIKE a.clean_title||'%')",
            (key, key),
        ).fetchall()
    if not rows:
        LOG.debug("anime_only stage=no_alias key=%s", key)
        return None
    best_score = min((0 if r["clean_title"] == key else 1,
                      -len(r["clean_title"])) for r in rows)
    winners = {(r["imdb_id"], r["anidb_id"]) for r in rows
               if (0 if r["clean_title"] == key else 1,
                   -len(r["clean_title"])) == best_score}
    imdb_ids = {imdb for imdb, _ in winners}
    if len(imdb_ids) != 1:
        LOG.warning("anime_only stage=ambiguous_imdb key=%s ids=%s",
                    key, sorted(imdb_ids))
        return None
    imdb_id = next(iter(imdb_ids))
    if episode is None:
        # Without episode coordinates, multiple AniDB parts can still share
        # one IMDb identity. Returning the IMDb identity is safe.
        return {"imdb_id": imdb_id, "season": season, "episode": None,
                "anidb_id": None, "anidb_episode": None}
    if episode <= 0 or (season is not None and season <= 0):
        return None
    if season is None:
        mapped = resolve_absolute_episode(index, imdb_id, episode)
        if mapped is None:
            return None
        return mapped

    mapped = resolve_titled_anime(index, imdb_id, parsed_title, season, episode)
    if mapped is None:
        if has_specific_anime_alias(index, imdb_id, parsed_title):
            LOG.info("anime_only stage=part_unresolved imdb=%s key=%s season=%s episode=%s",
                     imdb_id, key, season, episode)
            return None
        mapped = resolve_season_episode(index, imdb_id, season, episode)
        if mapped is None:
            return None
        mapped = {**mapped, "season": season, "episode": episode}
    return {"imdb_id": imdb_id, **mapped}
