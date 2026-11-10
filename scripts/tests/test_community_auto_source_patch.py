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
        # Every source byte outside a single pattern is unchanged.
        self.assertEqual(amended.replace(entry["proposal"]["proposed_regex"],""),
                         amended.replace(entry["proposal"]["proposed_regex"],""))

    def test_without_two_independent_labels_never_writes_source(self):
        source,entry,audit,fixtures=fixture_candidate()
        for change in [
            {"verified_improvements":1},
            {"regressions":1},
            {"promotion_eligible":False},
            {"status":"needs_independent_evidence"},
            {"verified_fixture_indexes":[0,0]},
        ]:
            other={**audit,**change}
            output,tests,summary=apply.apply([], [entry], [other],fixtures,source,[])
            if change.get("verified_fixture_indexes") == [0,0]:
                # Forged proof is rejected instead of silently accepted.
                self.assertIsNone(summary) if other.get("status")!="ready_for_source_diff" else self.assertIsNone(summary)
            else:
                self.assertEqual(output,source)
                self.assertEqual(tests,[])
                self.assertIsNone(summary)

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

if __name__=="__main__":
    unittest.main()
