# Tantivy runtime architecture

This document describes the current Rust implementation in PR #40.

## Data ownership

Tantivy is the sole title-discovery engine. The disk index stores IMDb aliases with their normalized forward and reverse terms, original title, year, type, region, ordering and IMDb ID, plus mapped AniDB title aliases. Exact-name, prefix, reverse-suffix, country-qualified, shorter/half-prefix, duplicate-title and anime alias matching all use Tantivy.

SQLite remains authoritative for relational lookups: `seasons`, `season_episodes`, `imdb_ratings`, `anime_ids`, `anime_series_map` and `episode_map`. MatchLab's deterministic matcher still decides which candidate wins; Tantivy scores cannot override deterministic ranking.

## Refresh and publication

A complete import stages an immutable SQLite snapshot. It validates all required metadata, runs integrity checks, and records a unique `metadata_state/tantivy_generation` identifier. The importer builds a Tantivy index at `metadata.sqlite3.tantivy/schema5-<generation>` using an unpublished sibling staging directory.

The Tantivy builder commits documents, verifies the committed count, writes a completion marker and syncs index directory entries. Only then does the importer atomically rename the staging SQLite database over the published database.

A request's SQLite snapshot contains the index-generation identifier it must use; in-flight old connections cannot inadvertently mix their data with the new title index. A successful refresh therefore does not rebuild an index on the first request. Normal startup opens the persisted index. A legacy database without a generation marker gets a compatible index built during startup.

The index cache permits simultaneous readers of several generations and bounds its entries. Historical completed indexes are retained conservatively: at least two prior generations are kept, with removal permitted only beyond seven days.

## Performance and reliability

Exact IMDb title matching runs a Tantivy term query first, avoiding costly prefix enumeration in the common case. AniDB alias prefix lookups are combined into one Tantivy term-set query. Less common prefix/suffix fallbacks collect all candidates before deterministic ranking; there is no arbitrary TopDocs cutoff.

Tantivy indexing uses a configured 64 MB writer buffer, but peak memory usage is higher due to reader caches and importer data. Budget disk space for simultaneous generations. Green CI alone does not establish capacity at the full IMDb catalog size. Test cold startup, sustained query throughput, refresh overlaps, crash recovery, p95/p99 latency and peak memory on production-like data before a high-volume rollout.

## Lessons from StremThru

The implementation was reviewed against these upstream components:

- [IMDb mapping worker](https://github.com/MunifTanjim/stremthru/blob/main/internal/worker/map_imdb_torrent.go) and [title search](https://github.com/MunifTanjim/stremthru/blob/main/internal/imdb_title/db.go): categorize releases before matching, prioritize exact title/year, and validate candidates after indexed retrieval. MatchLab retains these principles but uses Tantivy rather than SQLite FTS5 or PostgreSQL text search.
- [AniDB matcher](https://github.com/MunifTanjim/stremthru/blob/main/internal/worker/map_anidb_torrent.go): avoid repeating normalization/edit-distance calculations across candidates, and reject uncertain episode mappings. MatchLab retains deterministic ordering and verified crosswalks rather than importing an unrelated fuzzy similarity cutoff.
- [Parser worker](https://github.com/MunifTanjim/stremthru/blob/main/internal/worker/torrent_parser.go) and [Torznab bounded fetch pool](https://github.com/MunifTanjim/stremthru/blob/main/internal/stremio/torz/stream.go): throttle expensive work and reuse parsed results. MatchLab bounds concurrent blocking match operations with `MATCHLAB_MAX_CONCURRENT_MATCHES`, and its parser cache evicts only the oldest live item under pressure.

The AniDB alias search now submits all eligible prefix terms in a single Tantivy term-set query and preserves the former deterministic result ordering. Do **not** copy StremThru's early limited FTS candidate shortlist into MatchLab: it could discard valid IMDb identities before complete ranking.

## Validation

```bash
RUSTFLAGS='-D warnings' cargo test --workspace
docker build -t matchlab:test .
```
