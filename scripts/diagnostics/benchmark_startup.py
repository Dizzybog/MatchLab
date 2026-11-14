#!/usr/bin/env python3
"""Measure a local MatchLab child using deterministic synthetic SQLite fixtures.

Example (after cargo build --release -p matchlab-server):
  python3 scripts/diagnostics/benchmark_startup.py --output-dir /tmp/matchlab-startup
  python3 scripts/diagnostics/benchmark_startup.py --first-request match \
      --sizes 100000 --output-dir /tmp/matchlab-match-first

No public data is downloaded. Every metadata source points to a reserved,
non-listening loopback port. Existing fixtures carry the current bundled
override marker and a recent full_refresh timestamp. This is an index rebuild
and process restart benchmark, not an uncached-disk or full-catalog import test.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import socket
import sqlite3
import statistics
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
RELEASE = "Benchmark.Aurora.2020.1080p.WEB-DL.x264.mkv"
EXPECTED_SID = "tt9000001"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
SCHEMA = """
CREATE TABLE titles(imdb_id TEXT NOT NULL,title TEXT NOT NULL,clean_title TEXT NOT NULL,
    year INTEGER NOT NULL,category TEXT NOT NULL,region TEXT,
    original INTEGER NOT NULL DEFAULT 0,ordering INTEGER NOT NULL DEFAULT 0,
    type TEXT NOT NULL DEFAULT '',
    PRIMARY KEY(imdb_id,clean_title,year,region,ordering));
CREATE TABLE seasons(imdb_id TEXT PRIMARY KEY,max_season INTEGER NOT NULL);
CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER NOT NULL DEFAULT 0);
CREATE TABLE season_episodes(imdb_id TEXT NOT NULL,season INTEGER NOT NULL,
    episode_count INTEGER NOT NULL,max_episode INTEGER NOT NULL,PRIMARY KEY(imdb_id,season));
CREATE TABLE anime_ids(group_id TEXT NOT NULL,type TEXT NOT NULL,value TEXT NOT NULL,
    PRIMARY KEY(group_id,type,value));
CREATE TABLE anime_series_map(imdb_id TEXT NOT NULL,anidb_id TEXT NOT NULL,
    tvdb_id TEXT NOT NULL DEFAULT '',trakt_season INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(imdb_id,anidb_id,tvdb_id,trakt_season));
CREATE TABLE episode_map(anidb_id TEXT NOT NULL,tvdb_id TEXT NOT NULL,
    anidb_season INTEGER NOT NULL,tvdb_season INTEGER NOT NULL,start INTEGER NOT NULL,
    end INTEGER NOT NULL,offset INTEGER NOT NULL DEFAULT 0,map TEXT NOT NULL DEFAULT '');
CREATE TABLE anidb_titles(anidb_id TEXT NOT NULL,title TEXT NOT NULL,clean_title TEXT NOT NULL,
    title_type TEXT NOT NULL,language TEXT NOT NULL,
    PRIMARY KEY(anidb_id,clean_title,title_type,language));
