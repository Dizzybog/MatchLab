"""Offline checks for the controlled HTTP evaluation harness; no network."""
import json
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1]))
from diagnostics import audit_api as audit


def reply(body=None, status=200, headers=None, error=None):
    return audit.response_data(status, headers or {}, json.dumps(body or {}).encode(),
                               0.002, error)


class FakeTransport:
    name = "offline_fake"

    def __init__(self, responses):
        self.responses = iter(responses)
        self.calls = []

    def request(self, *arguments):
        self.calls.append(arguments)
        return next(self.responses)


def case(identifier="one", expected=None, **extra):
    return {"id": identifier, "request": {"title": "Example.S01E01"},
            "expected": expected, **extra}


class ApiAuditTests(unittest.TestCase):
    def test_partial_objects_exact_arrays_boolean_and_missing_null(self):
        self.assertEqual(audit.compare_subset({"a": {"b": [1, True]}},
                                             {"a": {"b": [1, True], "other": 3}}), [])
        self.assertTrue(audit.compare_subset({"x": None}, {}))
        self.assertTrue(audit.compare_subset({"x": True}, {"x": 1}))
        self.assertTrue(audit.compare_subset({"a": [1]}, {"a": [1, 2]}))

    def test_source_assertions_preserve_missing_vs_null_and_coercions(self):
        assertions = [{"path": ["missing"], "op": "absent"},
                      {"path": ["missing"], "op": "ne", "value": None},
                      {"path": ["items"], "op": "eq", "coercion": "len", "value": 2},
                      {"path": ["items", 0], "op": "eq", "value": 4}]
        self.assertEqual(audit.compare_assertions(assertions, {"items": [4, 5]}), [])
        self.assertEqual(len(audit.compare_assertions(assertions,
                                                      {"missing": None, "items": [4, 5]})), 2)
        self.assertTrue(audit.compare_assertions(
            [{"path": ["object"], "op": "eq", "value": {"one": 1}}],
            {"object": {"one": 1, "two": 2}}))

    def test_parser_adapter_and_unsupported_translation_never_calls_api(self):
        inputs = {"cases": [{"title": "Example", "translate_languages": False,
                             "expected": [{"path": ["title"], "op": "eq", "value": "Example"}],
                             "sources": [{"file": "test.rs", "line": 12}]},
                            {"title": "English", "translate_languages": True, "expected": []}]}
        cases = audit.validate_cases(inputs)
        transport = FakeTransport([reply({"title": "Example"})])
        report = audit.run_audit(cases, "https://example.invalid", transport=transport,
                                  delay=0, sleep=lambda _: None)
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(transport.calls[0][0], "https://example.invalid/parse")
        self.assertEqual(report["summary"]["assertions"]["passed"], 1)
        self.assertEqual(report["results"][1]["outcome"], "unsupported_configuration")
        self.assertEqual(report["summary"]["identifier_metrics"], {})

    def test_429_retry_honors_delay_and_other_errors_are_not_retried(self):
        waits = []
        transport = FakeTransport([reply(status=429, headers={"retry-after": "2"}),
                                   reply({"title": "Example"}), reply(status=503)])
        report = audit.run_audit([case(expected={"title": "Example"}), case("two")],
                                  "https://example.invalid", transport=transport,
                                  sleep=waits.append)
        self.assertIn(2.0, waits)
        self.assertEqual(len(transport.calls), 3)
        self.assertEqual(report["results"][1]["outcome"], "http_error")
        self.assertEqual(report["summary"]["latency_ms"], {"p50": 2.0, "p95": 2.0})
        self.assertEqual(audit.retry_after("Thu, 01 Jan 1970 00:01:00 GMT", now=10), 50)

    def test_large_retry_after_stops_remaining_requests(self):
        transport = FakeTransport([reply(status=429, headers={"retry-after": "120"})])
        report = audit.run_audit([case(), case("two")], "https://example.invalid",
                                  transport=transport, sleep=lambda _: None)
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(report["results"][0]["outcome"], "rate_limited")
        self.assertEqual(report["results"][1]["outcome"], "not_run_rate_limit")

    def test_metrics_exclude_partial_reference_and_unknown_expectations(self):
        positive = {"sid_start": "tt1234567:1:1", "sid_end": "tt1234567:1:1"}
        negative = {"sid_start": None, "sid_end": None}
        wrong = {"sid_start": "tt7654321:1:1", "sid_end": "tt7654321:1:1"}
        cases = [case("true", positive, classification="labeled"),
                 case("wrong", positive, classification="labeled"),
                 case("abstain", negative, classification="labeled"),
                 case("false-positive", negative, classification="labeled"),
                 case("partial", {"sid_start": "tt1234567:1:1"}, classification="labeled"),
                 case("reference", positive, classification="differential_reference"),
                 case("unknown"), case("unavailable", positive, classification="labeled")]
        transport = FakeTransport([reply(body) for body in
                                   (positive, wrong, negative, wrong, positive, wrong, wrong)]
                                  + [reply(status=503)])
        report = audit.run_audit(cases, "https://example.invalid", transport=transport,
                                  delay=0, sleep=lambda _: None)
        metrics = report["summary"]["identifier_metrics"]["sid"]
        self.assertEqual(metrics["labeled_cases"], 5)
        self.assertEqual(metrics["true_matches"], 1)
        self.assertEqual(metrics["wrong_ids"], 2)
        self.assertEqual(metrics["correct_abstentions"], 1)
        self.assertEqual(metrics["unavailable"], 1)
        self.assertAlmostEqual(metrics["precision"], 1 / 3)
        self.assertAlmostEqual(metrics["match_recall"], 1 / 3)

    def test_cloudflare_block_is_transport_outcome(self):
        transport = FakeTransport([reply({"error": "error code: 1010"}, status=403,
                                         headers={"server": "cloudflare"})])
        report = audit.run_audit([case(expected={"title": "Example"})],
                                  "https://example.invalid", transport=transport,
                                  delay=0, sleep=lambda _: None)
        self.assertEqual(report["results"][0]["outcome"], "transport_blocked")
        self.assertIsNone(report["results"][0]["comparison"])

    def test_curl_transport_uses_argument_list_and_bounds_response(self):
        observed = []

        def run(arguments, **options):
            observed.append((arguments, options))
            Path(arguments[arguments.index("--dump-header") + 1]).write_text(
                "HTTP/1.1 200 Connection established\r\n\r\n"
                "HTTP/2 200\r\nContent-Type: application/json\r\n\r\n")
            Path(arguments[arguments.index("--output") + 1]).write_bytes(b"0123456789")
            return SimpleNamespace(stdout=b'{"http_code":200}', stderr=b"too large", returncode=63)

        with patch.object(audit.subprocess, "run", side_effect=run):
            transport = audit.CurlTransport()
            result = transport.request("https://example.invalid/parse", {"title": "$(literal)"}, 20, 5)
        arguments, options = observed[0]
        self.assertIsInstance(arguments, list)
        self.assertNotIn("shell", options)
        self.assertNotIn("--insecure", arguments)
        self.assertNotIn("--user-agent", arguments)
        self.assertEqual(result["error"]["kind"], "response_too_large")
        self.assertEqual(result["raw_body"], "01234")
        self.assertEqual(result["headers"]["content-type"], "application/json")


if __name__ == "__main__":
    unittest.main()
