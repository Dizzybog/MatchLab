"""Single-result IMDb matching API; no guess when the title is ambiguous."""

from __future__ import annotations

import json
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from RTN import parse

from .index import MetadataIndex


def match_title(index: MetadataIndex, title: str, year: int | None = None,
                *, series: bool | None = None) -> dict | None:
    """Return one validated match, or None. Never return a candidate list."""
    if not isinstance(title, str) or not title.strip():
        raise ValueError("title must be a non-empty string")
    parsed = parse(title)
    # RTN handles normal SxxExx / E numbering. Explicit "Episode 1067"
    # is treated as absolute only when no season was provided.
    absolute = re.search(r"(?i)\b(?:episode|ep)\s*[. _-]*([0-9]{1,5})\b", title)
    parsed_seasons = list(parsed.seasons or [])
    parsed_episodes = list(parsed.episodes or [])
    if absolute and not parsed_episodes:
        parsed_episodes = [int(absolute.group(1))]
    parsed_title = parsed.parsed_title
    if absolute and not parsed_seasons:
        parsed_title = re.sub(r"(?i)\b(?:episode|ep)\s*[. _-]*[0-9]{1,5}\b", "", parsed_title).strip(" ._-")
    inferred_series = bool(parsed_seasons or parsed_episodes) if series is None else series
    target_year = year if year is not None else parsed.year
    matches = index.search(
        parsed_title, target_year, series=inferred_series,
        required_max_season=max(parsed_seasons, default=0), limit=20,
    )
    if not matches:
        return None
    best = matches[0]
    # A uniquely resolvable anime episode beats identically named non-anime
    # titles, including live-action adaptations without episode crosswalks.
    if len(matches) > 1 and parsed_episodes:
        season_number = min(parsed_seasons) if parsed_seasons else None
        episode_number = min(parsed_episodes)
        verified = []
        for candidate in matches:
            if not candidate.anime_ids.get("anidb"):
                continue
            result = (
                index.resolve_anime_episode(candidate.imdb_id, season_number, episode_number)
                if season_number is not None
                else index.resolve_anime_absolute(candidate.imdb_id, episode_number)
            )
            if result is not None:
                verified.append(candidate)
        if len(verified) == 1:
            best = verified[0]
        elif len(verified) > 1:
            return None
        elif not target_year:
            return None
    elif len(matches) > 1:
        other = matches[1]
        if not target_year or abs(best.year - target_year) == abs(other.year - target_year):
            return None
    imdb_season = min(parsed_seasons) if parsed_seasons else None
    imdb_episode = min(parsed_episodes) if parsed_episodes else None
    anime = None
    is_anime = bool(best.anime_ids.get("anidb"))
    if imdb_episode is not None and is_anime:
        if imdb_season is None:
            mapped = index.resolve_anime_absolute(best.imdb_id, imdb_episode)
            if mapped is None:
                return None
            imdb_season, imdb_episode = mapped["season"], mapped["episode"]
            anidb_id, anidb_episode = mapped["anidb_id"], mapped["anidb_episode"]
        else:
            mapped = index.resolve_anime_episode(best.imdb_id, imdb_season, imdb_episode)
            if mapped is None:
                return None
            anidb_id, anidb_episode = mapped
        anime = {
            "anidb_id": anidb_id,
            "episode": anidb_episode,
            "asid": f"{anidb_id}:{anidb_episode}",
        }
    return {
        "imdb_id": best.imdb_id,
        "season": imdb_season,
        "episode": imdb_episode,
        "sid": (f"{best.imdb_id}:{imdb_season}:{imdb_episode}"
                if imdb_season is not None and imdb_episode is not None
                else best.imdb_id),
        "anime": anime,
        "title": best.title,
        "year": best.year,
        "category": best.category,
        "matched_title": best.matched_title,
        "anime_ids": best.anime_ids,
        "parsed": parsed.model_dump(mode="json"),
    }


def serve(index: MetadataIndex, host: str = "127.0.0.1", port: int = 8765) -> None:
    class Handler(BaseHTTPRequestHandler):
        def _respond(self, status: int, payload: object) -> None:
            body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _match(self, title: object, year: object = None, series: object = None) -> None:
            if not isinstance(title, str) or not title.strip():
                self._respond(400, {"error": "title is required"})
                return
            try:
                parsed_year = int(year) if year is not None else None
                if series is not None and not isinstance(series, bool):
                    raise ValueError("series must be boolean")
                result = match_title(index, title, parsed_year, series=series)
            except (TypeError, ValueError) as exc:
                self._respond(400, {"error": str(exc)})
                return
            self._respond(200, result)

        def do_GET(self) -> None:
            request = urlparse(self.path)
            if request.path == "/health":
                from .service import metadata_due
                import sqlite3
                current = not metadata_due(index.path)
                try:
                    with sqlite3.connect(index.path) as db:
                        has_data = db.execute(
                            "SELECT 1 FROM metadata_state "
                            "WHERE source='full_refresh' LIMIT 1"
                        ).fetchone() is not None
                except sqlite3.Error:
                    has_data = False
                self._respond(200 if has_data else 503, {
                    "status": "ready" if current else ("stale" if has_data else "initializing"),
                    "metadata_current": current,
                })
                return
            if request.path != "/match":
                self.send_error(404)
                return
            params = parse_qs(request.query)
            self._match(params.get("title", [""])[0],
                        params.get("year", [None])[0])

        def do_POST(self) -> None:
            if urlparse(self.path).path != "/match":
                self.send_error(404)
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if size < 1 or size > 16384:
                    raise ValueError("request body must be between 1 and 16384 bytes")
                data = json.loads(self.rfile.read(size))
                if not isinstance(data, dict):
                    raise ValueError("request body must be an object")
                self._match(data.get("t_title", data.get("title")),
                            data.get("year"), data.get("series"))
            except (ValueError, TypeError, UnicodeDecodeError) as exc:
                self._respond(400, {"error": str(exc)})

        def log_message(self, *_args: object) -> None:
            return

    ThreadingHTTPServer((host, port), Handler).serve_forever()
