#!/usr/bin/env python3
"""150-case external MatchLab API smoke test. Python standard library only.

Run: python3 scripts/test_public_api.py --url https://matchlab.notprod.fyi --out matchlab-api-report.json
This script sends 150 sequential POST requests; it does not modify server data.
"""
import argparse
from collections import deque
import json
import statistics
import time
import urllib.error
import urllib.request
from pathlib import Path

FILMS = ["The Matrix|1999","The Godfather|1972","The Dark Knight|2008","Inception|2010","Interstellar|2014","Dune|2021","Dune Part Two|2024","Oppenheimer|2023","Barbie|2023","Fight Club|1999","Pulp Fiction|1994","Forrest Gump|1994","The Shawshank Redemption|1994","The Lord of the Rings The Fellowship of the Ring|2001","The Lord of the Rings The Two Towers|2002","The Lord of the Rings The Return of the King|2003","The Hobbit An Unexpected Journey|2012","Avatar|2009","Avatar The Way of Water|2022","Titanic|1997","Gladiator|2000","Gladiator II|2024","Jurassic Park|1993","Jaws|1975","Alien|1979","Aliens|1986","Terminator 2 Judgment Day|1991","The Terminator|1984","Blade Runner|1982","Blade Runner 2049|2017","Mad Max Fury Road|2015","Furiosa A Mad Max Saga|2024","The Batman|2022","Joker|2019","Top Gun Maverick|2022","John Wick|2014","John Wick Chapter 4|2023","The Prestige|2006","Memento|2000","Se7en|1995","The Silence of the Lambs|1991","The Departed|2006","Goodfellas|1990","Casino|1995","Heat|1995","Whiplash|2014","La La Land|2016","Everything Everywhere All at Once|2022","Parasite|2019","The Truman Show|1998"]
SHOWS = ["Breaking Bad","Better Call Saul","Game of Thrones","House of the Dragon","The Last of Us","The Walking Dead","Stranger Things","Dark","The Wire","The Sopranos","Lost","Friends","The Office","Parks and Recreation","Brooklyn Nine Nine","Modern Family","How I Met Your Mother","The Big Bang Theory","Community","Arrested Development","Succession","Severance","The Bear","Ted Lasso","The Mandalorian","Andor","Ahsoka","The Boys","Gen V","Invincible","Fallout","Westworld","Black Mirror","True Detective","Fargo","Sherlock","Doctor Who","Peaky Blinders","Vikings","The Crown","Narcos","Ozark","Money Heist","Squid Game","The Witcher","Wednesday","The Umbrella Academy","The Expanse","Foundation","90 Day Fiance"]
ANIME = ["Naruto","Naruto Shippuden","One Piece","Bleach","Bleach Thousand Year Blood War","Dragon Ball","Dragon Ball Z","Dragon Ball Super","Attack on Titan","Death Note","Fullmetal Alchemist Brotherhood","Jujutsu Kaisen","Demon Slayer Kimetsu no Yaiba","My Hero Academia","Hunter x Hunter","One Punch Man","Mob Psycho 100","Chainsaw Man","Spy x Family","Frieren Beyond Journeys End","Solo Leveling","Vinland Saga","Tokyo Ghoul","Tokyo Revengers","Blue Lock","Haikyu","Kuroko no Basket","Food Wars","Steins Gate","Code Geass","Cowboy Bebop","Samurai Champloo","Neon Genesis Evangelion","Gurren Lagann","Kill la Kill","Sword Art Online","Re Zero","Konosuba","Overlord","That Time I Got Reincarnated as a Slime","Mushoku Tensei","The Rising of the Shield Hero","Dr Stone","Black Clover","Fairy Tail","JoJos Bizarre Adventure","Pokemon","Digimon Adventure","Detective Conan","Sailor Moon"]

