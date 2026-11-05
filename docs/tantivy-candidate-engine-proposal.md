# Proposal: Tantivy as the complete IMDb candidate retrieval engine

Status: **Design proposal only**. This change intentionally does not alter runtime matching behavior. Implementation should be split into reviewable follow-up commits/PRs.

## Goal and non-goals

Move IMDb alias retrieval (exact, prefix, reverse suffix, country-qualified, shorter prefix, half-prefix, and anime aliases) to Tantivy while preserving the deterministic IMDb V4 matching semantics and StremThru SQL precedence. Keep SQLite as the authoritative imported metadata store and for remaining relational episode/anime mappings where appropriate. Do **not** replace the torrent parser, introduce a Python backend, or use fuzzy relevance scoring as the authoritative winner.

## Current repository observations

- `imdb_v4.rs` uses Tantivy only for prefix and reverse suffix candidate IDs; other stages still query SQLite.
- `tantivy_search.rs` builds an in-memory index lazily on the first request, under a global mutex, from `SELECT DISTINCT imdb_id,clean_title,year FROM titles WHERE year>0`.
- The current `TopDocs(limit * 8)` cap and year filtering *after* collection can omit the best candidate before IMDb V4 ranks it.
- Per-ID SQLite lookups after Tantivy can multiply SQL work. The index is invalidated using SQLite file mtime.
- `metadata_import.rs` already uses staging SQLite files and publishes after import; `metadata_download.rs` downloads into a temporary workspace.
- `imdb_v4.rs` retains the one-season tolerance: `max_season >= max(1, requested_season - 1)`. **Do not tighten this** without reliable completed-series metadata.

## Proposed schema

Use one versioned Tantivy index with one document per distinct IMDb alias (not one per IMDb ID), including:
- `imdb_id` (stored/exact)
- `clean_title` (exact term, stored)
- `reversed_clean_title` (exact term for suffix prefix enumeration)
- `year` (indexed and stored)
- `title_type` / `category` (indexed and stored)
- `region`, `original`, `ordering` (stored; optionally indexed where justified)
- `alias_source` (IMDb original/AKA, country alias, anime alias)
- optional precomputed `max_season`, `is_anime`, `num_votes`, and alias-count/duplicate metadata, with a defined source of truth and refresh semantics.

Deduplicate only identical alias documents while retaining all distinct aliases for each IMDb ID. Preserve the original alias's identity for deterministic tie-breaking.

## Query contract

Implement a single `CandidateEngine` with explicit ordered stages, not one blended relevance-ranked query:

1. Validate explicit `tt...` ID and its title/season constraints (direct lookup).
2. Country-qualified exact alias for series.
3. Exact normalized IMDb alias; apply the existing anime-vs-film special priority where applicable.
4. Forward prefix; prefer exact, then longest valid title.
5. Shorter-prefix fallback.
6. Reverse suffix (only for keys >= 12 characters; require a genuinely longer alias).
7. Half-prefix fallback (only for keys >= 6 characters).
8. Anime alias fallback and crosswalk validation.

The precise ordering must be verified against the SQL V4 implementation and existing Rust tests before implementation. The current Rust `search` order is the baseline; don't silently reorder stages. Query all aliases that meet each stage's structural conditions, then apply the existing release containment, title-type, season/anime, and ranking gates **before** choosing a winner. The V4 ranker remains authoritative; never use Tantivy score to determine the final IMDb ID.

For candidate bounds, apply year/category constraints inside Tantivy when safe. Year +/-1 is a fast-path optimization only, with a deliberate wider fallback where V4 requires it. Never truncate by arbitrary index/document order before exactness, title length, eligible season, and alias ranking are considered. Use a deterministic, bounded shortlist per stage and a defined overflow policy; compare against exhaustive SQLite reference on large/ambiguous alias sets.

## Persistent publication and lifecycle

Build Tantivy **during metadata import**. Prefer a RAM-resident search index for the active generation, but persist a validated index snapshot on disk for restart recovery. Benchmark `Index::create_in_ram()` against an on-disk index served from the OS page cache: RAM residency does not by itself guarantee a meaningful latency advantage. Keep index construction off the HTTP request path. Build a replacement within the 8 GiB container limit (including overlap with the active generation), and use a fallback or delayed swap if the replacement would exceed the memory budget. Store the validated snapshot in a staging directory on the same filesystem as its published generation. Validate both SQLite and Tantivy, including row counts, schema version, sample exact/prefix/suffix queries and index openability, before publication. Publish a single generation manifest/pointer referring to both assets; switch the active generation atomically, rather than attempting two independent renames. Keep the previous generation for rollback. On startup, open the persisted index snapshot and warm its search pages, or restore it into RAM if benchmarks justify the additional copy and startup cost; never rebuild millions of aliases synchronously in an HTTP request.

A reader must hold a stable generation handle for the duration of its search. Avoid a process-wide lock around index construction or searching. If the index is absent, incompatible, or corrupt, use a controlled SQLite fallback (or fail readiness with an explicit diagnostic), not an invisible partial search. Respect the 1 CPU / 8 GiB deployment constraints with bounded writer memory and measured disk headroom.

## Precomputed ranking facts

Consider precomputing duplicate clean-title flags, alias counts, original/US alias preferences, IMDb vote counts, and season maxima at import. Keep anime episode crosswalks relational until parity is demonstrated. Any precomputed facts must be versioned with the SQLite generation and must preserve V4 tie-breakers.

## Regression and performance gates

- Compare old and new engines on a fixed corpus and report IMDb ID, stage, alias, and rank components for mismatches.
- Cover `Supernatural S02` with the existing +1 tolerance **unchanged**, ongoing seasons, series vs movie, country-qualified names, anime-over-film, AniDB episode mapping, short title collisions, prefix/suffix, half-prefix, and ambiguous aliases with >100 candidate IDs.
- Verify year missing/present and +/-1 fallback behavior, metadata refresh, crash during staging/publication, rollback, concurrent requests, and corrupt/missing index.
- Benchmark cold start, warm p50/p95/p99 query latency, peak RSS, import time, index size, and 1 CPU / 8 GiB limits for both in-RAM and OS-cached disk-index variants. Record baseline before claiming speedups.
- Ensure `cargo test --workspace` and existing CI pass.

## Implementation milestones

1. Add golden-case parity harness and baseline benchmarks.
2. Introduce RAM-first Tantivy serving with a persistent recovery snapshot and versioned generation publication, retaining current matching semantics.
3. Move exact/country/shorter-prefix/half-prefix/anime candidate retrieval behind `CandidateEngine` stage-by-stage.
4. Move precomputed rank facts only after proving result parity.
5. Remove obsolete SQLite search paths/indexes only after fallback, migration and benchmark evidence.

## Acceptance criteria

- No changed winner for established V4 golden cases unless documented as an intentional bug fix.
- No on-request full-index rebuild, no global query/build mutex, no arbitrary TopDocs truncation that changes winners.
- All IMDb title candidate retrieval goes through the unified engine; SQLite remains authoritative for metadata and allowed relational validation.
- Atomic, recoverable metadata+index publication; bounded memory; successful CI and measured benchmarks.
