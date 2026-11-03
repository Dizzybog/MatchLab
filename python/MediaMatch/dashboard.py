"""Browser dashboard and read-only matching diagnostics.

The UI is bundled with the Python package and served by the existing API.
No external JavaScript, CDN, separate process or extra dependencies.
"""

from __future__ import annotations

import logging
import re
import time

from .index import clean_title

LOG = logging.getLogger(__name__)


def diagnose(index, title: str, year: int | None = None,
             series: bool | None = None) -> dict:
    from .api import match_title, parse
    from .anime_only import resolve_anime_title, is_known_anime_title

    started = time.monotonic()
    parsed = parse(title)
    parsed_year = year if year is not None else parsed.year
    seasons = list(parsed.seasons or [])
    episodes = list(parsed.episodes or [])
    absolute = re.search(r"(?i)\b(?:episode|ep)\s*[. _-]*([0-9]{1,5})\b", title)
    if absolute and not episodes:
        episodes = [int(absolute.group(1))]
    parsed_title = parsed.parsed_title
    if absolute and not seasons:
        parsed_title = re.sub(
            r"(?i)\b(?:episode|ep)\s*[. _-]*[0-9]{1,5}\b", "", parsed_title
        ).strip(" ._-")
    inferred_series = bool(seasons or episodes) if series is None else series

    imdb_candidates = index.search(
        parsed_title, parsed_year, series=inferred_series,
        required_max_season=max(seasons, default=0), limit=20,
        release_title=title, has_episode=bool(episodes),
    )
    imdb = [
        {"imdb_id": m.imdb_id, "title": m.title, "matched_title": m.matched_title,
         "year": m.year, "category": m.category, "media_type": m.media_type,
         "anime_ids": m.anime_ids, "rank": n + 1}
        for n, m in enumerate(imdb_candidates)
    ]
    anime = None
    known_anime = False
    if inferred_series and episodes:
        anime = resolve_anime_title(
            index, parsed_title, min(seasons) if seasons else None, min(episodes)
        )
        known_anime = is_known_anime_title(index, parsed_title)
    # The final result uses exactly the same production matching path.
    selected = match_title(index, title, year, series=series)
    reason = (
        "A validated anime candidate takes precedence over IMDb candidates."
        if selected and selected.get("match_source") == "anime_priority"
        else "The IMDb matcher selected the best eligible candidate."
        if selected else
        "No unambiguous eligible match was found."
    )
    return {
        "input": {"title": title, "year": year, "series": series},
        "parsed": parsed.model_dump(mode="json"),
        "normalized_title": clean_title(parsed_title),
        "interpreted": {"title": parsed_title, "year": parsed_year,
                        "series": inferred_series, "seasons": seasons, "episodes": episodes},
        "imdb_candidates": imdb,
        "anime_candidate": anime,
        "anime_title_recognized": known_anime,
        "selected": selected,
        "reason": reason,
        "duration_ms": round((time.monotonic() - started) * 1000, 1),
    }


