"""New community sharing formats: exact-rule and source-rule PR proposals."""
import hashlib
import json
import sys
import unittest
from pathlib import Path
sys.path.insert(0,str(Path(__file__).parents[1]))
import community_rule_intake as intake
import review_community_regex as review

SHA="a"*40
def sha(s):return hashlib.sha256(s.encode()).hexdigest()
def common(kind,epoch):
    return {"kind":kind,"evidence_count":24,"distinct_titles":8,
        "canonical_confirmations":8,"source_build":SHA+":"+str(epoch)}
def technical():
    field,signal,value="resolution","1440p","1440p"
    return {**common("learned_field_regex",intake.FIELD_EPOCH),
        "verification":"local_canonical_and_explicit_release_marker",
        "field":field,"signal":signal,
        "regex":intake.technical_pattern(field,signal),"canonical":value,
        "fingerprint":sha(f"community-field:{field}:{signal}:{json.dumps(value)}")}
def tv():
    return {**common("learned_tv_regex",intake.TV_EPOCH),
        "verification":"explicit_tv_marker_and_canonical_confirmations",
        "family":"sxe","fields":["seasons","episodes"],
        "regex":intake.TV_GRAMMARS["sxe"],
        "native_patterns":intake.TV_NATIVE_PATTERNS["sxe"],
        "fingerprint":sha("community-tv:sxe")}
def patch():
    catalog=json.loads(intake.CATALOG.read_text())["handlers"]
    index=33
    source=catalog[index]
    extra=intake.technical_pattern(source["name"],"1440p")
    value="(?:"+source["pattern"]+")|(?:"+extra+")"
    return {**common("learned_source_regex_patch",intake.FIELD_EPOCH),
        "verification":"source_catalog_and_canonical_release_markers",
        "handler_index":index,"field":source["name"],"signal":"1440p",
        "original_regex":source["pattern"],"proposed_regex":value,
        "transform":source["transform"],
        "fingerprint":sha(f"community-patch:{index}:{source['pattern']}:{value}")}
def batch(proposals):
    fps=sorted(r["fingerprint"] for r in proposals)
    batch_id=sha(SHA+"\n1\n"+"\n".join(fps))
    return {"body":intake.MARKER+"\n"+json.dumps({
        "protocol":3,"learning_epoch":1,"build_sha":SHA,
        "source":"matchlab-community-batch","batch_id":batch_id,
        "created_unix":1,"verified":False,"reports":[],"rules":proposals
    })}
class TestAllRegexSharing(unittest.TestCase):
    def test_all_three_regex_families_survive_github_intake(self):
        for candidate in [technical(),tv(),patch()]:
            self.assertIsNotNone(intake.valid_extended_rule(candidate,SHA))
            self.assertEqual(len(intake.parse_batch(batch([candidate]),SHA,lambda *_:True)),1)
            chosen,changed=review.screen([candidate],[])
            self.assertTrue(changed)
            self.assertFalse(chosen[0]["automatic_activation"])
            self.assertEqual(chosen[0]["proposal"],candidate)
            _,again=review.screen([candidate],chosen)
            self.assertFalse(again,"review queue must deduplicate")
    def test_rejects_forged_source_and_regex_mutations(self):
        for proposal in [technical(),tv(),patch()]:
            for key in ("fingerprint","source_build"):
                changed={**proposal,key:"bad"}
                self.assertIsNone(intake.valid_extended_rule(changed,SHA))
        p=patch()
        p["proposed_regex"]="(?s)(.*)+"
        self.assertIsNone(intake.valid_extended_rule(p,SHA))
        p=patch()
        p["handler_index"]=33+1
        self.assertIsNone(intake.valid_extended_rule(p,SHA))
        p=tv()
        p["regex"]=r"(?s)(.*)+"
        self.assertIsNone(intake.valid_extended_rule(p,SHA))
        p=tv()
        p["native_patterns"]={**p["native_patterns"],"episodes":r"(?s)(.*)+"}
        self.assertIsNone(intake.valid_extended_rule(p,SHA))
        p=tv()
        p["source_build"]=SHA+":1"
        self.assertIsNone(intake.valid_extended_rule(p,SHA))
    def test_two_rules_are_allowed_one_pr_review_at_a_time(self):
        rows=[technical(),tv()]
        accepted=intake.parse_batch(batch(rows),SHA,lambda *_:True)
        self.assertEqual(len(accepted),2)
        first,changed=review.screen(accepted,[])
        self.assertTrue(changed)
        self.assertEqual(len(first),1)
        second,changed=review.screen(accepted,first)
        self.assertTrue(changed)
        self.assertEqual(len(second),2)
    def test_no_untrusted_extra_fields_or_title_samples(self):
        row=technical()
        row["samples"]=["Private.Movie.2024"]
        self.assertIsNone(intake.valid_extended_rule(row,SHA))
