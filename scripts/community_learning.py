#!/usr/bin/env python3
"""Strict GitHub-only Community Learning intake and shadow-model training.

Issue bodies are completely untrusted. This tool never executes content and
never turns self-reported predictions into ground-truth labels.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
from datetime import datetime, timedelta, timezone

MARKER = "<!-- matchlab-community-v1 -->"
CODES = {
    "matching_error", "invalid_selected_coordinate", "unusual_season_number",
    "unmatched_explicit_episode", "explicit_episode_mismatch",
    "parser_marker_disagreement", "unresolved_absolute_anime_episode",
    "reversed_selected_range",
}
PROTOCOL = 1
EPOCH = 1
MAX_ISSUES = 100
MAX_CANDIDATES = 100
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


def validate(issue, current_sha):
    """Only current, bounded, unverified structured reports enter review."""
    if not isinstance(issue, dict) or issue.get("pull_request"):
        return None
    if issue.get("title") != "[community-learning] parser anomaly":
        return None
    body = issue.get("body")
    if not isinstance(body, str) or not body.startswith(MARKER + "\n"):
        return None
    if len(body.encode("utf-8")) > 4096:
        return None
    try:
        obj = json.loads(body[len(MARKER) + 1:])
    except (ValueError, TypeError):
        return None
    if not isinstance(obj, dict) or (
        obj.get("protocol") != PROTOCOL or obj.get("learning_epoch") != EPOCH
        or obj.get("build_sha") != current_sha or obj.get("verified") is not False
        or obj.get("source") != "matchlab-community"
        or not allowed_title(obj.get("title"))
        or obj.get("status") not in ("matched", "unmatched", "error")
    ):
        return None
    alerts = obj.get("alerts")
    if not isinstance(alerts, list) or not (1 <= len(alerts) <= 8) or any(
        not isinstance(x, str) or x not in CODES for x in alerts
    ):
        return None
    parsed = obj.get("parsed")
    selected = obj.get("selected")
    if not isinstance(parsed, dict) or not isinstance(selected, dict):
        return None
    if parsed.get("title") is not None and not allowed_title(parsed["title"]):
        return None
    for name in ("seasons", "episodes"):
        nums = parsed.get(name)
        if not isinstance(nums, list) or len(nums) > 12 or any(
            type(n) is not int or not 0 <= n <= 99999 for n in nums
        ):
            return None
    imdb = selected.get("imdb_id")
    if imdb is not None and not (
        isinstance(imdb, str) and imdb.startswith("tt") and
        len(imdb) in (9, 10) and imdb[2:].isascii() and imdb[2:].isdigit()
    ):
        return None
    for name in ("season", "episode"):
        n = selected.get(name)
        if n is not None and (type(n) is not int or not 0 <= n <= 99999):
            return None
    # Enforce the minimal shape, not just the key values. Unknown fields
    # might contain paths, passwords or personal data.
    if set(obj) != {"protocol", "learning_epoch", "build_sha", "source",
                    "title", "status", "alerts", "parsed", "selected", "verified"}:
        return None
    if set(parsed) != {"title", "seasons", "episodes"} or set(selected) != {
        "imdb_id", "season", "episode"
    }:
        return None
    # Only structured, validated values from our allowlist are re-emitted.
    return obj


def intake(issues, sha, now):
    result = []
    seen = set()
    cutoff = now - timedelta(days=2)
    for issue in issues[:MAX_ISSUES]:
        created = issue.get("created_at") if isinstance(issue, dict) else None
        try:
            when = datetime.fromisoformat(created.replace("Z", "+00:00"))
        except (TypeError, ValueError, AttributeError):
            continue
        if when < cutoff or when > now + timedelta(minutes=5):
            continue
        report = validate(issue, sha)
        if report is None:
            continue
        signature = json.dumps([report["title"], report["status"],
            report["alerts"], report["parsed"], report["selected"]],
            sort_keys=True, ensure_ascii=False)
        key = hashlib.sha256(signature.encode()).hexdigest()
        if key in seen:
            continue
        seen.add(key)
        result.append({"issue":issue.get("number"),
            "key":key, "report":report, "verified":False})
        if len(result) >= MAX_CANDIDATES:
            break
    return result


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
    p.add_argument("--issues", required=True)
    p.add_argument("--verified", required=True)
    p.add_argument("--sha", required=True)
    p.add_argument("--review-output", required=True)
    p.add_argument("--model-output", required=True)
    args = p.parse_args()
    if len(args.sha) != 40 or not all(c in "0123456789abcdef" for c in args.sha.lower()):
        raise SystemExit("sha must be a full Git commit id")
    issues = json.loads(Path(args.issues).read_text(encoding="utf-8"))
    if not isinstance(issues, list):
        raise SystemExit("GitHub response must be a list")
    items = intake(issues, args.sha, datetime.now(timezone.utc))
    Path(args.review_output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.review_output).write_text(
        "\n".join(json.dumps(row, sort_keys=True, ensure_ascii=False)
                  for row in items) + ("\n" if items else ""), encoding="utf-8")
    verified = read_verified(args.verified, args.sha)
    model = train(verified, args.sha)
    Path(args.model_output).write_text(
        json.dumps(model or {"mode":"inactive","reason":
           "Insufficient independently reviewed current-build examples or failed holdout"},
           indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"review candidates: {len(items)}; current-build verified: {len(verified)}; shadow model: {'ready' if model else 'inactive'}")


if __name__ == "__main__":
    main()
