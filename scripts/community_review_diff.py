#!/usr/bin/env python3
"""Avoid meaningless community-learning PRs from JSON whitespace rewrites.

The rule JSON files are trusted review data, not executable regex. Compare
decoded JSON values, restore exact bytes for unchanged files, and gate PR
creation only when a candidate list genuinely changes.
"""
import json
import sys
from pathlib import Path

def semantic_change(before: Path, after: Path) -> bool:
    original=before.read_bytes()
    proposed=after.read_bytes()
    try:
        old=json.loads(original)
        new=json.loads(proposed)
    except (ValueError, UnicodeError) as error:
        raise ValueError(f"invalid review JSON: {error}") from error
    if old == new:
        # A Rust reviewer may serialize [] instead of [\\n]. Do not create
        # a PR for JSON formatting or accidently mark an empty proposal done.
        after.write_bytes(original)
        return False
    if not isinstance(old,list) or not isinstance(new,list):
        raise ValueError("community rule files must contain JSON arrays")
    return True

def check(rules_before: Path, rules_after: Path,
          extra_before: Path, extra_after: Path) -> bool:
    changed_rules=semantic_change(rules_before,rules_after)
    changed_extra=semantic_change(extra_before,extra_after)
    return changed_rules or changed_extra

def source_proven(before_handler: Path, after_handler: Path,
                  before_fixtures: Path, after_fixtures: Path) -> bool:
    """Only a tested native handler diff plus new trusted regressions opens PR."""
    if before_handler.read_bytes() == after_handler.read_bytes():
        return False
    try:
        old = json.loads(before_fixtures.read_text(encoding="utf8"))
        new = json.loads(after_fixtures.read_text(encoding="utf8"))
    except (UnicodeError, ValueError) as error:
        raise ValueError(f"invalid promoted regressions: {error}") from error
    if not isinstance(old, list) or not isinstance(new, list):
        raise ValueError("invalid promoted fixture schema")
    if new[:len(old)] != old or len(new) <= len(old):
        raise ValueError("source changed without verified regression additions")
    return True


def main() -> None:
    if len(sys.argv) not in (6, 10):
        raise SystemExit(
            "usage: community_review_diff.py old_rules new_rules old_proposals "
            "new_proposals github_env [handlers_before handlers_after "
            "fixtures_before fixtures_after]"
        )
    paths=[Path(p) for p in sys.argv[1:]]
    changed=check(*paths[:4])
    if len(paths)==9:
        # Preserve review-only suggestions in the durable backlog and daily
        # audit; an unproven JSON proposal must not create a no-op code PR.
        changed = (changed and source_proven(*paths[5:9]))
    with paths[4].open("a",encoding="utf8") as handle:
        handle.write("COMMUNITY_RULE_CHANGED="+str(changed).lower()+"\n")
    print("community_native_review="+("source_patch_verified" if changed else "no_proven_source_patch"))

if __name__=="__main__":
    main()
