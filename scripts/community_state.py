#!/usr/bin/env python3
"""Versioned, crash-safe GitHub Release storage for Community Learning.

A Release is a container, not proof that an asset was uploaded. Every save
creates a *new* asset; old snapshots remain recoverable until confirmation.
No zero-byte files are uploaded, and malformed existing state fails closed.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

REPO = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
HEX = re.compile(r"[0-9a-f]{64}\Z")
SPECS = {
    "ledger": ("community-learning-ledger", "fingerprints", "txt"),
    "backlog": ("community-learning-backlog", "backlog", "json"),
    "decisions": ("community-learning-decisions", "decisions", "json"),
}
HEADER = "# matchlab-community-ledger-v1"
KEEP = 6
MAX_LEDGER = 100_000
MAX_PENDING = 20_000


def normalize(kind, raw):
    """Validate the full snapshot before any write or a later cleanup."""
    if kind == "ledger":
        try:
            text = raw.decode("utf-8")
        except UnicodeError as exc:
            raise ValueError("Invalid fingerprint encoding") from exc
        lines = text.splitlines()
        if lines and lines[0] == HEADER:
            lines = lines[1:]
        if any(not HEX.fullmatch(line) for line in lines):
            raise ValueError("Invalid fingerprint record; refusing to reset ledger")
        if len(lines) != len(set(lines)) or len(lines) > MAX_LEDGER:
            raise ValueError("Duplicate or oversized fingerprint ledger")
        return ("\n".join(sorted(lines)) + ("\n" if lines else "")).encode()
    try:
        data = json.loads(raw)
    except (UnicodeError, ValueError) as exc:
        raise ValueError("Invalid community state JSON") from exc
    if kind == "decisions":
        from community_decisions import validate
        validate(data)
        return (json.dumps(data, ensure_ascii=False, sort_keys=True,
                           separators=(",", ":")) + "\n").encode()
    if not isinstance(data, dict) or set(data) != {"pending"}:
        raise ValueError("Invalid backlog schema")
    pending = data["pending"]
    if not isinstance(pending, list) or len(pending) > MAX_PENDING:
        raise ValueError("Invalid or oversized backlog")
    seen = set()
    for entry in pending:
        if not isinstance(entry, dict) or not HEX.fullmatch(
                entry.get("fingerprint", "") if isinstance(
                    entry.get("fingerprint"), str) else ""):
            raise ValueError("Invalid pending candidate")
        if entry["fingerprint"] in seen:
            raise ValueError("Duplicate backlog fingerprint")
        seen.add(entry["fingerprint"])
    return (json.dumps(data, ensure_ascii=False, sort_keys=True,
                       separators=(",", ":")) + "\n").encode()


def stored_bytes(kind, normalized):
    if kind == "ledger" and not normalized:
        return (HEADER + "\n").encode()
    return normalized


def asset_name(kind, name):
    _, prefix, ext = SPECS[kind]
    return name == f"{prefix}.{ext}" or bool(
        re.fullmatch(rf"{prefix}-[0-9]+-[0-9]+-(?:prepared|final|ledger)\.{ext}", name)
    )


def snapshots(kind, release):
    assets = release.get("assets", [])
    if not isinstance(assets, list):
        raise ValueError("Invalid Release assets")
    chosen = [a for a in assets if isinstance(a, dict) and
              isinstance(a.get("name"), str) and asset_name(kind, a["name"])]
    # Asset IDs are unique and monotonically assigned by GitHub. This also
    # orders consecutive prepared/final uploads within the same second.
    return sorted(chosen, key=lambda a: int(a["id"]), reverse=True)


class Storage:
    def __init__(self, repo, token):
        if not REPO.fullmatch(repo) or not token:
            raise ValueError("GH_REPO and GH_TOKEN are required")
        self.repo = repo
        self.token = token

    def api(self, method, route, body=None, missing_ok=False):
        url = "https://api.github.com/repos/" + self.repo + route
        data = None if body is None else json.dumps(body).encode()
        req = Request(url, data=data, method=method, headers={
            "Authorization": "Bearer " + self.token,
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "matchlab-community-state",
            **({"Content-Type": "application/json"} if data is not None else {}),
        })
        try:
            with urlopen(req, timeout=30) as resp:
                raw = resp.read()
            return json.loads(raw) if raw else None
        except HTTPError as exc:
            if exc.code == 404 and missing_ok:
                return None
            raise RuntimeError(f"GitHub API {method} {route}: HTTP {exc.code}") from exc

    def release(self, kind):
        return self.api("GET", "/releases/tags/" + SPECS[kind][0], missing_ok=True)

    def gh(self, *args):
        env = {**os.environ, "GH_TOKEN": self.token, "GH_REPO": self.repo}
        subprocess.run(["gh", *args], env=env, check=True,
                       capture_output=True, text=True, timeout=90)

    def load(self, kind, path):
        release = self.release(kind)
        available = snapshots(kind, release) if release else []
        if not available:
            # The initial Release may have been created before a failed
            # first upload; this is not evidence that dedup ever existed.
            print(f"community_{kind}=bootstrap_empty (no Release asset)")
            raw = (b"" if kind == "ledger" else
                   b'{"decisions":{}}\n' if kind == "decisions"
                   else b'{"pending":[]}\n')
        else:
            asset = available[0]
            if not isinstance(asset.get("size"), int) or asset["size"] <= 0:
                raise ValueError("Latest community state asset is empty; refusing silent reset")
            with tempfile.TemporaryDirectory() as temp:
                self.gh("release", "download", SPECS[kind][0],
                        "--repo", self.repo, "--pattern", asset["name"],
                        "--dir", temp)
                source = Path(temp) / asset["name"]
                if not source.is_file():
                    raise RuntimeError("GitHub reported asset but download did not produce it")
                raw = source.read_bytes()
            print(f"community_{kind}=restored {asset['name']}")
        clean = normalize(kind, raw)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(clean)

    def save(self, kind, path, phase, run_id, attempt):
        if not re.fullmatch(r"[0-9]+", run_id) or not re.fullmatch(r"[0-9]+", attempt):
            raise ValueError("Numeric GitHub run ID and attempt required")
        # Missing input is a pipeline defect, never a request to reset state.
        clean = normalize(kind, path.read_bytes())
        release = self.release(kind)
        if not release:
            tag, _, _ = SPECS[kind]
            self.gh("release", "create", tag, "--repo", self.repo,
                    "--title", "Community learning persistent " + kind,
                    "--notes", "Versioned machine-owned review state; no active rules.",
                    "--latest=false")
        tag, prefix, ext = SPECS[kind]
        filename = f"{prefix}-{run_id}-{attempt}-{phase}.{ext}"
        with tempfile.TemporaryDirectory() as temp:
            source = Path(temp) / filename
            source.write_bytes(stored_bytes(kind, clean))
            self.gh("release", "upload", tag, str(source), "--repo", self.repo)
        # Verify before pruning. A failed upload must never remove the last
        # recoverable state, including the original legacy asset.
        verified = None
        for _ in range(3):
            release = self.release(kind)
            matches = [a for a in snapshots(kind, release)
                       if a["name"] == filename and a.get("size", 0) > 0]
            if matches:
                verified = matches[0]
                break
            time.sleep(1)
        if verified is None:
            raise RuntimeError("Uploaded community state was not verified by GitHub")
        assets = snapshots(kind, release)
        print(f"community_{kind}=persisted {filename} size={verified['size']}")
        # Cleanup is deliberately best-effort, after durable confirmation.
        for old in assets[KEEP:]:
            if old["id"] == verified["id"]:
                continue
            try:
                self.api("DELETE", "/releases/assets/" + str(old["id"]))
            except RuntimeError as exc:
                print(f"Warning: retained old {kind} snapshot: {exc}")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("command", choices=("load", "save"))
    p.add_argument("kind", choices=tuple(SPECS))
    p.add_argument("--path", required=True, type=Path)
    p.add_argument("--phase", choices=("prepared", "final", "ledger"),
                   default="ledger")
    args = p.parse_args()
    store = Storage(os.environ.get("GH_REPO", ""), os.environ.get("GH_TOKEN", ""))
    if args.command == "load":
        store.load(args.kind, args.path)
    else:
        store.save(args.kind, args.path, args.phase,
                   os.environ.get("GITHUB_RUN_ID", ""),
                   os.environ.get("GITHUB_RUN_ATTEMPT", "1"))


if __name__ == "__main__":
    main()
