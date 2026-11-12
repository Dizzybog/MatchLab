# MatchLab API reference

Detailed endpoint behavior, anime and episode mappings, airdate matching, range handling, and debugging.

## Endpoints

The API listens on port **8765**.

### Check status

```bash
curl http://127.0.0.1:8765/health
```

The HTTP server starts immediately, **before Tantivy startup indexing**.
`GET /health/live` returns HTTP `200` as soon as the listener is running.
`GET /health` and `GET /health/ready` return HTTP `503` with JSON status
`initializing` (waiting for imported metadata), `indexing` (building or
opening Tantivy), or `error` (failed index initialization). Both return
HTTP `200` and `{"status":"ready"}` when the current metadata and index
are fully available. Dashboard health labels and a non-wrapping notice
show the current phase; proxy HTML/503 responses no longer generate raw
JSON parse exceptions. Until ready, `/match` and `/alias` return a
structured HTTP `503` indexing response rather than attempting a
request-time index build. A failed index remains visibly failed rather
than silently reporting successful readiness.

### Minimal matching API — `/match`

`GET /match` and `POST /match` return **exactly four fields**: `sid_start`, `sid_end`, `asid_start` and `asid_end`. The matcher runs independently: you **do not** need to call `/parse` first.

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H "Content-Type: application/json" \
  -d '{"title":"Supernatural.S01E01-E22.1080p"}'
```

Example response (assuming the selected IMDb identity is `tt0460681`):

```json
{
  "sid_start": "tt0460681:1:1",
  "sid_end": "tt0460681:1:22",
  "asid_start": null,
  "asid_end": null
}
```

- **Movie:** both SID fields contain the IMDb title ID, e.g. `tt1160419`.
- **Single TV episode:** both SID fields contain `ttID:season:episode`.
- **Episode/season pack:** SID fields identify the first and last episode when verified endpoints are available. Missing episode metadata is not guessed.
- **Anime:** ASID fields contain `anidb_id:episode` when verified mappings exist. An interval may cross AniDB IDs; the start and end can therefore have different prefixes.
- **No reliable match:** all four fields are `null`. Missing individual mappings are `null`; no synthetic IDs are returned.

**Series coordinate preservation:** MatchLab keeps explicit `SxxEyy` coordinates attached to the selected IMDb series even when an optional AniDB crosswalk is unavailable. For a single explicitly parsed episode without a season, it defaults to **season 1** unless imported first-season bounds disprove that episode (or an unresolved large anime absolute number makes this unsafe). Verified AniDB/TVDB remapping always wins.

If the title has a season but no episode and the imported `season_episodes` table cannot establish the season's final episode, MatchLab retains the season internally (`POST /debug/match/strict` → `selected.season`) and **does not invent an episode**. The legacy four-field `/match` SID contract cannot serialize a season without an episode: it returns the bare IMDb title ID. Clients such as StremThru can retain the known season from `/parse`. With neither coordinate, the same bare IMDb ID is returned; an independently verified AniDB identity can still return ASIDs with null SIDs if IMDb is unavailable.

For a season-labelled episode range, if the AniDB crosswalk is incomplete but imported IMDb season bounds independently confirm the entire range, MatchLab publishes both IMDb SID endpoints and leaves both ASIDs null. If the IMDb episode table also lacks the bounds, the season is retained in strict diagnostics and the public SID stays a bare title ID. No episode-zero placeholder is ever emitted.


For a URL query, use `GET /match?title=Supernatural.S01E01`. Both
endpoints accept `title` or `t_title` as the request input. A supplied
`year` can constrain matching.

**Production year policy for the public `GET/POST /match` API only:** A parsed or explicitly supplied torrent year
must match the IMDb title's production year **exactly** (no ±1-year
tolerance). When no year was parsed, or the candidate IMDb year is
different, the candidate is eligible **only if at least one** separate
identity signal is present:

- a parsed **season** for a series, such as `S02` or `Season 2`
- an imported **anime identity** linked to that IMDb candidate
- a parsed **quality** value, for example `WEB-DL`, `WEBRip` or `BluRay`

A parsed **resolution** such as `1080p` or `2160p` does **not** count.
Neither codecs (such as `x265`) nor video extensions substitute for a
quality value. The matcher checks the parser's `quality` field explicitly;
this helps exclude unrelated video files that only advertise resolution.

An unmarked episode number alone is not evidence. This rule is applied
to each candidate before the winner is chosen, not just to the highest
ranked title. **It does not apply to `POST /debug/match` or the dashboard
tester.** Entering `Fight Club` in the dashboard without a year or release
quality still returns ranked IMDb match candidates. Other matching
constraints, including season bounds and verified AniDB mappings, remain
unchanged. A date in a daily-show filename (YYYY-MM-DD) remains an
episode airdate, **not a production year**; the separate Cinemeta
airdate resolver retains its own ±1-calendar-day timezone tolerance.

### Chinese bilingual release paths and absolute episodes

A release such as `【高清剧集网发布 …】少年白马醉春风2[第41集]…/少年白马醉春风.The.Young.Brewmasters.Adventure.S02E41.2024….mp4` contains both a Chinese **series-absolute episode number** and an explicit file `S02E41`. The parser retains the normal file coordinates (`seasons:[2], episodes:[41]`), the Latin alternative title and year, and adds `absolute_episode_candidate:41` to the structured parse result. This field is a **hint**, not a verified episode.

The public matcher may reinterpret such a conflicting `S02E41` marker **only** when IMDb metadata proves that season 2 does not extend to episode 41, a unique verified AniDB/TVDB crosswalk maps absolute episode 41 into season 2, and the resulting canonical season/episode passes metadata bounds. For example, when that crosswalk yields season 2 episode 21, the returned SID is `ttID:2:21` and the ASID comes from the same verified mapping. An ambiguous or missing crosswalk does not justify inventing `S02E21`; a known valid canonical `S02E41` remains authoritative.

### Full parser API — `/parse`

`GET /parse` and `POST /parse` return the **complete structured parser output** for the torrent title, without requiring an IMDb/AniDB database lookup.

```bash
curl -X POST http://127.0.0.1:8765/parse \
  -H "Content-Type: application/json" \
  -d '{"title":"Supernatural.S02E03.1080p.WEB-DL"}'
