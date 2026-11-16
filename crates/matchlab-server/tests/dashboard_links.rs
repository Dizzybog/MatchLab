//! Exercise the real dashboard ID-link JavaScript with Node when available.
use std::process::Command;

const HTML: &str = include_str!("../src/dashboard.html");

const CHECKS: &str = r#"
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
"#;

#[test]
fn id_link_renderers_validate_destinations_and_escape_untrusted_values() {
    let start = HTML.find("const esc=s=>").expect("dashboard escape helper");
    let end = start + HTML[start..].find("async function json(url,options){").expect("dashboard json helper");
    let script = format!("{}\n{CHECKS}", &HTML[start..end]);
    let Ok(output) = Command::new("node").arg("-e").arg(&script).output() else {
        eprintln!("Node.js not present; the link wiring test below still runs");
        return;
    };
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn dashboard_uses_root_absolute_api_paths_on_dash() {
    for path in [
        "json('/api/dashboard/history",
        "json('/api/dashboard/stats",
        "json('/api/dashboard/diagnose",
        "fetch('/health')",
        "json('/api/ml/status'",
    ] {
        assert!(HTML.contains(path), "{path}");
    }
    assert!(HTML.contains("if(event.target.closest('a.id-link'))return;"));
    assert!(HTML.contains("field('AniDB ID',r.anime_id,anidbLinks(r.anime_id))"));
}
