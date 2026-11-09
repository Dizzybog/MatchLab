#!/usr/bin/env python3
"""GitHub Actions-only reviewer for ML-generated field and source regex changes.

Untrusted comments become review-only data, never executable Python/regex/
GitHub Actions code. Runtime rules require independent source verification,
regression tests and human approval before the production parser is changed.
"""
import json
import sys
from pathlib import Path
from community_rule_intake import valid_extended_rule,valid_rule

ALLOWED={"learned_field_regex","learned_tv_regex","learned_source_regex_patch",
    "learned_field_regex_review","verified_regex_proposal"}
MAX_QUEUE=8192

def screen(incoming,old):
    if not isinstance(incoming,list) or not isinstance(old,list) or len(old)>MAX_QUEUE:
        raise ValueError("invalid review candidates")
    fingerprints={item.get("fingerprint") for item in old if isinstance(item,dict)}
    queued=list(old)
    changed=False
    for candidate in incoming[:24]:
        if not isinstance(candidate,dict) or candidate.get("kind") not in ALLOWED:
            continue
        source=candidate.get("source_build","")
        if not isinstance(source,str) or ":" not in source:continue
        sha=source.split(":",1)[0]
        valid=(valid_rule(candidate,sha) if
            candidate["kind"]=="verified_regex_proposal"
            else valid_extended_rule(candidate,sha))
        if valid is None or candidate["fingerprint"] in fingerprints:continue
        if len(queued)>=MAX_QUEUE:
            raise ValueError("community review queue full; do not acknowledge unseen rules")
        # A full daily batch can contain multiple independent regexes. Every
        # accepted rule remains review-only and is persisted in the PR, even
        # if the curated anime gate admits only one rule today.
        proposal={
            "fingerprint":candidate["fingerprint"],
            "status":"review_required",
            "automatic_activation":False,
            "source_sha":sha,
            "proposal":candidate,
            "review_checks":[
                "confirm independent original-parser replay with true labels",
                "check false positives, titles with numbers and episode packs",
                "test parser/matcher precision and throughput on both CPUs",
                "approve exact source diff before enabling in production",
            ],
        }
        queued.append(proposal)
        fingerprints.add(candidate["fingerprint"])
        changed=True
    return queued,changed

def main():
    if len(sys.argv)!=4:
        raise SystemExit("usage: review_community_regex.py incoming.json existing.json output.json")
    incoming=json.loads(Path(sys.argv[1]).read_text(encoding="utf8"))
    existing=json.loads(Path(sys.argv[2]).read_text(encoding="utf8"))
    accepted,changed=screen(incoming,existing)
    Path(sys.argv[3]).write_text(json.dumps(accepted,indent=2,ensure_ascii=False)+"\n",encoding="utf8")
    print("community_extended_regex="+("human_review_pr" if changed else "none"))

if __name__=="__main__":
    main()
