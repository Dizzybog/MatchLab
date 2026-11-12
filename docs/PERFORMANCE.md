# Parser and matcher throughput audit (Graphify, 2026-11-12)

## Scope and baseline

This audit used Graphify's indexed `main` code graph at commit
`e310d8f1f8ceaace092175330f87b202c139735e` (1,897 code nodes,
5,527 edges), and inspected the actual source of the latest open PR #120
at `49e41cba0a621c67d57e49940af4c6b0059bb432`. The optimization
branch starts *at* that PR head: no banner, country-alias or absolute-episode
fixes from #120 are intentionally replaced.

This is a **static optimization audit, not a performance measurement**.
Do not claim a numeric speedup until before/after benchmarks and representative
loads have been run on identical hardware and metadata. In particular, do not
weaken IMDb identity checks or AniDB/TVDB crosswalks for throughput.

## Critical paths examined

| Area | Existing mechanism | CPU / concurrency consideration | Decision |
| --- | --- | --- | --- |
| Title parsing | Native regex handlers, Aho-Corasick prefilter, optional canonical/ML engines | Hundreds of already-prefiltered handlers; unnecessary secondary work and JSON cloning matter on the hot path | Keep existing prefilter; optimize flat paths and owned path layers |
| Hierarchical releases | Parse each path component independently; cache shared parents | Full `Map<String, Value>` cloned to expose `path_layers` after a previous parse/cache clone | Move the completed owned layer into `path_layers` after metadata merge |
| URL banners from #120 | Balanced Unicode-aware path tokenizer | Scan every UTF-8 character even when the release has no directory separator | Short-circuit the common flat release while preserving bracket-aware path handling |
| Tantivy index access | Global mutex around immutable generation lookup | Even warmed-index reads contend on one exclusive lock under many concurrent requests | Use shared `RwLock` reads, double-checked exclusive insertion only on generation misses |
| Exact / external alias resolution | One indexed lookup per IMDb ID, followed by result union | `N` separate reader acquisitions, posting queries, and result-set merges | Issue one `TermSetQuery` for all IDs and reuse the original document hydration logic |
| Query cache | Per-key singleflight plus global ready-map mutex | Cloning a large candidate vector or diagnostic under the global mutex serializes unrelated titles | Store `Arc<T>`; clone the payload outside the cache mutex |
| IMDb candidate ranking | Exact/prefix/suffix Tantivy discovery, season/year checks and global deterministic scoring | Multiple source paths intentionally compete; changing their order can cause false IMDb winners | Preserve ranking, title fit, year, region, season and vote semantics |
| Season / anime mapping | SQL-backed episode bounds and verified AniDB/TVDB crosswalk | Inadequate evidence must *not* be promoted to SID/ASID | No mapping policy change |
| HTTP matching | Tokio `spawn_blocking` plus concurrent-match semaphore | Already protects async workers from CPU-heavy synchronous matching | Keep existing concurrency limit rather than spawning unbounded extra workers |
| ML learning | Offline evidence/replay workers, bounded queues and cache | Must not add synchronous learning on production request path | No ML behavior change |

## Implemented changes

1. A separator-free release bypasses the bracket-aware character scanner.
2. Path components transfer ownership into diagnostic `path_layers` rather
   than deeply cloning nested parsed values.
3. Immutable warmed Tantivy index generations share a read lock, retaining the
   original fail-closed index-readiness check and bounded generation cache.
4. Explicit multi-IMDb alias resolution uses one Tantivy union query, with the
   same set-based de-duplication and title/identity boundaries as before.
5. The bounded query cache keeps completed results in `Arc<T>`, moving
   potentially costly `T::clone` calls outside its global lock. Each request
   still receives a separately cloned result; API ownership is unchanged.

No new dependencies, schema changes, external APIs or network requests.

## Correctness and concurrency checks

- Existing parser equivalence, release-parity and hierarchical metadata tests.
- PR #120 regressions: Chinese URL-bearing publisher banners, independent path
  coordinates, unverified absolute episode 62, and Australian LEGO Masters.
- New multi-IMDb alias union test compares the one-query result with the union
  of independently queried IDs and verifies AniDB aliases are preserved.
- New cache regression test detects payload cloning while holding the global
  ready-map mutex.
- New eight-reader Tantivy test verifies the same generation returns identical
  results through independent SQLite connections.
- Existing end-to-end server and episode resolution tests should remain green.

## Measurement protocol

Run on the **same CPU architecture, container limits, metadata revision,
dataset and environment variables** before and after this PR:

```bash
cargo test --workspace
cargo test -p matchlab-parser --test chinese_release_path
cargo bench -p matchlab-parser --bench matchlab_parser_bench
```

The Criterion suite includes existing 128-item parallel batches and now adds
flat releases, real bracketed multi-level releases and an equivalent sequential
128-item baseline. For production HTTP tests, measure p50/p95/p99 latency,
requests/s, CPU seconds per 1,000 matches, SQLite read volume, resident memory
and 1/4/8/16/32 in-flight requests. Compare *identical* outputs, not merely
whether requests succeed.

Treat throughput improvements as unverified until results are recorded. Use
the real production workload, including repeated titles and unique titles;
a cache-only benchmark overstates gains.

## Further candidates for a measured follow-up

1. Profile `imdb_matcher::search` candidate normalization and per-ID
   relational eligibility reads under high-cardinality workloads. Preserve
   all title discovery paths: shortening search to one direction is unsafe.
2. Measure per-request Tantivy generation-string lookups; caching revisions
   must remain correct across SQLite snapshots, atomic refresh and WAL updates.
3. Assess whether the bounded single-flight cache has a performance-friendly
   eviction strategy under highly unique traffic. Never make cache error
   results persistent.
4. Benchmark parser handler gate effectiveness on multilingual and bracketed
   releases before modifying the current Aho-Corasick/PCRE2 pipeline.
5. Stress-test the existing `spawn_blocking` semaphore against different
   CPU quotas. Increasing parallel tasks beyond actual cores can raise CPU
   contention and worsen p99 latency.

**Merge policy:** Keep the optimization PR stacked on #120 while that PR is
open. Do not auto-merge. CI and representative before/after performance results
are required before claiming the speed target has been reached.
