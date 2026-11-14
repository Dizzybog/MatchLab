#!/usr/bin/env python3
"""Sequential, bounded HTTP evaluation of a labeled MatchLab JSON corpus.

Input is a list, or {"cases": [...]}. Each case has id, request, optional
expected (a partial response object), source, classification and notes.
Optional endpoint is /parse or /match. Only explicitly labeled/verified/
ground_truth/production_expectation cases contribute identifier metrics;
reference and differential observations are comparisons, never ground truth.

Example: python3 scripts/diagnostics/audit_api.py corpus.json --base-url URL
         --endpoint /parse --output report.json
No concurrency, semantic retries, extra packages or TLS bypasses are used.
"""
import argparse
from collections import Counter
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
import json
import hashlib
import math
import os
from pathlib import Path
import re
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPSHandler, Request, build_opener

ENDPOINTS = {"/parse", "/match"}
LABELED = {"labeled", "verified", "ground_truth", "production_expectation"}
MISSING = object()


def strict_json(text):
    def invalid(value):
        raise ValueError("Nonfinite JSON number: " + value)
    return json.loads(text, parse_constant=invalid)


def compare_subset(expected, actual, path="$", differences=None):
    """Objects are partial expectations; arrays and scalar values are exact."""
    if differences is None:
        differences = []
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key, value in expected.items():
            compare_subset(value, actual.get(key, MISSING), path + "." + key,
                           differences)
    elif isinstance(expected, list) and isinstance(actual, list):
        if len(expected) != len(actual):
            differences.append({"path": path + ".length", "expected": len(expected),
                                "actual": len(actual)})
        for index, value in enumerate(expected[:len(actual)]):
            compare_subset(value, actual[index], f"{path}[{index}]", differences)
    elif actual is MISSING or (isinstance(expected, bool) != isinstance(actual, bool)) or expected != actual:
        row = {"path": path, "expected": expected,
               "actual": None if actual is MISSING else actual}
        if actual is MISSING:
            row["actual_missing"] = True
        differences.append(row)
    return differences


def compare_assertions(assertions, actual):
    def exact_equal(left, right):
        if isinstance(left, dict):
            return (isinstance(right, dict) and set(left) == set(right)
                    and all(exact_equal(value, right[key]) for key, value in left.items()))
        if isinstance(left, list):
            return (isinstance(right, list) and len(left) == len(right)
                    and all(exact_equal(a, b) for a, b in zip(left, right)))
        return isinstance(left, bool) == isinstance(right, bool) and left == right

    differences = []
    for assertion in assertions:
        value = actual
        path = assertion["path"]
        for key in path:
            if isinstance(value, dict) and key in value:
                value = value[key]
            elif isinstance(value, list) and type(key) is int and 0 <= key < len(value):
                value = value[key]
            else:
                value = MISSING
                break
        if assertion.get("coercion") == "len" and value is not MISSING:
            value = len(value) if isinstance(value, (list, dict, str)) else MISSING
        operation = assertion["op"]
        expected = assertion.get("value")
        equal = value is not MISSING and exact_equal(expected, value)
        matches = (value is MISSING if operation == "absent" else
                   equal if operation == "eq" else not equal)
        if not matches:
            differences.append({"path": path, "op": operation,
                                "coercion": assertion.get("coercion"),
                                "expected": expected, "actual": None if value is MISSING else value,
                                "actual_missing": value is MISSING})
    return differences


def retry_after(value, now=None):
    if not value:
        return None
    try:
        seconds = float(value)
        if math.isfinite(seconds) and seconds >= 0:
            return seconds
    except ValueError:
        pass
    try:
        stamp = parsedate_to_datetime(value)
        if stamp.tzinfo is None:
            stamp = stamp.replace(tzinfo=timezone.utc)
        return max(0.0, stamp.timestamp() - (time.time() if now is None else now))
    except (ValueError, TypeError, OverflowError):
        return None


def read_headers(raw):
    headers = {}
    for block in re.split(r"\r?\n\r?\n", raw):
        lines = block.splitlines()
        if not lines or not lines[0].startswith("HTTP/"):
            continue
        headers = {}
        for line in lines[1:]:
            if ":" in line:
                key, value = line.split(":", 1)
                headers[key.lower()] = value.strip()
    return headers


def response_data(status, headers, body, elapsed, error=None):
    text = body.decode("utf-8", errors="replace")
    parsed = None
    json_error = None
    try:
        parsed = strict_json(text)
    except (ValueError, RecursionError) as exc:
        json_error = str(exc)
    return {"status": status, "headers": headers, "response": parsed,
            "raw_body": text, "elapsed_ms": round(elapsed * 1000, 3),
            "error": error, "json_error": json_error}


