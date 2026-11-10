#!/usr/bin/env python3
"""Safely merge a verified regex alternative into its ORIGINAL native handler.

Used only by trusted GitHub Actions after:
  (a) schema/source-ancestry validation of the community proposal;
  (b) the Rust auditor's full-parser replay;
  (c) >=2 *maintainer-owned* positive label fixtures and 0 regressions.

This only stages a human-review PR. It never merges a GitHub PR itself.
Historical/unlabeled/ambiguous/community-claimed examples cannot authorize edits.
"""
import argparse
import copy
import json
import re
from pathlib import Path

from community_rule_intake import technical_pattern

MAX_PROMOTED=1000
ENTRY=re.compile(r"(?ms)^    \{\n.*?^    \}(?=,?\n)")
PATTERN=re.compile(r'(?m)^(      "pattern": )("(?:\\.|[^"\\])*")(?=,?$)')


def update_handler_bytes(text,index,old,new):
    """Modify only one escaped JSON pattern value; never reformat 150KB catalogue."""
    original=json.loads(text)
    handlers=original["handlers"]
    if not (0<=index<len(handlers)) or handlers[index].get("pattern")!=old:
        raise ValueError("upstream handler diverged")
    spans=list(ENTRY.finditer(text))
    if len(spans)!=len(handlers):
        raise ValueError("unexpected generated catalogue formatting")
    source=spans[index]
    block=source.group()
    matches=list(PATTERN.finditer(block))
    if len(matches)!=1 or json.loads(matches[0].group(2))!=old:
        raise ValueError("cannot isolate original source pattern")
    match=matches[0]
    updated=block[:match.start(2)]+json.dumps(new,ensure_ascii=False)+block[match.end(2):]
    merged=text[:source.start()]+updated+text[source.end():]
    expected=copy.deepcopy(original)
    expected["handlers"][index]["pattern"]=new
    if json.loads(merged)!=expected:
        raise ValueError("source modification was not limited to one regex")
    return merged


def apply(before,after,audits,fixtures,source,promoted):
    if not all(isinstance(x,list) for x in (before,after,audits,fixtures,promoted)):
        raise ValueError("invalid automation JSON array")
    old={x.get("fingerprint") for x in before if isinstance(x,dict)}
    new=[x for x in after if isinstance(x,dict) and
         x.get("fingerprint") not in old]
    by_fp={a.get("fingerprint"):a for a in audits if isinstance(a,dict)}
    parsed=json.loads(source)
    history={x["title"] for x in promoted}
    for entry in new:
        if entry.get("status")!="review_required" or entry.get("automatic_activation") is not False:
            continue
        rule=entry.get("proposal")
        if not isinstance(rule,dict) or rule.get("kind")!="learned_source_regex_patch":
            continue
        fp=entry.get("fingerprint")
        audit=by_fp.get(fp,{})
        if not (audit.get("status")=="ready_for_source_diff"
                and audit.get("promotion_eligible") is True
                and audit.get("regressions")==0
                and audit.get("verified_improvements",0)>=2):
            continue
        if fp!=rule.get("fingerprint") or not re.fullmatch(r"[0-9a-f]{64}",fp or ""):
            raise ValueError("review/audit fingerprint mismatch")
        idx=rule["handler_index"]
        if type(idx) is not int or not 0<=idx<len(parsed["handlers"]):
            raise ValueError("invalid source handler index")
        row=parsed["handlers"][idx]
        field=rule["field"]
        extra=technical_pattern(field,rule["signal"])
        original=rule["original_regex"]
        replacement=rule["proposed_regex"]
        if (extra is None or row.get("kind")!="regex" or
            field not in ("codec","resolution","bit_depth") or
            row.get("name")!=field or row.get("pattern")!=original or
            row.get("transform")!=rule["transform"] or
            replacement!=f"(?:{original})|(?:{extra})" or len(replacement)>512):
            raise ValueError("untrusted or stale source regex replacement")
        indexes=audit.get("verified_fixture_indexes")
        if (not isinstance(indexes,list) or len(indexes)<2 or
            any(type(i) is not int or not 0<=i<len(fixtures) for i in indexes) or
            len(set(indexes))<2):
            raise ValueError("trusted fixture proof missing")
        proven=[fixtures[i] for i in sorted(set(indexes))]
        if len(promoted)+len(proven)>MAX_PROMOTED:
            raise ValueError("approved community regression corpus is full")
        # Ensure that the fixture labels actually cover the target field.
        if any(not isinstance(f.get("expected"),dict) or
               field not in f["expected"] for f in proven):
            raise ValueError("review proof does not label the changed field")
        merged=update_handler_bytes(source,idx,original,replacement)
        new_tests=promoted+[
            f for f in proven if f["title"] not in history]
        if len(new_tests)==len(promoted):
            raise ValueError("positive training proof already recorded")
        return merged,new_tests,{
            "fingerprint":fp,"field":field,"handler_index":idx,
            "verified_improvements":audit["verified_improvements"],
            "stored_regressions":len(new_tests)-len(promoted)
        }
    return source,promoted,None


def main():
    p=argparse.ArgumentParser()
    for key in ("before","after","audit","fixtures","handlers","promoted","summary"):
        p.add_argument("--"+key,required=True)
    args=p.parse_args()
    before=json.loads(Path(args.before).read_text())
    after=json.loads(Path(args.after).read_text())
    audits=json.loads(Path(args.audit).read_text())
    fixtures=json.loads(Path(args.fixtures).read_text())
    file=Path(args.handlers)
    existing=file.read_text()
    promoted_path=Path(args.promoted)
    promoted=json.loads(promoted_path.read_text())
    replacement,new_tests,summary=apply(
        before,after,audits,fixtures,existing,promoted)
    if summary:
        file.write_text(replacement)
        promoted_path.write_text(json.dumps(new_tests,ensure_ascii=False,indent=2)+"\n")
    Path(args.summary).write_text(json.dumps(summary,ensure_ascii=False,indent=2)+"\n")
    print("community_auto_source_diff="+("review_required" if summary else "none"))


if __name__=="__main__":
    main()