```

The JSON contains every field the parser recognizes, such as `title`, `seasons`, `episodes`, `year`, `date`, `resolution`, `quality`, `codec`, `languages` and release metadata. A trailing unmarked number, for example `Jojo's bizarre adventure stone ocean 27`, may produce an `anime_episode_candidate` object with `title`, `episode`, `numbering: "absolute"` and `verified: false`. This is **not** a confirmed episode in `/parse`. During `/match`, MatchLab promotes the candidate only when Tantivy resolves the shortened title to a unique exact anime alias; the existing IMDb/AniDB crosswalk still determines whether any canonical SID/ASID can be published. Titles such as `District 9`, numbers in movie titles, production years and technical markers never become verified anime episodes merely by ending in digits. The same verification applies to names with and without a separating dash.

The native parser additionally exposes `half_episode: true` for conservatively recognized fractional anime episode notation such as `S01E06.5` or a verified fansub-style `12.5v2`. The numeric `episodes` field holds the integer portion; the half-episode flag is **descriptive only** and does not manufacture an IMDb or AniDB fractional episode ID. Explicit `第...季` season ordinals, Russian `<number> сезон <number> серия` and Cyrillic-confusable `S01Е02` markers are handled as native release-name syntax. East Asian absolute episode numbers remain unverified hints unless authoritative episode metadata confirms a mapping.

**Special episodes (season 0):** Standalone release tags such as `OVA`, `OAV`, `OAD`, `Special`, `Specials`, `SP01`, `Bonus Episode`, `Extra Episode`, `Unaired Episode`, `Omake`, `Picture Drama`, `Webisode` and `Recap Special` produce `seasons: [0]`, `special_episode: true`, and a descriptive `special_type`. The parent folders `Specials/` and `OVA/` also establish season 0 when no closer explicit season is present. An explicit `S01E03` in the leaf takes precedence, and OVA collections/batches and bare `ONA` (an online distribution format) are *not* automatically treated as standalone specials. Existing episode coordinates are preserved; if no special episode ordinal can be established, MatchLab does not invent an `S00E01` or AniDB mapping. During matching, `0` remains a valid IMDb season coordinate, while metadata-verified crosswalks continue to take precedence.

An optional `episode_title` field identifies descriptive text immediately after an explicitly marked episode (for example, `Show.S01E03.Pilot.1080p.WEB-DL` produces `episode_title: "Pilot"`). This is informational metadata: it never replaces the canonical show title, episode coordinates, or metadata-verified IMDb/AniDB mapping. Ambiguous bare anime episode numbers, group names, codecs, resolutions, subtitle tags, and language codes do not produce an episode title. Fields not recognized in the input are omitted; the exact set depends on the title. `GET /parse?title=...` is also supported.

### Alias lookup: `GET /alias`, `POST /alias`

Returns all exact normalized IMDb/AniDB title associations, without ranking,
prefix scanning or choosing a single winner. The indexed sources contain the
IMDb title/AKA catalog, AniDB titles and all identities loaded into
`anime_ids` by animeApi: **AniDB, TVDB, MyAnimeList (MAL), AniList and Kitsu**.

Pass exactly one selector in the query string or in a POST JSON body:

