#!/usr/bin/env python3
"""Untrusted community batch intake; current-parser replay; shadow-only ML.

Following active-learning principles, a suspected anomaly is NOT a label.
Older builds may contribute hypotheses if their SHA is an ancestor of main.
Evidence is independently re-evaluated with the current parser before review.
"""
import argparse
import hashlib
import json
import math
import re
import subprocess
from pathlib import Path
from datetime import datetime, timedelta, timezone

MARKER = "<!-- matchlab-community-batch-v2 -->"
PROTOCOL = 2
EPOCH = 1
MAX_COMMENTS = 500
MAX_CANDIDATES = 200
MAX_LEDGER = 100_000
MAX_BATCH = 16
CODES = {
    "matching_error", "invalid_selected_coordinate", "unusual_season_number",
    "unmatched_explicit_episode", "explicit_episode_mismatch",
    "parser_marker_disagreement", "unresolved_absolute_anime_episode",
    "reversed_selected_range",
}
FEATURES = sorted({"status:matched", "status:unmatched", "status:error",
                   "has_episode", "has_season", "selected_episode", "selected_season"}
                  | {"alert:" + code for code in CODES})


def allowed_title(title):
    if not isinstance(title, str) or not (1 <= len(title.encode("utf-8")) <= 240):
        return False
    if any(c in title for c in "/\\@\r\n\0") or any(ord(c) < 32 for c in title):
        return False
    text = title.lower()
    return not any(k in text for k in ("http:", "https:", "www.", "magnet:",
             "token=", "apikey=", "password=", "secret=", "bearer ",
             "private=", "api_key="))


def normalized(title):
    return " ".join("".join(c if c.isalnum() else " " for c in title.lower()).split())


def fingerprint(title, alerts):
    return hashlib.sha256((normalized(title) + "\n" + ",".join(sorted(set(alerts))))
                          .encode("utf-8")).hexdigest()


def valid_sha(sha):
    return isinstance(sha, str) and len(sha) == 40 and all(
        c in "0123456789abcdef" for c in sha
    )


def git_ancestor(source_sha, current_sha):
    if not valid_sha(source_sha) or not valid_sha(current_sha):
        return False
    try:
        result = subprocess.run(["git", "merge-base", "--is-ancestor",
                                 source_sha, current_sha], check=False,
                                timeout=3, capture_output=True)
        return result.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def validate_report(row, sha):
    if not isinstance(row, dict) or set(row) != {
        "protocol", "learning_epoch", "build_sha", "source",
        "fingerprint", "title", "status", "alerts", "parsed", "selected",
        "verified"
    }:
        return None
    if (row["protocol"] != PROTOCOL or row["learning_epoch"] != EPOCH
        or row["build_sha"] != sha or row["source"] != "matchlab-community"
        or row["verified"] is not False or not allowed_title(row["title"])
        or row["status"] not in ("matched", "unmatched", "error")):
        return None
    codes = row["alerts"]
    if not isinstance(codes, list) or not (1 <= len(codes) <= 8) or any(
        not isinstance(c, str) or c not in CODES for c in codes
    ):
        return None
    if not isinstance(row["parsed"], dict) or set(row["parsed"]) != {
        "title", "seasons", "episodes"
    }:
        return None
    if not isinstance(row["selected"], dict) or set(row["selected"]) != {
        "imdb_id", "season", "episode"
    }:
        return None
    if row["parsed"]["title"] is not None and not allowed_title(
        row["parsed"]["title"]
    ):
        return None
    for key in ("seasons", "episodes"):
        numbers = row["parsed"][key]
        if not isinstance(numbers, list) or len(numbers) > 12 or any(
            type(n) is not int or not 0 <= n <= 99999 for n in numbers
        ):
            return None
    imdb = row["selected"]["imdb_id"]
    if imdb is not None and not (
        isinstance(imdb, str) and re.fullmatch(r"tt[0-9]{7,8}", imdb)
    ):
        return None
    for field in ("season", "episode"):
        number = row["selected"][field]
        if number is not None and (type(number) is not int or
                                   not 0 <= number <= 99999):
            return None
    digest = row["fingerprint"]
    if not isinstance(digest, str) or digest != fingerprint(
        row["title"], codes
    ):
        return None
    return row


