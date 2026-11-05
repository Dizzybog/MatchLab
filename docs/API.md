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

### Hierarchical paths and folder inheritance

Pass a full relative path as the `title` or `t_title` value. Forward slashes and backslashes separate path components. MatchLab parses the **leaf** (last component) first. If the leaf supplies season/episode coordinates but lacks a meaningful series title, it looks for a title in the nearest suitable parent folder. Folder names supply missing context, **not** a replacement for the leaf's explicit episode information.

| Input | Intended interpretation |
| --- | --- |
| `Supernatural S01-S12/S02E01` | Series `Supernatural`, season 2, episode 1; `S01-S12` is collection context. |
| `Supernatural S01-S12/S02E01.mkv` | Same episode, with a filename extension. |
| `One.piece.E768-E1064` | Absolute episode interval 768–1064 (297 episodes), potentially crossing season boundaries. |

A parent folder does not automatically assign its season range to a child file. Explicit child coordinates take precedence. This is title inheritance, not a filesystem crawler: MatchLab parses the path **string supplied in the request** and does not inspect directories on disk.

### Anime identity and episode mapping

Anime matching uses the same local metadata snapshot as IMDb matching, supplemented by **AniDB titles**, **animeApi** identity links and **Anime-Lists** episode mappings. It does not rely on AI inference or fabricate an IMDb/AniDB relationship.

1. The Rust parser extracts a title, optional season/episode and any absolute episode interval.
2. IMDb matching searches original and alternate titles (including country-specific aliases) and ranks eligible movie or TV-series identities.
3. Anime title matching independently checks AniDB aliases and linked IMDb identities. Exact anime aliases and anime-part titles can be relevant even when the release lacks explicit season tokens.
4. Where an episode is supplied, the resolver uses the imported crosswalk tables to translate between absolute anime numbering, AniDB numbering and mapped TVDB-style season/episode coordinates. Explicit mappings and split-season rules are respected.
5. A verified anime match can outrank an ordinary IMDb title candidate. An unrelated anime sharing an alias must not override an exact IMDb TV-series title.
6. If a required episode mapping is missing or ambiguous, the service does not invent canonical episode coordinates. The selected IMDb identity may still be available without a verified anime episode.

**Important:** Episode mappings in this project are **TVDB-style** coordinates from Anime-Lists, not independently verified IMDb season assignments. Do not assume the returned season number always equals IMDb's own season numbering.

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

