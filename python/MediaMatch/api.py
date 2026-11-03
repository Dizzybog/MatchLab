"""Single-result IMDb matching API; no guess when the title is ambiguous."""

from __future__ import annotations

import json
import logging
import re
import signal
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from types import SimpleNamespace
from PTT import parse_title

def parse(raw_title: str):
    """Parse the entire release name using the built-in torrent parser."""
    data = parse_title(raw_title)
    def numbers(value):
        if value is None: return []
        if isinstance(value, int): return [value]
        if isinstance(value, (list, tuple)): return [int(x) for x in value if isinstance(x, int) or str(x).isdigit()]
        return [int(value)] if str(value).isdigit() else []
    return SimpleNamespace(
        parsed_title=data.get("title") or "",
        year=data.get("year"),
        seasons=numbers(data.get("seasons")),
        episodes=numbers(data.get("episodes")),
        model_dump=lambda **_: data,
    )

from .index import MetadataIndex
from .anime_title_matcher import resolve_titled_anime, has_specific_anime_alias

LOG = logging.getLogger(__name__)


def match_title(index: MetadataIndex, title: str, year: int | None = None,
                *, series: bool | None = None) -> dict | None:
    """Return one validated match, or None. Never return a candidate list."""
    if not isinstance(title, str) or not title.strip():
        raise ValueError("title must be a non-empty string")
    LOG.info("match_start title=%r year=%s series=%s", title, year, series)
    parsed = parse(title)
    LOG.debug("match_parsed result=%s", json.dumps(parsed.model_dump(mode="json"), ensure_ascii=False, default=str))
    # The built-in parser handles normal SxxExx / E numbering. Explicit "Episode 1067"
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
    # Both candidate pools are evaluated independently. Anime identities
    # originate only from anime tables; IMDb candidates from V4 search.
    from .anime_only import resolve_anime_title, is_known_anime_title
    anime_match = None
    anime_recognized = False
    if inferred_series and parsed_episodes:
        anime_match = resolve_anime_title(
            index, parsed_title,
            min(parsed_seasons) if parsed_seasons else None,
            min(parsed_episodes),
        )
        anime_recognized = is_known_anime_title(index, parsed_title)
    matches = index.search(
        parsed_title, target_year, series=inferred_series,
        required_max_season=max(parsed_seasons, default=0), limit=20,
        release_title=title, has_episode=bool(parsed_episodes),
    )
    LOG.debug("candidate_pools title=%r imdb_ids=%s anime=%s anime_recognized=%s",
              title, [m.imdb_id for m in matches], anime_match, anime_recognized)
    if anime_match is not None:
        anime_id = anime_match["anidb_id"]
        anime_episode = anime_match["anidb_episode"]
        canonical_season = anime_match["season"]
        canonical_episode = anime_match["episode"]
        imdb_id = anime_match["imdb_id"]
        LOG.info("match_selected source=anime_priority imdb=%s season=%s episode=%s anidb=%s anidb_episode=%s",
                 imdb_id, canonical_season, canonical_episode, anime_id, anime_episode)
        return {
            "imdb_id": imdb_id, "season": canonical_season,
            "episode": canonical_episode,
            "sid": (f"{imdb_id}:{canonical_season}:{canonical_episode}"
                    if canonical_season is not None and canonical_episode is not None
                    else imdb_id),
            "anime": ({"anidb_id": anime_id, "episode": anime_episode,
                       "asid": f"{anime_id}:{anime_episode}"}
                      if anime_id and anime_episode else None),
            "title": parsed_title, "year": target_year or 0,
            "category": "series", "matched_title": parsed_title,
            "anime_ids": ({"anidb": anime_id} if anime_id else {}),
            "parsed": parsed.model_dump(mode="json"),
            "match_source": "anime_priority",
        }
    # A recognized anime with an invalid/ambiguous mapping is not an
    # eligible anime candidate. A valid IMDb candidate may still win.
    if anime_recognized:
        LOG.info("anime_candidate_rejected title=%r reason=unresolved_or_ambiguous", parsed_title)
    if not matches:
        LOG.info("match_no_candidates title=%r", title)
        return None
    LOG.debug("match_candidates title=%r imdb_ids=%s", title, [m.imdb_id for m in matches])
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
            if season_number is not None:
                result = (resolve_titled_anime(index, candidate.imdb_id,
                                               parsed_title, season_number, episode_number)
                          or (None if has_specific_anime_alias(index, candidate.imdb_id,
                                                              parsed_title)
                              else index.resolve_anime_episode(candidate.imdb_id,
                                                              season_number, episode_number)))
            else:
                result = index.resolve_anime_absolute(candidate.imdb_id, episode_number)
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
            titled = resolve_titled_anime(index, best.imdb_id, parsed_title,
                                          imdb_season, imdb_episode)
            if titled is not None:
                anidb_id, anidb_episode = titled["anidb_id"], titled["anidb_episode"]
                imdb_season, imdb_episode = titled["season"], titled["episode"]
            else:
                if has_specific_anime_alias(index, best.imdb_id, parsed_title):
                    LOG.warning("anime_specific_title_unresolved imdb=%s title=%r season=%d episode=%d",
                                best.imdb_id, parsed_title, imdb_season, imdb_episode)
                    return None
                mapped = index.resolve_anime_episode(best.imdb_id, imdb_season, imdb_episode)
                if mapped is None:
                    LOG.info("anime_unresolved imdb=%s title=%r season=%d episode=%d",
                             best.imdb_id, parsed_title, imdb_season, imdb_episode)
                    return None
                anidb_id, anidb_episode = mapped["anidb_id"], mapped["anidb_episode"]
        anime = {
            "anidb_id": anidb_id,
            "episode": anidb_episode,
            "asid": f"{anidb_id}:{anidb_episode}",
        }
    LOG.info("match_selected title=%r imdb_id=%s season=%s episode=%s anime=%s", title, best.imdb_id, imdb_season, imdb_episode, anime)
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
            LOG.info("http_response id=%s method=%s path=%s status=%d body=%s elapsed_ms=%.1f",
                     getattr(self, "request_id", "unknown"), self.command, self.path, status,
                     json.dumps(payload, ensure_ascii=False, default=str),
                     (time.monotonic() - getattr(self, "started", time.monotonic())) * 1000)
            body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _match(self, title: object, year: object = None, series: object = None) -> None:
            LOG.info("http_match id=%s title=%r year=%r series=%r", self.request_id, title, year, series)
            if not isinstance(title, str) or not title.strip():
                self._respond(400, {"error": "title is required"})
                return
            try:
                # Distinguish "no match" from "metadata still downloading".
                import sqlite3
                with index.connect() as db:
                    ready = db.execute(
                        "SELECT 1 FROM metadata_state WHERE source='full_refresh'"
                    ).fetchone() is not None
                if not ready:
                    self._respond(503, {"error": "metadata_initializing"})
                    return
                parsed_year = int(year) if year is not None else None
                if series is not None and not isinstance(series, bool):
                    raise ValueError("series must be boolean")
                result = match_title(index, title, parsed_year, series=series)
            except (TypeError, ValueError) as exc:
                LOG.warning("http_bad_request id=%s error=%s", self.request_id, exc, exc_info=True)
                self._respond(400, {"error": str(exc)})
                return
            except Exception:
                LOG.exception("http_match_failed id=%s", self.request_id)
                self._respond(500, {"error": "internal_server_error"})
                return
            self._respond(200, result)

        def do_GET(self) -> None:
            self._start_request()
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
            self._start_request()
            if urlparse(self.path).path != "/match":
                self.send_error(404)
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if size < 1 or size > 16384:
                    raise ValueError("request body must be between 1 and 16384 bytes")
                raw = self.rfile.read(size)
                LOG.info("http_request_body id=%s body=%s", self.request_id, raw.decode("utf-8", errors="replace"))
                data = json.loads(raw)
                if not isinstance(data, dict):
                    raise ValueError("request body must be an object")
                self._match(data.get("t_title", data.get("title")),
                            data.get("year"), data.get("series"))
            except (ValueError, TypeError, UnicodeDecodeError) as exc:
                LOG.warning("http_bad_request id=%s error=%s", self.request_id, exc, exc_info=True)
                self._respond(400, {"error": str(exc)})
            except Exception:
                LOG.exception("http_request_failed id=%s", self.request_id)
                self._respond(500, {"error": "internal_server_error"})

        def _start_request(self) -> None:
            self.started = time.monotonic()
            self.request_id = uuid.uuid4().hex[:12]
            LOG.info("http_request id=%s remote=%s method=%s path=%s", self.request_id, self.client_address[0], self.command, self.path)

        def send_error(self, code: int, message: str | None = None, explain: str | None = None) -> None:
            self._respond(code, {"error": message or self.responses.get(code, ("HTTP error",))[0]})

        def log_message(self, format: str, *args: object) -> None:
            LOG.debug("http_server id=%s %s", getattr(self, "request_id", "unknown"), format % args)

    server = ThreadingHTTPServer((host, port), Handler)
    server.daemon_threads = True
    LOG.info("api_listening host=%s port=%s", host, port)
    old_handlers = {}
    def stop(signum, _frame):
        LOG.info("shutdown_signal signal=%s", signum)
        threading.Thread(target=server.shutdown, daemon=True).start()
    if threading.current_thread() is threading.main_thread():
        for sig in (signal.SIGTERM, signal.SIGINT):
            old_handlers[sig] = signal.getsignal(sig)
            signal.signal(sig, stop)
    try:
        server.serve_forever(poll_interval=0.2)
    finally:
        server.server_close()
        for sig, old in old_handlers.items():
            signal.signal(sig, old)
        LOG.info("api_stopped")
