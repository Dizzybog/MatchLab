"""Regression tests for community PRs that must contain real JSON changes."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0,str(Path(__file__).parents[1]))
import community_review_diff as review


class ReviewDiffTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)

    def files(self,old_rules,new_rules,old_extra="[]",new_extra="[]"):
        out=[]
        for i,text in enumerate((old_rules,new_rules,old_extra,new_extra)):
            path=self.root / f"{i}.json"
            path.write_text(text,encoding="utf8")
            out.append(path)
        return out

    def test_empty_json_formatting_does_not_create_pr(self):
        paths=self.files("[\n]\n","[]\n","[]\n","[\n]\n")
        self.assertFalse(review.check(*paths))
        self.assertEqual(paths[1].read_bytes(),paths[0].read_bytes())
        self.assertEqual(paths[3].read_bytes(),paths[2].read_bytes())

    def test_reordered_json_object_keys_are_semantically_equal(self):
        paths=self.files('[{"a":1,"b":2}]','[{"b":2,"a":1}]')
        self.assertFalse(review.check(*paths))

    def test_real_review_data_creates_pr_and_preserves_unmodified_rule_file(self):
        paths=self.files("[\n]\n","[]\n","[]","[{\"fingerprint\":\"ok\"}]")
        self.assertTrue(review.check(*paths))
        self.assertEqual(paths[1].read_bytes(),paths[0].read_bytes())
        self.assertEqual(json.loads(paths[3].read_text()),[{"fingerprint":"ok"}])

    def test_unparseable_review_json_fails_closed(self):
        paths=self.files("[]","not-json")
        with self.assertRaises(ValueError):
            review.check(*paths)

if __name__=="__main__":
    unittest.main()
