#!/usr/bin/env python3
"""Intake untrusted v3 GitHub batch comments containing proposed regex.

This runs ONLY in GitHub Actions, never in the MatchLab Docker image.
No issue text is ever executed. Rule approval remains a human-reviewed PR.
"""
import argparse
from datetime import datetime, timedelta, timezone
import hashlib
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).parent))
from community_learning import allowed_title, git_ancestor, valid_sha

MARKER="<!-- matchlab-community-batch-v3 -->"
CODES={"season","episode","series","part","year","movie","sample",
       "bluray","webrip","webdl","hdtv","hdr","codec","audio",
       "x264","x265","h264","h265","hevc","hdr10","dovi",
       "ep","eps","new","the","eng","sub","dub"}
MAX_BODY=16384
MAX_RULES=2
MAX_SAMPLES=12

def canonical(marker):
    if not isinstance(marker,str) or not re.fullmatch("[a-z]{3,9}",marker):
        return None
    if marker in CODES:
        return None
    return r"(?i)(?:^|[ ._-])"+marker+r"[ ._-]*([1-9][0-9]{0,4})(?:$|[ ._-])"

def valid_sample(sample,marker):
    if not isinstance(sample,dict) or set(sample)!={
        "title","imdb_id","absolute_episode","season","episode",
        "anidb_id","anidb_episode"
    }:return False
    title=sample["title"]
    if not allowed_title(title) or len(title)>180:return False
    if not re.fullmatch(r"tt[0-9]{7,8}",str(sample["imdb_id"])):return False
    if not re.fullmatch(r"[0-9]{1,9}",str(sample["anidb_id"])):return False
    for name in ("absolute_episode","season","episode","anidb_episode"):
        if type(sample[name]) is not int or not 0<sample[name]<=99999:return False
    expression=canonical(marker)
    result=re.search(expression,title)
    if result is None or int(result.group(1))!=sample["absolute_episode"]:
        return False
    return True

def valid_rule(row,build_sha):
    if not isinstance(row,dict) or set(row)!={
        "kind","fingerprint","marker","regex","source_build","source_metadata",
        "evidence_count","distinct_imdb","distinct_episodes",
        "verification","examples"
    }:return None
    marker=row["marker"]
    if row["kind"]!="verified_regex_proposal" or canonical(marker)!=row["regex"]:
        return None
    if row["source_build"]!=build_sha+":1":
        return None
    if (not isinstance(row["source_metadata"],str) or
        len(row["source_metadata"])>200):return None
    fp=hashlib.sha256(("community-regex:"+marker).encode()).hexdigest()
    if row["fingerprint"]!=fp:return None
    for name,minimum in (("evidence_count",12),("distinct_imdb",4),
                          ("distinct_episodes",5)):
        value=row[name]
        if type(value) is not int or not minimum<=value<=2048:return None
    if row["verification"]!="local_exact_imdb_title_and_anidb_tvdb_crosswalk":
        return None
    examples=row["examples"]
    if not isinstance(examples,list) or not 8<=len(examples)<=MAX_SAMPLES:
        return None
    if not all(valid_sample(v,marker) for v in examples):return None
    # Proposal must contain useful independent examples, not 8 copies of
    # one torrent under cosmetic variations.
    if len(set((v["imdb_id"],v["absolute_episode"]) for v in examples))<8:
        return None
    if len(set(v["imdb_id"] for v in examples))<3:
        return None
    if len(set(v["absolute_episode"] for v in examples))<5:
        return None
    return row

def parse_batch(comment,current_sha,ancestor=git_ancestor):
    if not isinstance(comment,dict):return None
    body=comment.get("body")
    if not isinstance(body,str) or not body.startswith(MARKER+"\n"):
        return None
    if len(body.encode("utf-8"))>MAX_BODY:return None
    try:batch=json.loads(body[len(MARKER)+1:])
    except (ValueError,TypeError):return None
    if not isinstance(batch,dict) or set(batch)!={
        "protocol","learning_epoch","build_sha","source","batch_id",
        "created_unix","verified","reports","rules"
    }:return None
    sha=batch["build_sha"]
    if (batch["protocol"]!=3 or batch["learning_epoch"]!=1 or
        not valid_sha(sha) or not ancestor(sha,current_sha) or
        batch["verified"] is not False or
        batch["source"]!="matchlab-community-batch" or
        type(batch["created_unix"]) is not int or
        not isinstance(batch["reports"],list) or
        not isinstance(batch["rules"],list) or
        len(batch["rules"])>MAX_RULES or
        len(batch["reports"])+len(batch["rules"])>16):
        return None
    rules=[]
    for candidate in batch["rules"]:
        valid=valid_rule(candidate,sha)
        if valid is None:return None
        rules.append(valid)
    fingerprints=[r["fingerprint"] for r in rules]
    fingerprints.extend(r.get("fingerprint") for r in batch["reports"] if isinstance(r,dict))
    if len(fingerprints)!=len(batch["rules"])+len(batch["reports"]):
        return None
    if any(not isinstance(v,str) or
           not re.fullmatch("[0-9a-f]{64}",v) for v in fingerprints):
        return None
    if len(set(fingerprints))!=len(fingerprints):
        return None
    expected=hashlib.sha256((sha+"\n1\n"+"\n".join(sorted(fingerprints))).encode()).hexdigest()
    if batch["batch_id"]!=expected:return None
    return rules

def collect(all_comments,sha,seen,now=None):
    if now is None:now=datetime.now(timezone.utc)
    cutoff=now-timedelta(days=2)
    output=[]
    seen=set(seen)
    for sub in all_comments:
        comments=sub if isinstance(sub,list) else [sub]
        for comment in comments:
            if not isinstance(comment,dict):continue
            try:
                created=datetime.fromisoformat(comment["created_at"].replace("Z","+00:00"))
            except (ValueError,TypeError,KeyError,AttributeError):
                continue
            if not cutoff<=created<=now+timedelta(minutes=5):continue
            rules=parse_batch(comment,sha)
            if rules is None:continue
            for rule in rules:
                if rule["fingerprint"] in seen:continue
                seen.add(rule["fingerprint"])
                output.append(rule)
                if len(output)>=24:return output,seen
    return output,seen

def main():
    parser=argparse.ArgumentParser()
    for name in ("comments","seen","sha","out","seen-out"):
        parser.add_argument("--"+name,required=True)
    a=parser.parse_args()
    if not valid_sha(a.sha):raise SystemExit("invalid full SHA")
    raw=json.loads(Path(a.comments).read_text(encoding="utf8"))
    if not isinstance(raw,list):raise SystemExit("invalid GitHub response")
    seen=set(Path(a.seen).read_text(encoding="utf8").splitlines())
    rules,updated=collect(raw,a.sha,seen)
    Path(a.out).write_text(json.dumps(rules,ensure_ascii=False,indent=2)+"\n",encoding="utf8")
    Path(a.seen_out).write_text("\n".join(sorted(updated))+
        ("\n" if updated else ""),encoding="utf8")
    print(f"{len(rules)} new, schema-validated community regex proposals; independent main branch checks pending")

if __name__=="__main__":
    main()