class CurlTransport:
    """Preserves normal curl identity, inherited proxy/CA settings and TLS."""
    name = "curl"

    def __init__(self):
        self.binary = shutil.which("curl")
        if self.binary is None:
            raise ValueError("curl is required; choose --transport urllib if unavailable")

    def request(self, url, payload, timeout, max_bytes):
        started = time.monotonic()
        with tempfile.TemporaryDirectory(prefix="matchlab-audit-") as folder:
            directory = Path(folder)
            data = directory / "request.json"
            data.write_text(json.dumps(payload, ensure_ascii=True), encoding="utf-8")
            headers, body = directory / "headers", directory / "body"
            command = [self.binary, "--silent", "--show-error", "--retry", "0",
                       "--max-time", str(timeout), "--max-filesize", str(max_bytes),
                       "--proto", "=http,https", "--request", "POST",
                       "--header", "Content-Type: application/json",
                       "--data-binary", "@" + str(data), "--dump-header", str(headers),
                       "--output", str(body), "--write-out", "%{json}", url]
            try:
                process = subprocess.run(command, capture_output=True, timeout=timeout + 5,
                                         check=False)
            except subprocess.TimeoutExpired:
                return response_data(None, {}, b"", time.monotonic() - started,
                                     {"kind": "timeout", "message": "curl process timed out"})
            try:
                metadata = strict_json(process.stdout.decode("utf-8"))
            except (ValueError, UnicodeError):
                metadata = {}
            raw = b""
            if body.exists():
                with body.open("rb") as stream:
                    raw = stream.read(max_bytes + 1)
            error = None
            if process.returncode or len(raw) > max_bytes:
                kind = ("timeout" if process.returncode == 28 else
                        "response_too_large" if process.returncode == 63 or len(raw) > max_bytes
                        else "network_error")
                error = {"kind": kind, "curl_exit_code": process.returncode,
                         "message": process.stderr.decode("utf-8", errors="replace")[:4096]}
            header_map = read_headers(headers.read_text(encoding="iso-8859-1")) if headers.exists() else {}
            return response_data(metadata.get("http_code") or None, header_map,
                                 raw[:max_bytes], time.monotonic() - started, error)


class UrllibTransport:
    name = "urllib"

    def __init__(self):
        # SSL_CERT_FILE/SSL_CERT_DIR are honored by the default SSL context.
        bundle = os.environ.get("REQUESTS_CA_BUNDLE") or os.environ.get("CURL_CA_BUNDLE")
        context = ssl.create_default_context(cafile=bundle)
        self.opener = build_opener(HTTPSHandler(context=context))

    def request(self, url, payload, timeout, max_bytes):
        started = time.monotonic()
        request = Request(url, data=json.dumps(payload).encode("utf-8"), method="POST",
                          headers={"Content-Type": "application/json"})
        try:
            try:
                response = self.opener.open(request, timeout=timeout)
            except HTTPError as exc:
                response = exc
            with response:
                body = response.read(max_bytes + 1)
                error = ({"kind": "response_too_large", "message": "Response exceeded byte limit"}
                         if len(body) > max_bytes else None)
                return response_data(response.code,
                                     {k.lower(): v for k, v in response.headers.items()},
                                     body[:max_bytes], time.monotonic() - started, error)
        except (URLError, OSError, TimeoutError) as exc:
            reason = getattr(exc, "reason", exc)
            kind = "timeout" if isinstance(reason, (socket.timeout, TimeoutError)) else "network_error"
            return response_data(None, {}, b"", time.monotonic() - started,
                                 {"kind": kind, "message": str(exc)})


def validate_cases(payload):
    cases = payload.get("cases") if isinstance(payload, dict) else payload
    if not isinstance(cases, list):
        raise ValueError("Corpus must be an array or an object containing cases")
    seen = set()
    normalized = []
    for case in cases:
        if isinstance(case, dict) and "title" in case and "request" not in case:
            key = case["title"] + "\0" + str(case.get("translate_languages", False))
            case = {**case, "id": "parser-" + hashlib.sha256(key.encode()).hexdigest()[:16],
                    "request": {"title": case["title"]}, "endpoint": "/parse",
                    "classification": "parser_regression", "expected": {},
                    "assertions": case.get("expected", [])}
        if not isinstance(case, dict) or not isinstance(case.get("id"), str):
            raise ValueError("Every case requires a string id")
        if case["id"] in seen:
            raise ValueError("Duplicate case id: " + case["id"])
        seen.add(case["id"])
        request = case.get("request")
        if not isinstance(request, dict) or not isinstance(request.get("title"), str):
            raise ValueError("Every case requires request.title")
        if set(request) - {"title", "year", "series"}:
            raise ValueError("Unknown request field in " + case["id"])
        if case.get("expected") is not None and not isinstance(case["expected"], dict):
            raise ValueError("Expected must be a partial object or null")
        if case.get("endpoint", "/match") not in ENDPOINTS:
            raise ValueError("Unsupported case endpoint")
        for assertion in case.get("assertions", []):
            if (not isinstance(assertion, dict) or assertion.get("op") not in {"eq", "ne", "absent"}
                    or not isinstance(assertion.get("path"), list)
                    or any(type(key) not in (str, int) for key in assertion["path"])
                    or assertion.get("coercion") not in (None, "len")):
                raise ValueError("Unsupported assertion in " + case["id"])
        normalized.append(case)
    return normalized


