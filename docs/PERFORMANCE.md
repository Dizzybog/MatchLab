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

## Second-pass CPU audit — Graphify index `60927db` (November 12, 2026)

Graphify reindexed the merged #120/#121 state (1,913 nodes / 5,567
relationships). Source-level review identified the following additional
work. Each item must preserve production `/match`, `/debug/match`,
`/parse`, Unicode ranking and verified SID/ASID behavior.

| Component | Optimization | Correctness barrier |
| --- | --- | --- |
| `tantivy_search::titles_by_ids` | One `TermSetQuery` instead of a loop of `TermQuery` calls | Exact ID union; no alias loss or TopDocs truncation |
| `main::diagnose_logged` | One shared metadata/WAL fingerprint for both cache namespace and request retry gates | Recheck after computation, replay on generation change and fail closed on a second change |
| `main::parse_cached` | Clone shared `Arc<Value>` after releasing the global mutex | Exact parser generation and 30-second TTL |
| `imdb_matcher::filter_eligible` | Query unique IMDb candidates in at most 256-ID batches; separate season bounds from AniDB fallback | Same season threshold, SQL fallback, aliases and input order |
| `anime_title::resolve_range` | Preselect part aliases/family/rows, resolve ordinary and remapped episode ranges in one crosswalk snapshot | Per-episode uniqueness, season bounds and exact AniDB identity |
| `matchlab-parser::parse_context_impl` | Skip rescanning prefilter after edge removals; rescan interior deletions | No newly joined literal can be created by removing a complete prefix or suffix |
| `cached_path_component` | Clone parsed JSON after dropping the shared cache lock | Never reuse an obsolete parser-patch generation |
| `imdb_matcher::search` | Reuse Unicode title tokens and source length in candidate rank computation | Same title-fit tiers and tie-break scores |
| `dashboard_history::record_async` | Move already-serialized diagnostics into bounded history queue | History remains nonblocking; errors and selected ID stay recorded |

### Reproduction / verification

```bash
cargo test --workspace
cargo test -p matchlab-parser
cargo test -p matchlab-server
cargo bench -p matchlab-parser --bench matchlab_parser_bench
```

The parser benchmark includes 128- and 512-item sequential/parallel
comparisons with common title roots, anime, CJK URL banners and TV path
hierarchies. Benchmark execution (not merely compilation) must be run
on the target CPU quota with the same metadata and environment in both
branches; benchmark setup and caches can strongly influence the result.
For production, additionally compare 1/2/4/8/16/32 concurrent HTTP
`/match` requests on cold and warm candidate sets while recording
CPU seconds per 1000 calls, p50/p95/p99 latency, allocations and peak RSS.
No numerical gains are asserted without those measurements.

### Non-goals / invariants

- No entire-database SQLite title scans; title identity discovery remains
  Tantivy-only. Season eligibility, IMDb ratings and verified AniDB/TVDB
  crosswalks remain relational.
- No guessed episode defaults, altered rank tie-breakers, early TopDocs
  truncation, or shared final SID across distinct full torrent paths.
- No unbounded worker spawning. `MATCHLAB_MAX_CONCURRENT_MATCHES`
  remains configurable; on a single CPU, higher configured concurrency
  may harm throughput/latency and needs load testing.
- No automatic activation of unverified ML rules and no API/schema changes.

## Third-pass Graphify CPU review — main `0456e4f`

Graphify reindexed the merged PR #122 tree on 2026-11-12
(1,918 nodes, 5,584 edges). The additional improvements below reduce
unnecessary work in candidate enrichment, crosswalk ranges, and high-cardinality
cache traffic; they do **not** change the index or matching contracts.

- **IMDb shortlist enrichment:** Candidate discovery, title-fit ranking and
  tie retention still happen first. For more than four surviving IMDb works,
  fetch votes and anime IDs with SQL `IN` batches of at most 200 IDs.
  Restrict AniDB fallback to identities without an explicit AniDB ID.
  Preserve the direct, low-overhead query path for short lists, and retain
  missing-ratings-as-zero behavior.
- **Explicit anime episode maps:** Convert `episode_map.map` into validated
  pairs once per metadata snapshot, not once for each episode. Avoid building
  temporary filtered mapping arrays for every absolute episode.
  The crosswalk still checks each episode's separate ambiguity and permitted
  SID/ASID ranges. Tests compare 120 episode intervals to independent scalar
  conversions, including non-bijective mappings.
- **Strict season-range gates:** For more than four candidate IMDb identities
  surviving the public year gate, read season limits in batches and retain
  the original result order. Missing or invalid season bounds still reject
  candidates. A 345-candidate fixture compares scalar/batch results.
- **High-cardinality single-flight cache:** Replace the O(capacity)
  oldest-entry search on each insertion with a bounded FIFO insertion-order
  queue and unique sequence tags. Expired/reinserted keys never evict their
  replacement through stale tickets. Queue compaction is infrequent and
  bounded; error results, TTLs and generation-scoped keys remain unchanged.

### Validation and measurement

```sh
cargo test --workspace
cargo test -p matchlab-server
cargo test -p matchlab-parser
cargo check --workspace --all-targets
cargo bench -p matchlab-parser --bench matchlab_parser_bench
```

CI includes Rust tests and native ARM64/AMD64 build and ML smoke checks.
Benchmark *execution* is a separate step. For any throughput or CPU-gain
claim compare this PR against `0456e4f` on equivalent hardware, identical
metadata, cold/warm lookup mixes, 1/2/4/8/16/32 concurrent callers and
identical response JSON. Record CPU seconds per 1,000 matches, latency
p50/p95/p99, requests/second and peak memory. No speedup has yet been measured.