| Selector | Description |
| --- | --- |
| `imdb_id=tt0388629` | Every IMDb alias, linked AniDB alias and alternate ID |
| `anidb_id=69` | Names for a given AniDB work and IDs of its linked IMDb group(s) |
| `kitsu_id=987` | Titles linked to a Kitsu ID |
| `anilist_id=101` | Titles linked to an AniList ID |
| `mal_id=21` | Titles linked to a MyAnimeList ID |
| `tvdb_id=81797` | Titles linked to a TVDB ID |
| `query=One Piece` | Exact normalized IMDb and AniDB title, all matching IDs |
| `q=kitsu:987` | Automatic ID detection, also `mal:`, `anilist:`, `tvdb:`, `anidb:`, `imdb:` |
| `id_type=kitsu&id=987` | Generic typed ID lookup for any type imported in `anime_ids` |

```bash
curl -G --data-urlencode 'query=One Piece' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'imdb_id=tt0388629' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'anidb_id=69' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'kitsu_id=987' http://127.0.0.1:8765/alias
curl -X POST http://127.0.0.1:8765/alias \
  -H 'Content-Type: application/json' \
  -d '{"id_type":"anilist","id":"101"}'
```

The response preserves the existing `mode`, `input`, `normalized`,
`imdb_ids`, `anidb_ids`, `aliases`, `total`, `limit`, `offset` and
`has_more` fields, and adds `tvdb_ids`, `mal_ids`, `anilist_ids`,
`kitsu_ids`, `ids` (grouped by type), and `linked_ids_by_imdb`.
The latter preserves IMDb-group provenance to avoid suggesting that a
Kitsu or AniList ID applies directly to every AniDB part in a collection.
For text lookup, `imdb_ids` and `anidb_ids` continue to refer only to
**exact normalized alias matches**; other ID families are associated
through those IMDb identities. Multiple works with identical aliases or
third-party IDs are all preserved.

The underlying Tantivy schema stores names, AniDB IDs, and typed alternate
identities as separately indexed documents. **No SQLite title scans** run
on requests. The index has one-time schema7 regeneration on image upgrade
from the existing SQLite metadata; this does not re-download metadata.

Defaults: 500 aliases/page, up to 2,000 using `limit` and `offset`.
IDs are assembled across all hits, independently of `aliases` pagination.
Input is at most 512 UTF-8 bytes, and conflicting selectors are rejected.

### Automatic AniDB enrichment during `/match`

A normal **single-episode match** should return **both a canonical SID
and an AniDB ASID** when there is a uniquely verified crosswalk. For
example, with the One Piece Anime-Lists mapping (AniDB 69):

```bash
curl -G --data-urlencode 'title=One Piece S21E03' \
  http://127.0.0.1:8765/match
```

```json
{"sid_start":"tt0388629:21:3","sid_end":"tt0388629:21:3",
 "asid_start":"69:894","asid_end":"69:894"}
```

The matcher resolves AniDB coordinates while selecting candidates, and
also runs a **final, crosswalk-verified single-episode enrichment** when
an IMDb season/episode pair was assigned later (for instance, Cinemeta
airdate matching). This requires **no second API request** and does not
override selected IMDb IDs, episode boundaries, or match sources.
Unmapped, conflicting, non-anime and multi-episode releases retain a null
ASID rather than inventing one.

### Bidirectional episode conversion: `GET /episode/resolve`, `POST /episode/resolve`

Convert canonical IMDb episode coordinates into the corresponding AniDB
identity and AniDB episode number, **or convert back** from AniDB using
the same verified Anime-Lists/TVDB crosswalk already used by `/match`.

```bash
# IMDb season 21, episode 1 -> AniDB 69, absolute episode 892
curl -G --data-urlencode 'imdb_id=tt0388629' \
  --data-urlencode 'season=21' --data-urlencode 'episode=1' \
  http://127.0.0.1:8765/episode/resolve

# AniDB 69, episode 892 -> IMDb season 21, episode 1
curl -G --data-urlencode 'anidb_id=69' \
  --data-urlencode 'episode=892' \
  http://127.0.0.1:8765/episode/resolve

curl -X POST http://127.0.0.1:8765/episode/resolve \
  -H 'Content-Type: application/json' \
  -d '{"anidb_id":"69","episode":1089}'
```

Example response for a **unique** reverse mapping (dependent on imported
Anime-Lists metadata):

```json
{
  "direction": "anidb_to_imdb",
  "input": {"anidb_id": "69", "episode": 892},
  "found": true,
  "ambiguous": false,
  "count": 1,
  "selected": {
    "imdb_id": "tt0388629", "season": 21, "episode": 1,
    "anidb_id": "69", "anidb_episode": 892,
    "sid": "tt0388629:21:1", "asid": "69:892"
  },
  "matches": [
    {
      "imdb_id": "tt0388629", "season": 21, "episode": 1,
      "anidb_id": "69", "anidb_episode": 892,
      "sid": "tt0388629:21:1", "asid": "69:892"
    }
  ]
}
```