def cases():
    assert len(FILMS) == len(SHOWS) == len(ANIME) == 50
    for i, raw in enumerate(FILMS):
        title, year = raw.rsplit("|", 1)
        yield {"kind": "movie", "title": f"{title.replace(' ', '.')}.{year}.{['1080p.BluRay.x264','2160p.WEB-DL.DDP5.1','720p.WEBRip.x265','1080p.REMUX','2160p.HDR.WEB-DL'][i % 5]}-GROUP", "expected_year": int(year)}
    for i, title in enumerate(SHOWS):
        season, episode = 1 + i % 4, 1 + i % 12
        yield {"kind": "series", "title": f"{title.replace(' ', '.')}.S{season:02d}E{episode:02d}.{['1080p.WEB-DL','720p.WEBRip','2160p.WEB.H265','1080p.BluRay','WEB.x264'][i % 5]}-GROUP", "expected_season": season, "expected_episode": episode}
    for i, title in enumerate(ANIME):
        season, episode = 1 + i % 3, 1 + i % 12
        yield {"kind": "anime", "title": f"{title.replace(' ', '.')}.S{season:02d}E{episode:02d}.{['1080p.WEB-DL','720p.WEBRip','1080p.BluRay','WEB.H264','1080p.HEVC'][i % 5]}-GROUP", "expected_season": season, "expected_episode": episode}

def check(case, result):
    problems = []
    if result is None:
        return ["no_match"]
    if not isinstance(result, dict):
        return ["unexpected_response_shape"]
    imdb_id = result.get("imdb_id")
    if not imdb_id and not result.get("sid") and not result.get("anime"):
        problems.append("missing_identifier")
    if case["kind"] == "movie":
        if result.get("category") not in (None, "movie"):
            problems.append("wrong_category")
        if result.get("year") is not None and result["year"] != case["expected_year"]:
            problems.append("year_mismatch")
    elif case["kind"] == "series":
        if result.get("season") is not None and result["season"] != case["expected_season"]:
            problems.append("season_mismatch")
        if result.get("episode") is not None and result["episode"] != case["expected_episode"]:
            problems.append("episode_mismatch")
    # Anime may legitimately use different AniDB/TVDB numbering: report fields,
    # but do not flag season/episode differences without a verified mapping.
    return problems

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="https://matchlab.notprod.fyi")
    parser.add_argument("--out", default="matchlab-api-report.json")
    parser.add_argument("--timeout", type=float, default=20)
    args = parser.parse_args()
    base = args.url.rstrip("/")
    records = []
    # Sliding-window limiter: at most four request starts in any 10-second window.
    request_starts = deque()
    for index, case in enumerate(cases(), 1):
        while len(request_starts) >= 4:
            wait = 10.01 - (time.monotonic() - request_starts[0])
            if wait > 0:
                time.sleep(wait)
            request_starts.popleft()
        started = time.monotonic()
        request_starts.append(started)
        req = urllib.request.Request(base + "/match", data=json.dumps({"t_title": case["title"]}).encode(), headers={"Content-Type": "application/json", "Accept": "application/json"}, method="POST")
        status = None
        result = None
        error = None
        try:
            with urllib.request.urlopen(req, timeout=args.timeout) as response:
                status = response.status
                result = json.load(response)
        except urllib.error.HTTPError as exc:
            status = exc.code
            error = exc.read(1000).decode("utf-8", errors="replace")
        except (urllib.error.URLError, TimeoutError, ValueError, OSError) as exc:
            error = str(exc)
        elapsed_ms = round((time.monotonic() - started) * 1000, 1)
        problems = check(case, result) if status == 200 else ["http_or_network_error"]
        records.append({**case, "status": status, "elapsed_ms": elapsed_ms, "problems": problems, "error": error, "response": result})
        print(f"{index:3}/150 {case['kind']:6} HTTP {str(status):4} {elapsed_ms:8.1f} ms {','.join(problems) or 'ok'} {case['title']}", flush=True)
    latencies = [r["elapsed_ms"] for r in records if r["status"] == 200]
    summary = {"total": len(records), "by_kind": {kind: {"total": sum(r["kind"] == kind for r in records), "http_errors": sum(r["kind"] == kind and r["status"] != 200 for r in records), "no_match": sum(r["kind"] == kind and "no_match" in r["problems"] for r in records), "flagged": sum(r["kind"] == kind and bool(r["problems"]) for r in records)} for kind in ("movie", "series", "anime")}, "median_ms": round(statistics.median(latencies), 1) if latencies else None, "max_ms": max(latencies) if latencies else None}
    Path(args.out).write_text(json.dumps({"base_url": base, "summary": summary, "results": records}, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    print(f"Report: {args.out}")
    if any(r["status"] != 200 for r in records):
        raise SystemExit(1)

if __name__ == "__main__":
    main()
