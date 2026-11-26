# Native Rust metadata lifecycle

The production Docker image contains only the Rust HTTP API and native metadata importer. No Python runtime is required.

## Setup

Run `docker compose up -d`. The service uses a persistent volume at `/data` and listens on localhost port 8765. Without a populated SQLite database, `/health` returns 503.

Automatic initialization and refresh use seven built-in public HTTPS URLs. No environment variables are required. Set `MATCHLAB_METADATA_URLS` only to override all seven sources, in this order:

1. IMDb `https://datasets.imdbws.com/title.basics.tsv.gz`
2. IMDb `https://datasets.imdbws.com/title.akas.tsv.gz`
3. IMDb `https://datasets.imdbws.com/title.episode.tsv.gz`
4. IMDb ratings `https://datasets.imdbws.com/title.ratings.tsv.gz`
5. animeApi `https://raw.githubusercontent.com/nattadasu/animeApi/refs/heads/v3/database/animeapi.tsv`
6. Anime-Lists `https://raw.githubusercontent.com/Anime-Lists/anime-lists/refs/heads/master/anime-list.xml`
7. AniDB titles `https://anidb.net/api/anime-titles.xml.gz`

`MATCHLAB_METADATA_REFRESH_HOURS` defaults to 168 and supports 1–720 hours. The Rust service starts the first refresh on startup, then repeats at the interval. Refreshes are serialized; failures are logged and the last published SQLite database is preserved. The metadata importer builds a staging database, validates integrity, builds and verifies the matching Tantivy generation under `metadata.sqlite3.tantivy/`, and only then atomically replaces the published SQLite database. Each SQLite snapshot records its Tantivy generation ID. Old in-flight readers retain their old generation; no successful refresh rebuilds the newly published index. Historical completed generations are retained for at least seven days, preserving two previous generations.

Alternatively, import six local files using `matchlab-server refresh-metadata <database.sqlite3> <basics.tsv.gz> <akas.tsv.gz> <episodes.tsv.gz> <animeapi.tsv> <anime-list.xml> <anidb-titles.xml.gz>`.

## Anime-Lists entries with multiple IMDb IDs

Anime-Lists can associate a single AniDB entry with several IMDb movies or
specials using a comma-separated `imdbid` attribute. MatchLab imports each
syntactically valid IMDb title ID separately; it never stores the comma-joined
value as an IMDb ID or arbitrarily chooses the first ID. When the same AniDB
alias points to multiple equally plausible IMDb identities, anime-alias
priority leaves the title unmatched until other independent title and episode
evidence resolves the ambiguity.

A previously published SQLite/Tantivy generation can still contain the
old malformed strings after upgrading the image. Runtime validation suppresses
these invalid identities, but **the split associations appear only after the
next successful full metadata refresh**. The refresh is normally weekly.
To prompt an earlier refresh, temporarily set
`MATCHLAB_METADATA_REFRESH_HOURS: "1"` in the Compose environment and
recreate the MatchLab container; a refresh runs when the previous snapshot
is at least one hour old. Watch for `scheduled metadata refresh completed`,
then restore the normal 168-hour interval. Do not delete the metadata volume:
the importer atomically replaces completed snapshots while preserving the
last usable generation if downloading or indexing fails.

## SQLite index lifecycle

Tantivy handles all title and anime-alias discovery. SQLite maintains relational
primary keys for IMDb IDs, season boundaries, anime identities, and episode mapping.
The full metadata refresh stages the SQLite database without redundant search indexes,
builds `episode_lookup_idx` immediately before the Anime-Lists and AniBridge imports
(which need efficient lookup while they update rows), and creates
`anime_series_by_anidb`, `episode_by_tvdb`, and `episode_by_anidb_order`
**after bulk ingestion** but **before** `ANALYZE`, integrity checks, Tantivy
generation construction, and atomic publication.

The `anime_ids(group_id,type,value)` primary-key index covers runtime anime-ID
lookup; the old `anime_lookup_idx(type,value)` secondary index is omitted.
The standalone `import-imdb` command still creates the complete relational
index set after its own import; full multi-source refresh defers the later indexes.
The separate request-history and Cinemeta airdate SQLite files shed duplicate
indexes on their next initialization; existing metadata snapshots are immutable
and adopt the revised index layout on their next successful refresh.

These optimizations are checked with `EXPLAIN QUERY PLAN` regression tests.
The importer logs the elapsed time spent constructing relational indexes, but
those timings alone do not establish an overall speedup. Validate full-catalog
import times, peak memory, and p95/p99 match latency on production-scale data
before quantifying performance gains.

## Generic anime episode crosswalk inference

