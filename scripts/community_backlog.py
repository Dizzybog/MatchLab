#!/usr/bin/env python3
"""Durable GitHub review backlog for independently screened rule candidates.

This file is a GitHub Release asset, not executable code. A proposal is removed
only after its fingerprint enters a successfully created human-review PR.
Failed screening and interrupted daily workflows never silently discard it.
"""
import argparse
import hashlib
import json
import re
from pathlib import Path
from community_decisions import due, signature

MAX_PENDING=20_000
MAX_REVIEW=4096
MAX_DAILY=24
HEX=re.compile(r"^[0-9a-f]{64}$")

def fp_of(row):
    if not isinstance(row,dict):return None
    fp=row.get("fingerprint")
    return fp if isinstance(fp,str) and HEX.fullmatch(fp) else None

def read(path,default):
    p=Path(path)
    return json.loads(p.read_text(encoding="utf-8")) if p.exists() else default

def curated_fingerprints(rules,extended):
    known={p["fingerprint"] for p in extended
           if isinstance(p,dict) and fp_of(p)}
    for row in rules:
        if isinstance(row,dict) and isinstance(row.get("marker"),str):
            known.add(hashlib.sha256(
                ("community-regex:"+row["marker"]).encode()).hexdigest())
    return known

def merge(old,incoming,seen,curated):
    if not isinstance(old,dict) or set(old)!={"pending"}:
        raise ValueError("Invalid previous community backlog")
    queue=old["pending"]
    if not isinstance(queue,list) or not isinstance(incoming,list):
        raise ValueError("Invalid candidate list")
    if len(queue)>MAX_PENDING or len(incoming)>MAX_REVIEW:
        raise ValueError("Community backlog exceeds capacity; do not discard data")
    done=set(seen)|set(curated)
    out=[]
    added=set()
    for row in queue+incoming:
        fp=fp_of(row)
        if fp is None:raise ValueError("Invalid queued rule fingerprint")
        if fp in done or fp in added:continue
        out.append(row)
        added.add(fp)
    if len(out)>MAX_PENDING:
        raise ValueError("Community backlog full; source comments remain on GitHub")
    return {"pending":out}

def finalize(backlog,attempted,accepted):
    accepted=set(accepted)
    attempt=set(attempted)
    if not isinstance(backlog,dict) or set(backlog)!={"pending"}:
        raise ValueError("Invalid backlog")
    queue=backlog["pending"]
    # Failed/unresolved suggestions rotate to the back, not deleted.
    remaining=[row for row in queue if fp_of(row) not in accepted]
    return {"pending":[row for row in remaining if fp_of(row) not in attempt]+
            [row for row in remaining if fp_of(row) in attempt]}

def main():
    p=argparse.ArgumentParser()
    p.add_argument("phase",choices=["prepare","finalize"])
    p.add_argument("--state",required=True)
    p.add_argument("--out",required=True)
    p.add_argument("--candidate-file")
    p.add_argument("--seen")
    p.add_argument("--curated")
    p.add_argument("--extended")
    p.add_argument("--daily")
    p.add_argument("--accepted")
    p.add_argument("--decisions")
    p.add_argument("--handlers")
    p.add_argument("--fixtures")
    p.add_argument("--parser-src")
    args=p.parse_args()
    old=read(args.state,{"pending":[]})
    if args.phase=="prepare":
        rules=read(args.curated,[])
        extended=read(args.extended,[])
        seen=set(Path(args.seen).read_text().splitlines())
        new=merge(old,read(args.candidate_file,[]),seen,
                  curated_fingerprints(rules,extended))
        Path(args.out).write_text(json.dumps(new,ensure_ascii=False,indent=2)+"\n")
        eligible=new["pending"][:MAX_DAILY]
        deferred=0
        if args.decisions:
            if not args.handlers or not args.fixtures:
                raise ValueError("decisions require immutable handlers and fixtures")
            decisions=read(args.decisions,{"decisions":{}})
            eligible,deferred=due(new["pending"],decisions,signature(
                args.handlers,args.fixtures,args.parser_src),MAX_DAILY)
        Path(args.daily).write_text(json.dumps(eligible,
            ensure_ascii=False,indent=2)+"\n")
        print("community_backlog_pending="+str(len(new["pending"]))+
              " review_due="+str(len(eligible))+
              " cached_decisions="+str(deferred))
    else:
        if args.daily is None or args.accepted is None:
            raise ValueError("daily/accepted inputs required")
        attempted=[fp_of(row) for row in read(args.daily,[])]
        accepted=read(args.accepted,[])
        if not isinstance(accepted,list) or any(
            not isinstance(fp,str) or HEX.fullmatch(fp) is None for fp in accepted
        ):raise ValueError("invalid accepted fingerprint list")
        out=finalize(old,attempted,accepted)
        Path(args.out).write_text(json.dumps(out,ensure_ascii=False,indent=2)+"\n")
        print("community_backlog_pending="+str(len(out["pending"])))

if __name__=="__main__":
    main()
