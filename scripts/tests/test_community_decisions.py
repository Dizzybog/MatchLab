"""Crash-safe decision cache: no repeated useless audits until source changes."""
from datetime import datetime, timezone
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_decisions as decisions
import community_state


class DecisionCacheTests(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.TemporaryDirectory()
        self.addCleanup(self.root.cleanup)
        self.path = Path(self.root.name)
        self.handlers = self.path / "handlers.json"
        self.fixtures = self.path / "fixtures.json"
        self.handlers.write_text('{"handlers":[]}')
        self.fixtures.write_text("[]")
        self.sign = decisions.signature(self.handlers, self.fixtures)
        self.a = "a" * 64
        self.b = "b" * 64

    def test_same_evidence_is_cached_but_changed_source_or_fixtures_rechecks(self):
        pending = [{"fingerprint": self.a}, {"fingerprint": self.b}]
        state = {"decisions": {
            self.a: {"status": "no_effect", "signature": self.sign,
                     "checked_at": "2026-11-11T12:00:00+00:00"}
        }}
        due, deferred = decisions.due(pending, state, self.sign)
        self.assertEqual([r["fingerprint"] for r in due], [self.b])
        self.assertEqual(deferred, 1)
        self.fixtures.write_text('[{"title":"new trusted fixture"}]')
        next_sign = decisions.signature(self.handlers, self.fixtures)
        self.assertNotEqual(self.sign, next_sign)
        due, deferred = decisions.due(pending, state, next_sign)
        self.assertEqual(len(due), 2)
        self.assertEqual(deferred, 0)

    def test_parser_source_tree_changes_invalidate_cached_verdicts(self):
        native = self.path / "parser"
        native.mkdir()
        (native / "lib.rs").write_text("stable lib")
        before = decisions.signature(self.handlers, self.fixtures, native)
        (native / "prefilter.rs").write_text("new prefilter")
        after = decisions.signature(self.handlers, self.fixtures, native)
        self.assertNotEqual(before, after)
        (native / "prefilter.rs").write_text("modified prefilter")
        self.assertNotEqual(after, decisions.signature(
            self.handlers, self.fixtures, native))

    def test_all_nonpromoted_decisions_are_cached_and_review_ready_is_not(self):
        pending = [{"fingerprint": self.a}]
        for status in sorted(decisions.DECISIONS):
            state = {"decisions": {
                self.a: {"status": status, "signature": self.sign,
                         "checked_at": "2026-11-11T12:00:00+00:00"}
            }}
            due, deferred = decisions.due(pending, state, self.sign)
            if status == "ready_for_source_diff":
                self.assertEqual(len(due), 1)
            else:
                self.assertEqual((len(due), deferred), (0, 1))

    def test_update_keeps_pending_only_and_never_trusts_arbitrary_status(self):
        pending = [{"fingerprint": self.a}]
        before = {"decisions": {
            self.b: {"status": "no_effect", "signature": self.sign,
                     "checked_at": "2026-11-11T12:00:00+00:00"}
        }}
        after = decisions.update(before, [{
            "fingerprint": self.a, "status": "needs_independent_evidence",
        }], pending, self.sign, "2026-11-11T13:00:00+00:00")
        self.assertEqual(set(after["decisions"]), {self.a})
        self.assertEqual(after["decisions"][self.a]["signature"], self.sign)
        with self.assertRaises(ValueError):
            decisions.update(before, [{
                "fingerprint": self.a, "status": "merge_without_review",
            }], pending, self.sign, "now")

    def test_versioned_release_state_round_trip_and_corruption_guards(self):
        from test_community_state import FakeState
        store = FakeState(missing=True)
        path = self.path / "decisions.json"
        store.load("decisions", path)
        self.assertEqual(json.loads(path.read_text()), {"decisions": {}})
        payload = {"decisions": {
            self.a: {"status": "regression", "signature": self.sign,
                     "checked_at": "2026-11-11T12:00:00+00:00"}
        }}
        path.write_text(json.dumps(payload))
        store.save("decisions", path, "final", "901", "1")
        store.load("decisions", path)
        self.assertEqual(json.loads(path.read_text()), payload)
        with self.assertRaises(ValueError):
            community_state.normalize("decisions", b'{"decisions":{"bad":{}}}')
        with self.assertRaises(ValueError):
            community_state.normalize("decisions", b'{"pending":[]}')


if __name__ == "__main__":
    unittest.main()
