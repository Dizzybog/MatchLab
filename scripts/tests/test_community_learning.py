"""Security regression tests for untrusted community learning reports."""
import json
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_learning as cl

SHA = "a" * 40


def issue(overrides=None):
    obj = {
        "protocol": 1, "learning_epoch": 1, "build_sha": SHA,
        "source": "matchlab-community", "title": "Bleach_338",
        "status": "matched", "alerts": ["unresolved_absolute_anime_episode"],
        "parsed": {"title": "Bleach", "seasons": [], "episodes": [338]},
        "selected": {"imdb_id": "tt0434665", "season": None, "episode": None},
        "verified": False,
    }
    if overrides:
        obj.update(overrides)
    return {
        "title": "[community-learning] parser anomaly",
        "body": cl.MARKER + "\n" + json.dumps(obj),
        "number": 1, "created_at": datetime.now(timezone.utc).isoformat()
    }


class CommunityLearningSecurityTests(unittest.TestCase):
    def test_accepts_only_current_exact_build(self):
        self.assertIsNotNone(cl.validate(issue(), SHA))
        self.assertIsNone(cl.validate(issue(), "b" * 40))
        self.assertIsNone(cl.validate(issue({"learning_epoch": 0}), SHA))
        self.assertIsNone(cl.validate(issue({"protocol": 2}), SHA))

    def test_never_promotes_community_labels_to_verified_truth(self):
        self.assertIsNone(cl.validate(issue({"verified": True}), SHA))
        self.assertIsNone(cl.validate(issue({"verified": "true"}), SHA))
        self.assertIsNone(cl.validate(issue({"extra_token": "secret"}), SHA))

    def test_drops_public_private_paths_and_metadata(self):
        for title in ("/home/user/private.mkv", "C:\\Private\\A.mkv",
                      "myemail@example.org", "https://example.com/1",
                      "token=private", "A\nB"):
            self.assertIsNone(cl.validate(issue({"title": title}), SHA))
        self.assertIsNone(cl.validate(issue({"parsed":
            {"title":"Show","episodes":[1],"seasons":[],"private":"secret"}}), SHA))

    def test_dedup_and_recency(self):
        now = datetime.now(timezone.utc)
        self.assertEqual(len(cl.intake([issue(), issue()], SHA, now)), 1)
        self.assertEqual(len(cl.intake([issue()], "c" * 40, now)), 0)

    def test_no_ml_model_without_independent_verified_truth(self):
        self.assertIsNone(cl.train([], SHA))
        self.assertIsNone(cl.fit([]))


if __name__ == "__main__":
    unittest.main()