def validate_comment(comment, current_sha, is_ancestor=git_ancestor):
    if not isinstance(comment, dict):
        return None
    text = comment.get("body")
    if not isinstance(text, str) or len(text.encode("utf-8")) > 16384 or not (
        text.startswith(MARKER + "\n")
    ):
        return None
    try:
        batch = json.loads(text[len(MARKER) + 1:])
    except (ValueError, TypeError):
        return None
    if not isinstance(batch, dict) or set(batch) != {
        "protocol", "learning_epoch", "build_sha", "source",
        "batch_id", "created_unix", "verified", "reports"
    }:
        return None
    sha = batch["build_sha"]
    if (batch["protocol"] != PROTOCOL or batch["learning_epoch"] != EPOCH
        or batch["source"] != "matchlab-community-batch"
        or batch["verified"] is not False or not valid_sha(sha)
        or not is_ancestor(sha, current_sha)
        or type(batch["created_unix"]) is not int
        or not isinstance(batch["reports"], list)
        or not 1 <= len(batch["reports"]) <= MAX_BATCH):
        return None
    reports = []
    for row in batch["reports"]:
        clean = validate_report(row, sha)
        if clean is None:
            return None
        reports.append(clean)
    fingerprints = sorted(row["fingerprint"] for row in reports)
    if len(fingerprints) != len(set(fingerprints)):
        return None
    expected = hashlib.sha256(
        (sha + "\n" + str(EPOCH) + "\n" +
         "\n".join(fingerprints)).encode("utf-8")
    ).hexdigest()
    if batch["batch_id"] != expected:
        return None
    return reports


def replay_current(title, parser_bin):
    try:
        result = subprocess.run([parser_bin], input=json.dumps(title) + "\n",
                                text=True, capture_output=True, timeout=5,
                                check=False)
        if result.returncode:
            return None
        obj = json.loads(result.stdout.strip())
        return obj.get("parsed") if obj.get("ok") is True else None
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return None


def explicit_coordinates(title):
    match = re.search(r"(?<![A-Za-z0-9])S([0-9]{1,3})E([0-9]{1,4})(?![A-Za-z0-9])",
                      title, re.I)
    if match:
        return int(match[1]), int(match[2])
    match = re.search(r"(?<![A-Za-z0-9])cap[. :_-]*([0-9]{3,4})(?![A-Za-z0-9])",
                      title, re.I)
    if match:
        return int(match[1]) // 100, int(match[1]) % 100
    return None


def current_status(row, current):
    """Only classify *provably* corrected parser-marker disagreements.
    IMDb identity/ASID complaints need current metadata; do not guess."""
    if current is None:
        return "replay_unavailable"
    if "parser_marker_disagreement" in row["alerts"]:
        marker = explicit_coordinates(row["title"])
        if marker is not None:
            seasons = current.get("seasons") or []
            episodes = current.get("episodes") or []
            if marker[0] in seasons and marker[1] in episodes:
                if row["alerts"] == ["parser_marker_disagreement"]:
                    return "resolved_in_current_parser"
                return "other_alerts_need_metadata_review"
            return "still_present_in_current_parser"
    return "needs_current_metadata_review"


def flatten_comments(payload):
    if not isinstance(payload, list):
        return []
    return [item for batch in payload for item in (
        batch if isinstance(batch, list) else [batch]
    ) if isinstance(item, dict)][:MAX_COMMENTS]


def intake(comments, current_sha, now, seen, parser_bin,
           is_ancestor=git_ancestor, replay=replay_current):
    output = []
    added = set()
    seen_all = set(seen)
    cutoff = now - timedelta(days=2)
    for comment in flatten_comments(comments):
        created = comment.get("created_at")
        try:
            when = datetime.fromisoformat(created.replace("Z", "+00:00"))
        except (TypeError, ValueError, AttributeError):
            continue
        if when < cutoff or when > now + timedelta(minutes=5):
            continue
        batch = validate_comment(comment, current_sha, is_ancestor)
        if batch is None:
            continue
        for row in batch:
            key = row["fingerprint"]
            if key in seen_all or key in added:
                continue
            current = replay(row["title"], parser_bin)
            status = current_status(row, current)
            if status == "resolved_in_current_parser":
                # A known parser problem was already fixed. Do not learn it.
                continue
            if len(seen_all) + len(added) >= MAX_LEDGER:
                raise RuntimeError("Community dedup ledger reached its hard limit")
            added.add(key)
            output.append({
                "fingerprint":key,"status":status,"report":row,
                "current_parse":{
                    "title": current.get("title"),
                    "seasons": current.get("seasons"),
                    "episodes": current.get("episodes")
                } if isinstance(current, dict) else None,
                "verified":False
            })
            if len(output) >= MAX_CANDIDATES:
                break
        if len(output) >= MAX_CANDIDATES:
            break
    return output, seen_all | added


def features(item):
    codes = item.get("alerts") or []
    parsed = item.get("parsed") or {}
    selected = item.get("selected") or {}
    return {k for k, truth in {
        "status:" + item.get("status", ""): True,
        "has_episode": bool(parsed.get("episodes")),
        "has_season": bool(parsed.get("seasons")),
        "selected_episode": selected.get("episode") is not None,
        "selected_season": selected.get("season") is not None,
        **{"alert:" + a: True for a in codes if a in CODES},
    }.items() if truth and k in FEATURES}


