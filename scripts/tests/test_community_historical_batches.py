"""Historical community batch regression: Issue #90's source SHA mismatch.

A trusted current build may aggregate observations and rules from older
ancestor builds. They enter review, not training truth or active regex.
"""
import hashlib
import json
import sys
import unittest
from datetime import datetime,timezone
from pathlib import Path

sys.path.insert(0,str(Path(__file__).parents[1]))
import community_learning as observations
import community_rule_intake as rules

OLD="a"*40
BATCH="b"*40
UNKNOWN="c"*40
NOW=datetime.now(timezone.utc)

def ancestor(source,target):
    # git merge-base --is-ancestor accepts a commit as its own ancestor.
    return source==target or (source,target)==(OLD,BATCH)

def report(title="Example.Show.s02e18.WEB-DL",src=OLD):
    alert=["field_missing_seasons"]
    return {
        "protocol":3,"learning_epoch":1,"build_sha":src,
        "source":"matchlab-community",
        "fingerprint":observations.fingerprint(title,alert),
        "title":title,"status":"unmatched","alerts":alert,
        "parsed":{"title":"Example Show","seasons":[],"episodes":[18]},
        "selected":{"imdb_id":None,"season":None,"episode":None},
        "verified":False,
    }

def field(signal="av1",src=OLD):
    field_name="codec"
    canonical="AV1"
    return {
        "kind":"learned_field_regex","field":field_name,"signal":signal,
        "regex":rules.technical_pattern(field_name,signal),
        "canonical":canonical,"source_build":src+":"+str(rules.FIELD_EPOCH),
        "evidence_count":30,"distinct_titles":12,"canonical_confirmations":10,
        "verification":"local_canonical_and_explicit_release_marker",
        "fingerprint":rules.digest(
            f"community-field:{field_name}:{signal}:{json.dumps(canonical)}"),
    }

def comment(reports,rules_list,src=BATCH):
    fingerprints=sorted(
        [r["fingerprint"] for r in reports+rules_list])
    payload={
        "protocol":3,"learning_epoch":1,"build_sha":src,
        "source":"matchlab-community-batch",
        "created_unix":int(NOW.timestamp()),"verified":False,
        "reports":reports,"rules":rules_list,
        "batch_id":hashlib.sha256(
            (src+"\n1\n"+"\n".join(fingerprints)).encode()).hexdigest()
    }
    return {"body":rules.MARKER+"\n"+json.dumps(payload),
            "created_at":NOW.isoformat(),"id":1}

class HistoricalBatches(unittest.TestCase):
    def test_old_verified_ancestor_report_is_replayed_not_discarded(self):
        entry=comment([report()],[])
        reports=observations.validate_comment(entry,BATCH,ancestor)
        self.assertEqual(len(reports),1)
        self.assertEqual(reports[0]["build_sha"],OLD)
        result,_=observations.intake(
            [entry],BATCH,NOW,set(),"unused",ancestor,
            lambda *_:{"title":"Example Show","seasons":[],"episodes":[18]})
        self.assertEqual(len(result),1)
        self.assertFalse(result[0]["verified"])
        self.assertEqual(result[0]["status"],"possible_missing_field_in_current_parser")
        self.assertIsNone(observations.validate_comment(
            entry,BATCH,lambda *_:False))

    def test_rule_uses_historical_source_but_never_automatic_activation(self):
        entry=comment([], [field()])
        accepted=rules.parse_batch(entry,BATCH,ancestor)
        self.assertEqual(len(accepted),1)
        self.assertEqual(accepted[0]["source_build"],OLD+":5")
        self.assertEqual(rules.parse_batch(
            entry,BATCH,lambda source,target:source==target),[])
        self.assertIsNone(rules.valid_extended_rule(
            {**field(),"source_build":UNKNOWN+":5"},BATCH,ancestor))

    def test_one_bad_rule_does_not_erase_another_valid_rule(self):
        good=field("av1")
        bad=field("h264")
        bad["regex"]=r"(?s)(.*)+"
        entry=comment([],[bad,good])
        accepted=rules.parse_batch(entry,BATCH,ancestor)
        self.assertEqual([row["fingerprint"] for row in accepted],
                         [good["fingerprint"]])
        payload=json.loads(entry["body"][len(rules.MARKER)+1:])
        payload["batch_id"]="0"*64
        tampered={**entry,"body":rules.MARKER+"\n"+json.dumps(payload)}
        self.assertIsNone(rules.parse_batch(tampered,BATCH,ancestor))

    def test_one_invalid_observation_does_not_erase_valid_one(self):
        good=report()
        bad=report("Different.Show.s03e01")
        bad["unexpected_private_property"]="not-allowed"
        selected=observations.validate_comment(
            comment([bad,good],[]),BATCH,ancestor)
        self.assertEqual(len(selected),1)
        self.assertEqual(selected[0]["fingerprint"],good["fingerprint"])

    def test_cross_branch_or_bad_epoch_never_accepted(self):
        self.assertIsNone(observations.validate_comment(
            comment([report(src=UNKNOWN)],[]),BATCH,ancestor))
        invalid={**field(),"source_build":OLD+":99"}
        self.assertIsNone(rules.valid_extended_rule(invalid,BATCH,ancestor))

if __name__=="__main__":
    unittest.main()
