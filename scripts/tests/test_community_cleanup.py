"""Regression tests for bounded, bot-only Community Learning housekeeping."""
from datetime import datetime, timedelta, timezone
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_cleanup as cc

NOW = datetime(2026, 11, 11, 12, tzinfo=timezone.utc)
REPO = "Dizzybog/MatchLab"
SHA = "a" * 40


def iso(days):
    return (NOW - timedelta(days=days)).isoformat()


def issue(number=1, days=28, **kwargs):
    created = iso(days)
    result = {
        "number": number, "title": cc.ISSUE_PREFIX +
        (NOW - timedelta(days=days)).date().isoformat(),
        "state": "open", "user": {"login": cc.BOT},
        "created_at": created, "updated_at": created,
        "comments": 0, "labels": [],
    }
    result.update(kwargs)
    return result


def pull(number=3, branch="bot/community-regex-20260901",
         state="open", created_days=60, updated_days=40, **kwargs):
    result = {
        "number": number, "title": cc.PR_TITLE, "body": cc.PR_SIGNATURE,
        "user": {"login": cc.BOT}, "state": state, "draft": False,
        "created_at": iso(created_days), "updated_at": iso(updated_days),
        "closed_at": iso(9) if state == "closed" else None,
        "labels": [], "requested_reviewers": [],
        "head": {"ref": branch, "sha": SHA,
                 "repo": {"full_name": REPO}},
        "base": {"ref": "main"},
    }
    result.update(kwargs)
    return result


def branch(name="bot/community-regex-20260901"):
    return {"name": name, "protected": False, "commit": {"sha": SHA}}


class FakeAPI:
    repo = REPO

    def __init__(self, issues=(), prs=(), branches=(), reviews=()):
        self.issues, self.prs, self.branches = list(issues), list(prs), list(branches)
        self.reviews = list(reviews)
        self.writes = []

    def pages(self, path):
        if path.startswith("/issues?"):
            return self.issues
        if path.startswith("/pulls?"):
            return self.prs
        if path == "/branches":
            return self.branches
        if path.startswith("/pulls/") and path.endswith("/reviews"):
            return self.reviews
        raise AssertionError(path)

    def request(self, method, path, payload=None):
        if method != "GET":
            self.writes.append((method, path, payload))
            return {}
        if path.startswith("/issues/"):
            number = int(path.rsplit("/", 1)[1])
            return next(i for i in self.issues if i["number"] == number)
        if path.startswith("/pulls/"):
            number = int(path.rsplit("/", 1)[1])
            return next(p for p in self.prs if p["number"] == number)
        if path.startswith("/branches/"):
            name = path[len("/branches/"):]
            return next(b for b in self.branches if b["name"] == name)
        if path.startswith("/pulls?state=open&head="):
            return []
        if path.startswith("/git/ref/heads/"):
            return {"object": {"sha": SHA}}
        if path == "/commits/" + SHA:
            return {"author": {"login": cc.BOT},
                    "commit": {"author": {"name": cc.BOT, "date": iso(40)},
                               "committer": {"name": cc.BOT, "date": iso(40)}}}
        raise AssertionError((method, path))


