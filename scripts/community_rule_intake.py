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


# All non-anime regex changes enter the GitHub PR queue as review-only
# data. No user-supplied regex is run in Python or loaded by the server.
TECH_FIELDS={"resolution","codec","quality","audio","bit_depth","hdr",
    "channels","languages","subbed","dubbed","extension"}
TV_GRAMMARS={
    "sxe":r"(?i)(?:^|[._ -])s(?P<s>[0-9]{1,3})[._ -]*e(?P<e>[0-9]{1,5})(?:$|[._ -])",
    "x":r"(?i)(?:^|[._ -])(?P<s>[0-9]{1,3})x(?P<e>[0-9]{1,5})(?:$|[._ -])",
    "words":r"(?i)(?:^|[._ -])season[._ -]+(?P<s>[0-9]{1,3})[._ -]+episode[._ -]+(?P<e>[0-9]{1,5})(?:$|[._ -])",
}
FIELD_EPOCH=5
TV_EPOCH=2
DISCOVERY_EPOCH=2
# Must match the restricted native Rust TV regex installed by tv_learning.rs.
# GitHub receives the exact added patterns, not only the diagnostic grammar.
TV_NATIVE_PATTERNS={
    "sxe":{
        "seasons":r"(?i)(?:^|[._ -])s([0-9]{1,3})[._ -]*e[0-9]{1,5}(?:$|[._ -])",
        "episodes":r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,5})(?:$|[._ -])",
    },
    "x":{
        "seasons":r"(?i)(?:^|[._ -])([0-9]{1,3})x[0-9]{1,5}(?:$|[._ -])",
        "episodes":r"(?i)(?:^|[._ -])[0-9]{1,3}x([0-9]{1,5})(?:$|[._ -])",
    },
    "words":{
        "seasons":r"(?i)(?:^|[._ -])season[._ -]+([0-9]{1,3})[._ -]+episode[._ -]+[0-9]{1,5}(?:$|[._ -])",
        "episodes":r"(?i)(?:^|[._ -])season[._ -]+[0-9]{1,3}[._ -]+episode[._ -]+([0-9]{1,5})(?:$|[._ -])",
    },
}
CATALOG=Path(__file__).resolve().parents[1] / "crates/matchlab-parser/src/generated/handlers.json"
def digest(s):return hashlib.sha256(s.encode()).hexdigest()
def bounded_int(v,small=0,big=2_000_000):
    return type(v) is int and small<=v<=big
def technical_pattern(field,signal):
    if field not in TECH_FIELDS or not isinstance(signal,str):return None
    if not 2<=len(signal)<=24 or not re.fullmatch("[a-z0-9_.]+",signal):
        return None
    parts=[re.escape(p) for p in re.split("[_.]",signal) if p]
    if not 1<=len(parts)<=4:return None
    pattern=r"(?i)(?:^|[ ._-])(?:"+r"[ ._-]*".join(parts)+r")(?:$|[ ._-])"
    return pattern if len(pattern)<=140 else None