**This is data-driven, not a list of per-anime titles.** During every successful
metadata refresh, MatchLab combines Anime-Lists numbering semantics, animeApi
AniDB/IMDb identity links and IMDb's imported `season_episodes` bounds to
materialize **finite episode-map rows once**, before the live snapshot is
published. No additional API key, request-time web service, or hot-path SQLite
search is required.

Only two safe patterns are eligible:

- **Separate AniDB works per IMDb season:** A unique AniDB ID directly linked
  to that IMDb identity has an open-ended Anime-Lists
  `defaulttvdbseason=N` rule. If the corresponding IMDb season contains
  consecutive episode numbers (the imported count equals the maximum), the
  default row is bounded to that season's observed episodes. A similarly
  named AniDB work discovered *only* through the shared TVDB family cannot
  claim the IMDb identity by inference.
- **One AniDB work across IMDb seasons:** A single directly linked AniDB ID
  explicitly uses `defaulttvdbseason=a` (absolute numbering). MatchLab
  computes each known IMDb season's cumulative episode start/end and builds
  finite absolute-to-season offsets, stopping at gaps, incomplete counts,
  existing explicit rows or competing AniDB season ownership. A merely
  open-ended `defaulttvdbseason=1` does **not** imply that the AniDB work
  owns all later seasons.

Explicit and finite upstream Anime-Lists and AniBridge mappings
have priority; incomplete or conflicting evidence is never silently completed.
The inferred ranges use the ordinary bidirectional
`/episode/resolve` and `/match` crosswalk, with no title-specific runtime
conditions. This covers **any anime with sufficient evidence**, not just the
test examples. Some source gaps still need upstream fixes (AniBridge or Anime-Lists) or stronger
upstream AniDB episode-count evidence. In particular, IMDb season counts
**alone** cannot prove which AniDB work owns a particular season.

The metadata build logs `generic Anime-Lists crosswalk inference completed`
with the number of generated rows and ambiguous groups; it records the count
under `metadata_state/animelists_inferred`. The metadata fingerprint includes
the generic inference revision, so upgrading an old image schedules a full
refresh. The importer also applies these rules after a manual
`import-animelists` command.

## AniBridge episode mappings

Every full metadata refresh also downloads the daily
[AniBridge mappings](https://github.com/anibridge/anibridge-mappings)
release (`mappings.min.json`, about 10 MB). AniBridge merges Anime-Lists with
other sources, validates every range against TVDB and TMDB metadata and adds
thousands of manual corrections. After the regular Anime-Lists import,
MatchLab replaces the regular-episode `episode_map` rows of every AniDB work
that AniBridge maps to TVDB with AniBridge's bounded ranges. Anime-Lists stays
the source for IMDb links, specials and every work AniBridge does not map.

From these ranges the importer derives, without any hand-maintained list:

- **Absolute numbering of later works.** The works of one TVDB family are
  ordered by their TVDB position; each later work starts after the AniDB
  episode count of the works before it (Bleach: Thousand-Year Blood War
  starts at 367, Pokémon Journeys at 1089). A TVDB gap larger than a few
  episodes, an overlap or an open-ended work ends the chain. A work in a
  TVDB season that the crosswalk splits into Trakt cours gets no offset.
- **IMDb's finer season split.** When IMDb divides the same run into more
  seasons than TVDB, with every TVDB season boundary also an IMDb boundary
  and the same total, the ranges are rewritten into IMDb seasons (Black
  Clover: TVDB S1 1-170 is IMDb S1-S4; the sequel is IMDb S5). A coarser
  IMDb split, or a single IMDb season, keeps the TVDB seasons.
- **Leading episodes of a work that begins its TVDB season** but whose
  AniBridge range starts later (Pokémon Diamond and Pearl from episode 49).

Contradictory AniBridge ranges (one AniDB episode in two places) keep the
Anime-Lists rows. A failed AniBridge download leaves the refresh on
Anime-Lists alone.

The snapshot records `anibridge` and a versioned mapping-pipeline marker in
`metadata_state`. On startup, a missing or outdated marker triggers an
immediate full metadata refresh, even if the ordinary seven-day schedule has
not elapsed. The existing database remains usable until the new generation
is ready.

## Verification

```bash
RUSTFLAGS='-D warnings' cargo test --workspace
docker build -t matchlab:local .
curl http://127.0.0.1:8765/health
```

The container has no built-in authentication. Keep the localhost port binding or use an authenticated HTTPS reverse proxy.

The HTTP listener starts while metadata initializes. Docker health checks return HTTP 503 until the first complete import succeeds. The initial download requires outbound HTTPS connectivity and sufficient disk space.
