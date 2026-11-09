import hashlib
import sys
from pathlib import Path
import unittest

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
import community_backlog as backlog

def rule(marker):
    return {"kind":"historical_regex_review",
        "fingerprint":hashlib.sha256(marker.encode()).hexdigest(),
        "regex":marker}

class BacklogTests(unittest.TestCase):
    def test_offline_history_is_deduped_against_pending_and_curated(self):
        old={"pending":[rule("one"),rule("two")]}
        done={rule("one")["fingerprint"]}
        got=backlog.merge(old,[rule("two"),rule("three")],done,set())
        self.assertEqual([p["regex"] for p in got["pending"]],
                         ["two","three"])
    def test_failed_pr_does_not_ack_and_rotates_unreviewed_items(self):
        a,b,c=rule("a"),rule("b"),rule("c")
        queue={"pending":[a,b,c]}
        self.assertEqual([r["regex"] for r in
            backlog.finalize(queue,[a["fingerprint"]],[])["pending"]],
            ["b","c","a"])
        self.assertEqual([r["regex"] for r in
            backlog.finalize(queue,[a["fingerprint"]],[a["fingerprint"]])["pending"]],
            ["b","c"])
    def test_capacity_failure_never_truncates_queue(self):
        with self.assertRaises(ValueError):
            backlog.merge({"pending":[rule(str(i)) for i in
                    range(backlog.MAX_PENDING+1)]},[],set(),set())
    def test_merged_anime_and_extended_fingerprints_are_globally_known(self):
        marker="epis"
        fp=hashlib.sha256(("community-regex:"+marker).encode()).hexdigest()
        curated=backlog.curated_fingerprints(
            [{"marker":marker}],[{"fingerprint":rule("z")["fingerprint"]}])
        self.assertIn(fp,curated)
        self.assertIn(rule("z")["fingerprint"],curated)

if __name__=="__main__":
    unittest.main()