class CommunityCleanupTests(unittest.TestCase):
    def test_only_old_bot_daily_threads_and_no_unread_late_comments(self):
        self.assertTrue(cc.daily_issue(issue(), NOW))
        self.assertFalse(cc.daily_issue(issue(days=14), NOW))
        self.assertFalse(cc.daily_issue(issue(user={"login": "Dizzybog"}), NOW))
        self.assertFalse(cc.daily_issue(issue(title="Parser bug"), NOW))
        self.assertFalse(cc.daily_issue(issue(labels=[{"name": cc.KEEP_LABEL}]), NOW))
        self.assertFalse(cc.daily_issue(issue(updated_at=iso(1)), NOW))
        self.assertFalse(cc.daily_issue(issue(comments=2, updated_at=iso(7)), NOW))
        self.assertTrue(cc.daily_issue(issue(comments=2, updated_at=iso(16)), NOW))
        self.assertFalse(cc.daily_issue(issue(pull_request={"url": "..."}), NOW))

    def test_only_unreviewed_signed_stale_bot_parser_prs(self):
        p = pull()
        self.assertTrue(cc.stale_pr(p, REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(user={"login": "Dizzybog"}), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(title="feat(parser): human change"), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(body="arbitrary text"), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(updated_at=iso(4)), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(draft=True), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(requested_reviewers=[{"login": "maintainer"}]), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(head={"ref": "fix/parser", "sha": SHA,
            "repo": {"full_name": REPO}}), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(labels=[{"name": cc.KEEP_LABEL}]), REPO, NOW))
        self.assertFalse(cc.stale_pr(pull(head={"ref": "bot/community-regex-20260901",
            "sha": SHA, "repo": {"full_name": "attacker/fork"}}), REPO, NOW))

    def test_branch_rules_protect_active_and_manual_refs(self):
        old = branch()
        self.assertIsNone(cc.stale_branch(old, [pull()], REPO, NOW))
        self.assertEqual(cc.stale_branch(old, [pull(state="closed")], REPO, NOW),
                         "review-closed")
        self.assertIsNone(cc.stale_branch(
            old, [pull(state="closed", closed_at=iso(3))], REPO, NOW))
        self.assertIsNone(cc.stale_branch(
            old, [pull(state="closed", head={"ref": old["name"],
                 "sha": "b" * 40, "repo": {"full_name": REPO}})], REPO, NOW))
        self.assertEqual(cc.stale_branch(old, [], REPO, NOW), "abandoned")
        self.assertIsNone(cc.stale_branch(branch("bot/community-regex-20261110"),
                                           [], REPO, NOW))
        self.assertIsNone(cc.stale_branch(branch("fix/parser-thing"), [], REPO, NOW))
        self.assertIsNone(cc.stale_branch({**old, "protected": True}, [], REPO, NOW))

    def test_no_side_effects_in_dry_run_and_exact_apply_targets(self):
        api = FakeAPI(
            issues=[issue(), issue(2, 2)],
            prs=[pull(number=3), pull(number=4, branch="fix/parser", state="open"),
                 pull(number=5, state="closed")],
            branches=[branch(), branch("fix/parser"), branch("bot/community-regex-20261110")],
        )
        planned = cc.run(api, NOW, apply=False)
        self.assertEqual(planned["daily_issues"], [1])
        self.assertEqual(planned["stale_review_prs"], [3])
        self.assertEqual(planned["bot_branches"], [])
        self.assertEqual(api.writes, [])
        result = cc.run(api, NOW, apply=True)
        self.assertEqual(result["daily_issues"], [1])
        self.assertEqual([(method, route) for method, route, _ in api.writes],
                         [("PATCH", "/issues/1"), ("PATCH", "/pulls/3"),
                          ("POST", "/issues/3/comments")])

    def test_merged_bot_branch_deletes_only_after_grace(self):
        api = FakeAPI(prs=[pull(state="closed")], branches=[branch()])
        plan = cc.run(api, NOW, apply=True)
        self.assertEqual(plan["bot_branches"], ["bot/community-regex-20260901"])
        self.assertEqual([(method, route) for method, route, _ in api.writes],
                         [("DELETE", "/git/refs/heads/bot/community-regex-20260901")])

    def test_latest_issue_update_prevents_stale_snapshot_closure(self):
        class EditedAPI(FakeAPI):
            def request(self, method, path, payload=None):
                if method == "GET" and path == "/issues/1":
                    return issue(updated_at=iso(1))
                return super().request(method, path, payload)
        api = EditedAPI(issues=[issue()])
        cc.run(api, NOW, apply=True)
        self.assertEqual(api.writes, [])

    def test_latest_pr_update_prevents_stale_snapshot_closure(self):
        class EditedAPI(FakeAPI):
            def request(self, method, path, payload=None):
                if method == "GET" and path == "/pulls/3":
                    return pull(updated_at=iso(1))
                return super().request(method, path, payload)
        api = EditedAPI(prs=[pull()])
        cc.run(api, NOW, apply=True)
        self.assertEqual(api.writes, [])

    def test_reopened_pr_blocks_branch_deletion(self):
        class ReopenedAPI(FakeAPI):
            def request(self, method, path, payload=None):
                if method == "GET" and path.startswith("/pulls?state=open&head="):
                    return [{"number": 3}]
                return super().request(method, path, payload)
        api = ReopenedAPI(prs=[pull(state="closed")], branches=[branch()])
        cc.run(api, NOW, apply=True)
        self.assertEqual(api.writes, [])

    def test_human_approval_blocks_automatic_pr_close(self):
        api = FakeAPI(prs=[pull()], reviews=[
            {"state": "APPROVED", "user": {"login": "Dizzybog"}}])
        plan = cc.run(api, NOW, apply=True)
        self.assertEqual(plan["stale_review_prs"], [])
        self.assertEqual(api.writes, [])


if __name__ == "__main__":
    unittest.main()
