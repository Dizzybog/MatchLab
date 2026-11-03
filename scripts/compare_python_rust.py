#!/usr/bin/env python3
"""Compare Python and Rust matching APIs using the same title corpus.

Both services must point at the same fully initialized SQLite metadata snapshot.
No third-party dependencies. Exit nonzero for unexpected mismatches or HTTP errors.
"""
import argparse
import json
import sys
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path


def call(base, title, timeout):
    payload = json.dumps({"t_title": title}).encode()
    req = urllib.request.Request(
        base.rstrip("/") + "/match", data=payload,
        headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)


def identity(value):
    if not isinstance(value, dict):
        return None
    anime = value.get("anime")
    if not isinstance(anime, dict):
        anime = {}
    return {
        "imdb_id": value.get("imdb_id"),
        "season": value.get("season"),
        "episode": value.get("episode"),
        "sid": value.get("sid"),
        "anidb_id": anime.get("anidb_id"),
        "anidb_episode": anime.get("episode"),
        "asid": anime.get("asid"),
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("corpus", type=Path, help="UTF-8 file: one torrent title per line")
    p.add_argument("--python", default="http://127.0.0.1:8765")
    p.add_argument("--rust", default="http://127.0.0.1:8766")
    p.add_argument("--timeout", type=float, default=15)
    p.add_argument("--output", type=Path, default=Path("parity-report.json"))
    p.add_argument("--strict", action="store_true",
                   help="Fail if any identity differs (default reports differences)")
    args = p.parse_args()
    titles = [line.strip() for line in args.corpus.read_text(encoding="utf-8").splitlines()
              if line.strip() and not line.lstrip().startswith("#")]
    if not titles:
        p.error("Corpus has no titles")
    results = []
    counts = Counter()
    for title in titles:
        entry = {"title": title}
        for name, base in (("python", args.python), ("rust", args.rust)):
            try:
                entry[name] = identity(call(base, title, args.timeout))
            except (urllib.error.URLError, TimeoutError, ValueError, OSError) as exc:
                entry[name + "_error"] = str(exc)
        if "python_error" in entry or "rust_error" in entry:
            status = "error"
        elif entry["python"] == entry["rust"]:
            status = "match"
        elif entry["python"] is None or entry["rust"] is None:
            status = "missing"
        else:
            status = "different"
        entry["status"] = status
        counts[status] += 1
        results.append(entry)
        print(f"{status:9} {title}", flush=True)
    report = {"total": len(titles), "counts": dict(counts), "results": results}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"Report: {args.output} | {dict(counts)}")
    if counts["error"] or (args.strict and (counts["different"] or counts["missing"])):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
