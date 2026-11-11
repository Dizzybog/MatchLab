"""Cross-family native source planning, safe source diffs and PR gates."""
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
from community_regex_plan import resolve, build, anchored_general
from community_apply_verified_regex import apply
from community_rule_intake import (
    FIELD_EPOCH, TV_EPOCH, TV_NATIVE_PATTERNS, TV_GRAMMARS,
    digest, technical_pattern,
)
from review_community_regex import screen

CATALOG = json.loads(
    (Path(__file__).resolve().parents[2] /
     "crates/matchlab-parser/src/generated/handlers.json").read_text())
HANDLERS = CATALOG["handlers"]
SOURCE = (
    Path(__file__).resolve().parents[2] /
    "crates/matchlab-parser/src/generated/handlers.json"
).read_text()
SHA = "a" * 40


def field_rule(field, signal, canonical):
    token = json.dumps(canonical, ensure_ascii=False, separators=(",", ":"))
    return {
        "kind": "learned_field_regex", "field": field,
        "signal": signal, "canonical": canonical,
        "regex": technical_pattern(field, signal),
        "source_build": SHA + ":" + str(FIELD_EPOCH),
        "evidence_count": 24, "distinct_titles": 8,
        "canonical_confirmations": 8,
        "verification": "local_canonical_and_explicit_release_marker",
        "fingerprint": digest(
            f"community-field:{field}:{signal}:{token}"),
    }


def tv_rule(family="sxe"):
    return {
        "kind": "learned_tv_regex", "family": family,
        "fields": ["seasons", "episodes"],
        "regex": TV_GRAMMARS[family],
        "native_patterns": TV_NATIVE_PATTERNS[family],
        "source_build": SHA + ":" + str(TV_EPOCH),
        "evidence_count": 24, "distinct_titles": 8,
        "canonical_confirmations": 8,
        "verification": "explicit_tv_marker_and_canonical_confirmations",
        "fingerprint": digest("community-tv:" + family),
    }


def general_rule(index, mode, regex):
    field = HANDLERS[index]["name"]
    return {
        "kind": "learned_general_regex_review", "field": field,
        "handler_index": index, "mode": mode, "regex": regex,
        "review_only": True, "source_build": SHA + ":1",
        "verification": "native_handler_metamorphic_replay_requires_upstream_review",
        "evidence_count": 24, "distinct_titles": 8,
        "canonical_confirmations": 8,
        "fingerprint": digest(
            f"general:{field}:{index}:{mode}:{regex}"),
    }


def eligible(rule, changes, fixtures):
    return {
        "fingerprint": rule["fingerprint"],
        "status": "ready_for_source_diff",
        "promotion_eligible": True, "regressions": 0,
        "verified_improvements": max(2, len(changes)),
        "distinct_verified_titles": 2,
        "verified_fixture_indexes": [0, 1],
        "verified_fields": sorted({x["field"] for x in changes}),
        "source_changes": changes,
    }


def reviewed(rule):
    return {"fingerprint": rule["fingerprint"],
            "status": "review_required", "automatic_activation": False,
            "proposal": rule}


class NativePlanTests(unittest.TestCase):
    def test_field_families_select_immutable_transform_template(self):
        for field, marker, expected in [
            ("audio", "opus", ["OPUS"]),
            ("codec", "h264", "avc"),
            ("quality", "bluray", "BluRay"),
            ("resolution", "1440p", "1440p"),
            ("extension", "mp4", "mp4"),
            ("languages", "english", ["en"]),
        ]:
            rule = field_rule(field, marker, expected)
            changes, reason = resolve(rule, HANDLERS)
            self.assertEqual(reason, "replay_ready", field)
            self.assertEqual(len(changes), 1, field)
            op = changes[0]
            self.assertEqual(op["operation"], "add")
            self.assertEqual(HANDLERS[op["handler_index"]]["name"], field)
            self.assertEqual(
                op["transform"], HANDLERS[op["handler_index"]]["transform"])
            if field == "extension":
                self.assertEqual(op["pattern"], r"(?i)\.mp4$")
            self.assertEqual(list(build([rule], HANDLERS)), [rule["fingerprint"]])

    def test_native_tv_coordinates_stay_an_atomic_two_rule_change(self):
        for family in TV_NATIVE_PATTERNS:
            rule = tv_rule(family)
            ops, reason = resolve(rule, HANDLERS)
            self.assertEqual(reason, "replay_ready")
            self.assertEqual([x["field"] for x in ops], ["seasons", "episodes"])
            self.assertTrue(all(x["operation"] == "add" for x in ops))
            self.assertEqual(
                [x["pattern"] for x in ops],
                [TV_NATIVE_PATTERNS[family]["seasons"],
                 TV_NATIVE_PATTERNS[family]["episodes"]])

    def test_general_add_and_restricted_replacement(self):
        original = general_rule(
            80, "add",
            r"(?i)(?:^|[ ._-])(Directors[ ._-]+Cut)(?:$|[ ._-])")
        self.assertTrue(anchored_general(original["regex"]))
        additions, why = resolve(original, HANDLERS)
        self.assertEqual(why, "replay_ready")
        self.assertEqual(additions[0]["operation"], "add")
        correction = general_rule(80, "correct", original["regex"])
        patches, why = resolve(correction, HANDLERS)
        self.assertEqual(why, "replay_ready")
        self.assertEqual(patches[0]["operation"], "patch")
        self.assertTrue(patches[0]["pattern"].startswith("(?:"))
        forged = {**original, "handler_index": 73}
        self.assertEqual(resolve(forged, HANDLERS)[0], [])

    def test_corrective_uses_existing_tv_source_and_never_guesses(self):
        src = next((i for i, row in enumerate(HANDLERS)
                    if row.get("kind") == "regex" and row.get("name") == "episodes"
                    and row.get("transform") == "array(integer)"
                    and row.get("options", {}).get("remove") is False
                    and row.get("options", {}).get("skipIfAlreadyFound") is True))
        rule = {
            "kind": "learned_corrective_regex", "field": "episodes",
            "handler_index": src, "family": "sxe",
            "regex": TV_NATIVE_PATTERNS["sxe"]["episodes"],
        }
        ops, reason = resolve(rule, HANDLERS)
        self.assertEqual(reason, "replay_ready")
        self.assertEqual(ops[0]["operation"], "correct")
        self.assertEqual(resolve(
            {**rule, "handler_index": 229}, HANDLERS)[0], [])

    def test_ambiguous_and_historical_proposals_are_review_only(self):
        self.assertEqual(resolve({
            "kind": "learned_field_regex_review", "field": "network",
            "signal": "futuretag", "regex": "abc",
        }, HANDLERS)[0], [])
        self.assertEqual(resolve({
            "kind": "historical_regex_review", "field": "audio"
        }, HANDLERS)[0], [])
        self.assertEqual(resolve({
            "kind": "learned_general_regex_review", "field": "edition",
            "handler_index": 80, "regex": r"(?s)(.*)+", "mode": "add",
        }, HANDLERS)[0], [])

    def test_invalid_untrusted_proposal_cannot_enter_plan(self):
        rule = field_rule("audio", "opus", ["OPUS"])
        altered = {**rule, "regex": r"(?s)(.*)+"}
        with self.assertRaises(ValueError):
            build([altered], HANDLERS)


