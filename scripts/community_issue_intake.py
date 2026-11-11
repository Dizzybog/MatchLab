#!/usr/bin/env python3
"""Collect community reports from *all* recently active official daily issues.

GitHub's /issues API also returns pull requests; never use the first page as
an exhaustive issue list. Include today's comments and late comments to older
bot issues, and fail closed rather than silently truncate an oversized intake.
"""
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
from community_cleanup import BOT, GitHub, REPO_PATTERN, timestamp

WINDOW_DAYS = 60
MAX_ISSUES = 2000
MAX_COMMENTS = 5000
TITLE = re.compile(r"\[community-learning\] \d{4}-\d{2}-\d{2}\Z")


def official(row):
    return (isinstance(row, dict) and not row.get("pull_request") and
            (row.get("user") or {}).get("login") == BOT and
            isinstance(row.get("title"), str) and
            TITLE.fullmatch(row["title"]) is not None and
            type(row.get("number")) is int)


def collect(api, now):
    cutoff = now - timedelta(days=WINDOW_DAYS)
    issues = api.pages("/issues?state=all&sort=updated&direction=desc")
    eligible = [
        row for row in issues if official(row) and
        (stamp := timestamp(row.get("updated_at"))) and stamp >= cutoff
    ]
    if len(eligible) > MAX_ISSUES:
        raise RuntimeError("Too many community issues; refusing partial intake")
    result = []
    count = 0
    # Existing downstream consumers accept a list of page/list entries.
    for issue in eligible:
        if not issue.get("comments"):
            continue
        comments = api.pages(f"/issues/{issue['number']}/comments")
        selected = [
            entry for entry in comments if isinstance(entry, dict) and
            (stamp := timestamp(entry.get("created_at"))) and
            cutoff <= stamp <= now + timedelta(minutes=5)
        ]
        count += len(selected)
        if count > MAX_COMMENTS:
            raise RuntimeError("Too many comments; refusing partial intake")
        result.append(selected)
    print(f"community_issues_scanned={len(eligible)} comments={count}")
    return result


def main():
    import argparse
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args()
    repo, token = os.environ.get("GH_REPO", ""), os.environ.get("GH_TOKEN", "")
    if not REPO_PATTERN.fullmatch(repo) or not token:
        p.error("GH_REPO and GH_TOKEN required")
    rows = collect(GitHub(repo, token), datetime.now(timezone.utc))
    args.out.write_text(json.dumps(rows, ensure_ascii=False) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