def role(case):
    classification = str(case.get("classification", "")).lower()
    if "reference" in classification or "differential" in classification:
        return "reference"
    if not case.get("expected") and not case.get("assertions"):
        return "unlabeled"
    return "labeled" if classification in LABELED else "expectation"


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    low, high = math.floor(position), math.ceil(position)
    return round(ordered[low] + (ordered[high] - ordered[low]) * (position - low), 3)


def summarize(results):
    outcomes = Counter(row["outcome"] for row in results)
    timings = [row["elapsed_ms"] for row in results if row.get("attempts")]
    summary = {"cases": len(results), "outcomes": dict(sorted(outcomes.items())),
               "roles": dict(Counter(row["evaluation_role"] for row in results)),
               "compared": sum(row.get("comparison") is not None for row in results),
               "matches_expectation": sum(row.get("comparison", {}).get("matches", False)
                                          for row in results if row.get("comparison") is not None),
               "latency_ms": {"p50": percentile(timings, 0.5), "p95": percentile(timings, 0.95)},
               "identifier_metrics": {},
               "metric_scope": "Only explicitly labeled cases with both expected identifier endpoints. "
                               "Reference, partial and unknown expectations are excluded. "
                               "Recall includes unavailable labeled positive cases; precision excludes them."}
    summary["assertions"] = {
        "manual_unscored": sum(len(row.get("assertions_manual", [])) for row in results),
        "scored": sum(row["comparison"].get("assertions", {}).get("total", 0)
                      for row in results if row.get("comparison") is not None),
        "passed": sum(row["comparison"].get("assertions", {}).get("passed", 0)
                      for row in results if row.get("comparison") is not None),
    }
    for prefix in ("sid", "asid"):
        keys = (prefix + "_start", prefix + "_end")
        labeled = [row for row in results if row["endpoint"] == "/match"
                   and row["evaluation_role"] == "labeled"
                   and all(key in row.get("expected", {}) for key in keys)]
        if not labeled:
            continue
        counts = Counter()
        for row in labeled:
            expected = [row["expected"][key] for key in keys]
            positive = any(value is not None for value in expected)
            counts["expected_positive" if positive else "expected_abstention"] += 1
            actual = row.get("response")
            if row["outcome"] not in {"matched_expectation", "mismatch", "reference", "unlabeled"} or not isinstance(actual, dict) or not all(key in actual for key in keys):
                counts["unavailable"] += 1
                continue
            values = [actual[key] for key in keys]
            if values == expected:
                counts["true_matches" if positive else "correct_abstentions"] += 1
            elif any(value is not None for value in values):
                counts["wrong_ids"] += 1
            else:
                counts["false_abstentions"] += 1
        true, wrong = counts["true_matches"], counts["wrong_ids"]
        summary["identifier_metrics"][prefix] = {
            "labeled_cases": len(labeled), **dict(counts),
            "precision": true / (true + wrong) if true + wrong else None,
            "match_recall": true / counts["expected_positive"] if counts["expected_positive"] else None,
        }
    return summary


