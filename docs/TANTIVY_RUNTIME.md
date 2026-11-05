# Tantivy runtime architecture

This document describes the current Rust implementation in PR #40.

## Data ownership

Tantivy is the sole title-discovery engine. The disk index stores IMDb aliases with their normalized forward and reverse terms, original title, year, type, region, ordering and IMDb ID, plus mapped AniDB title aliases. Exact-name, prefix, reverse-suffix, country-qualified, shorter/half-prefix, duplicate-title and anime alias matching all use Tantivy.

SQLite remains authoritative for relational lookups: `seasons`, `season_episodes`, `imdb_ratings`, `anime_ids`, `anime_series_map` and `episode_map`. IMDb V4 still decides which candidate wins; Tantivy scores cannot override deterministic ranking.

## Refresh and publication

A complete import stages an immutable SQLite snapshot. It validates all required metadata, runs integrity checks, and records a unique `metadata_state/tantivy_generation` identifier. The importer builds a Tantivy index at `metadata.sqlite3.tantivy/schema5-<generation>` using an unpublished sibling staging directory.

The Tantivy builder commits documents, verifies the committed count, writes a completion marker and syncs index directory entries. Only then does the importer atomically rename the staging SQLite database over the published database.

A request's SQLite snapshot contains the index-generation identifier it must use; in-flight old connections cannot inadvertently mix their data with the new title index. A successful refresh therefore does not rebuild an index on the first request. Normal startup opens the persisted index. A legacy database without a generation marker gets a compatible index built during startup.

The index cache permits simultaneous readers of several generations and bounds its entries. Historical completed indexes are retained conservatively: at least two prior generations are kept, with removal permitted only beyond seven days.

## Performance and reliability

Exact IMDb title matching runs a Tantivy term query first, avoiding costly prefix enumeration in the common case. Less common prefix/suffix fallbacks collect all candidates before deterministic ranking; there is no arbitrary TopDocs cutoff.

Tantivy indexing uses a configured 64 MB writer buffer, but peak memory usage is higher due to reader caches and importer data. Budget disk space for simultaneous generations. Green CI alone does not establish capacity at the full IMDb catalog size. Test cold startup, sustained query throughput, refresh overlaps, crash recovery, p95/p99 latency and peak memory on production-like data before a high-volume rollout.

## Validation

```bash
RUSTFLAGS='-D warnings' cargo test --workspace
docker build -t matchlab:test .
```
