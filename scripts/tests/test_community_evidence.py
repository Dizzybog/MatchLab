"""Source-aware evidence selection never turns shared reports into labels."""
from datetime import datetime, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_evidence as evidence

NOW = datetime(2026, 11, 11, 12, tzinfo=timezone.utc)


class EvidenceTests(unittest.TestCase):
    def test_real_title_only_with_same_batch_and_matching_source_marker(self):
        candidate = {"fingerprint": "a" * 64, "kind": "learned_field_regex",
                     "signal": "opus", "field": "audio"}
        unrelated = {"fingerprint": "b" * 64, "kind": "learned_field_regex",
                     "signal": "opus", "field": "audio"}
        reports = [
            {"title": "Some.Show.S01E01.Opus.720p"},
            {"title": "Another.Movie.2024.x265.mkv"},
            {"title": "Private.Show.token=secret.Opus"},
            {"title": "Some.Show.S01E01.Opus.720p"},
        ]

        def validate_rules(comment, sha):
            return [candidate] if comment.get("id") == 1 else [unrelated]

        def validate_reports(comment, sha):
            return reports

        comments = [[{"id": 1, "created_at": NOW.isoformat()},
                     {"id": 2, "created_at": NOW.isoformat()}]]
        result = evidence.gather(
            comments, [candidate], "a" * 40, NOW,
            validate_rules=validate_rules,
            validate_reports=validate_reports)
        self.assertEqual(result[candidate["fingerprint"]],
                         ["Some.Show.S01E01.Opus.720p"])

    def test_regex_injection_and_bad_source_are_never_compiled(self):
        self.assertIsNone(evidence.marker_regex(
            {"kind": "learned_field_regex", "signal": "(.*)+"}))
        self.assertIsNone(evidence.marker_regex(
            {"kind": "learned_general_regex_review", "regex": "(.*)+"}))
        self.assertIsNone(evidence.marker_regex(
            {"kind": "learned_tv_regex", "family": "attacker"}))
        self.assertIsNotNone(evidence.marker_regex(
            {"kind": "learned_general_regex_review",
             "regex": r"(?i)(?:^|[ ._-])(Directors[ ._-]+Cut)(?:$|[ ._-])"}))

    def test_no_rules_never_processes_reports(self):
        result = evidence.gather([], [], "a" * 40, NOW,
                                 validate_rules=lambda *_: [],
                                 validate_reports=lambda *_: [])
        self.assertEqual(result, {})


if __name__ == "__main__":
    unittest.main()