class NativeReviewTests(unittest.TestCase):
    def fixtures(self, field):
        return [
            {"title": "Distinct.Show.S01E01.Opus.WEB-DL",
             "expected": {field: ["OPUS"]}},
            {"title": "Second.Movie.2025.Opus.mkv",
             "expected": {field: ["OPUS"]}},
        ]

    def test_source_append_contains_exact_native_template_and_regressions(self):
        rule = field_rule("audio", "opus", ["OPUS"])
        changes, _ = resolve(rule, HANDLERS)
        fixtures = self.fixtures("audio")
        audit = eligible(rule, changes, fixtures)
        reviewed_rules, changed = screen([rule], [], [audit])
        self.assertTrue(changed)
        amended, promoted, summary = apply(
            [], reviewed_rules, [audit], fixtures, SOURCE, [])
        self.assertEqual(len(promoted), 2)
        self.assertEqual(summary["fields"], ["audio"])
        parsed = json.loads(amended)["handlers"]
        self.assertEqual(parsed[:len(HANDLERS)], HANDLERS)
        self.assertEqual(len(parsed), len(HANDLERS) + 1)
        self.assertEqual(parsed[-1]["pattern"], changes[0]["pattern"])
        self.assertEqual(parsed[-1]["transform"], changes[0]["transform"])

    def test_tv_source_appends_two_native_handlers_without_reformatting(self):
        rule = tv_rule()
        changes, _ = resolve(rule, HANDLERS)
        fixtures = [
            {"title": "First.Show.S04E02.WEB", "expected": {
                "seasons": [4], "episodes": [2]}},
            {"title": "Different.Show.S08E10.WEB", "expected": {
                "seasons": [8], "episodes": [10]}},
        ]
        audit = eligible(rule, changes, fixtures)
        reviewed_rules, accepted = screen([rule], [], [audit])
        self.assertTrue(accepted)
        amended, promoted, summary = apply(
            [], reviewed_rules, [audit], fixtures, SOURCE, [])
        rows = json.loads(amended)["handlers"]
        self.assertEqual(len(rows), len(HANDLERS) + 2)
        self.assertEqual([r["name"] for r in rows[-2:]],
                         ["seasons", "episodes"])
        self.assertTrue(all(r["learned_tv_guard"] for r in rows[-2:]))
        self.assertEqual(summary["operations"], ["add", "add"])

    def test_forged_audit_plan_never_writes_a_source_diff(self):
        rule = field_rule("audio", "opus", ["OPUS"])
        changes, _ = resolve(rule, HANDLERS)
        fixtures = self.fixtures("audio")
        audit = eligible(rule, changes, fixtures)
        bad = {**audit, "source_changes": [
            {**changes[0], "pattern": r"(?s)(.*)+"}
        ]}
        with self.assertRaises(ValueError):
            apply([], [reviewed(rule)], [bad], fixtures, SOURCE, [])
        unproved = {**audit, "verified_fields": []}
        with self.assertRaises(ValueError):
            apply([], [reviewed(rule)], [unproved], fixtures, SOURCE, [])
        unresolved = {**audit, "promotion_eligible": False}
        self.assertEqual(
            apply([], [reviewed(rule)], [unresolved],
                  fixtures, SOURCE, [])[2], None)


if __name__ == "__main__":
    unittest.main()