`selected` is null when the conversion is missing **or ambiguous**.
When two verified IMDb records can represent the same AniDB episode,
`matches` reports both and `ambiguous` is true. No arbitrary selection,
unverified season-1 default or IMDb title ranking is applied.
The reverse lookup derives candidate coordinates from explicit episode
maps, season offsets and split parts, then **rechecks each result through
the existing IMDb → AniDB resolver**. Canonical public `/match` remains
unchanged; the conversion API is an explicit metadata query, not a torrent
name parser.

### Airdate matching (no API key)

When a TV release contains a date instead of explicit SxxExx coordinates, MatchLab
uses the **same native Rust parser `date` field** exposed by `/parse`.
Regex handlers normalize matched release dates to ISO `YYYY-MM-DD`, and the
matcher resolves that date against the series' Cinemeta episode metadata.
There is no second, independent date regex in the IMDb matching path.

The bundled grammar accepts common `YYYY.MM.DD`, `YYYY-MM-DD`,
`YYYY_MM_DD`, `YYYYMMDD`, `DD.MM.YYYY`, named-month day-first,
month-first and year-first forms, and unambiguous US month-first numeric
dates where the day exceeds 12. Values are validated as real calendar dates,
including leap years; invalid dates are omitted. Numeric DD/MM/YYYY versus
MM/DD/YYYY is inherently ambiguous when both numbers are <= 12, so the
native parser defaults to day-first interpretation, not locale inference.
A completely arbitrary date notation is not guaranteed to be recognized.
Date-only releases are **never expanded into full IMDb season packs**, even
if a date lookup fails. Explicit SxxExx coordinates always win.

```text
The.Daily.Show.2022.01.20.Francis.Ngannou.1080p
```

```bash
curl 'http://127.0.0.1:8765/match?title=The.Daily.Show.2022.01.20.Francis.Ngannou.1080p'
```

On a verified date match, the normal four-field response contains
`ttID:season:episode` in both SID fields. Explicit SxxExx information
takes precedence over a date.

**Timezone tolerance (±1 day):** MatchLab always checks the requested
release date first. Only when there is **no episode on that date**, it
checks the preceding and following calendar days in the same locally
cached Cinemeta episode list. It accepts the fallback only when both
adjacent days together identify **one unique season/episode**. If the
exact date contains conflicting episodes, or different episodes match
the two neighboring days, it does not guess. No date more than one day
away is eligible. For example, `Jimmy.Kimmel.2024-09-16.WEB.x264-TORRENTGALAXY`
may match an episode dated September 17 when no September 16 episode exists.

The debug response exposes the original `release_date`, the actual
Cinemeta `air_date`, and `match_source=cinemeta_airdate_1day` for an
adjacent-day match. The public `/match` response remains unchanged.

Cinemeta is accessed only when needed, and its episode list is cached
for seven days in `/data/matchlab_airdates.sqlite3`. No API key is required.
If the upstream feed fails and the cache contains no suitable episodes,
MatchLab does not invent episode coordinates. The existing IMDb metadata
database is never modified by these lookups.

### Matching caches: exact-input parser and shared IMDb/AniDB lookup

The native Rust parser maintains a **30-second exact-input cache** shared by
`/parse` and `/match` (up to 1,024 entries). The key is the complete input
string, including any file path.

For repeated matches of **different file paths** in one torrent, MatchLab
additionally caches the expensive, deterministic **Tantivy title and anime
identity lookup candidates**. The cache uses the normalized title query,
lookup kind (exact, prefix/suffix, AniDB alias or identity), and the current
**immutable metadata generation**. The same title lookup is reused by
different episodes without repeating the Tantivy search. Each lookup has
a 10-minute TTL; caches are process-local, bounded to 512 keys per type, and
skip candidate sets larger than 256 items to avoid retaining expensive
high-cardinality title lookups. Simultaneous requests for the same key
share a single in-flight lookup instead of independently executing it.

**Crucially, final SID/ASID results are not shared between files.** The
`year`, `series`, season, episode, anime subfolder/part, and all
crosswalk validation are still applied **for each file**. This avoids
accidentally returning the episode 1 SID for episode 2, or conflating
different anime parts. Lookup errors are not cached. SQLite relational
metadata is not cached, and all text discovery remains in Tantivy.

The metadata generation key prevents old title candidates from being reused
after a full IMDb/AniDB refresh. In-memory SQLite databases bypass the
process-wide Tantivy lookup cache altogether so separate test databases
cannot mix identities.

There is currently **no infohash identifier on the /match API**: this is
generation-safe **shared-title caching**, which benefits torrent siblings
without requiring clients to send a new field. A future explicit
infohash-keyed torrent context cache would need additional validity guards
for files with conflicting years and anime subfolder titles.

For detailed ranking, candidates, episode mapping segments and debugging, use `POST /debug/match`. The dashboard at `/dash` also uses diagnostic data (legacy `/` redirects to `/dash`).

