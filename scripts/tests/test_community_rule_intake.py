"""Regression and adversarial tests for GitHub-only community regex PR intake."""
from datetime import datetime,timezone
import hashlib
import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0,str(Path(__file__).parents[1]))
import community_rule_intake as rules
import community_learning as original

SHA="a"*40
NOW=datetime.now(timezone.utc)

def proposal(marker="epis"):
    samples=[]
    for n in range(8):
        samples.append({
            "title":f"SampleShow{n%4}.EPIS.{301+n}.1080p.WEB-DL",
            "imdb_id":f"tt{1234567+n%4}",
            "absolute_episode":301+n,
            "season":2,"episode":n+1,
            "anidb_id":str(1000+n%4),
            "anidb_episode":301+n
        })
    return {
        "kind":"verified_regex_proposal",
        "fingerprint":hashlib.sha256(("community-regex:"+marker).encode()).hexdigest(),
        "marker":marker,"regex":rules.canonical(marker),
        "source_build":SHA+":1","source_metadata":"verified-revision",
        "evidence_count":20,"distinct_imdb":4,"distinct_episodes":8,
        "verification":"local_exact_imdb_title_and_anidb_tvdb_crosswalk",
        "examples":samples
    }

def comment(rule=None,sha=SHA):
    item=rule if rule is not None else proposal()
    digest=hashlib.sha256((sha+"\n1\n"+item["fingerprint"]).encode()).hexdigest()
    payload={
        "protocol":3,"learning_epoch":1,"build_sha":sha,
        "source":"matchlab-community-batch","batch_id":digest,
        "created_unix":int(NOW.timestamp()),"verified":False,
        "reports":[],"rules":[item]
    }
    return {"body":rules.MARKER+"\n"+json.dumps(payload),
            "created_at":NOW.isoformat(),"id":1}

class CommunityRuleTests(unittest.TestCase):
    def test_valid_independent_evidence_can_enter_pr_review(self):
        self.assertIsNotNone(rules.valid_rule(proposal(),SHA))
        batch=comment()
        self.assertEqual(len(rules.parse_batch(batch,SHA,lambda *_:True)),1)
        out,seen=rules.collect([batch],SHA,set(),NOW,lambda *_:True)
        self.assertEqual(len(out),1)
        self.assertEqual(len(seen),0)
        # Untrusted reports cannot poison the permanent ledger: rejected
        # proposals may be reconsidered after independent evidence appears.
        out2,seen2=rules.collect([batch],SHA,seen,NOW,lambda *_:True)
        self.assertEqual(len(out2),1)
        self.assertEqual(seen2,seen)
        accepted={out[0]['fingerprint']}
        out3,_=rules.collect([batch],SHA,accepted,NOW,lambda *_:True)
        self.assertEqual(out3,[])
        # Rule-only batches remain valid for the legacy anomaly reviewer.
        self.assertEqual(original.validate_comment(batch,SHA,lambda *_:True),[])

    def test_rejects_injection_and_unsafe_regex(self):
        for marker in ("episode","h265","xxx.*","a\nb","en"):
            self.assertIsNone(rules.canonical(marker))
        item=proposal()
        item["regex"]="(?s)(.*)+"
        self.assertIsNone(rules.valid_rule(item,SHA))
        item=proposal()
        item["examples"][0]["title"]="/private/Series.EPIS.301"
        self.assertIsNone(rules.valid_rule(item,SHA))
        item=proposal()
        item["examples"][0]["title"]="example@host.EPIS.301"
        self.assertIsNone(rules.valid_rule(item,SHA))
        item=proposal()
        item["examples"][0]["absolute_episode"]=999
        self.assertIsNone(rules.valid_rule(item,SHA))

    def test_forged_build_and_old_epochs_are_rejected(self):
        batch=comment()
        self.assertIsNone(rules.parse_batch(batch,"b"*40,lambda *_:False))
        item=proposal()
        item["source_build"]="b"*40+":1"
        self.assertIsNone(rules.valid_rule(item,SHA))
        batch=comment()
        content=json.loads(batch["body"][len(rules.MARKER)+1:])
        content["learning_epoch"]=0
        batch["body"]=rules.MARKER+"\n"+json.dumps(content)
        self.assertIsNone(rules.parse_batch(batch,SHA,lambda *_:True))

    def test_requires_heterogeneous_evidence(self):
        item=proposal()
        item["examples"]=[item["examples"][0]]*8
        self.assertIsNone(rules.valid_rule(item,SHA))
        item=proposal()
        item["distinct_imdb"]=1
        self.assertIsNone(rules.valid_rule(item,SHA))

if __name__=="__main__":
    unittest.main()
