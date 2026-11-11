"""Fail-closed tests for community source-regex merge automation."""
import json
import sys
import unittest
from pathlib import Path
sys.path.insert(0,str(Path(__file__).parents[1]))
import community_apply_verified_regex as apply
from community_rule_intake import technical_pattern

CATALOG=Path(__file__).resolve().parents[2]/"crates/matchlab-parser/src/generated/handlers.json"

def fixture_candidate():
    original=CATALOG.read_text()
    handlers=json.loads(original)["handlers"]
    index=next(i for i,h in enumerate(handlers)
               if h.get("name")=="codec" and
               h.get("pattern")==r"\b[hx][\. \-]?264\b")
    native=handlers[index]
    replacement=f'(?:{native["pattern"]})|(?:{technical_pattern("codec","h264")})'
    rule={"kind":"learned_source_regex_patch","fingerprint":"a"*64,
          "field":"codec","handler_index":index,"signal":"h264",
          "original_regex":native["pattern"],"proposed_regex":replacement,
          "transform":native["transform"]}
    entry={"status":"review_required","automatic_activation":False,
           "fingerprint":rule["fingerprint"],"proposal":rule}
    audit={"fingerprint":rule["fingerprint"],"status":"ready_for_source_diff",
           "promotion_eligible":True,"regressions":0,"verified_improvements":2,
           "verified_fixture_indexes":[0,1]}
    fixtures=[{"title":f"Fixture.Show.S01E{n:02d}.h264.WEB-DL",
               "expected":{"codec":"avc"}} for n in (1,2)]
    return original,entry,audit,fixtures

class AutoSourcePatchTests(unittest.TestCase):
    def test_single_handler_source_diff_keeps_all_other_json_identical(self):
        source,entry,audit,fixtures=fixture_candidate()
        amended,tests,summary=apply.apply([], [entry], [audit],fixtures,source,[])
        self.assertIsNotNone(summary)
        before=json.loads(source)["handlers"]
        after=json.loads(amended)["handlers"]
        self.assertEqual(len(before),len(after))
        changed=[i for i,(a,b) in enumerate(zip(before,after)) if a!=b]
        self.assertEqual(changed,[entry["proposal"]["handler_index"]])
        self.assertEqual(len(tests),2)
        self.assertEqual(summary["verified_improvements"],2)
        # Exactly ONE line of the generated catalogue is modified.
        old_lines=source.splitlines()
        new_lines=amended.splitlines()
        self.assertEqual(len(old_lines),len(new_lines))
        self.assertEqual(sum(a!=b for a,b in zip(old_lines,new_lines)),1)

    def test_without_two_independent_labels_never_writes_source(self):
        source,entry,audit,fixtures=fixture_candidate()
        for change in [
            {"verified_improvements":1},
            {"regressions":1},
            {"promotion_eligible":False},
            {"status":"needs_independent_evidence"},
        ]:
            other={**audit,**change}
            output,tests,summary=apply.apply([], [entry], [other],fixtures,source,[])
            self.assertEqual(output,source)
            self.assertEqual(tests,[])
            self.assertIsNone(summary)
        forged={**audit,"verified_fixture_indexes":[0,0]}
        with self.assertRaises(ValueError):
            apply.apply([], [entry], [forged],fixtures,source,[])

    def test_a_rule_without_source_identity_is_never_merged(self):
        source,entry,audit,fixtures=fixture_candidate()
        entry["proposal"]["kind"]="historical_regex_review"
        self.assertIsNone(apply.apply([], [entry], [audit],fixtures,source,[])[2])

    def test_invalid_source_original_fails_closed(self):
        source,entry,audit,fixtures=fixture_candidate()
        entry["proposal"]["original_regex"]=".*"
        with self.assertRaises(ValueError):
            apply.apply([], [entry], [audit],fixtures,source,[])

    def test_no_new_review_entry_does_not_regenerate_code(self):
        source,entry,audit,fixtures=fixture_candidate()
        output,tests,summary=apply.apply(
            [entry],[entry],[audit],fixtures,source,[])
        self.assertEqual((output,tests,summary),(source,[],None))

class CommunityAutoWorkflowIntegrationTests(unittest.TestCase):
    def test_validated_rule_is_reviewed_and_stages_only_verified_source_patch(self):
        from community_rule_intake import digest, valid_extended_rule, FIELD_EPOCH
        from review_community_regex import screen
        source,entry,audit,fixtures=fixture_candidate()
        rule=entry["proposal"]
        original=rule["original_regex"]
        proposed=rule["proposed_regex"]
        index=rule["handler_index"]
        proposal={
            **rule,
            "evidence_count":24,"distinct_titles":8,
            "canonical_confirmations":8,
            "source_build":"a"*40+":"+str(FIELD_EPOCH),
            "verification":"source_catalog_and_canonical_release_markers",
            "fingerprint":digest(f"community-patch:{index}:{original}:{proposed}"),
        }
        audit={**audit,"fingerprint":proposal["fingerprint"],
            "distinct_verified_titles":2}
        self.assertIsNotNone(valid_extended_rule(proposal,"a"*40))
        reviewed,changed=screen([proposal],[],[audit])
        self.assertTrue(changed)
        self.assertEqual(len(reviewed),1)
        self.assertFalse(reviewed[0]["automatic_activation"])
        amended,tests,summary=apply.apply(
            [],reviewed,[audit],fixtures,source,[])
        self.assertIsNotNone(summary)
        self.assertEqual(tests,fixtures)
        before=json.loads(source)["handlers"]
        after=json.loads(amended)["handlers"]
        self.assertEqual([i for i,(old,new) in enumerate(zip(before,after))
                          if old!=new],[index])
        self.assertEqual(after[index]["pattern"],proposed)
        # Already-reviewed suggestions and proven no-ops do not open PRs.
        self.assertEqual(screen([proposal],reviewed,[audit]),(reviewed,False))
        noop,changed=screen([proposal],[],[{**audit,"status":"no_effect"}])
        self.assertFalse(changed)
        self.assertEqual(noop,[])

    def test_workflow_promotes_only_after_trusted_rust_audit_and_tests(self):
        workflow=(Path(__file__).resolve().parents[2]/
            ".github/workflows/community-learning.yml").read_text()
        audit=workflow.index("--example audit_community_regex")
        review=workflow.index("scripts/review_community_regex.py")
        patch=workflow.index("scripts/community_apply_verified_regex.py")
        test=workflow.index("RUSTFLAGS=\"-D warnings\" cargo test --workspace")
        create=workflow.index("gh pr create --repo")
        self.assertLess(audit,review)
        self.assertLess(review,patch)
        self.assertLess(patch,test)
        self.assertLess(test,create)
        self.assertIn("if: env.COMMUNITY_RULE_CHANGED == 'true'",workflow)
        self.assertIn("automatic_activation",Path(__file__).resolve().parents[1]
            .joinpath("review_community_regex.py").read_text())
        self.assertNotIn("gh pr merge",workflow)


if __name__=="__main__":
    unittest.main()