CREATE TABLE metadata_state(source TEXT PRIMARY KEY,updated_at TEXT NOT NULL,row_count INTEGER NOT NULL);
CREATE INDEX anime_series_by_anidb ON anime_series_map(anidb_id);
CREATE INDEX episode_lookup_idx ON episode_map(anidb_id,anidb_season,start,end);
CREATE INDEX episode_by_tvdb ON episode_map(tvdb_id,tvdb_season,offset,start);
CREATE INDEX episode_by_anidb_order ON episode_map(anidb_id,tvdb_season,offset,start);
"""


def override_marker() -> str:
    value = 0xCBF29CE484222325
    for byte in (ROOT / "data/anime-list.override.xml").read_bytes():
        value = ((value ^ byte) * 0x100000001B3) & ((1 << 64) - 1)
    return f"animelists_override_{value:016x}"


def letters(number: int) -> str:
    result = ""
    while True:
        result = chr(ord("a") + number % 26) + result
        number = number // 26 - 1
        if number < 0:
            return result


def create_fixture(directory: Path, aliases: int) -> Path:
    directory.mkdir(parents=True)
    path = directory / "metadata.sqlite3"
    with sqlite3.connect(path) as db:
        db.executescript(SCHEMA)
        rows = ((f"tt{9_000_001 + index:07d}",
                 "Benchmark Aurora" if index == 0 else f"Benchmark Catalog {letters(index)}",
                 "benchmarkaurora" if index == 0 else f"benchmarkcatalog{letters(index)}",
                 2020 if index == 0 else 1980 + index % 46,
                 "movie", "", 1, 0, "movie") for index in range(aliases))
        db.executemany("INSERT INTO titles VALUES(?,?,?,?,?,?,?,?,?)", rows)
        stamp = str(int(time.time()))
        db.executemany("INSERT INTO metadata_state VALUES(?,?,?)", [
            ("imdb", stamp, aliases), ("full_refresh", stamp, aliases),
            (override_marker(), stamp, 0),
            ("tantivy_generation", f"v3-startup-benchmark-{aliases}", 0),
        ])
        db.execute("ANALYZE")
        if db.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise RuntimeError("generated SQLite fixture failed integrity_check")
    return path


def request(base: str, endpoint: str, payload: dict | None = None, timeout: float = 10) -> dict:
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(base + endpoint, data=data,
                                 headers={"Content-Type": "application/json"})
    started = time.perf_counter()
    try:
        with OPENER.open(req, timeout=timeout) as response:
            status, raw = response.status, response.read()
    except urllib.error.HTTPError as error:
        status, raw = error.code, error.read()
    try:
        body = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        body = raw.decode(errors="replace")
    return {"status": status, "duration_ms": (time.perf_counter() - started) * 1000,
            "body": body}


def wait_health(process: subprocess.Popen, base: str, endpoint: str,
                started: float, timeout: float, poll: float,
                stop: threading.Event | None = None) -> dict:
    deadline = started + timeout
    last = None
    while time.perf_counter() < deadline and not (stop and stop.is_set()):
        if process.poll() is not None:
            return {"error": f"child exited with status {process.returncode}", "last": last}
        try:
            last = request(base, endpoint, timeout=1)
            if last["status"] == 200:
                return {"spawn_to_ms": (time.perf_counter() - started) * 1000,
                        "response": last}
        except (OSError, urllib.error.URLError, TimeoutError):
            pass
        if stop:
            stop.wait(poll)
        else:
            time.sleep(poll)
    return {"spawn_to_ms": None, "last": last, "observation_seconds": timeout}


def rss_kib(pid: int) -> int | None:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return None


def distribution(values: list[float]) -> dict:
    ordered = sorted(values)
    return {"n": len(values), "min_ms": min(values), "median_ms": statistics.median(values),
            "p95_ms": ordered[max(0, (95 * len(values) + 99) // 100 - 1)], "max_ms": max(values)}


def index_state(metadata: Path) -> dict:
    root = Path(str(metadata) + ".tantivy")
    files = [path for path in root.rglob("*") if path.is_file()] if root.exists() else []
    return {"exists": root.exists(), "bytes": sum(path.stat().st_size for path in files),
            "complete_markers": {str(path.relative_to(root)): path.read_text().strip()
                                 for path in files if path.name == ".matchlab-complete"}}


def summarize(runs: list[dict]) -> list[dict]:
    result = []
    for scenario, aliases in sorted({(run["scenario"], run["imdb_alias_rows"]) for run in runs}):
        group = [run for run in runs if run["scenario"] == scenario and run["imdb_alias_rows"] == aliases]
        good = [run for run in group if "error" not in run]
        summary = {"scenario": scenario, "imdb_alias_rows": aliases,
                   "runs": len(group), "successful_runs": len(good)}
        for label, path in [("spawn_to_live", ("live", "spawn_to_ms")),
                            ("spawn_to_ready", ("ready", "spawn_to_ms")),
                            ("first_parse", ("first_parse", "duration_ms")),
                            ("first_match", ("first_match", "duration_ms")),
                            ("warm_parse_process_medians", ("warm_parse", "median_ms")),
                            ("warm_match_process_medians", ("warm_match", "median_ms"))]:
            values = [run.get(path[0], {}).get(path[1]) for run in good]
            values = [value for value in values if value is not None]
            if values:
                summary[label] = distribution(values)
        peaks = [run["peak_rss_kib"] for run in good if run.get("peak_rss_kib") is not None]
        if peaks:
            summary["peak_rss_kib"] = {"min": min(peaks), "median": statistics.median(peaks), "max": max(peaks)}
        result.append(summary)
    return result


def run_child(binary: Path, metadata: Path, run_dir: Path, dead_port: int,
              scenario: str, aliases: int, iteration: int, args: argparse.Namespace) -> dict:
    run_dir.mkdir(parents=True)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("MATCHLAB_")}
    environment.update({"MATCHLAB_DB": str(metadata),
                        "MATCHLAB_HISTORY_DB": str(run_dir / "history.sqlite3"),
                        "MATCHLAB_HOST": f"127.0.0.1:{port}",
                        "MATCHLAB_ML_SIZE_GB": "0", "MATCHLAB_STRICT_MODE": "true",
                        "MATCHLAB_MAX_CONCURRENT_MATCHES": "2", "RUST_LOG": "matchlab_server=info",
                        "MATCHLAB_METADATA_URLS": ",".join(
                            f"https://127.0.0.1:{dead_port}/{number}" for number in range(7))})
    for variable in ("NO_PROXY", "no_proxy"):
        inherited = environment.get(variable, "")
        environment[variable] = ",".join(filter(None, [inherited, "127.0.0.1", "localhost"]))
    log_path = run_dir / "server.log"
    result = {"scenario": scenario, "imdb_alias_rows": aliases, "iteration": iteration,
              "metadata": str(metadata), "metadata_bytes": metadata.stat().st_size if metadata.exists() else 0,
              "index_before": index_state(metadata), "log": str(log_path),
              "first_request": args.first_request, "application_request_order": [],
              "first_match_has_prior_parse_request": args.first_request == "parse"}
    stop = threading.Event()
    ready_result = {}
    peak_rss = []
    with log_path.open("w") as log:
        started = time.perf_counter()
        process = subprocess.Popen([str(binary)], cwd=ROOT, env=environment,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)

        def poll_ready() -> None:
            ready_result.update(wait_health(process, base, "/health/ready", started,
                                            args.missing_observe_seconds if aliases == 0 else args.timeout,
                                            args.poll_seconds, stop))

        def poll_memory() -> None:
            while not stop.is_set():
                value = rss_kib(process.pid)
                if value is not None:
                    peak_rss.append(value)
                stop.wait(0.005)

        def probe_parser() -> None:
            result["application_request_order"].append("first_parse")
            result["first_parse"] = request(base, "/parse", {"title": RELEASE})
            if result["first_parse"]["status"] != 200:
                raise RuntimeError(f"parser unavailable: {result['first_parse']}")
            result["first_parse"]["spawn_to_complete_ms"] = (time.perf_counter() - started) * 1000
            result["application_request_order"].append(f"warm_parse_{args.samples}")
            parses = [request(base, "/parse", {"title": RELEASE}) for _ in range(args.samples)]
            if any(sample["status"] != 200 or sample["body"] != result["first_parse"]["body"] for sample in parses):
                raise RuntimeError("a warm synthetic parse changed fields or status")
            result["warm_parse"] = distribution([sample["duration_ms"] for sample in parses])

        ready_thread = threading.Thread(target=poll_ready, daemon=True)
        memory_thread = threading.Thread(target=poll_memory, daemon=True)
        ready_thread.start()
        memory_thread.start()
        try:
            result["live"] = wait_health(process, base, "/health/live", started, args.timeout, args.poll_seconds)
            if result["live"].get("spawn_to_ms") is None:
                raise RuntimeError(f"local child did not become live: {result['live']}")
            if args.first_request == "parse":
                probe_parser()
            ready_thread.join(args.timeout + 2)
            result["ready"] = dict(ready_result)
            if aliases and result["ready"].get("spawn_to_ms") is None:
                raise RuntimeError(f"local child did not become ready: {result['ready']}")
            result["application_request_order"].append("first_match")
            result["first_match"] = request(base, "/match", {"title": RELEASE})
            result["first_match"]["spawn_to_complete_ms"] = (time.perf_counter() - started) * 1000
            if aliases:
                if result["first_match"]["status"] != 200 or result["first_match"]["body"].get("sid_start") != EXPECTED_SID:
                    raise RuntimeError(f"synthetic known match failed: {result['first_match']}")
                result["application_request_order"].append(f"warm_match_{args.samples}")
                samples = [request(base, "/match", {"title": RELEASE}) for _ in range(args.samples)]
                if any(sample["status"] != 200 or sample["body"].get("sid_start") != EXPECTED_SID for sample in samples):
                    raise RuntimeError("a warm synthetic match changed identity or status")
                result["warm_match"] = distribution([sample["duration_ms"] for sample in samples])
            elif result["first_match"]["status"] != 503:
                raise RuntimeError("missing metadata unexpectedly accepted matching")
            if args.first_request == "match":
                # Missing metadata returns 503 before entering the matcher;
                # only a successful match exercises a process-cold parser.
                if not aliases:
                    result["note"] = "The initial match returned 503 without initializing the parser."
                probe_parser()
            # Let independently scheduled startup stages flush their log lines.
            stop.wait(0.025)
        except Exception as error:
            result["error"] = str(error)
        finally:
            stop.set()
            ready_thread.join(2)
            memory_thread.join(2)
            value = rss_kib(process.pid)
            if value is not None:
                peak_rss.append(value)
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
    result["peak_rss_kib"] = max(peak_rss) if peak_rss else None
    result["index_after"] = index_state(metadata)
    result["startup_log"] = log_path.read_text(errors="replace").splitlines()
    if aliases and any("scheduled metadata refresh failed" in line or "metadata download started" in line
                       for line in result["startup_log"]):
        result["error"] = "fixture unexpectedly attempted a metadata refresh; benchmark invalid"
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/matchlab-server")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--sizes", nargs="+", type=int, default=[100, 10_000, 100_000])
    parser.add_argument("--restarts", type=int, default=5)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--first-request", choices=("parse", "match"), default="parse",
                        help="first application probe; match waits for readiness and runs before parser probes")
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--poll-seconds", type=float, default=0.01)
    parser.add_argument("--missing-observe-seconds", type=float, default=1)
    parser.add_argument("--fixtures-only", action="store_true", help="create/check fixtures without starting a child")
    args = parser.parse_args()
    if min(args.sizes) < 1 or args.restarts < 1 or args.samples < 1 or min(args.timeout, args.poll_seconds, args.missing_observe_seconds) <= 0:
        parser.error("sizes, restarts, samples and timeouts must be positive")
    binary = args.binary.resolve()
    if not args.fixtures_only and not binary.is_file():
        parser.error(f"build the release binary first: {binary}")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="startup-", dir=args.output_dir.resolve()))
    fixtures = [(size, create_fixture(work / f"catalog-{size}", size)) for size in args.sizes]
    report = {"format_version": 1, "created_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest() if binary.exists() else None,
              "platform": platform.platform(), "cpu_count": os.cpu_count(), "work_directory": str(work),
              "limitations": ["Synthetic uniformly distributed movie aliases; zero AniDB/episode rows.",
                              "OS page cache is not dropped; no-index means missing Tantivy index, not cold disk.",
                              "No public network import; missing-metadata sources fail at a loopback refused port.",
                              "Readiness is polled concurrently; discovery precision is limited by poll and request scheduling.",
                              "Peak RSS is Linux VmHWM sampled every 5ms through the request probes.",
                              "Same-title warm requests exercise caches, not distinct-title throughput.",
                              ("The first match runs after parser probes, so its parser cache is already warm."
                               if args.first_request == "parse" else
                               "Successful first matches run before parser probes; a missing-metadata 503 does not initialize the parser.")],
              "fixtures": [{"imdb_alias_rows": size, "unique_imdb_works": size,
                            "anidb_alias_rows": 0, "metadata": str(path),
                            "metadata_bytes": path.stat().st_size} for size, path in fixtures],
              "settings": {"restarts": args.restarts, "samples": args.samples, "poll_seconds": args.poll_seconds,
                           "first_request": args.first_request,
                           "ml_enabled": False, "strict": True, "max_concurrent_matches": 2,
                           "override_marker": override_marker()}, "runs": []}
    if not args.fixtures_only:
        # Keeping this port bound without listen() prevents another process
        # from accidentally turning a fail-fast source into a live endpoint.
        with socket.socket() as refused:
            refused.bind(("127.0.0.1", 0))
            dead_port = refused.getsockname()[1]
            report["runs"].append(run_child(binary, work / "missing" / "metadata.sqlite3",
                                            work / "runs" / "missing", dead_port, "missing_metadata", 0, 0, args))
            for size, metadata in fixtures:
                for iteration in range(args.restarts + 1):
                    scenario = "no_index" if iteration == 0 else "disk_index_restart"
                    run = run_child(binary, metadata, work / "runs" / f"{size}-{iteration}",
                                    dead_port, scenario, size, iteration, args)
                    report["runs"].append(run)
                    print(f"{scenario} aliases={size} iteration={iteration} ready_ms={run.get('ready', {}).get('spawn_to_ms')} error={run.get('error')}", flush=True)
                    if "error" in run:
                        break
    destination = args.output_dir.resolve() / "report.json"
    report["summary"] = summarize(report["runs"])
    destination.write_text(json.dumps(report, indent=2) + "\n")
    print(destination)
    return int(any("error" in run for run in report["runs"]))


if __name__ == "__main__":
    raise SystemExit(main())
