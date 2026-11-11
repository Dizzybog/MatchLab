#!/usr/bin/env python3
"""Persist evidence-based review decisions without losing unmerged proposals.

A definitive/no-proof result is cached only for the EXACT canonical handler
catalogue and independently labeled fixture corpus. When either changes, the
same original GitHub proposal is eligible for a fresh Rust replay. No Issue
claim can become a ground-truth label through this cache.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re

HEX = re.compile(r"[0-9a-f]{64}\Z")
DECISIONS = {
    "review_only", "invalid_source", "no_effect", "regression",
    "needs_independent_evidence", "ready_for_source_diff",
}
DEFER = DECISIONS - {"ready_for_source_diff"}
MAX_DECISIONS = 20_000


def signature(handlers, fixtures, parser_src=None):
    left = Path(handlers).read_bytes()
    right = Path(fixtures).read_bytes()
    code = b""
    if parser_src:
        root = Path(parser_src)
        files = sorted(root.rglob("*.rs")) if root.is_dir() else [root]
        if not files or len(files) > 64:
            raise ValueError("Invalid native Rust parser source tree")
        code = b"".join((str(path.relative_to(root) if root.is_dir()
                        else path.name).encode() + b"\0" +
                        path.read_bytes() + b"\0") for path in files)
    return hashlib.sha256(left + b"\0" + right + b"\0" + code).hexdigest()


def validate(state):
    if not isinstance(state, dict) or set(state) != {"decisions"}:
        raise ValueError("Invalid durable community decision state")
    data = state["decisions"]
    if not isinstance(data, dict) or len(data) > MAX_DECISIONS:
        raise ValueError("Community decisions capacity or schema invalid")
    for fp, info in data.items():
        if not isinstance(fp, str) or HEX.fullmatch(fp) is None:
            raise ValueError("Invalid decision fingerprint")
        if (not isinstance(info, dict) or set(info) !=
            {"status", "signature", "checked_at"} or
            info["status"] not in DECISIONS or
            not isinstance(info["signature"], str) or
            HEX.fullmatch(info["signature"]) is None or
            not isinstance(info["checked_at"], str) or
            len(info["checked_at"]) > 48):
            raise ValueError("Invalid durable decision record")
    return state


def due(pending, state, current_signature, limit=24):
    validate(state)
    if not isinstance(pending, list):
        raise ValueError("Invalid pending candidate queue")
    selected = []
    deferred = 0
    for proposal in pending:
        fp = proposal.get("fingerprint") if isinstance(proposal, dict) else None
        if not isinstance(fp, str) or HEX.fullmatch(fp) is None:
            raise ValueError("Invalid pending candidate fingerprint")
        last = state["decisions"].get(fp)
        if (last and last["signature"] == current_signature and
                last["status"] in DEFER):
            deferred += 1
            continue
        if len(selected) < limit:
            selected.append(proposal)
    return selected, deferred


def update(state, audit, pending, current_signature, now):
    validate(state)
    if not isinstance(audit, list) or not isinstance(pending, list):
        raise ValueError("Invalid audited results or pending backlog")
    allowed = set()
    for row in pending:
        fp = row.get("fingerprint") if isinstance(row, dict) else None
        if not isinstance(fp, str) or HEX.fullmatch(fp) is None:
            raise ValueError("Invalid pending fingerprint")
        allowed.add(fp)
    results = {fp: record for fp, record in state["decisions"].items()
               if fp in allowed}
    visited = set()
    for item in audit:
        if not isinstance(item, dict):
            raise ValueError("Invalid native auditor result")
        fp, status = item.get("fingerprint"), item.get("status")
        if not isinstance(fp, str) or HEX.fullmatch(fp) is None or fp in visited:
            raise ValueError("Duplicate or invalid decision result")
        visited.add(fp)
        if status not in DECISIONS:
            raise ValueError("Unknown audit decision status")
        if fp in allowed:
            results[fp] = {"status": status, "signature": current_signature,
                           "checked_at": now}
    return validate({"decisions": results})


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--existing", required=True, type=Path)
    p.add_argument("--audit", required=True, type=Path)
    p.add_argument("--backlog", required=True, type=Path)
    p.add_argument("--handlers", required=True, type=Path)
    p.add_argument("--fixtures", required=True, type=Path)
    p.add_argument("--parser-src", type=Path)
    p.add_argument("--out", required=True, type=Path)
    args = p.parse_args()
    state = json.loads(args.existing.read_text(encoding="utf-8"))
    audit = json.loads(args.audit.read_text(encoding="utf-8"))
    pending = json.loads(args.backlog.read_text(encoding="utf-8"))["pending"]
    now = datetime.now(timezone.utc).isoformat()
    updated = update(state, audit, pending, signature(
        args.handlers, args.fixtures, args.parser_src), now)
    args.out.write_text(json.dumps(updated, sort_keys=True, indent=2) + "\n",
                        encoding="utf-8")
    print("community_decisions_saved=" + str(len(updated["decisions"])))


if __name__ == "__main__":
    main()