### Hierarchical paths: deepest metadata wins

Pass the full relative path as `title` or `t_title`. Both forward slashes and
backslashes separate components. MatchLab parses **every level independently**
from the outermost directory to the leaf filename. For each recognized,
non-empty field **except `title`**, a deeper value replaces the value from its
parent (year, season, episodes, resolution, quality, codec, language, etc.).
Fields not supplied at a deeper level remain inherited. A child episode or
season discards stale parent episode-range endpoints.

| Path | Effective parsed metadata |
| --- | --- |
| `Doctor.Who.2005.S01/Doctor.Who.S01E06.mkv` | Year 2005, season 1, episode 6 |
| `Doctor.Who.2005/Doctor.Who.2006.S01E06.mkv` | Year **2006**, season 1, episode 6 |
| `The.Show.2005.S01.2160p/Season.2.720p/The.Show.E06.1080p.mkv` | Year 2005, season 2, episode 6, resolution **1080p** |
| `Supernatural S01-S12/S02E01.mkv` | Season 2, episode 1; the parent pack does not override the file |

**Title is an exception.** Ordinary movie/series matching keeps the meaningful
leaf title, or inherits the nearest parent title when the leaf has no usable
title. The parser exposes non-generic directory titles in
`path_title_candidates` (outermost to innermost) for the matcher.

**Anime-specific title context:** A subdirectory may be an actual AniDB part,
cour, or season title distinct from the series' base name. When the matcher
finds an **exact, unambiguous AniDB alias** in a subdirectory that belongs to
an eligible IMDb identity already found from the leaf title, the **nearest**
matching subdirectory title takes precedence **for anime episode mapping only**.
For instance, `Sword Art Online/Sword Art Online Alicization/Sword.Art.Online.S01E01.mkv`
can identify the Alicization part even when the file uses the base series
title. Non-anime matches do not receive this override. Parent titles are
never accepted on their own as evidence to silently change an unrelated
IMDb identity.

An explicitly supplied API `year` overrides the parsed value, including the
deepest component's year. This operates on the path **string** provided to
MatchLab; it does not crawl folders or read the filesystem.

### Anime identity and episode mapping

### Title relevance and prefix/suffix ranking

The deterministic matcher first checks country-qualified and exact normalized
IMDb aliases. Without an exact hit, it queries Tantivy for aliases extending
the parsed title at either edge and for complete aliases at the **start or end**
of the parsed title. The embedded-prefix/suffix query uses one Tantivy term
union; no SQLite title scan, per-character round trip, learned model or index
schema change is needed.

A partial IMDb title match normally requires a **normalized word boundary**,
at least 50% coverage of the longer title, and at most 16 unmatched
characters. If a parser accidentally consumes part of a name as technical
metadata (e.g. `Televisionseries1` becomes `Television`), a longer alias
is also eligible when its **complete words** are independently present in
the original release. Numeric title suffixes get the same conservative
check. This prevents `Dune` matching `Duneland` merely because the first
characters overlap. Candidates are ordered by exact match,
relative coverage, leading versus trailing edge (only on equal coverage), then
title length. For otherwise eligible IMDb candidates, the only business
ranking criteria (in order) are:

1. **Title relevance** (exact match, relative coverage, whole-word edge and length).
2. **Distance from a release year**, when one was provided; otherwise all
   candidates have the same distance (zero), regardless of season/episode.
3. **Region preference**, retaining the existing `US`-first rule only where
   different IMDb works share the same normalized alias; this is not a user's
   locale setting. An explicitly country-qualified series title is already
   handled by the earlier exact-alias lookup.
4. **IMDb `numVotes`**, descending.

IMDb ID is used only as a deterministic last resort for complete ties; it is
not a semantic score. Original-title flags, media type, candidate start-year
ordering and IMDb AKA ordering are no longer ranking tie-breakers.
Media type and season/episode eligibility still decide **which candidates are
valid**, and independently verified episodes can still promote the correct
identity. Half-word prefix fallback is intentionally removed. Short exact
titles such as `Up` remain valid.

AniDB aliases now include sufficiently specific suffix matches in the same
single index query as prefixes. Unlike compact normalized keys, the original
release title preserves word starts: a trailing AniDB alias is accepted only
when it begins at a full source-title word boundary (e.g. `My Favorite BlueArchive`
may match `BlueArchive`, but `MyFavoriteBlueArchive` may not). Callers that
only have a compact key are restricted to exact/prefix aliases. Cached Tantivy
lookup results remain independent of raw-title word boundaries, which are
validated separately on each request; no index migration is necessary. Anime episode identity is evaluated in lexical
rank groups: when the best-looking anime alias cannot verify the requested
episode, a lower-ranked alias with an independently proven crosswalk may still
resolve it. Ambiguous, equally ranked *verified* anime identities are rejected.
No missing season, episode or ASID is inferred from title length.