HTML = r'''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>MatchLab · Torrent Parse Match</title>
<style>
:root{color-scheme:dark;--bg:#090e19;--panel:#111b2c;--panel2:#17243a;--border:#2b3b53;--text:#edf3fc;--muted:#98a9c2;--accent:#78b8ff;--green:#58d8b0;--amber:#ffcf79}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(ellipse at 20% 0%,#172e4e 0,transparent 48%),var(--bg);font:15px/1.55 system-ui,-apple-system,Segoe UI,sans-serif;color:var(--text)}
main{max-width:1160px;margin:0 auto;padding:35px 22px 80px}.top{display:flex;justify-content:space-between;align-items:center;gap:16px;flex-wrap:wrap}.brand{display:flex;align-items:center;gap:12px;font-weight:750;font-size:17px}.logo{width:40px;height:40px;display:grid;place-items:center;border:1px solid #4274a5;border-radius:13px;background:#173355;color:#9bd0ff;font-size:21px}.status{color:var(--muted);font-size:13px}.dot{display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--amber);margin-right:8px}.dot.ok{background:var(--green)}
h1{font-size:clamp(31px,5vw,48px);letter-spacing:-1.6px;line-height:1.15;margin:55px 0 12px}h1 span{color:var(--accent)}.intro{color:var(--muted);max-width:670px;margin:0 0 30px}
.panel{background:linear-gradient(145deg,rgba(23,36,58,.94),rgba(15,24,40,.95));border:1px solid var(--border);border-radius:18px;padding:24px;box-shadow:0 12px 40px #0002}.formgrid{display:grid;grid-template-columns:minmax(0,1fr) 130px 160px auto;gap:12px;align-items:end}
label{display:block;color:#b7c8df;font-size:12px;font-weight:650;margin-bottom:7px}input,select,button{font:inherit;border-radius:10px}input,select{background:#0b1525;color:var(--text);border:1px solid #3b4d68;padding:13px;width:100%;outline:none;min-width:0}input:focus,select:focus{border-color:var(--accent);box-shadow:0 0 0 3px #78b8ff20}button{border:0;background:var(--accent);color:#061426;padding:13px 19px;font-weight:750;cursor:pointer}button:hover{filter:brightness(1.1)}button:disabled{opacity:.5;cursor:wait}
.examples{display:flex;gap:8px;flex-wrap:wrap;margin-top:16px}.chip{background:#20324c;border:1px solid #354966;color:#c8d9f0;padding:7px 11px;font-size:12px;font-weight:550}
.sectionhead{display:flex;justify-content:space-between;align-items:center;margin:32px 0 13px;gap:10px}h2{font-size:19px;letter-spacing:-.4px;margin:0}.meta{color:var(--muted);font-size:12px}.grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:16px}.statgrid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:12px}.stat{padding:17px;border:1px solid var(--border);background:#101c2e;border-radius:13px}.stat b{display:block;font-size:22px;margin-top:4px}.stat small{color:var(--muted);font-size:12px}.result{border-color:#357b6f;background:linear-gradient(130deg,#12342e,#14283b)}.result .big{font-size:27px;font-weight:800;letter-spacing:-.7px;overflow-wrap:anywhere}.tag{display:inline-block;font-size:11px;padding:4px 9px;border-radius:30px;background:#23466c;color:#b3d9ff;margin:4px 6px 4px 0}.tag.good{background:#164a3f;color:#83efc9}.tag.warn{background:#503b1f;color:#ffd794}.muted{color:var(--muted)}.candidate{padding:13px 0;border-bottom:1px solid #293950}.candidate:last-child{border:0}.candidate strong{display:block}.candidate .meta{margin-top:3px}pre{margin:0;white-space:pre-wrap;overflow-wrap:anywhere;color:#b8d4f5;font-size:12px;line-height:1.7}details{margin-top:16px}summary{cursor:pointer;color:var(--accent);font-weight:650}.empty{color:var(--muted);padding:15px 0}.error{color:#ffb4b4;background:#4c222b;border:1px solid #8c3d49;padding:14px;border-radius:12px;margin-top:16px}#results{display:none}.footer{margin-top:35px;color:var(--muted);font-size:12px}
@media(max-width:850px){.formgrid{grid-template-columns:1fr 1fr}.formgrid .titlefield{grid-column:1/-1}.formgrid button{align-self:end}.statgrid{grid-template-columns:1fr 1fr}}@media(max-width:600px){.grid,.formgrid{grid-template-columns:1fr}.formgrid .titlefield{grid-column:auto}main{padding:22px 14px 50px}.panel{padding:17px}h1{margin-top:35px}.statgrid{gap:8px}}
</style>
</head>
<body><main>
<div class="top"><div class="brand"><div class="logo">◈</div> Torrent Parse Match <span class="muted">/ MatchLab</span></div><div class="status"><span id="dot" class="dot"></span><span id="status">Checking metadata…</span></div></div>
<h1>Understand every <span>match.</span></h1>
<p class="intro">Paste a torrent release name to inspect its parsed metadata, IMDb candidates, anime crosswalk and final matching decision — all from your local index.</p>
<form id="form" class="panel"><div class="formgrid">
<div class="titlefield"><label for="title">Torrent release title</label><input id="title" name="title" placeholder="Example.Show.S02E04.1080p.WEB-DL" maxlength="4096" required autofocus></div>
<div><label for="year">Override year</label><input id="year" type="number" min="1870" max="2100" placeholder="Auto"></div>
<div><label for="series">Content type</label><select id="series"><option value="">Auto detect</option><option value="true">TV series</option><option value="false">Movie</option></select></div>
<button id="submit" type="submit">Analyze ↗</button></div>
<div class="examples"><span class="meta" style="align-self:center">Try:</span><button type="button" class="chip" data-example="The.Office.UK.S01E01.1080p">Country alias</button><button type="button" class="chip" data-example="Example.Anime.Arc.Two.S01E04">Anime part</button><button type="button" class="chip" data-example="Dune.Part.Two.2024.2160p.WEB-DL">Movie</button></div>
<div id="error" role="alert" hidden></div></form>
<div id="results" aria-live="polite">
<div class="sectionhead"><h2>Match overview</h2><span class="meta" id="duration"></span></div>
<div class="statgrid"><div class="stat"><small>IMDb candidates</small><b id="imdbcount">0</b></div><div class="stat"><small>Anime candidate</small><b id="animecount">—</b></div><div class="stat"><small>Detected type</small><b id="type">—</b></div><div class="stat"><small>Result</small><b id="resultstatus">—</b></div></div>
<div class="sectionhead"><h2>Selected result</h2></div>
<div class="panel result" id="winner"></div>
<div class="sectionhead"><h2>Candidate comparison</h2><span class="meta">IMDb V4 and anime metadata are evaluated independently</span></div>
<div class="grid"><div class="panel"><h2>IMDb candidates</h2><div id="imdbcandidates"></div></div><div class="panel"><h2>Anime candidate</h2><div id="animecandidate"></div></div></div>
<div class="sectionhead"><h2>Parser & diagnostics</h2></div>
<div class="grid"><div class="panel"><h2>Interpreted release</h2><div id="parsed"></div></div><div class="panel"><h2>Raw response</h2><details open><summary>JSON diagnostics</summary><pre id="raw"></pre></details></div></div>
</div><p class="footer">Local diagnostic interface · No third-party scripts · Requests and results are recorded in the API logs. Protect this dashboard when exposing the service to the internet.</p>
</main>
<script>
const $=id=>document.getElementById(id);
const esc=v=>String(v??"—").replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
const fmt=v=>v===null||v===undefined?"—":esc(v);
function render(d){
$("results").style.display="block";$("duration").textContent=d.duration_ms+" ms";
$("imdbcount").textContent=d.imdb_candidates.length;
$("animecount").textContent=d.anime_candidate?"Verified":d.anime_title_recognized?"Unresolved":"None";
$("type").textContent=d.interpreted.series?"Series":"Movie";
$("resultstatus").textContent=d.selected?"Matched":"No match";
const s=d.selected;
$("winner").innerHTML=s?'<span class="tag good">MATCHED</span><span class="tag">'+esc(s.match_source||"imdb_v4")+'</span><div class="big">'+esc(s.imdb_id)+'</div><p>'+esc(s.title)+'</p><div><span class="tag">SID: '+fmt(s.sid)+'</span>'+(s.anime?.asid?'<span class="tag good">ASID: '+esc(s.anime.asid)+'</span>':'')+'</div><p class="muted">'+esc(d.reason)+'</p>':'<span class="tag warn">NO MATCH</span><p class="muted">'+esc(d.reason)+'</p>';
$("imdbcandidates").innerHTML=d.imdb_candidates.length?d.imdb_candidates.map(c=>'<div class="candidate"><strong>#'+c.rank+' · '+esc(c.title)+'</strong><div class="meta">'+esc(c.imdb_id)+' · '+fmt(c.year)+' · '+esc(c.media_type)+'</div><div class="meta">Matched: '+esc(c.matched_title)+'</div></div>').join(""):'<p class="empty">No eligible IMDb candidates.</p>';
const a=d.anime_candidate;
$("animecandidate").innerHTML=a?'<div class="candidate"><strong>'+esc(a.imdb_id)+'</strong><div class="meta">AniDB: '+fmt(a.anidb_id)+' · Episode: '+fmt(a.anidb_episode)+'</div><div class="meta">Canonical season '+fmt(a.season)+' · Episode '+fmt(a.episode)+'</div><span class="tag good">Validated mapping</span></div>':d.anime_title_recognized?'<p class="empty">Anime alias recognized, but mapping is ambiguous or unavailable.</p>':'<p class="empty">No matching anime alias.</p>';
$("parsed").innerHTML='<div class="candidate"><strong>'+esc(d.interpreted.title)+'</strong><div class="meta">Normalized: '+esc(d.normalized_title)+'</div></div><div class="candidate">Year: '+fmt(d.interpreted.year)+'<br>Seasons: '+esc(d.interpreted.seasons.join(", ")||"—")+'<br>Episodes: '+esc(d.interpreted.episodes.join(", ")||"—")+'</div><details><summary>All parser fields</summary><pre>'+esc(JSON.stringify(d.parsed,null,2))+'</pre></details>';
$("raw").textContent=JSON.stringify(d,null,2);
}
async function health(){try{let r=await fetch("/health");let j=await r.json();$("dot").classList.toggle("ok",r.ok);$("status").textContent=j.status==="ready"?"Metadata ready":j.status==="stale"?"Metadata stale":"Metadata initializing";}catch{$("status").textContent="API unavailable";}}
$("form").addEventListener("submit",async e=>{e.preventDefault();$("error").hidden=true;$("submit").disabled=true;$("submit").textContent="Analyzing…";try{
const data={title:$("title").value};if($("year").value)data.year=Number($("year").value);if($("series").value)data.series=$("series").value==="true";
const response=await fetch("/debug/match",{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(data)});
const result=await response.json();if(!response.ok)throw Error(result.error||"Request failed");render(result);
}catch(e){$("error").hidden=false;$("error").className="error";$("error").textContent=e.message;}finally{$("submit").disabled=false;$("submit").textContent="Analyze ↗";}});
document.querySelectorAll("[data-example]").forEach(b=>b.addEventListener("click",()=>{$("title").value=b.dataset.example;$("form").requestSubmit();}));health();
</script></body></html>'''