def read_verified(path, sha):
    if not Path(path).exists():
        return []
    rows = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        item = json.loads(line)
        # Verified labels are scoped to a fixed validated executable SHA.
        # Previously verified examples remain for regression review but are
        # not silently carried across unvalidated parser revisions.
        if item.get("protocol") != PROTOCOL or item.get("learning_epoch") != EPOCH:
            continue
        if item.get("validated_on_sha") != sha:
            continue
        if item.get("verified") is not True or type(item.get("label")) is not bool:
            continue
        if not allowed_title(item.get("title")):
            continue
        if item.get("status") not in ("matched", "unmatched", "error"):
            continue
        if not isinstance(item.get("evidence"), str) or len(item["evidence"]) < 8:
            continue
        if not isinstance(item.get("alerts"), list) or any(
            a not in CODES for a in item["alerts"]
        ):
            continue
        rows.append(item)
    return rows


def fit(items):
    count = [0, 0]
    hit = [{feature: 0 for feature in FEATURES} for _ in range(2)]
    for item in items:
        index = int(item["label"])
        count[index] += 1
        for feat in features(item):
            hit[index][feat] += 1
    if min(count) < 10:
        return None
    return {
        "class_count": count,
        "weights": [
            {k: (v + 1) / (count[i] + 2) for k, v in hit[i].items()}
            for i in range(2)
        ],
    }


def predict(model, item):
    present = features(item)
    counts = model["class_count"]
    total = sum(counts)
    scores = []
    for cls in (0, 1):
        score = math.log(counts[cls] / total)
        for key, prob in model["weights"][cls].items():
            score += math.log(prob if key in present else (1 - prob))
        scores.append(score)
    return scores[1] > scores[0]


def train(items, sha):
    # Stable deterministic split prevents re-running one model repeatedly
    # against a cherry-picked test set. Holdout must include both classes.
    train_rows, test_rows = [], []
    for item in items:
        digest = hashlib.sha256(item["title"].encode()).digest()
        (test_rows if digest[0] < 52 else train_rows).append(item)
    if len(items) < 60 or len(test_rows) < 10:
        return None
    model = fit(train_rows)
    if model is None:
        return None
    true_pos = false_pos = true_neg = false_neg = 0
    for item in test_rows:
        actual = item["label"]
        guessed = predict(model, item)
        true_pos += int(guessed and actual)
        false_pos += int(guessed and not actual)
        true_neg += int(not guessed and not actual)
        false_neg += int(not guessed and actual)
    if (true_pos + false_neg < 3 or false_pos + true_neg < 3):
        return None
    precision = true_pos / max(1, true_pos + false_pos)
    accuracy = (true_pos + true_neg) / len(test_rows)
    if precision < .80 or accuracy < .70:
        return None
    return {
        "model_kind":"bernoulli_naive_bayes", "mode":"shadow_only",
        "protocol":PROTOCOL, "learning_epoch":EPOCH,
        "validated_on_sha":sha, "training_rows":len(train_rows),
        "holdout_rows":len(test_rows),
        "holdout_precision":round(precision,4),
        "holdout_accuracy":round(accuracy,4),
        **model,
    }



def main():
    p = argparse.ArgumentParser()
    p.add_argument("--comments", required=True)
    p.add_argument("--verified", required=True)
    p.add_argument("--sha", required=True)
    p.add_argument("--parser-bin", required=True)
    p.add_argument("--seen", required=True)
    p.add_argument("--seen-output", required=True)
    p.add_argument("--review-output", required=True)
    p.add_argument("--model-output", required=True)
    args = p.parse_args()
    if not valid_sha(args.sha):
        raise SystemExit("sha must be a full Git commit id")
    comments = json.loads(Path(args.comments).read_text(encoding="utf-8"))
    ledger = Path(args.seen)
    seen = set(ledger.read_text(encoding="utf-8").splitlines()) if ledger.exists() else set()
    if len(seen) > MAX_LEDGER or any(not re.fullmatch("[0-9a-f]{64}", s) for s in seen):
        raise SystemExit("Invalid community learning ledger; fail closed")
    reviewed, seen_new = intake(comments, args.sha,
        datetime.now(timezone.utc), seen, args.parser_bin)
    Path(args.review_output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.review_output).write_text(
        "\n".join(json.dumps(row, sort_keys=True, ensure_ascii=False)
                  for row in reviewed) + ("\n" if reviewed else ""), encoding="utf-8")
    Path(args.seen_output).write_text(
        "\n".join(sorted(seen_new)) + ("\n" if seen_new else ""),encoding="utf-8")
    verified = read_verified(args.verified, args.sha)
    model = train(verified, args.sha)
    Path(args.model_output).write_text(json.dumps(
        model or {"mode":"inactive","reason":"Insufficient current-source independently verified labels or holdout validation failed"},
        indent=2,sort_keys=True) + "\n",encoding="utf-8")
    print(f"new unresolved candidate fingerprints: {len(reviewed)}; verified labels: {len(verified)}; shadow model: {'ready' if model else 'inactive'}")


if __name__ == "__main__":
    main()
