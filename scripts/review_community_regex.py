#!/usr/bin/env python3
"""GitHub Actions-only reviewer for ML-generated field and source regex changes.

Untrusted comments become review-only data, never executable Python/regex/
GitHub Actions code. Runtime rules require independent source verification,
regression tests and human approval before the production parser is changed.
"""
import json
import sys
from pathlib import Path
from community_rule_intake import valid_extended_rule, valid_rule

ALLOWED={"learned_field_regex","learned_tv_regex","learned_source_regex_patch","learned_corrective_regex","learned_general_regex_review",
    "learned_field_regex_review","historical_regex_review",
    "verified_regex_proposal"}
MAX_QUEUE=4096

def screen(incoming,old,audits=None):
    if not isinstance(incoming,list) or not isinstance(old,list) or len(old)>MAX_QUEUE:
        raise ValueError("invalid review candidates")
    fingerprints={item.get("fingerprint") for item in old if isinstance(item,dict)}
    audit_by_fp={r.get("fingerprint"):r for r in (audits or [])
        if isinstance(r,dict)}
    for candidate in incoming[:24]:
        if not isinstance(candidate,dict) or candidate.get("kind") not in ALLOWED:
            continue
        source=candidate.get("source_build","")
        if not isinstance(source,str) or ":" not in source:continue
        sha=source.split(":",1)[0]
        verified=(valid_rule(candidate,sha) if candidate.get("kind")==
            "verified_regex_proposal" else valid_extended_rule(candidate,sha))
        if verified is None:continue
        if candidate["fingerprint"] in fingerprints:continue
        audit=audit_by_fp.get(candidate["fingerprint"],{})
        # In production the trusted Rust auditor is mandatory. Prefer an
        # actually verified source diff over earlier review-only proposals.
        # Other kinds remain in the persistent queue and diagnostic artifact,
        # but cannot occupy the one-per-run code PR slot.
        if audits is not None and not (
            audit.get("status") == "ready_for_source_diff"
            and audit.get("promotion_eligible") is True
            and audit.get("regressions") == 0
            and audit.get("verified_improvements", 0) >= 2
            and audit.get("distinct_verified_titles", 0) >= 2
            and bool(audit.get("source_changes") or
                     candidate.get("kind")=="learned_source_regex_patch")
        ):
            continue
        # Proven no-ops and conflicts do not deserve another PR. They stay
        # visible in the daily audit artifact and original Issue comment.
        if audit.get("status") in {"no_effect","invalid_source","regression"}:
            continue
        # Self-reported evidence is never independent attestation.
        # Propose precisely the source regex change, but DO NOT load the
        # untrusted comment as an active handler or execute its pattern.
        proposal={
            "fingerprint":candidate["fingerprint"],
            "status":"review_required",
            "automatic_activation":False,
            "source_sha":sha,
            "proposal":candidate,
            "audit_status":audit.get("status","not_audited"),
            "audit_reason":audit.get("reason","source and field validation required"),
            "review_checks":[
                "confirm independent original-parser replay with true labels",
                "check false positives, titles with numbers and episode packs",
                "test parser/matcher precision and throughput on both CPUs",
                "approve exact source diff before enabling in production",
            ],
        }
        return old+[proposal],True
    return old,False

def main():
    if len(sys.argv) not in (4,5):
        raise SystemExit("usage: review_community_regex.py incoming.json existing.json output.json [audit.json]")
    incoming=json.loads(Path(sys.argv[1]).read_text(encoding="utf8"))
    existing=json.loads(Path(sys.argv[2]).read_text(encoding="utf8"))
    audits=json.loads(Path(sys.argv[4]).read_text(encoding="utf8")) if len(sys.argv)==5 else None
    accepted,changed=screen(incoming,existing,audits)
    Path(sys.argv[3]).write_text(json.dumps(accepted,indent=2,ensure_ascii=False)+"\n",encoding="utf8")
    print("community_extended_regex="+("human_review_pr" if changed else "none"))

if __name__=="__main__":
    main()