def run_audit(cases, base_url, endpoint="/match", transport=None, delay=0.15,
              timeout=20.0, max_bytes=2 * 1024 * 1024, rate_limit_retries=1,
              max_retry_after=60.0, sleep=time.sleep, progress=None):
    transport = transport or CurlTransport()
    results = []
    stop_reason = None
    for index, case in enumerate(cases):
        selected_endpoint = case.get("endpoint", endpoint)
        row = {**case, "endpoint": selected_endpoint, "evaluation_role": role(case),
               "expected": case.get("expected"), "attempts": [], "comparison": None,
               "elapsed_ms": 0, "outcome": "not_run_rate_limit"}
        results.append(row)
        if selected_endpoint == "/parse" and case.get("translate_languages") is True:
            row["outcome"] = "unsupported_configuration"
            row["error"] = {"kind": "unsupported_configuration",
                            "message": "Public /parse uses translate_languages=false"}
            continue
        if stop_reason:
            row["error"] = {"kind": "rate_limit_stop", "message": stop_reason}
            continue
        started = time.monotonic()
        for attempt in range(rate_limit_retries + 1):
            if index or attempt:
                sleep(delay)
            response = transport.request(base_url.rstrip("/") + selected_endpoint,
                                         case["request"], timeout, max_bytes)
            row.update(response)
            row["attempts"].append({"status": response["status"],
                                    "elapsed_ms": response["elapsed_ms"],
                                    "error": response.get("error")})
            if response["status"] != 429:
                break
            wait = retry_after(response["headers"].get("retry-after"))
            wait = max(delay, 1.0 if wait is None else wait)
            if wait > max_retry_after or attempt == rate_limit_retries:
                stop_reason = f"HTTP 429: required wait {wait:.3f}s; audit stopped conservatively"
                break
            row["attempts"][-1]["retry_after_seconds"] = wait
            sleep(wait)
        row["wall_elapsed_ms"] = round((time.monotonic() - started) * 1000, 3)
        if row.get("error"):
            row["outcome"] = row["error"]["kind"]
        elif row["status"] == 403 and ("1010" in row.get("raw_body", "") or "cloudflare" in row.get("headers", {}).get("server", "").lower()):
            row["outcome"] = "transport_blocked"
        elif row["status"] == 429:
            row["outcome"] = "rate_limited"
        elif row["status"] is None or not 200 <= row["status"] < 300:
            row["outcome"] = "http_error"
        elif row.get("json_error"):
            row["outcome"] = "invalid_json"
        elif row["evaluation_role"] == "unlabeled":
            row["outcome"] = "unlabeled"
        else:
            assertions = case.get("assertions", [])
            differences = compare_subset(row["expected"] or {}, row["response"])
            assertion_diffs = compare_assertions(assertions, row["response"])
            differences.extend(assertion_diffs)
            row["comparison"] = {"matches": not differences, "differences": differences,
                                 "assertions": {"total": len(assertions),
                                                "passed": len(assertions) - len(assertion_diffs)}}
            row["outcome"] = ("reference" if row["evaluation_role"] == "reference" else
                              "mismatch" if differences else "matched_expectation")
        if progress:
            progress(index + 1, len(cases), row)
    return {"schema_version": 1, "metadata": {
                "timestamp_utc": datetime.now(timezone.utc).isoformat(),
                "base_url": base_url, "default_endpoint": endpoint,
                "transport": transport.name, "delay_seconds": delay,
                "timeout_seconds": timeout, "max_response_bytes": max_bytes,
                "rate_limit_retries": rate_limit_retries, "stop_reason": stop_reason},
            "summary": summarize(results), "results": results}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--endpoint", choices=sorted(ENDPOINTS), default="/match")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--transport", choices=("curl", "urllib"), default="curl")
    parser.add_argument("--delay", type=float, default=0.15)
    parser.add_argument("--timeout", type=float, default=20.0)
    parser.add_argument("--max-response-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--rate-limit-retries", type=int, default=1)
    parser.add_argument("--max-retry-after", type=float, default=60.0)
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()
    address = urlsplit(args.base_url)
    if address.scheme not in {"http", "https"} or not address.netloc or address.username or address.password or address.query or address.fragment:
        parser.error("base URL must be HTTP(S), without credentials, query or fragment")
    if (not all(math.isfinite(value) for value in (args.delay, args.timeout, args.max_retry_after))
            or args.delay < 0 or args.timeout <= 0 or args.max_retry_after < 0
            or args.max_response_bytes <= 0 or args.rate_limit_retries < 0):
        parser.error("invalid delay, timeout, byte limit or retry configuration")
    cases = validate_cases(strict_json(args.corpus.read_text(encoding="utf-8")))
    transport = CurlTransport() if args.transport == "curl" else UrllibTransport()

    def progress(index, total, row):
        if not args.quiet:
            print(f"{index}/{total} {row['id']} {row['outcome']} HTTP={row.get('status')} "
                  f"{row['elapsed_ms']:.1f}ms", file=sys.stderr, flush=True)

    report = run_audit(cases, args.base_url, args.endpoint, transport, args.delay,
                       args.timeout, args.max_response_bytes, args.rate_limit_retries,
                       args.max_retry_after, progress=progress)
    report["metadata"]["corpus"] = str(args.corpus)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_name(args.output.name + ".tmp")
    temporary.write_text(json.dumps(report, ensure_ascii=True, indent=2, allow_nan=False)
                         + "\n", encoding="utf-8")
    temporary.replace(args.output)
    print(json.dumps(report["summary"], sort_keys=True))


if __name__ == "__main__":
    main()
