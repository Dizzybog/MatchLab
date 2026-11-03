"""Single-result IMDb matching API; no guess when the title is ambiguous."""

from __future__ import annotations

import json
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
    inferred_series = bool(parsed.seasons or parsed.episodes) if series is None else series
    target_year = year if year is not None else parsed.year
    matches = index.search(
        parsed.parsed_title, target_year, series=inferred_series,
        required_max_season=max(parsed.seasons, default=0), limit=20,
    )
    if not matches:
        return None
    best = matches[0]
    # Without a year, identically named works are ambiguous.
    if len(matches) > 1:
        other = matches[1]
        if not target_year or abs(best.year - target_year) == abs(other.year - target_year):
            return None
    imdb_season = min(parsed.seasons) if parsed.seasons else None
    imdb_episode = min(parsed.episodes) if parsed.episodes else None
    anime = None
    if imdb_episode is not None and best.anime_ids.get("anidb"):
        resolved = index.resolve_anime_episode(
            best.imdb_id, imdb_season if imdb_season is not None else 1, imdb_episode,
        )
        if resolved:
            anime = {
                "anidb_id": resolved[0],
                "episode": resolved[1],
                "asid": f"{resolved[0]}:{resolved[1]}",
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
