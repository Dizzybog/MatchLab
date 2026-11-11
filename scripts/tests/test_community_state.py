"""Fault-injection tests for versioned Community Learning persistence."""
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_state as state


class FakeState(state.Storage):
    def __init__(self, missing=False, fail_upload=False):
        super().__init__("Dizzybog/MatchLab", "fake-token")
        self.releases = {} if missing else {
            "community-learning-ledger": {"assets": []},
        }
        self.assets = {}
        self.next_id = 10
        self.deleted = []
        self.fail_upload = fail_upload

    def api(self, method, route, body=None, missing_ok=False):
        if method == "GET" and route.startswith("/releases/tags/"):
            return self.releases.get(route.rsplit("/", 1)[1])
        if method == "DELETE" and route.startswith("/releases/assets/"):
            number = int(route.rsplit("/", 1)[1])
            self.deleted.append(number)
            for release in self.releases.values():
                release["assets"] = [
                    a for a in release["assets"] if a["id"] != number]
            return None
        raise AssertionError((method, route))

    def gh(self, *args):
        if args[:2] == ("release", "create"):
            self.releases[args[2]] = {"assets": []}
        elif args[:2] == ("release", "download"):
            name = args[args.index("--pattern") + 1]
            folder = Path(args[args.index("--dir") + 1])
            (folder / name).write_bytes(self.assets[name])
        elif args[:2] == ("release", "upload"):
            if self.fail_upload:
                raise RuntimeError("simulated failed upload")
            source = Path(args[3])
            self.assets[source.name] = source.read_bytes()
            tag = args[2]
            self.releases[tag]["assets"].append({
                "id": self.next_id, "name": source.name,
                "size": source.stat().st_size,
            })
            self.next_id += 1
        else:
            raise AssertionError(args)


class CommunityStateTests(unittest.TestCase):
    def test_empty_release_bootstrap_and_upload_is_never_zero_bytes(self):
        store = FakeState()
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fingerprints.txt"
            store.load("ledger", path)
            self.assertEqual(path.read_bytes(), b"")
            store.save("ledger", path, "ledger", "123", "1")
            self.assertEqual(
                store.assets["fingerprints-123-1-ledger.txt"],
                (state.HEADER + "\n").encode(),
            )
            store.load("ledger", path)
            self.assertEqual(path.read_bytes(), b"")

    def test_legacy_asset_and_versioned_state_roundtrip(self):
        store = FakeState()
        old = "a" * 64 + "\n"
        store.assets["fingerprints.txt"] = old.encode()
        store.releases["community-learning-ledger"]["assets"] = [{
            "id": 1, "name": "fingerprints.txt", "size": len(old)
        }]
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fingerprints.txt"
            store.load("ledger", path)
            self.assertEqual(path.read_text(), old)
            store.save("ledger", path, "ledger", "123", "2")
            store.load("ledger", path)
            self.assertEqual(path.read_text(), old)

    def test_failed_upload_preserves_last_confirmed_asset(self):
        store = FakeState(fail_upload=True)
        store.assets["fingerprints.txt"] = ("a" * 64 + "\n").encode()
        store.releases["community-learning-ledger"]["assets"] = [{
            "id": 1, "name": "fingerprints.txt", "size": 65
        }]
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fingerprints.txt"
            store.load("ledger", path)
            with self.assertRaises(RuntimeError):
                store.save("ledger", path, "ledger", "99", "1")
        self.assertEqual(store.deleted, [])
        self.assertIn("fingerprints.txt", store.assets)

    def test_invalid_latest_fails_closed_and_does_not_replace_file(self):
        store = FakeState()
        store.assets["fingerprints-1-1-ledger.txt"] = b"not-a-hash\n"
        store.releases["community-learning-ledger"]["assets"] = [{
            "id": 1, "name": "fingerprints-1-1-ledger.txt", "size": 11
        }]
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fingerprints.txt"
            path.write_text("existing")
            with self.assertRaises(ValueError):
                store.load("ledger", path)
            self.assertEqual(path.read_text(), "existing")

    def test_backlog_prepared_final_crash_recovery_and_retention(self):
        store = FakeState(missing=True)
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "backlog.json"
            store.load("backlog", path)
            self.assertEqual(path.read_text(), '{"pending":[]}\n')
            for n in range(9):
                phase = "prepared" if n % 2 == 0 else "final"
                store.save("backlog", path, phase, str(n + 1), "1")
            self.assertEqual(len(
                store.releases["community-learning-backlog"]["assets"]),
                state.KEEP)
            self.assertEqual(len(store.deleted), 9 - state.KEEP)
            store.load("backlog", path)
            self.assertEqual(path.read_text(), '{"pending":[]}\n')

    def test_corrupt_inputs_are_not_uploaded(self):
        for raw in [b"\n", b"nothex\n", b"a" * 64 + b"\na" * 64 + b"\n"]:
            with self.assertRaises(ValueError):
                state.normalize("ledger", raw)
        for raw in [b"{}", b'{"pending":"bad"}',
                    b'{"pending":[{"fingerprint":"bad"}]}']:
            with self.assertRaises(ValueError):
                state.normalize("backlog", raw)


if __name__ == "__main__":
    unittest.main()
