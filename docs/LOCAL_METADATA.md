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
builds `episode_lookup_idx` immediately before Anime-Lists mapping overrides
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

## Bundled Anime-Lists mapping corrections

The production Rust binary includes `data/anime-list.override.xml`. Every full metadata refresh imports the regular Anime-Lists feed first, then **replaces** `episode_map` rows for each AniDB ID listed in the bundled exceptions. It also updates any existing `anime_series_map` TVDB family references while preserving IMDb links. The ordinary episode crosswalk uses the resulting combined SQLite tables; there is no runtime override lookup or extra network request.

The override import is part of the staged database build and happens before Tantivy index generation and publication. If it fails, the current working database is preserved. The staging snapshot records `animelists_override` and a versioned fingerprint marker in `metadata_state`. On startup, a missing or outdated marker triggers an immediate full metadata refresh, even if the ordinary seven-day schedule has not elapsed. The existing database remains usable until the new generation is ready. The same embedded corrections are used for custom `MATCHLAB_METADATA_URLS` sources. Updating the list requires a new image; the service then refreshes it automatically.

## Verification

```bash
RUSTFLAGS='-D warnings' cargo test --workspace
docker build -t matchlab:local .
curl http://127.0.0.1:8765/health
```

The container has no built-in authentication. Keep the localhost port binding or use an authenticated HTTPS reverse proxy.

The HTTP listener starts while metadata initializes. Docker health checks return HTTP 503 until the first complete import succeeds. The initial download requires outbound HTTPS connectivity and sufficient disk space.
