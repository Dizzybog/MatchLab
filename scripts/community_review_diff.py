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

def main() -> None:
    if len(sys.argv)!=6:
        raise SystemExit(
            "usage: community_review_diff.py old_rules new_rules old_proposals "
            "new_proposals github_env"
        )
    paths=[Path(p) for p in sys.argv[1:]]
    changed=check(*paths[:4])
    with paths[4].open("a",encoding="utf8") as handle:
        handle.write("COMMUNITY_RULE_CHANGED="+str(changed).lower()+"\n")
    print("community_semantic_review="+("changed" if changed else "unchanged"))

if __name__=="__main__":
    main()
