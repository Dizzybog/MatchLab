"""Exercise real dashboard ID-link JavaScript with Node when available."""
from pathlib import Path
import shutil
import subprocess
import unittest

HTML = Path(__file__).resolve().parents[2] / "crates/matchlab-server/src/dashboard.html"


class DashboardLinkTests(unittest.TestCase):
    def test_id_link_renderers_validate_destinations_and_escape_untrusted_values(self):
        node = shutil.which("node")
        if node is None:
            self.skipTest("Node.js not present; Rust tests still check link wiring")
        source = HTML.read_text(encoding="utf-8")
        start = source.index("const esc=s=>")
        end = source.index("async function json(url,options){", start)
        helpers = source[start:end]
        checks = r"""
const assert=require('node:assert/strict');
assert.match(imdbLink('tt1234567'), /href="https:\/\/www\.imdb\.com\/title\/tt1234567\/"/);
assert.match(imdbLink('tt1234567:1:42'), /\/tt1234567\/"/);
assert.match(imdbLink('tt1234567'), /rel="noopener noreferrer"/);
assert.match(imdbLink('tt1234567'), /target="_blank"/);
assert.equal(imdbLink(null), '—');
assert.equal(imdbLink(''), '—');
assert.equal(imdbLink('not-an-id'), 'not-an-id');
assert.equal(imdbLink('tt1234567\"><img src=x>'),
    'tt1234567&quot;&gt;&lt;img src=x&gt;');
assert.doesNotMatch(imdbLink('https://attacker.example'), /href=/);
assert.match(anidbLinks(732), /href="https:\/\/anidb\.net\/anime\/732"/);
assert.equal((anidbLinks('732, 14951').match(/class="id-link"/g)||[]).length, 2);
assert.match(anidbLinks('732:12'), /href="https:\/\/anidb\.net\/anime\/732"/);
assert.equal(anidbLinks(null), '—');
assert.equal(anidbLinks('abc'), 'abc');
assert.equal(anidbLinks('732, <img src=x>'), 
    /href="https:\/\/anidb\.net\/anime\/732"/.test(anidbLinks('732, <img src=x>'))
      ? anidbLinks('732')+', &lt;img src=x&gt;'
      : 'unreachable');
assert.doesNotMatch(anidbLinks('javascript:alert(1)'), /href=/);
assert.doesNotMatch(anidbLinks('732/../../666'), /href=/);
"""
        command = [node, "-e", helpers + "\n" + checks]
        result = subprocess.run(
            command, capture_output=True, text=True, check=False, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_dashboard_uses_root_absolute_api_paths_on_dash(self):
        source = HTML.read_text(encoding="utf-8")
        for path in (
            "json('/api/dashboard/history",
            "json('/api/dashboard/stats",
            "json('/api/dashboard/diagnose",
            "fetch('/health')",
            "json('/api/ml/status'",
        ):
            self.assertIn(path, source)
        self.assertIn("if(event.target.closest('a.id-link'))return;", source)
        self.assertIn("field('AniDB ID',r.anime_id,anidbLinks(r.anime_id))", source)


if __name__ == "__main__":
    unittest.main()