Anime matching uses the same local metadata snapshot as IMDb matching, supplemented by **AniDB titles**, **animeApi** identity links and **Anime-Lists** episode mappings. It does not rely on AI inference or fabricate an IMDb/AniDB relationship.

1. The Rust parser extracts a title, optional season/episode and any absolute episode interval.
2. IMDb matching searches original and alternate titles (including country-specific aliases) and ranks eligible movie or TV-series identities.
3. Anime title matching independently checks AniDB aliases and linked IMDb identities. Exact anime aliases and anime-part titles can be relevant even when the release lacks explicit season tokens.
4. Where an episode is supplied, the resolver uses the imported crosswalk tables to translate between absolute anime numbering, AniDB numbering and mapped TVDB-style season/episode coordinates. Explicit mappings and split-season rules are respected.
5. A verified anime match can outrank an ordinary IMDb title candidate. An unrelated anime sharing an alias must not override an exact IMDb TV-series title.
6. If a required episode mapping is missing or ambiguous, the service does not invent canonical episode coordinates. The selected IMDb identity may still be available without a verified anime episode.

**Important:** Episode mappings in this project are **TVDB-style** coordinates from Anime-Lists, not independently verified IMDb season assignments. Do not assume the returned season number always equals IMDb's own season numbering.

#### Single anime-part episodes without an explicit season

Release names such as `[Erai-raws] Amanchu! Advance - 01 [1080p].mkv`
may restart episode numbering for a specific sequel/part, while omitting
`S02E01`. The parser already reports `episodes: [1]`, `seasons: []`.

When the matcher's AniDB alias index finds an **exact, unique title** mapped
to the selected IMDb series **and exactly one positive TVDB season**, it
interprets the release episode as local to that named part and verifies the
result with the AniDB/TVDB episode crosswalk. Thus, where the mapping data
supports it, `Amanchu! Advance - 01` maps to TVDB-style S02E01, while
`Amanchu! - 01` remains S01E01. The AniDB episode ID is returned only
when that same verified crosswalk provides it.

If the exact title is absent, has multiple conflicting AniDB identities,
belongs to multiple mapped seasons, or its requested episode has no verified
crosswalk entry, the matcher first attempts **absolute-episode mapping**.
That takes precedence over any default season; for example, a verified
`One Piece - 894` still maps to S21E03, not S01E894.

If the chosen IMDb series has an imported anime identity but the episode
has no verified crosswalk and no explicit season, the matcher may emit a
**tentative S01 SID** for an ordinary local episode (for example,
`Psycho-Pass - 04` → `tt2379308:1:4`), while leaving ASID `null`.
A uniquely identified later part retains its verified season number.
IMDb season-one episode bounds are checked where available; otherwise
unmarked large absolute-looking episode numbers above 150 do not receive
a guessed season 1. Neither the default season nor the SID is treated as
proof of an AniDB mapping. Explicit `SxxExx` coordinates still take
precedence.

#### AniDB alias collisions for long-running anime

An exact normalized AniDB title can belong to multiple different works, such
as the main `One Piece` TV series and films/OVAs also titled `One Piece`.
For an **unmarked absolute episode batch**, MatchLab first collects the exact
AniDB-title/IMDb associations from Tantivy. If only one IMDb association
exists, the existing partial-range behavior is preserved.

When several associations share the exact title, MatchLab checks the episode
crosswalk for each identity. Only one IMDb identity must independently verify
**every episode** in the requested interval before the batch is promoted from
`anime_episode_range_candidate` to canonical `episode_start`/`episode_end`.
This uses cheap endpoint checks before any full-range verification. A second
fully mapped identity, missing crosswalk data, or too many ambiguous title
associations leaves the unmarked range tentative. Neither IMDb ranking nor
AniDB ID ordering is used to break a real tie.

For example, with suitable Anime-Lists mappings, `One Piece - 0892 ~ 1089`
spans TVDB-style season 21 episode 1 through season 22 episode 4; the public
response reports those verified SID endpoints and the AniDB 69 absolute episode
endpoints. The main title matcher still determines the selected IMDb title;
a batch is never attached to an unrelated IMDb winner.

#### AnimeTosho-style episode batches

Anime release names often omit SxxExx notation. MatchLab recognizes numeric
batch candidates such as:

| Release name fragment | Meaning after anime identity verification |
| --- | --- |
| Anime Title 1-7 / 01-07 / 001-007 | Absolute episodes 1–7 |
| Anime Title (01-25) / [01-25] | Absolute episodes 1–25 |
| Anime Title 01~12 / 01 – 12 / 01～12 | Absolute episodes 1–12 |
| Anime Title 01-12 Batch | Absolute episodes 1–12 |
| Anime Title 01-12 + OVA | Episodes 1–12 only; the additional OVA is not inferred |

