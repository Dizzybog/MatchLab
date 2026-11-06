# MatchLab API reference

Detailed endpoint behavior, anime and episode mappings, airdate matching, range handling, and debugging.

## Endpoints

The API listens on port **8765**.

### Check status

```bash
curl http://127.0.0.1:8765/health
```

The service returns HTTP `200` when the metadata is ready and `503` while it is initializing.

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

For a URL query, use `GET /match?title=Supernatural.S01E01`. Both endpoints accept `title` or `t_title` as the request input. A supplied `year` can constrain matching.

### Full parser API — `/parse`

`GET /parse` and `POST /parse` return the **complete structured parser output** for the torrent title, without requiring an IMDb/AniDB database lookup.

```bash
curl -X POST http://127.0.0.1:8765/parse \
  -H "Content-Type: application/json" \
  -d '{"title":"Supernatural.S02E03.1080p.WEB-DL"}'
```

The JSON contains every field the parser recognizes, such as `title`, `seasons`, `episodes`, `year`, `resolution`, `quality`, `codec`, `languages` and release metadata. Fields not recognized in the input are omitted; the exact set depends on the title. `GET /parse?title=...` is also supported.

### Airdate matching (no API key)

When a TV release contains a date instead of explicit SxxExx coordinates, MatchLab
identifies the IMDb series from the title preceding the date, then looks up
the episode's premiere date in Cinemeta's public episode feed.

```text
The.Daily.Show.2022.01.20.Francis.Ngannou.1080p
```

```bash
curl 'http://127.0.0.1:8765/match?title=The.Daily.Show.2022.01.20.Francis.Ngannou.1080p'
```

On a verified date match, the normal four-field response contains
`ttID:season:episode` in both SID fields. Explicit SxxExx information
takes precedence over a date. Cinemeta is accessed only when needed, and
its episode list is cached for seven days in `/data/matchlab_airdates.sqlite3`.
No API key is required. If the upstream feed fails or the date is missing
or ambiguous, MatchLab does not invent episode coordinates. The existing
IMDb metadata database is never modified by these lookups.

### Shared 30-second cache

Both APIs use the same native Rust parser. A bounded, in-memory cache stores parsed results by **exact input title** for **30 seconds**. If a client calls `/parse` and then `/match` with the same title, the second request can reuse the parsing result; the reverse order works too. `/match` always performs its own metadata matching and does not require a preceding parse call.

The cache is process-local (not shared between replicas), has a maximum of 1,024 entries, and does not persist across restarts. The cache stores parsing results, **not** full match results.

For detailed ranking, candidates, episode mapping segments and debugging, use `POST /debug/match`. The dashboard at `/` also uses diagnostic data.

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

Anime matching uses the same local metadata snapshot as IMDb matching, supplemented by **AniDB titles**, **animeApi** identity links and **Anime-Lists** episode mappings. It does not rely on AI inference or fabricate an IMDb/AniDB relationship.

1. The Rust parser extracts a title, optional season/episode and any absolute episode interval.
2. IMDb matching searches original and alternate titles (including country-specific aliases) and ranks eligible movie or TV-series identities.
3. Anime title matching independently checks AniDB aliases and linked IMDb identities. Exact anime aliases and anime-part titles can be relevant even when the release lacks explicit season tokens.
4. Where an episode is supplied, the resolver uses the imported crosswalk tables to translate between absolute anime numbering, AniDB numbering and mapped TVDB-style season/episode coordinates. Explicit mappings and split-season rules are respected.
5. A verified anime match can outrank an ordinary IMDb title candidate. An unrelated anime sharing an alias must not override an exact IMDb TV-series title.
6. If a required episode mapping is missing or ambiguous, the service does not invent canonical episode coordinates. The selected IMDb identity may still be available without a verified anime episode.

**Important:** Episode mappings in this project are **TVDB-style** coordinates from Anime-Lists, not independently verified IMDb season assignments. Do not assume the returned season number always equals IMDb's own season numbering.

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

