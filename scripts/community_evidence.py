#!/usr/bin/env python3
"""Build bounded, untrusted torrent-title *test inputs* for the Rust reviewer.

A shared Issue batch has reports and rules. Only associate explicit matching
release markers from the SAME authenticated-format batch; never mistake a
self-reported IMDb mapping or parsed value for an independent ground-truth
label. This file is runner-local and is not attached to public PRs/artifacts.
"""
import argparse
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import re
from community_learning import allowed_title, validate_comment
from community_rule_intake import parse_batch

MAX_PER_RULE = 12
WINDOW_DAYS = 60


def marker_regex(rule):
    kind = rule.get("kind")
    if kind in ("learned_field_regex", "learned_source_regex_patch",
                "learned_field_regex_review"):
        signal = rule.get("signal")
        if not isinstance(signal, str) or not re.fullmatch(r"[a-z0-9_.]{2,24}", signal):
            return None
        parts = [re.escape(p) for p in re.split(r"[_.]", signal) if p]
        if not parts:
            return None
        return re.compile(r"(?i)(?:^|[ ._-])(?:" +
                          r"[ ._-]*".join(parts) + r")(?:$|[ ._-])")
    if kind == "learned_general_regex_review":
        value = rule.get("regex", "")
        # This is a strict finite grammar. Do not compile untrusted regex
        # syntax even if the upstream validator is accidentally relaxed.
        prefix, suffix = r"(?i)(?:^|[ ._-])(", r")(?:$|[ ._-])"
        if not isinstance(value, str) or not (
                value.startswith(prefix) and value.endswith(suffix)):
            return None
        parts = value[len(prefix):-len(suffix)].split("[ ._-]+")
        if not 2 <= len(parts) <= 6 or any(
                not re.fullmatch(r"[A-Za-z0-9]{1,24}", part) for part in parts):
            return None
        return re.compile(r"(?i)(?:^|[ ._-])(?:" +
                          r"[ ._-]+".join(map(re.escape, parts)) +
                          r")(?:$|[ ._-])")
    if kind in ("learned_tv_regex", "learned_corrective_regex"):
        family = rule.get("family")
        patterns = {
            "sxe": r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e[0-9]{1,5}",
            "x": r"(?i)(?:^|[._ -])[0-9]{1,3}x[0-9]{1,5}",
            "words": r"(?i)season[._ -]+[0-9]{1,3}[._ -]+episode[._ -]+[0-9]{1,5}",
            "sxe_range": r"(?i)s[0-9]{1,3}[._ -]*e[0-9]{1,5}[._ -]*-",
        }
        return re.compile(patterns[family]) if family in patterns else None
    return None


def gather(comments, candidates, sha, now, validate_rules=parse_batch,
           validate_reports=validate_comment):
    wanted = {r["fingerprint"]: r for r in candidates if isinstance(r, dict)
              and isinstance(r.get("fingerprint"), str)}
    output = {fp: [] for fp in wanted}
    if not wanted:
        return output
    cutoff = now - timedelta(days=WINDOW_DAYS)
    for group in comments:
        for comment in (group if isinstance(group, list) else [group]):
            if not isinstance(comment, dict):
                continue
            try:
                created = datetime.fromisoformat(
                    comment["created_at"].replace("Z", "+00:00"))
            except (KeyError, TypeError, AttributeError, ValueError):
                continue
            if not cutoff <= created <= now + timedelta(minutes=5):
                continue
            rules = validate_rules(comment, sha)
            if not rules:
                continue
            matching = [r for r in rules if r["fingerprint"] in wanted]
            if not matching:
                continue
            reports = validate_reports(comment, sha)
            if not reports:
                continue
            for rule in matching:
                fp = rule["fingerprint"]
                matcher = marker_regex(rule)
                if matcher is None:
                    continue
                for report in reports:
                    title = report.get("title")
                    if (allowed_title(title) and
                            len(title.encode("utf-8")) <= 200 and
                            matcher.search(title) and
                            title not in output[fp] and
                            len(output[fp]) < MAX_PER_RULE):
                        output[fp].append(title)
    return output


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--comments", type=Path, required=True)
    p.add_argument("--candidates", type=Path, required=True)
    p.add_argument("--sha", required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args()
    comments = json.loads(args.comments.read_text(encoding="utf-8"))
    candidates = json.loads(args.candidates.read_text(encoding="utf-8"))
    if not isinstance(comments, list) or not isinstance(candidates, list):
        raise ValueError("Invalid issue intake or candidate format")
    result = gather(comments, candidates, args.sha,
                    datetime.now(timezone.utc))
    args.out.write_text(json.dumps(result, ensure_ascii=False) + "\n",
                        encoding="utf-8")
    print("community_actual_titles_selected=" + str(sum(map(len, result.values()))))


if __name__ == "__main__":
    main()