The /parse endpoint is **metadata-free**. It reports these as tentative
anime_episode_range_candidate objects containing the title, start/end,
numbering=absolute and verified=false. It does **not** treat the candidate
as canonical episode_start/episode_end. During /match, the service uses
Tantivy to find an **exact, unambiguous AniDB-linked title** corresponding
to the candidate title. Only then does it promote the endpoints and validate
each episode against the AniDB/TVDB crosswalk. A matching IMDb title alone
does not authorize an unmarked anime episode interval.

A validated candidate may resemble:

```json
{
  "anime_episode_range_candidate": {
    "title": "Spy x Family",
    "start": 1,
    "end": 7,
    "numbering": "absolute",
    "verified": false
  },
  "anime_title_verified": true,
  "episode_range_numbering": "absolute",
  "episode_start": 1,
  "episode_end": 7,
  "episode_count": 7
}
```

The verified flag in the original candidate stays false: it describes
untrusted lexical input. anime_title_verified establishes identity only;
episode_range.complete indicates whether **all** requested episodes were
mapped. A partial crosswalk never fills missing SID or ASID endpoints with
guessed values. Mapping is batched with one crosswalk read instead of running
the same SQLite queries once per episode.

The parser excludes release years (2015-2020), resolution dimensions
(1920x1080), season/volume/part labels (Season 1-7, Vol 1-7),
standalone title 009-1, and ambiguous prose. No interval is inferred when
the release explicitly requests movie matching or when the anime identity
is absent/ambiguous.

**Season-numbered intervals are different:** S01E01-E12,
S01E01-S01E12, and S1E01-12 carry a requested season and are not
interpreted as absolute anime numbering. Ordinary series use imported
IMDb season bounds, while AniDB-linked titles require verified episode
crosswalks. Mismatched season endpoints (S01E01-S02E12) are not
flattened into one season. A conservative 1,000-episode maximum applies
to newly recognized numeric batches.


#### Absolute episode intervals across seasons

For `One.piece.E768-E1064`, the parser preserves:

```json
{
  "episode_start": 768,
  "episode_end": 1064,
  "episode_count": 297
}
```

The diagnostic endpoint (`POST /debug/match`) resolves each episode independently. Its `episode_range` object contains `absolute_start`, `absolute_end`, `start_season`, `start_episode`, `end_season`, `end_episode`, `segments`, `unmapped_count` and `complete`. Each segment groups consecutive, verified episodes within a mapped season. A range can span multiple seasons. `complete` is `true` only if every episode has a mapping; missing endpoints remain `null`, and unmapped episodes are counted rather than interpolated. The public `/match` response exposes only `sid_start`, `sid_end`, `asid_start` and `asid_end`; full segment details remain in diagnostics.

The availability and correctness of conversions depend on the imported metadata and its season conventions.

When no reliable match exists, `/match` returns an object with four `null` values. Invalid input returns HTTP `400`.

To inspect the candidate lists programmatically, use the diagnostic endpoint:

```bash
curl -X POST http://127.0.0.1:8765/debug/match \
  -H "Content-Type: application/json" \
  -d '{"title":"Breaking.Bad.S02E03.1080p"}'
```

### Complete season packs

MatchLab imports IMDb `title.episode.tsv.gz` into a per-season `season_episodes` table, keyed by IMDb series ID and season number. Each row stores `episode_count` (number of numbered episode rows) and `max_episode` (highest observed episode number). The matcher uses **max_episode**, because episode numbering may have gaps.

For a season-only release such as `Supernatural.S02`, the selected match receives `season=2`, `episode=1`, `season_end=2`, and `episode_end=22` **if** the imported IMDb metadata reports episode 22 as the maximum. A concrete episode such as `Supernatural.S02E01` remains a single episode and is not expanded. When season metadata is missing, MatchLab does not invent an endpoint.

The season table uses IMDb season numbering; anime crosswalk mappings may use different TVDB-style season conventions.



### Read-only parsing diagnostics feed

The separate, opt-in debug feed allows an external reviewer or scheduled monitor
to poll the same **last 1,000** retained matching requests that the dashboard uses.
It **never triggers a new match** and does not alter history, metadata or the
Tantivy index. It has no extra SQLite table or schema migration.

Set a randomly generated secret of at least **32 bytes**:


```bash
export MATCHLAB_DEBUG_API_TOKEN="$(openssl rand -hex 32)"
```

Pass this variable into the MatchLab container using your secrets or Compose
configuration. Without a valid configuration, debug feed routes return 404.
With a configured secret, missing or incorrect bearer authorization returns 401.
Use HTTPS and protect the rest of the application behind your reverse proxy:
this token does **not** protect the pre-existing public dashboard endpoints.

