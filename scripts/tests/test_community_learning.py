"""Community Learning: historical-version replay, spoofing, dedup security."""
import hashlib
import json
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parents[1]))
import community_learning as cl

OLD = "a" * 40
LATEST = "b" * 40
MARKER = "<!-- matchlab-community-batch-v2 -->"


def make_report(title="Bleach_338", codes=None, sha=OLD):
    codes = codes or ["unresolved_absolute_anime_episode"]
    return {
        "protocol":2,"learning_epoch":1,"build_sha":sha,
        "source":"matchlab-community","fingerprint":cl.fingerprint(title,codes),
        "title":title,"status":"matched","alerts":codes,
        "parsed":{"title":"Bleach","seasons":[],"episodes":[338]},
        "selected":{"imdb_id":"tt0434665","season":None,"episode":None},
        "verified":False,
    }


def make_comment(rows=None, sha=OLD, overrides=None):
    rows = rows or [make_report(sha=sha)]
    batch_id=hashlib.sha256((sha+"\n1\n"+"\n".join(sorted(
        item["fingerprint"] for item in rows))).encode()).hexdigest()
    obj={
        "protocol":2,"learning_epoch":1,"build_sha":sha,
        "source":"matchlab-community-batch","batch_id":batch_id,
        "created_unix":int(datetime.now(timezone.utc).timestamp()),
        "verified":False,"reports":rows,
    }
    if overrides:
        obj.update(overrides)
    return {"body":MARKER+"\n"+json.dumps(obj),"created_at":
        datetime.now(timezone.utc).isoformat(),"id":1}


class CommunityTests(unittest.TestCase):
    def test_old_build_allowed_if_known_ancestor(self):
        report=make_comment()
        self.assertIsNotNone(cl.validate_comment(report,LATEST,lambda old,now: old==OLD))
        self.assertIsNone(cl.validate_comment(report,LATEST,lambda old,now: False))
        self.assertIsNone(cl.validate_comment(make_comment(overrides={
            "learning_epoch":2}),LATEST,lambda *_: True))

    def test_reject_forged_labels_and_sensitive_titles(self):
        self.assertIsNone(cl.validate_comment(make_comment(overrides={
            "verified":True}),LATEST,lambda *_: True))
        for title in ("user@server.com","https://malicious.example",
                "C:\\Users\\name\\file.mkv","/mnt/private/path",
                "token=mysecret","A\nB"):
            self.assertIsNone(cl.validate_comment(
                make_comment([make_report(title=title)]),LATEST,lambda *_: True))
        self.assertIsNone(cl.validate_comment(make_comment([{
            **make_report(),"unexpected_secret":"password"
        }]),LATEST,lambda *_:True))

    def test_current_parser_replay_drops_already_fixed_marker_disagreement(self):
        row=make_report("Show.S01E03",["parser_marker_disagreement"])
        payload=[make_comment([row])]
        now=datetime.now(timezone.utc)
        replay=lambda title, _:{"title":"Show","seasons":[1],"episodes":[3]}
        result,seen=cl.intake(payload,LATEST,now,set(),"unused",
                              lambda *_:True,replay)
        self.assertEqual((result,seen),([],set()))
        unresolved=lambda title,_:{"title":"Show","seasons":[2],"episodes":[3]}
        result,seen=cl.intake(payload,LATEST,now,set(),"unused",
                              lambda *_:True,unresolved)
        self.assertEqual(len(result),1)
        self.assertEqual(result[0]["status"],"still_present_in_current_parser")
        self.assertEqual(len(seen),1)

    def test_absolute_anime_requires_metadata_recheck_even_if_current_parse_changed(self):
        row=make_report()
        result,seen=cl.intake([make_comment([row])],LATEST,
                              datetime.now(timezone.utc),set(),"unused",
                              lambda *_:True,lambda *_:{
                                  "title":"Bleach","episodes":[338],"seasons":[]})
        self.assertEqual(result[0]["status"],"needs_current_metadata_review")
        self.assertEqual(len(seen),1)
        again,again_seen=cl.intake([make_comment([row])],LATEST,
                              datetime.now(timezone.utc),seen,"unused",
                              lambda *_:True,lambda *_:{})
        self.assertEqual(again,[])
        self.assertEqual(again_seen,seen)

    def test_bleach_s16_anidb_range_failure_not_discarded_as_parsing_fix(self):
        # Old client reports a season pack where IMDb endpoints exist, but
        # the AniDB range is incomplete. Parsing S16 correctly on main does
        # NOT prove that its AniDB episode mapping was fixed.
        row=make_report("Bleach S16",["incomplete_anime_episode_range"])
        row["parsed"]={"title":"Bleach","seasons":[16],"episodes":[]}
        row["selected"]={"imdb_id":"tt0434665","season":16,"episode":1}
        payload=[make_comment([row])]
        result,seen=cl.intake(payload,LATEST,datetime.now(timezone.utc),
            set(),"unused",lambda *_:True,lambda *_:{
                "title":"Bleach","seasons":[16],"episodes":[]})
        self.assertEqual(len(result),1)
        self.assertEqual(result[0]["status"],"needs_current_metadata_review")
        self.assertEqual(len(seen),1)
        # The normalized fingerprints prevent another report for the same
        # issue when title separators change.
        other=make_report("Bleach.S16",["incomplete_anime_episode_range"])
        self.assertEqual(row["fingerprint"],other["fingerprint"])
        after,after_seen=cl.intake([make_comment([other])],LATEST,
            datetime.now(timezone.utc),seen,"unused",
            lambda *_:True,lambda *_:{"title":"Bleach","seasons":[16]})
        self.assertEqual(after,[])
        self.assertEqual(after_seen,seen)

    def test_reject_duplicate_rows_and_overlong_payload(self):
        row=make_report()
        self.assertIsNone(cl.validate_comment(make_comment([row,row]),
            LATEST,lambda *_:True))
        corrupted=make_comment()
        corrupted["body"]+="x"*20000
        self.assertIsNone(cl.validate_comment(corrupted,LATEST,lambda *_:True))

    def test_no_shadow_ml_without_independent_labels(self):
        self.assertIsNone(cl.train([],LATEST))
        self.assertIsNone(cl.fit([]))


if __name__=="__main__":
    unittest.main()
