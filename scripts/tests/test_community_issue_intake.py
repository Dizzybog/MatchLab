"""Community comment recovery must be complete, current and bot-scoped."""
from datetime import datetime, timedelta, timezone
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_issue_intake as intake

NOW = datetime(2026, 11, 11, 12, tzinfo=timezone.utc)


def iso(days):
    return (NOW - timedelta(days=days)).isoformat()


def issue(number, days=10, author=intake.BOT, comments=1, title=None):
    return {
        "number": number, "title": title or (
            "[community-learning] " + (NOW - timedelta(days=days)).date().isoformat()),
        "user": {"login": author}, "state": "open",
        "updated_at": iso(days), "comments": comments,
    }


class API:
    def __init__(self, rows, comments):
        self.rows, self.comments = rows, comments
        self.writes = []
        self.paths = []

    def pages(self, path):
        self.paths.append(path)
        if path.startswith("/issues?"):
            return self.rows
        return self.comments[int(path.split("/")[2])]

    def request(self, method, path, payload=None):
        self.writes.append((method, path, payload))


class IssueIntakeTests(unittest.TestCase):
    def test_reads_today_and_old_active_thread_without_first_page_limit(self):
        rows = [issue(n, comments=0, title="Unrelated PR") for n in range(100)]
        rows += [issue(101, days=0), issue(102, days=33),
                 issue(103, days=70), issue(104, author="not-bot")]
        api = API(rows, {
            101: [{"body": "today", "created_at": iso(0)}],
            102: [{"body": "late", "created_at": iso(33)}],
        })
        result = intake.collect(api, NOW, create_daily=True)
        self.assertEqual([part[0]["body"] for part in result], ["today", "late"])
        self.assertEqual(api.writes, [])
        self.assertIn("/issues/102/comments", api.paths)

    def test_creates_daily_thread_only_when_no_official_one_exists(self):
        api = API([issue(1, days=0, author="someone-else", comments=0)], {})
        result = intake.collect(api, NOW, create_daily=True)
        self.assertEqual(result, [])
        self.assertEqual(api.writes[0][0:2], ("POST", "/issues"))
        self.assertEqual(api.writes[0][2]["title"], "[community-learning] 2026-11-11")

    def test_oversized_intake_fails_without_partial_output(self):
        api = API([issue(1)], {
            1: [{"body": "spam", "created_at": iso(0)}] * (
                intake.MAX_COMMENTS + 1),
        })
        with self.assertRaises(RuntimeError):
            intake.collect(api, NOW)

    def test_no_fake_bot_issues_or_pull_requests(self):
        legitimate = issue(1)
        self.assertTrue(intake.official(legitimate))
        for bad in [
            {**legitimate, "pull_request": {"url": "example"}},
            {**legitimate, "user": {"login": "imposter"}},
            {**legitimate, "title": "[community-learning] tomorrow"},
            {**legitimate, "number": "1"},
        ]:
            self.assertFalse(intake.official(bad))


if __name__ == "__main__":
    unittest.main()