```bash
# First, learn the latest retained request ID.
curl -fsS -H "Authorization: Bearer $MATCHLAB_DEBUG_API_TOKEN" \
  https://matchlab.example.com/api/debug/parsing/status

# Catch up from a saved cursor; request IDs are monotonically increasing.
curl -fsS -H "Authorization: Bearer $MATCHLAB_DEBUG_API_TOKEN" \
  'https://matchlab.example.com/api/debug/parsing?after_id=0&limit=100&suspect_only=true'

# Inspect a specific retained request's complete diagnostic and candidates.
curl -fsS -H "Authorization: Bearer $MATCHLAB_DEBUG_API_TOKEN" \
  https://matchlab.example.com/api/debug/parsing/42
```

/api/debug/parsing query parameters:
- `after_id`: Nonnegative, exclusive numeric cursor; defaults to 0. Use
  `status.latest_id` as the initial cursor if you only want future requests.
- `limit`: Scan 1–100 raw requests per page, default 100.
- `suspect_only=true`: Return only rows with heuristic anomaly
  flags. **Still advance** using `next_after_id` even when `items` is empty,
  to avoid re-reading unflagged requests indefinitely.
- `include_details=true`: Add the full stored diagnostic (including
  ranked candidate details). The default includes the original release title,
  timestamp, status, selected and parsed values, and anomaly flags.

The response returns `oldest_id`, `latest_id`, `retained`,
`next_after_id`, `has_more`, and `cursor_expired`.
When `has_more=true`, request the next page immediately. When it is false,
poll again later from `next_after_id`. If `cursor_expired=true`, the 1,000-row
retention cap caused a data gap; reset to the earliest available history and
record the gap. The feed is not a permanent historical archive.

The anomaly flags are **conservative review hints**, not automatic corrections.
Current checks cover explicit `SxxEyy` and Spanish `Cap.103` / `Cap.1004`
marker disagreement with selected or parsed coordinates, unresolved absolute
anime episode hints (such as `Stone Ocean 27`), unmatched explicitly episodic
releases, impossible nonpositive coordinates, unusually large seasons,
reversed episode ranges, and matching errors. A verified AniDB mapping that deliberately changes an explicit episode
is not reported as an explicit-marker mismatch.

**Security:** Only the HTTPS `Authorization: Bearer` header is accepted,
never URL tokens, cookies, or HTTP Basic Auth. Results carry
`Cache-Control: no-store`. Keep the token out of logs, browser URLs,
commits and GitHub issues. An API consumer must have a network route to the
service; deploying this endpoint does not itself make an unreachable hostname
accessible. Reviewers or automation can use the feed to prepare and verify PRs,
but no unattended polling or PR creation is started merely by enabling it.

## Local Machine Learning status

`GET /api/ml/status` reports whether the local learning worker is
enabled, the number of currently active field-specific technical regex
overlays, counts grouped by technical field, and the rule-generation hash.
It returns **no torrent names, IMDb IDs, history, private hashes, or model
inputs**. No GitHub token is required. This is a monitoring endpoint, not a
control for enabling learning or approving rules.

The JSON fields include `enabled`, `active_rule_count`,
`active_rules_by_field`, `revision`, `training_source`, `model`,
and `identity_and_coordinate_override`. The `training_source` value
now distinguishes the canonical Rust parser and independently allowlisted
raw filename markers for missing technical fields. The `model` value is
`rust_ftrl_proximal`; `online_learner` exposes only the bounded model
type, parameter size and trusted label counts (no release names or IDs).
The native online model trains in the background but its score remains
shadow-only and never authorizes an IMDb, episode or regex override.
The in-memory status does not query SQLite or run inference.

`online_learner` is a privacy-safe object describing the native FTRL
classifier, its 128 KiB parameter state, accepted independent labels,
counterfactual negatives, title-group holdout pairs, pairwise accuracy,
recent pairwise accuracy and the conservative `drift_review_suggested`
flag. These are **shadow evaluation statistics**, not calibrated scores or
actual IMDb parsing/matching accuracy. The model is never run on an HTTP
request and can never authorize a regex or identifier change.

`training_and_cache` reports the configured corpus evidence limit
(`training_max_examples`), shared disk budget, persistent cache row limit,
positive and no-match TTLs, and in-process cache hits/misses. The cache
reuses repeated exact parse/match requests only within the current
source/rules/metadata namespace; obsolete results are always re-parsed and
re-matched on demand. It is never a substitute for new verification.

`candidate_discovery` exposes only grouped counts of unknown release
markers eligible for human review. The underlying marker strings, titles,
hashes and model candidate predictions do not leave the local opt-in
SQLite corpus; these candidates cannot automatically become active
regexes or GitHub proposals.

Set `MATCHLAB_ML_ENABLED=true` on the MatchLab container to opt in.
Absolute-anime episode rules and technical-field overlays operate under
separate validators; this endpoint enumerates the technical-field rules.
See [Community Learning](COMMUNITY_LEARNING.md) for evidence thresholds,
limitations, privacy and the separate GitHub sharing opt-in.
