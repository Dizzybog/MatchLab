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
def review_discovery(field="network",marker="futuretag"):
    pattern=r"(?i)(?:^|[ ._-])"+marker+r"(?:$|[ ._-])"
    return {**common("learned_field_regex_review",intake.DISCOVERY_EPOCH),
        "evidence_count":24,"distinct_titles":12,
        "verification":"unverified_model_discovery_review_only",
        "field":field,"signal":marker,"regex":pattern,
        "canonical_confirmations":0,
        "review_only":True,
        "fingerprint":sha(f"community-review:{field}:{marker}:{pattern}")}
def title_boundary():
    pattern=intake.TITLE_BOUNDARY_REVIEW_PATTERNS["episode"]
    signal="underextracted_episode"
    return {**common("learned_title_boundary_regex_review",1),
        "evidence_count":16,"distinct_titles":4,
        "canonical_confirmations":0,"field":"title",
        "signal":signal,"regex":pattern,"review_only":True,
        "verification":"matcher_alias_matches_complete_raw_title_review_only",
        "fingerprint":sha(f"title-boundary:{signal}:{pattern}")}

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
    def test_any_parser_field_can_submit_discovery_for_review_only(self):
        for field in ("title","year","group","network","languages",
                      "seasons","episodes","quality","audio"):
            candidate=review_discovery(field=field)
            self.assertIsNotNone(intake.valid_extended_rule(candidate,SHA),field)
            self.assertEqual(len(intake.parse_batch(
                batch([candidate]),SHA,lambda *_:True)),1)
            queued,added=review.screen([candidate],[])
            self.assertTrue(added)
            self.assertFalse(queued[0]["automatic_activation"])
        forged=review_discovery()
        forged["regex"]=r"(?s)(.*)+"
        self.assertIsNone(intake.valid_extended_rule(forged,SHA))
        forged=review_discovery()
        forged["canonical_confirmations"]=8
        self.assertIsNone(intake.valid_extended_rule(forged,SHA))
        forged=review_discovery()
        forged["field"]="imdb_id"
        self.assertIsNone(intake.valid_extended_rule(forged,SHA))
        forged=review_discovery()
        forged["source_build"]=SHA+":1"
        self.assertIsNone(intake.valid_extended_rule(forged,SHA))
    def test_verified_title_boundary_proposals_are_review_only_and_private(self):
        candidate=title_boundary()
        self.assertIsNotNone(intake.valid_extended_rule(candidate,SHA))
        self.assertEqual(len(intake.parse_batch(
            batch([candidate]),SHA,lambda *_:True)),1)
        queued,changed=review.screen([candidate],[])
        self.assertTrue(changed)
        self.assertFalse(queued[0]["automatic_activation"])
        for key,value in [
            ("regex",r"(?s)(.*)+"),
            ("source_build",SHA+":2"),
            ("field","imdb_id"),
            ("canonical_confirmations",4),
            ("verification","automatically_activated"),
            ("signal","underextracted_custom_marker"),
            ("fingerprint","0"*64),
            ("evidence_count",11),
            ("distinct_titles",3),
        ]:
            forged={**candidate,key:value}
            self.assertIsNone(intake.valid_extended_rule(forged,SHA),key)
        self.assertNotIn("raw_title",candidate)
        self.assertNotIn("imdb_id",candidate)
        self.assertNotIn("matched_title",candidate)

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
    def test_offline_historical_backfill_is_review_only(self):
        from unittest.mock import patch
        old={"kind":"historical_regex_review","original_kind":"learned_field_regex",
            "fingerprint":sha("historical-codec-x266"),
            "source_build":"b"*40+":5","field":"codec","signal":"x266",
            "regex":r"(?i)(?:^|[ ._-])x266(?:$|[ ._-])",
            "evidence_count":8,"distinct_titles":4,
            "canonical_confirmations":0,"review_only":True,
            "verification":"historical_archive_requires_independent_review"}
        with patch.object(intake,"git_ancestor",return_value=True):
            self.assertIsNotNone(intake.valid_extended_rule(old,SHA))
            self.assertEqual(len(intake.parse_batch(batch([old]),SHA,
                lambda *_:True)),1)
            proposal,changed=review.screen([old],[])
            self.assertTrue(changed)
            self.assertFalse(proposal[0]["automatic_activation"])
            _,again=review.screen([old],proposal)
            self.assertFalse(again)
        with patch.object(intake,"git_ancestor",return_value=False):
            self.assertIsNone(intake.valid_extended_rule(old,SHA))
        with patch.object(intake,"git_ancestor",return_value=True):
            self.assertIsNone(intake.valid_extended_rule(
                {**old,"verification":"untrusted"},SHA))
    def test_no_untrusted_extra_fields_or_title_samples(self):
        row=technical()
        row["samples"]=["Private.Movie.2024"]
        self.assertIsNone(intake.valid_extended_rule(row,SHA))