def valid_extended_rule(r,sha):
    if not isinstance(r,dict):return None
    kind=r.get("kind")
    common={"kind","fingerprint","evidence_count","distinct_titles",
        "canonical_confirmations","source_build","verification"}
    min_evidence=0 if kind=="historical_regex_review" else 12
    min_titles=0 if kind=="historical_regex_review" else 4
    if not all(bounded_int(r.get(k),v) for k,v in
        (("evidence_count",min_evidence),("distinct_titles",min_titles),
         ("canonical_confirmations",0))):return None
    if r["canonical_confirmations"]>r["evidence_count"]:return None
    if not isinstance(r.get("fingerprint"),str):return None
    if kind=="learned_field_regex_review":
        # Unknown-field predictions are intentionally review-only.
        # A self-reported model score is not proof of a source regex fix.
        expected=common|{"field","signal","regex","review_only"}
        if set(r)!=expected or r["source_build"]!=sha+f":{DISCOVERY_EPOCH}":
            return None
        field,signal=r["field"],r["signal"]
        if not isinstance(field,str) or not isinstance(signal,str):
            return None
        if (r["review_only"] is not True
            or r["verification"]!="unverified_model_discovery_review_only"
            or r["canonical_confirmations"]!=0
            or r["evidence_count"]<12 or r["distinct_titles"]<12):
            return None
        if not re.fullmatch(r"[a-z0-9]{3,16}",signal):
            return None
        try:
            catalog=json.loads(CATALOG.read_text())["handlers"]
        except (OSError,KeyError,TypeError,ValueError):
            return None
        if not any(h.get("kind")=="regex" and h.get("name")==field
                for h in catalog):return None
        pattern=r"(?i)(?:^|[ ._-])"+signal+r"(?:$|[ ._-])"
        if r["regex"]!=pattern:return None
        expected_fp=digest(f"community-review:{field}:{signal}:{pattern}")
    elif kind=="historical_regex_review":
        # Old local rules remain useful review evidence, but they cannot
        # inherit activation rights from a retired parser/metadata build.
        expected={"kind","original_kind","fingerprint","source_build",
            "field","signal","regex","evidence_count","distinct_titles",
            "canonical_confirmations","review_only","verification"}
        if set(r)!=expected:return None
        if r["original_kind"] not in {
            "verified_regex_proposal","learned_field_regex",
            "learned_tv_regex","learned_source_regex_patch",
            "learned_field_regex_review"
        }:return None
        source=r["source_build"]
        if not isinstance(source,str) or ":" not in source:return None
        old_sha,epoch=source.rsplit(":",1)
        if not valid_sha(old_sha) or not epoch.isdecimal() or not 1<=int(epoch)<=20:
            return None
        if not git_ancestor(old_sha,sha):return None
        if r["review_only"] is not True or r["verification"]!=(
            "historical_archive_requires_independent_review"
        ):return None
        if not isinstance(r["field"],str) or not re.fullmatch(
            "[a-z_]{2,40}",r["field"]
        ):return None
        if not isinstance(r["signal"],str) or not re.fullmatch(
            "[a-z0-9_.]{0,24}",r["signal"]
        ):return None
        if not isinstance(r["regex"],str) or not 1<=len(r["regex"])<=1024:
            return None
        if any(ord(c)<32 or ord(c)>126 for c in r["regex"]):return None
        if not re.fullmatch("[0-9a-f]{64}",r["fingerprint"]):return None
        if r["canonical_confirmations"] != 0:return None
        if not bounded_int(r["evidence_count"],0) or not bounded_int(
            r["distinct_titles"],0
        ):return None
        expected_fp=r["fingerprint"]  # opaque identity, review-only forever
    elif kind=="learned_field_regex":
        expected=common|{"field","signal","regex","canonical"}
        if set(r)!=expected or r["source_build"]!=sha+f":{FIELD_EPOCH}":return None
        field,signal=r["field"],r["signal"]
        pattern=technical_pattern(field,signal)
        if pattern is None or r["regex"]!=pattern:return None
        if r["verification"]!="local_canonical_and_explicit_release_marker":return None
        value=r["canonical"]
        if type(value) not in (str,int,bool,list):return None
        if isinstance(value,str) and not (1<=len(value)<=48 and
            re.fullmatch("[a-zA-Z0-9_. +\\-]+",value)):return None
        if type(value) is int and not 0<=value<=9999:return None
        if isinstance(value,list) and not (field=="languages" and
            1<=len(value)<=8 and all(isinstance(x,str) and
            re.fullmatch("[a-zA-Z]{1,5}",x) for x in value)):return None
        canon=json.dumps(value,ensure_ascii=False,separators=(",",":"))
        expected_fp=digest(f"community-field:{field}:{signal}:{canon}")
    elif kind=="learned_tv_regex":
        expected=common|{"family","fields","regex","native_patterns"}
        if set(r)!=expected or r["source_build"]!=sha+f":{TV_EPOCH}":return None
        family=r["family"]
        if not isinstance(family,str) or family not in TV_GRAMMARS:return None
        if (r["regex"]!=TV_GRAMMARS[family]
            or r["fields"]!=["seasons","episodes"]
            or r["native_patterns"]!=TV_NATIVE_PATTERNS[family]):
            return None
        if r["verification"]!="explicit_tv_marker_and_canonical_confirmations":
            return None
        if r["canonical_confirmations"]<2:return None
        expected_fp=digest("community-tv:"+family)
    elif kind=="learned_source_regex_patch":
        expected=common|{"handler_index","field","signal",
            "original_regex","proposed_regex","transform"}
        if set(r)!=expected or r["source_build"]!=sha+f":{FIELD_EPOCH}":return None
        if not bounded_int(r["handler_index"],0,4096):return None
        if r["field"] not in {"resolution","codec","bit_depth"}:return None
        extra=technical_pattern(r["field"],r["signal"])
        if extra is None:return None
        try:
            handlers=json.loads(CATALOG.read_text())["handlers"]
            source=handlers[r["handler_index"]]
        except (OSError,IndexError,KeyError,ValueError,TypeError):
            return None
        if source.get("kind")!="regex" or source.get("name")!=r["field"]:
            return None
        old=source.get("pattern")
        if not isinstance(old,str) or r["original_regex"]!=old:return None
        if r["transform"]!=source.get("transform"):return None
        new="(?:"+old+")|(?:"+extra+")"
        if len(new)>512 or r["proposed_regex"]!=new:return None
        if r["verification"]!="source_catalog_and_canonical_release_markers":
            return None
        if (r["evidence_count"]<16 or r["distinct_titles"]<6 or
            r["canonical_confirmations"]<2):return None
        expected_fp=digest(f"community-patch:{r['handler_index']}:{old}:{new}")
    else:return None
    return r if r["fingerprint"]==expected_fp else None

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
        valid=(valid_rule(candidate,sha) if isinstance(candidate,dict)
               and candidate.get('kind')=='verified_regex_proposal'
               else valid_extended_rule(candidate,sha))
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

def collect(all_comments,sha,seen,now=None,ancestor=git_ancestor):
    if now is None:now=datetime.now(timezone.utc)
    cutoff=now-timedelta(days=15)
    output=[]
    seen=set(seen)
    pending=set()
    for sub in all_comments:
        comments=sub if isinstance(sub,list) else [sub]
        for comment in comments:
            if not isinstance(comment,dict):continue
            try:
                created=datetime.fromisoformat(comment["created_at"].replace("Z","+00:00"))
            except (ValueError,TypeError,KeyError,AttributeError):
                continue
            if not cutoff<=created<=now+timedelta(minutes=5):continue
            rules=parse_batch(comment,sha,ancestor)
            if rules is None:continue
            for rule in rules:
                if rule["fingerprint"] in seen or rule["fingerprint"] in pending:continue
                pending.add(rule["fingerprint"])
                output.append(rule)
                if len(output)>=24:return output,seen
    # Do NOT poison the permanent ledger with only self-reported evidence.
    # A fingerprint becomes permanent only after trusted screening, regression
    # tests and successful creation of the human-review PR.
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
